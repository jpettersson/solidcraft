//! The control channel in the browser (`?control=<url>`).
//!
//! A page cannot listen on a socket, so the connection is inverted: the page long-polls
//! `<url>/next` for one request at a time (`{"id", "method", "params", "files"?}`), runs it
//! through the same handler as the desktop app's `--control` server and POSTs the reply line to
//! `<url>/reply`. A relay on the host speaks the ordinary JSON-lines protocol to agents and
//! `solidcraft-cli mcp --connect` (docs/control-protocol.md, "Web app").
//!
//! The page cannot reach the host's disk, so files cross beside the message, base64-encoded:
//! a request's `files` (`{path: data}`) go into the engine's file table before it runs, so
//! `doc.open` finds them; a reply carries the file its `result.path` names when the request
//! wrote it there (`ui.screenshot`, `ui.render`, `file.export`, `file.save_mesh`), for the relay
//! to write to that path. Nothing else is sent back: a read-only request never writes the host.
//!
//! A design the page has open is kept by the page itself, so its own file must be one the page
//! stores. `file.save`/`file.save_as` to a host path are refused (a later Save in the browser
//! would only reach the page's memory), and a design opened from a host path is kept in the
//! browser as a copy, as one opened from the computer is.

use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use serde_json::{Map, Value, json};
use solidcraft_engine::io::vfs;
use solidcraft_ui_egui::ControlRequest;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::base64;
use crate::files;

/// How often a running request is checked for its reply.
const POLL_MS: i32 = 10;
/// A request the app has not answered by then is replied to with `timeout`, as natively.
const REPLY_TIMEOUT_MS: f64 = 60_000.0;
/// Waits between attempts while the relay cannot be reached.
const RETRY_MIN_MS: i32 = 500;
const RETRY_MAX_MS: i32 = 10_000;
/// Requests whose `result.path` names a file they wrote to a host path, carried back.
const WRITERS: [&str; 4] = ["ui.screenshot", "ui.render", "file.export", "file.save_mesh"];
/// Requests that make `path` the open design's own file.
const SAVERS: [&str; 2] = ["file.save", "file.save_as"];
/// Requests that open `path` as a design (`ui.file` unless it inserts).
const OPENERS: [&str; 2] = ["doc.open", "ui.file"];

/// Largest file carried in either direction.
const MAX_FILE_BYTES: usize = 48 << 20;
/// Files one request may carry.
const MAX_FILES: usize = 8;

/// The relay's base URL from `?control=<url>`, when it is this page's own origin or loopback.
/// Anything else is refused: a link must not hand someone's design to a remote server.
pub fn url_from_query(search: &str) -> Option<String> {
    let raw = web_sys::UrlSearchParams::new_with_str(search).ok()?.get("control")?;
    let location = web_sys::window()?.location();
    let here = location.href().ok()?;
    let url = web_sys::Url::new_with_base(raw.trim(), &here).ok()?;
    let loopback = matches!(url.hostname().as_str(), "127.0.0.1" | "localhost" | "[::1]");
    let same_origin = location.origin().ok().is_some_and(|o| o == url.origin());
    if !(same_origin || loopback) || !matches!(url.protocol().as_str(), "http:" | "https:") {
        log::warn!("control: refused {raw:?} (only this page's origin or loopback may control it)");
        return None;
    }
    url.set_search("");
    url.set_hash("");
    Some(url.href().trim_end_matches('/').to_string())
}

/// Start polling `base` and return the requests for [`solidcraft_ui_egui::SolveApp::with_control`].
pub fn start(base: String, ctx: egui::Context) -> Receiver<ControlRequest> {
    let (tx, rx) = channel();
    // Tells the relay which page is polling, so the newest one takes over from an older one.
    let tab = format!("{:08x}", (js_sys::Math::random() * f64::from(u32::MAX)) as u32);
    log::info!("control: polling {base} as tab {tab}");
    wasm_bindgen_futures::spawn_local(run(base, tab, tx, ctx));
    rx
}

async fn run(base: String, tab: String, tx: Sender<ControlRequest>, ctx: egui::Context) {
    let mut wait = RETRY_MIN_MS;
    loop {
        let url = format!("{base}/next?tab={tab}&hidden={}", u8::from(hidden()));
        let msg = match fetch(&url, None).await {
            Ok((200, body)) => body,
            // The long poll ended with nothing to do.
            Ok((204, _)) => {
                wait = RETRY_MIN_MS;
                continue;
            }
            // A newer page took over the channel; polling again would take it back.
            Ok((409, _)) => {
                log::warn!("control: another page took over {base}; reload to control this one again");
                return;
            }
            Ok((status, body)) => {
                log::warn!("control: {url}: HTTP {status} {}", body.chars().take(200).collect::<String>());
                sleep(wait).await;
                wait = (wait * 2).min(RETRY_MAX_MS);
                continue;
            }
            Err(e) => {
                log::warn!("control: {url}: {e}");
                sleep(wait).await;
                wait = (wait * 2).min(RETRY_MAX_MS);
                continue;
            }
        };
        wait = RETRY_MIN_MS;
        let reply = match serde_json::from_str::<Value>(&msg) {
            Ok(m) => match serve(&m, &tx, &ctx).await {
                Some(r) => r,
                None => return,
            },
            Err(e) => json!({"id": null, "ok": false, "error": format!("bad JSON: {e}")}),
        };
        if let Err(e) = fetch(&format!("{base}/reply?tab={tab}"), Some(&reply.to_string())).await {
            log::warn!("control: reply: {e}");
        }
    }
}

/// Run one request; `None` once the app is gone.
async fn serve(msg: &Value, tx: &Sender<ControlRequest>, ctx: &egui::Context) -> Option<Value> {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let mut params = msg.get("params").cloned().unwrap_or(json!({}));
    let command = command_of(&method, &params);
    if let Some(e) = refused(&command, args(&method, &params)) {
        return Some(json!({"id": id, "ok": false, "error": e}));
    }
    let incoming = match take_files(msg.get("files")) {
        Ok(paths) => paths,
        Err(e) => return Some(json!({"id": id, "ok": false, "error": e})),
    };
    if OPENERS.contains(&command.as_str())
        && args(&method, &params).get("insert").and_then(Value::as_bool) != Some(true)
        && let Some(a) = args_mut(&method, &mut params)
    {
        keep_opened_copy(a, &incoming);
    }
    let (req, rrx) = ControlRequest::new(method, params);
    tx.send(req).ok()?;
    ctx.request_repaint();
    let deadline = js_sys::Date::now() + REPLY_TIMEOUT_MS;
    let mut reply = loop {
        match rrx.try_recv() {
            Ok(r) => break r,
            Err(TryRecvError::Disconnected) => break json!({"ok": false, "error": "timeout"}),
            Err(TryRecvError::Empty) if js_sys::Date::now() >= deadline => break json!({"ok": false, "error": "timeout"}),
            Err(TryRecvError::Empty) => sleep(POLL_MS).await,
        }
    };
    if let Some(o) = reply.as_object_mut() {
        if let Some(out) = written_file(&command, o, &incoming) {
            o.insert("files".into(), out);
        }
        o.insert("id".into(), id);
    }
    Some(reply)
}

/// Put a request's files into the engine's file table; the paths it brought.
fn take_files(files: Option<&Value>) -> Result<BTreeSet<String>, String> {
    let mut paths = BTreeSet::new();
    let Some(files) = files.and_then(Value::as_object) else { return Ok(paths) };
    if files.len() > MAX_FILES {
        return Err(format!("at most {MAX_FILES} files per request"));
    }
    for (path, data) in files {
        if !is_host_path(path) {
            return Err(format!("{path}: not a host path"));
        }
        let bytes = data.as_str().and_then(base64::decode).ok_or_else(|| format!("{path}: bad base64"))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!("{path}: file too large"));
        }
        vfs::insert(path, bytes);
        paths.insert(path.clone());
    }
    Ok(paths)
}

/// The command a request runs: `engine.execute`'s (or `command`'s) `command`, else the method.
fn command_of(method: &str, params: &Value) -> String {
    match method {
        "engine.execute" | "command" => params.get("command").or_else(|| params.get("id")).and_then(Value::as_str).unwrap_or("").to_string(),
        _ => method.to_string(),
    }
}

/// The command's own parameters (`engine.execute` nests them under `params`).
fn args<'a>(method: &str, params: &'a Value) -> &'a Value {
    match method {
        "engine.execute" | "command" => params.get("params").unwrap_or(&Value::Null),
        _ => params,
    }
}

fn args_mut<'a>(method: &str, params: &'a mut Value) -> Option<&'a mut Value> {
    match method {
        "engine.execute" | "command" => params.get_mut("params"),
        _ => Some(params),
    }
}

/// Why the page won't run this request, if it won't: a save to a host path would make the host
/// file the design's own, and a later Save in the browser can't reach it (it would stay in the
/// page's memory while saying "Save done").
fn refused(command: &str, args: &Value) -> Option<String> {
    let path = args.get("path").and_then(Value::as_str)?;
    if !SAVERS.contains(&command) || !is_host_path(path) {
        return None;
    }
    Some(format!(
        "{command}: this page can't keep the design at the host path {path} (a later Save in the browser would not reach it). Use file.export {{\"path\": \"{path}\"}} for a copy on the host, or save under {}",
        files::OPFS
    ))
}

/// A design opened from a host path is kept in the browser (a copy, as one opened from the
/// computer is), so the person's later saves are stored: the request opens the copy instead.
fn keep_opened_copy(args: &mut Value, incoming: &BTreeSet<String>) {
    let Some(path) = args.get("path").and_then(Value::as_str).map(str::to_string) else { return };
    if !incoming.contains(&path) || !solidcraft_engine::io::is_design_path(&path) {
        return;
    }
    let Ok(bytes) = vfs::read(&path, MAX_FILE_BYTES) else { return };
    let name = path.rsplit('/').next().unwrap_or("design.solidcraft");
    // The same design opened again reuses its copy.
    let same = format!("{}{name}", files::OPFS);
    let copy = if vfs::read(&same, MAX_FILE_BYTES).is_ok_and(|b| b == bytes) { same } else { files::free_name(name) };
    if vfs::write(&copy, &bytes).is_err() {
        return;
    }
    let _ = vfs::remove(&path);
    if let Some(o) = args.as_object_mut() {
        o.insert("path".into(), Value::String(copy));
    }
}

/// The file a successful reply's `result.path` names, when this request wrote it to a host
/// path: `{path: base64}`. Only requests that write files ([`WRITERS`]) send one: a reply that
/// merely mentions a path (`document.inspect`) never writes the host. It leaves the page's
/// memory once handed over.
fn written_file(command: &str, reply: &Map<String, Value>, incoming: &BTreeSet<String>) -> Option<Value> {
    if !WRITERS.contains(&command) || reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let path = reply.get("result")?.get("path")?.as_str()?;
    if !is_host_path(path) || incoming.contains(path) || !vfs::exists(path) {
        return None;
    }
    let bytes = vfs::read(path, MAX_FILE_BYTES).ok()?;
    let _ = vfs::remove(path);
    let mut out = Map::new();
    out.insert(path.to_string(), Value::String(base64::encode(&bytes)));
    Some(Value::Object(out))
}

/// A path on the host, not one of the browser's own (stored designs, downloads, uploads).
fn is_host_path(path: &str) -> bool {
    path.starts_with('/') && ![files::OPFS, files::DOWNLOADS, files::UPLOAD].iter().any(|p| path.starts_with(p)) && !path.contains('\0')
}

fn hidden() -> bool {
    web_sys::window().and_then(|w| w.document()).is_some_and(|d| d.hidden())
}

async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
}

/// GET `url`, or POST `body` to it: the status and the response text.
async fn fetch(url: &str, body: Option<&str>) -> Result<(u16, String), String> {
    let window = web_sys::window().ok_or("no window")?;
    let init = web_sys::RequestInit::new();
    init.set_method(if body.is_some() { "POST" } else { "GET" });
    init.set_cache(web_sys::RequestCache::NoStore);
    if let Some(b) = body {
        init.set_body(&JsValue::from_str(b));
    }
    let resp = JsFuture::from(window.fetch_with_str_and_init(url, &init)).await.map_err(js_err)?;
    let resp: web_sys::Response = resp.dyn_into().map_err(|_| "not a response".to_string())?;
    let text = JsFuture::from(resp.text().map_err(js_err)?).await.map_err(js_err)?;
    Ok((resp.status(), text.as_string().unwrap_or_default()))
}

fn js_err(e: JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}
