//! Designs kept on a host's disk (`?store=<url>`): an HTTP store is the source of truth, and the
//! browser's private storage (OPFS) only a working copy and the place a save waits while the
//! store can't be reached.
//!
//! The protocol is four requests (docs/web-store.md):
//! - `GET <url>/designs[?wait=<version>]`: `{version, designs: [{name, size, modified, etag}]}`;
//!   with `wait`, the store may hold the answer until the listing is no longer `version`.
//! - `GET <url>/designs/<name>`: the bytes, with their `ETag`.
//! - `PUT <url>/designs/<name>` with `If-Match: "<etag>"` (the version the save is based on) or
//!   `If-None-Match: *` (a new file): `{name, etag, size, modified}`, or **412** when the file
//!   changed in the meantime (nothing is written).
//!
//! The page mirrors the store into the engine's files under [`STORE`] (only changed files are
//! fetched; the OPFS copy makes a reload or an offline start instant), so opening a design, the
//! start page and Save work as they do with OPFS. Each open design remembers the version it was
//! based on. A save the store refuses with 412 goes beside the file as `<name> (conflict
//! <time>).<ext>` and the open design saves there from then on: a save never overwrites what
//! another device, an agent or git wrote.

/// Longest name the store accepts.
pub const MAX_NAME: usize = 200;
/// Most designs a listing may hold.
const MAX_ENTRIES: usize = 10_000;

/// A version of a stored file, as the store reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Remote {
    pub etag: String,
    pub size: u64,
    /// Last written (ms since 1970; 0 when unknown).
    pub modified: f64,
}

/// A name the store keeps: a bare file name (no folders, no leading dot, no control
/// characters) ending in `.solidcraft` or `.json`, at most [`MAX_NAME`] bytes.
pub fn valid_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.trim() == name
        && !name.starts_with('.')
        && !name.chars().any(|c| c == '/' || c == '\\' || c.is_control())
        && (lower.ends_with(".solidcraft") || lower.ends_with(".json"))
        && lower != ".solidcraft"
        && lower != ".json"
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (name.get(..i).unwrap_or(name), name.get(i..).unwrap_or("")),
        _ => (name, ""),
    }
}

/// `stem (conflict <stamp>).ext`, shortened to fit [`MAX_NAME`].
pub fn conflict_name(name: &str, stamp: &str) -> String {
    let (stem, ext) = split_ext(name);
    let tail = format!(" (conflict {stamp}){ext}");
    let mut stem = stem.to_string();
    while stem.len() + tail.len() > MAX_NAME && stem.pop().is_some() {}
    format!("{stem}{tail}")
}

/// `name` if `taken` says it is free, else `stem (2).ext`, `stem (3).ext`…
pub fn free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    for i in 2..10_000 {
        let n = format!("{stem} ({i}){ext}");
        if !taken(&n) {
            return n;
        }
    }
    format!("{stem} (copy){ext}")
}

/// An `ETag` header's value without quotes or the weak marker.
pub fn etag_value(header: &str) -> String {
    let h = header.trim();
    let h = h.strip_prefix("W/").unwrap_or(h);
    h.trim_matches('"').to_string()
}

/// The store's listing: its version and the valid names in it.
pub fn parse_listing(text: &str) -> Result<(String, std::collections::BTreeMap<String, Remote>), String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("the store's listing is not JSON: {e}"))?;
    let designs = v.get("designs").and_then(serde_json::Value::as_array).ok_or("the store's listing has no `designs`")?;
    let mut out = std::collections::BTreeMap::new();
    for d in designs.iter().take(MAX_ENTRIES) {
        let (Some(name), Some(etag)) = (d.get("name").and_then(|x| x.as_str()), d.get("etag").and_then(|x| x.as_str())) else { continue };
        if !valid_name(name) || etag.is_empty() {
            continue;
        }
        let size = d.get("size").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let modified = d.get("modified").and_then(serde_json::Value::as_f64).filter(|m| m.is_finite()).unwrap_or(0.0);
        out.insert(name.to_string(), Remote { etag: etag_value(etag), size, modified });
    }
    let version = match v.get("version") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(x @ serde_json::Value::Number(_)) => x.to_string(),
        _ => String::new(),
    };
    Ok((version, out))
}

/// The store's URL from `?store=<url>`, when it is the page's own origin (`origin`) or loopback,
/// over http(s); `None` otherwise. A link must not send someone's designs to another server.
pub fn allowed_url(raw: &str, origin: &str) -> bool {
    let raw = raw.trim();
    let Some((scheme, rest)) = raw.split_once("://") else { return false };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host =
        if authority.starts_with('[') { authority.split_inclusive(']').next().unwrap_or("") } else { authority.split(':').next().unwrap_or("") };
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "[::1]");
    loopback || format!("{scheme}://{authority}") == origin
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["Bracket.solidcraft", "a b (2).solidcraft", "part.json", "Ünïcode.SOLIDCRAFT"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".solidcraft",
            ".hidden.solidcraft",
            "a/b.solidcraft",
            "a\\b.solidcraft",
            "x.step",
            "x.solidcraft ",
            " x.solidcraft",
            "x\0.solidcraft",
            "line\nbreak.solidcraft",
            "../x.solidcraft",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(!valid_name(&format!("{}.solidcraft", "a".repeat(200))));
    }

    #[test]
    fn conflict_names_keep_the_extension_and_fit() {
        assert_eq!(conflict_name("Bracket.solidcraft", "2026-10-14 15.32.07"), "Bracket (conflict 2026-10-14 15.32.07).solidcraft");
        let long = format!("{}.solidcraft", "a".repeat(195));
        let c = conflict_name(&long, "2026-10-14 15.32.07");
        assert!(c.len() <= MAX_NAME && valid_name(&c), "{c}");
        assert!(c.ends_with(" (conflict 2026-10-14 15.32.07).solidcraft"));
    }

    #[test]
    fn free_names_count_up() {
        let taken = ["a.solidcraft", "a (2).solidcraft"];
        assert_eq!(free_name("a.solidcraft", |n| taken.contains(&n)), "a (3).solidcraft");
        assert_eq!(free_name("b.solidcraft", |n| taken.contains(&n)), "b.solidcraft");
    }

    #[test]
    fn etags_and_listings() {
        assert_eq!(etag_value("\"abc\""), "abc");
        assert_eq!(etag_value("W/\"abc\""), "abc");
        let (v, l) = parse_listing(
            r#"{"version":"v1","designs":[{"name":"a.solidcraft","etag":"e1","size":10,"modified":5},
                {"name":"../x.solidcraft","etag":"e2"},{"name":"b.json"},{"name":"c.json","etag":"\"e3\""}]}"#,
        )
        .unwrap();
        assert_eq!(v, "v1");
        assert_eq!(l.keys().collect::<Vec<_>>(), ["a.solidcraft", "c.json"]);
        assert_eq!(l["a.solidcraft"], Remote { etag: "e1".into(), size: 10, modified: 5.0 });
        assert_eq!(l["c.json"].etag, "e3");
        assert!(parse_listing("[]").is_err());
        assert!(parse_listing("nope").is_err());
    }

    #[test]
    fn only_the_pages_origin_or_loopback() {
        let o = "https://cad.example.org";
        assert!(allowed_url("https://cad.example.org/api/app/solidcraft-store", o));
        assert!(allowed_url("http://127.0.0.1:8080/store", o));
        assert!(allowed_url("http://localhost/store", o));
        assert!(allowed_url("http://[::1]:9/store", o));
        assert!(!allowed_url("https://evil.example/store", o));
        assert!(!allowed_url("https://cad.example.org.evil.example/store", o));
        assert!(!allowed_url("https://cad.example.org@evil.example/store", o));
        assert!(!allowed_url("http://127.0.0.1@evil.example/", o));
        assert!(!allowed_url("ftp://127.0.0.1/", o));
        assert!(!allowed_url("javascript:alert(1)", o));
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::*;

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::rc::Rc;

    use js_sys::Uint8Array;
    use solidcraft_engine::io::vfs;
    use wasm_bindgen::{JsCast as _, JsValue};
    use wasm_bindgen_futures::{JsFuture, spawn_local};

    use super::*;
    use crate::files;

    /// Where stored designs appear to the engine.
    pub const STORE: &str = "/store/";
    /// Largest design sent to or read from the store.
    pub const MAX_BYTES: usize = 256 << 20;

    /// How long a failed request waits before the next try (doubling up to the most).
    const RETRY_MIN_MS: f64 = 2_000.0;
    const RETRY_MAX_MS: f64 = 60_000.0;
    /// A store that answers a `wait` listing at once (it doesn't hold) is asked this often.
    const POLL_MS: i32 = 5_000;
    /// The cache's own files (never a valid design name: they start with a dot).
    const INDEX: &str = ".index.json";
    const PENDING_PREFIX: &str = ".pending.";

    /// What a save is, for the status line.
    #[derive(Clone, Debug, PartialEq)]
    enum Kind {
        Save,
        /// The one-time copy of a design kept only in this browser.
        Copy,
        /// A save that met a newer version on disk, written beside it.
        Conflict {
            of: String,
        },
    }

    /// Bytes on their way to the disk.
    #[derive(Clone)]
    struct Push {
        bytes: Rc<Vec<u8>>,
        /// The version it replaces (`None`: a new file).
        base: Option<String>,
        kind: Kind,
        /// Bumped by every save of the name, so an answer for older bytes isn't mistaken for these.
        generation: u64,
        /// The store refused it (not a network failure): it waits for Push now.
        stuck: bool,
        /// The person was told it isn't on disk.
        told: bool,
    }

    enum Done {
        Saved { name: String, generation: u64, remote: Remote },
        Conflict { name: String, current: Option<String> },
        Unreachable { name: String, error: String },
        Refused { name: String, error: String },
    }

    enum CacheOp {
        Write(String, Rc<Vec<u8>>),
        Remove(String),
        Index,
    }

    /// A stored design as the file panel lists it.
    #[derive(Clone, Debug)]
    pub struct StoreEntry {
        pub name: String,
        pub size: u64,
        pub modified: f64,
        /// Saved here but not on disk yet.
        pub pending: bool,
    }

    /// The store: what the host has, what this page holds, and what is on its way.
    pub struct Store {
        pub url: String,
        /// The latest listing (the host's truth), once one has arrived.
        remote: RefCell<Option<BTreeMap<String, Remote>>>,
        version: RefCell<String>,
        /// The versions the engine's files hold under [`STORE`] (and the OPFS copy).
        known: RefCell<BTreeMap<String, Remote>>,
        /// The version each open design was based on (`None`: a new file).
        pins: RefCell<BTreeMap<String, Option<String>>>,
        queue: RefCell<BTreeMap<String, Push>>,
        generation: Cell<u64>,
        /// Names with a request in flight.
        busy: RefCell<BTreeSet<String>>,
        fetched: RefCell<Vec<(String, Remote, Vec<u8>)>>,
        done: RefCell<Vec<Done>>,
        /// For the status line: (text, error).
        pub messages: RefCell<Vec<(String, bool)>>,
        /// The last request reached the store (`None`: none finished yet).
        online: Cell<Option<bool>>,
        last_error: RefCell<String>,
        /// When pushes may be tried again after a failure (ms since 1970), and the wait after that.
        retry_at: Cell<f64>,
        backoff: Cell<f64>,
        /// The designs listed changed (the start page's recent designs follow).
        pub changed: Cell<bool>,
        /// Designs from this browser's own storage already copied to the store.
        copied: RefCell<BTreeSet<String>>,
        /// The one-time copy offer was answered.
        pub offer_answered: Cell<bool>,
        /// Open designs that changed on disk, and the version the person was told about.
        told_changed: RefCell<BTreeMap<String, String>>,
        /// The OPFS folder of this store's copies (`None`: the browser has no OPFS).
        cache: Option<String>,
        cache_ops: RefCell<VecDeque<CacheOp>>,
        cache_running: Cell<bool>,
        /// The copy in OPFS has been read (the listing is usable even offline).
        pub loaded: Cell<bool>,
        ctx: RefCell<Option<egui::Context>>,
    }

    /// The store's URL from `?store=<url>`, absolute, when it is this page's origin or loopback.
    pub fn url_from_query(search: &str) -> Option<String> {
        let raw = web_sys::UrlSearchParams::new_with_str(search).ok()?.get("store")?;
        let location = web_sys::window()?.location();
        let url = web_sys::Url::new_with_base(raw.trim(), &location.href().ok()?).ok()?;
        url.set_search("");
        url.set_hash("");
        let href = url.href().trim_end_matches('/').to_string();
        if !allowed_url(&href, &location.origin().ok()?) {
            log::warn!("store: refused {raw:?} (only this page's origin or loopback may keep its designs)");
            return None;
        }
        Some(href)
    }

    fn now() -> f64 {
        js_sys::Date::now()
    }

    /// Local time as `2026-10-14 15.32.07` (no colons: file systems that refuse them).
    fn stamp() -> String {
        let d = js_sys::Date::new_0();
        format!(
            "{:04}-{:02}-{:02} {:02}.{:02}.{:02}",
            d.get_full_year(),
            d.get_month() + 1,
            d.get_date(),
            d.get_hours(),
            d.get_minutes(),
            d.get_seconds()
        )
    }

    /// FNV-1a: a short stable folder name for a store URL.
    fn fnv(s: &str) -> u64 {
        s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
    }

    fn js_err(e: JsValue) -> String {
        e.as_string().or_else(|| js_sys::Reflect::get(&e, &"message".into()).ok().and_then(|m| m.as_string())).unwrap_or_else(|| format!("{e:?}"))
    }

    async fn sleep(ms: i32) {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            if let Some(w) = web_sys::window() {
                let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
            }
        });
        let _ = JsFuture::from(promise).await;
    }

    /// An HTTP answer: status, `ETag`, body.
    struct Answer {
        status: u16,
        etag: Option<String>,
        body: Vec<u8>,
    }

    async fn request(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<Answer, String> {
        let window = web_sys::window().ok_or("no window")?;
        let init = web_sys::RequestInit::new();
        init.set_method(method);
        init.set_cache(web_sys::RequestCache::NoStore);
        let h = web_sys::Headers::new().map_err(js_err)?;
        for (k, v) in headers {
            h.set(k, v).map_err(js_err)?;
        }
        init.set_headers(&h);
        if let Some(b) = body {
            init.set_body(&Uint8Array::from(b).into());
        }
        let resp = JsFuture::from(window.fetch_with_str_and_init(url, &init)).await.map_err(js_err)?;
        let resp: web_sys::Response = resp.dyn_into().map_err(|_| "not a response".to_string())?;
        let etag = resp.headers().get("etag").ok().flatten().map(|e| etag_value(&e));
        let buf = JsFuture::from(resp.array_buffer().map_err(js_err)?).await.map_err(js_err)?;
        let body = Uint8Array::new(&buf);
        if body.length() as usize > MAX_BYTES {
            return Err("the answer is too large".into());
        }
        Ok(Answer { status: resp.status(), etag, body: body.to_vec() })
    }

    /// The store's error text (`{"error": …}` or plain text), short.
    fn reason(a: &Answer) -> String {
        let text = String::from_utf8_lossy(&a.body);
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| text.trim().to_string());
        let msg: String = msg.chars().take(200).collect();
        if msg.is_empty() { format!("HTTP {}", a.status) } else { format!("HTTP {}: {msg}", a.status) }
    }

    /// The proxy or the store itself is down (try again later), not a refusal.
    fn unreachable(status: u16) -> bool {
        status == 0 || status == 408 || status == 429 || status >= 500
    }

    fn encode(name: &str) -> String {
        js_sys::encode_uri_component(name).as_string().unwrap_or_default()
    }

    impl Store {
        pub fn new(url: String) -> Store {
            let cache = (!files::local_only()).then(|| format!("store-{:016x}", fnv(&url)));
            Store {
                url,
                remote: RefCell::new(None),
                version: RefCell::new(String::new()),
                known: RefCell::new(BTreeMap::new()),
                pins: RefCell::new(BTreeMap::new()),
                queue: RefCell::new(BTreeMap::new()),
                generation: Cell::new(0),
                busy: RefCell::new(BTreeSet::new()),
                fetched: RefCell::new(Vec::new()),
                done: RefCell::new(Vec::new()),
                messages: RefCell::new(Vec::new()),
                online: Cell::new(None),
                last_error: RefCell::new(String::new()),
                retry_at: Cell::new(0.0),
                backoff: Cell::new(RETRY_MIN_MS),
                changed: Cell::new(false),
                copied: RefCell::new(BTreeSet::new()),
                offer_answered: Cell::new(false),
                told_changed: RefCell::new(BTreeMap::new()),
                cache,
                cache_ops: RefCell::new(VecDeque::new()),
                cache_running: Cell::new(false),
                loaded: Cell::new(false),
                ctx: RefCell::new(None),
            }
        }

        pub fn set_ctx(&self, ctx: &egui::Context) {
            if self.ctx.borrow().is_none() {
                *self.ctx.borrow_mut() = Some(ctx.clone());
            }
        }

        fn wake(&self) {
            if let Some(c) = self.ctx.borrow().as_ref() {
                c.request_repaint();
            }
        }

        fn say(&self, text: impl Into<String>, error: bool) {
            self.messages.borrow_mut().push((text.into(), error));
            self.wake();
        }

        /// The last request reached the store; `None` until one has finished.
        pub fn online(&self) -> Option<bool> {
            self.online.get()
        }

        pub fn last_error(&self) -> String {
            self.last_error.borrow().clone()
        }

        /// Designs saved here that are not on disk yet.
        pub fn pending(&self) -> Vec<String> {
            self.queue.borrow().keys().cloned().collect()
        }

        /// The designs in the store (as this page last saw it), and the ones on their way there.
        pub fn entries(&self) -> Vec<StoreEntry> {
            let queue = self.queue.borrow();
            let mut out: Vec<StoreEntry> = self
                .known
                .borrow()
                .iter()
                .map(|(n, r)| StoreEntry { name: n.clone(), size: r.size, modified: r.modified, pending: queue.contains_key(n) })
                .collect();
            for (n, p) in queue.iter() {
                if !out.iter().any(|e| e.name == *n) {
                    out.push(StoreEntry { name: n.clone(), size: p.bytes.len() as u64, modified: now(), pending: true });
                }
            }
            out.sort_by_key(|e| e.name.to_lowercase());
            out
        }

        /// A store name for `name` that nothing uses yet.
        pub fn free_name(&self, name: &str) -> String {
            let taken = |n: &str| {
                self.known.borrow().contains_key(n)
                    || self.queue.borrow().contains_key(n)
                    || self.remote.borrow().as_ref().is_some_and(|r| r.contains_key(n))
                    || vfs::exists(&format!("{STORE}{n}"))
            };
            free_name(name, taken)
        }

        /// The version the open design `name` is based on (for the autosave, so a recovered
        /// design saves against the version it started from).
        pub fn base_of(&self, name: &str) -> Option<String> {
            self.pins.borrow().get(name).cloned().flatten()
        }

        /// A recovered design: it is based on `base`, whatever the store holds now.
        pub fn pin(&self, name: &str, base: Option<String>) {
            self.pins.borrow_mut().insert(name.to_string(), base);
        }

        /// The open designs' files. A design opened from the store is based on the version the
        /// page held when it opened; that is what its saves will replace.
        pub fn track(&self, open_paths: &[String]) {
            let open: BTreeSet<&str> = open_paths.iter().filter_map(|p| p.strip_prefix(STORE)).collect();
            let mut pins = self.pins.borrow_mut();
            pins.retain(|n, _| open.contains(n.as_str()));
            for n in open {
                if !pins.contains_key(n) {
                    pins.insert(n.to_string(), self.known.borrow().get(n).map(|r| r.etag.clone()));
                }
            }
        }

        /// A command wrote `name` (a save): send it to the store.
        pub fn save(&self, name: &str, bytes: Vec<u8>) {
            self.enqueue(name, bytes, Kind::Save);
            self.say(format!("Saving {name} to disk…"), false);
        }

        fn enqueue(&self, name: &str, bytes: Vec<u8>, kind: Kind) {
            if !valid_name(name) {
                self.say(format!("{name}: not a name the disk store keeps (a .solidcraft or .json file name, no folders)"), true);
                return;
            }
            let base = match self.pins.borrow().get(name) {
                Some(b) => b.clone(),
                None => self.known.borrow().get(name).map(|r| r.etag.clone()),
            };
            let generation = self.generation.get() + 1;
            self.generation.set(generation);
            let bytes = Rc::new(bytes);
            let mut queue = self.queue.borrow_mut();
            // A newer save of bytes still waiting replaces them; it is based on the same version.
            let base = queue.get(name).map(|p| p.base.clone()).unwrap_or(base);
            let kind = queue.get(name).map(|p| p.kind.clone()).filter(|k| *k != Kind::Save && kind == Kind::Save).unwrap_or(kind);
            queue.insert(name.to_string(), Push { bytes: bytes.clone(), base, kind, generation, stuck: false, told: false });
            drop(queue);
            self.cache_op(CacheOp::Write(format!("{PENDING_PREFIX}{name}"), bytes));
            self.cache_op(CacheOp::Index);
            self.wake();
        }

        /// Try every waiting save again now.
        pub fn push_now(self: &Rc<Self>) {
            for p in self.queue.borrow_mut().values_mut() {
                p.stuck = false;
                p.told = false;
            }
            self.retry_at.set(0.0);
            self.backoff.set(RETRY_MIN_MS);
            self.online.set(None);
            self.refresh();
            self.wake();
        }

        /// The one-time copy: designs kept only in this browser go to the store (under a free
        /// name; nothing on disk is replaced). Returns how many were sent.
        pub fn copy_in(&self, designs: Vec<(String, Vec<u8>)>) -> usize {
            let mut n = 0;
            for (name, bytes) in designs {
                if self.copied.borrow().contains(&name) || !valid_name(&name) {
                    continue;
                }
                let target = self.free_name(&name);
                vfs::insert(&format!("{STORE}{target}"), bytes.clone());
                self.enqueue(&target, bytes, Kind::Copy);
                self.copied.borrow_mut().insert(name);
                n += 1;
            }
            self.offer_answered.set(true);
            self.cache_op(CacheOp::Index);
            n
        }

        /// A design kept only in this browser was saved while a store is configured: it goes
        /// to the store under a free name, which is returned (its new file).
        pub fn adopt(&self, name: &str, bytes: Vec<u8>) -> String {
            let target = self.free_name(name);
            vfs::insert(&format!("{STORE}{target}"), bytes.clone());
            self.enqueue(&target, bytes, Kind::Save);
            self.copied.borrow_mut().insert(name.to_string());
            self.say(format!("{name} was kept only in this browser: saving it to disk as {target}…"), false);
            target
        }

        /// Was this browser design copied to the store already?
        pub fn was_copied(&self, name: &str) -> bool {
            self.copied.borrow().contains(name)
        }

        pub fn decline_offer(&self) {
            self.offer_answered.set(true);
            self.cache_op(CacheOp::Index);
        }

        /// Run between frames' work: apply answers, fetch what changed, send what waits.
        /// `loading`: a design is being opened (its file must not change under it). Returns
        /// the paths of open designs that now save elsewhere: (old, new).
        pub fn tick(self: &Rc<Self>, open_paths: &[String], loading: bool) -> Vec<(String, String)> {
            let open: BTreeSet<String> = open_paths.iter().filter_map(|p| p.strip_prefix(STORE)).map(str::to_string).collect();
            let moved = self.apply_done(&open);
            // A design that now saves to its conflict copy no longer holds the old file open.
            let mut open = open;
            for (from, to) in &moved {
                if let (Some(f), Some(t)) = (from.strip_prefix(STORE), to.strip_prefix(STORE)) {
                    open.remove(f);
                    open.insert(t.to_string());
                }
            }
            if !loading {
                self.apply_fetched(&open);
                self.reconcile(&open);
            }
            self.send();
            self.flush_cache();
            moved
        }

        fn apply_done(&self, open: &BTreeSet<String>) -> Vec<(String, String)> {
            let mut moved = Vec::new();
            let done: Vec<Done> = self.done.borrow_mut().drain(..).collect();
            for d in done {
                match d {
                    Done::Saved { name, generation, remote } => {
                        self.online.set(Some(true));
                        self.backoff.set(RETRY_MIN_MS);
                        let mut queue = self.queue.borrow_mut();
                        let Some(push) = queue.get_mut(&name) else { continue };
                        let bytes = push.bytes.clone();
                        let kind = push.kind.clone();
                        let finished = push.generation == generation;
                        if finished {
                            queue.remove(&name);
                        } else {
                            // Newer bytes wait: they continue from the version just written.
                            push.base = Some(remote.etag.clone());
                        }
                        drop(queue);
                        if let Some(pin) = self.pins.borrow_mut().get_mut(&name) {
                            *pin = Some(remote.etag.clone());
                        }
                        if let Some(r) = self.remote.borrow_mut().as_mut() {
                            r.insert(name.clone(), remote.clone());
                        }
                        self.known.borrow_mut().insert(name.clone(), remote);
                        self.changed.set(true);
                        if finished {
                            self.cache_op(CacheOp::Write(name.clone(), bytes));
                            self.cache_op(CacheOp::Remove(format!("{PENDING_PREFIX}{name}")));
                            let text = match &kind {
                                Kind::Save => format!("{name} saved to disk"),
                                Kind::Copy => format!("{name} copied to disk"),
                                Kind::Conflict { of } => format!(
                                    "{of} changed on disk since it was opened here (another device, an agent or git): your version is on disk as {name}"
                                ),
                            };
                            self.say(text, matches!(kind, Kind::Conflict { .. }));
                        }
                        self.cache_op(CacheOp::Index);
                    }
                    Done::Conflict { name, current } => {
                        let Some(push) = self.queue.borrow_mut().remove(&name) else { continue };
                        if push.kind == Kind::Copy {
                            // Someone took the name meanwhile: the copy takes the next free one.
                            let target = self.free_name(&name);
                            let _ = vfs::remove(&format!("{STORE}{name}"));
                            vfs::insert(&format!("{STORE}{target}"), push.bytes.to_vec());
                            self.cache_op(CacheOp::Remove(format!("{PENDING_PREFIX}{name}")));
                            self.enqueue(&target, push.bytes.to_vec(), Kind::Copy);
                            continue;
                        }
                        let of = match &push.kind {
                            Kind::Conflict { of } => of.clone(),
                            _ => name.clone(),
                        };
                        let target = self.free_name(&conflict_name(&of, &stamp()));
                        vfs::insert(&format!("{STORE}{target}"), push.bytes.to_vec());
                        if let (Some(c), Some(r)) = (current, self.remote.borrow_mut().as_mut()) {
                            let old = r.get(&name).cloned().unwrap_or(Remote { etag: String::new(), size: 0, modified: 0.0 });
                            r.insert(name.clone(), Remote { etag: c, ..old });
                        }
                        let generation = self.generation.get() + 1;
                        self.generation.set(generation);
                        self.queue.borrow_mut().insert(
                            target.clone(),
                            Push {
                                bytes: push.bytes.clone(),
                                base: None,
                                kind: Kind::Conflict { of: of.clone() },
                                generation,
                                stuck: false,
                                told: false,
                            },
                        );
                        self.cache_op(CacheOp::Remove(format!("{PENDING_PREFIX}{name}")));
                        self.cache_op(CacheOp::Write(format!("{PENDING_PREFIX}{target}"), push.bytes.clone()));
                        self.cache_op(CacheOp::Index);
                        if open.contains(&name) {
                            self.pins.borrow_mut().remove(&name);
                            self.pins.borrow_mut().insert(target.clone(), None);
                            moved.push((format!("{STORE}{name}"), format!("{STORE}{target}")));
                        }
                        // The page's copy of `name` is the person's version: the store's comes back.
                        self.known.borrow_mut().remove(&name);
                        self.changed.set(true);
                        self.say(
                            format!(
                                "{of} changed on disk since you opened it (another device, an agent or git), so it was not replaced: yours is saved beside it as {target}"
                            ),
                            true,
                        );
                    }
                    Done::Unreachable { name, error } => {
                        self.online.set(Some(false));
                        *self.last_error.borrow_mut() = error.clone();
                        let wait = self.backoff.get();
                        self.retry_at.set(now() + wait);
                        self.backoff.set((wait * 2.0).min(RETRY_MAX_MS));
                        let mut queue = self.queue.borrow_mut();
                        if let Some(p) = queue.get_mut(&name)
                            && !p.told
                        {
                            p.told = true;
                            drop(queue);
                            self.say(
                                format!(
                                    "{name} is NOT on disk yet (the disk store can't be reached): kept in this browser, sent when the store is back"
                                ),
                                true,
                            );
                        }
                        if let Some(c) = self.ctx.borrow().as_ref() {
                            c.request_repaint_after(std::time::Duration::from_secs_f64(wait / 1000.0));
                        }
                    }
                    Done::Refused { name, error } => {
                        self.online.set(Some(true));
                        if let Some(p) = self.queue.borrow_mut().get_mut(&name) {
                            p.stuck = true;
                        }
                        self.say(format!("{name} is NOT on disk: the store refused it ({error}). It is kept in this browser."), true);
                    }
                }
            }
            moved
        }

        fn apply_fetched(&self, open: &BTreeSet<String>) {
            let fetched: Vec<_> = self.fetched.borrow_mut().drain(..).collect();
            for (name, remote, bytes) in fetched {
                // An open design keeps the version it was opened from; waiting saves win.
                if open.contains(&name) || self.queue.borrow().contains_key(&name) {
                    continue;
                }
                let bytes = Rc::new(bytes);
                vfs::insert(&format!("{STORE}{name}"), bytes.to_vec());
                self.known.borrow_mut().insert(name.clone(), remote);
                self.cache_op(CacheOp::Write(name, bytes));
                self.cache_op(CacheOp::Index);
                self.changed.set(true);
            }
        }

        fn reconcile(self: &Rc<Self>, open: &BTreeSet<String>) {
            let Some(remote) = self.remote.borrow().clone() else { return };
            let mut gone = Vec::new();
            for (name, r) in &remote {
                let have = self.known.borrow().get(name).map(|k| k.etag.clone());
                if have.as_deref() == Some(r.etag.as_str()) || self.queue.borrow().contains_key(name) {
                    continue;
                }
                if self.busy.borrow().contains(name) {
                    continue;
                }
                if open.contains(name) {
                    let pinned = self.pins.borrow().get(name).cloned().flatten();
                    let told = self.told_changed.borrow().get(name) == Some(&r.etag);
                    if pinned.as_deref() != Some(r.etag.as_str()) && !told {
                        self.told_changed.borrow_mut().insert(name.clone(), r.etag.clone());
                        self.say(
                            format!(
                                "{name} changed on disk (another device, an agent or git): open it again to see that version; saving yours keeps both"
                            ),
                            false,
                        );
                    }
                    continue;
                }
                self.fetch(name.clone());
            }
            for name in self.known.borrow().keys() {
                if !remote.contains_key(name) && !open.contains(name) && !self.queue.borrow().contains_key(name) && !self.busy.borrow().contains(name)
                {
                    gone.push(name.clone());
                }
            }
            for name in gone {
                self.known.borrow_mut().remove(&name);
                let _ = vfs::remove(&format!("{STORE}{name}"));
                self.cache_op(CacheOp::Remove(name));
                self.cache_op(CacheOp::Index);
                self.changed.set(true);
            }
        }

        fn fetch(self: &Rc<Self>, name: String) {
            self.busy.borrow_mut().insert(name.clone());
            let me = self.clone();
            spawn_local(async move {
                let url = format!("{}/designs/{}", me.url, encode(&name));
                match request("GET", &url, &[], None).await {
                    Ok(a) if a.status == 200 => {
                        let listed = me.remote.borrow().as_ref().and_then(|r| r.get(&name).cloned());
                        let etag = a.etag.clone().or_else(|| listed.as_ref().map(|r| r.etag.clone())).unwrap_or_default();
                        let remote = Remote { etag, size: a.body.len() as u64, modified: listed.map(|r| r.modified).unwrap_or(0.0) };
                        me.fetched.borrow_mut().push((name.clone(), remote, a.body));
                    }
                    Ok(a) if a.status == 404 => {
                        if let Some(r) = me.remote.borrow_mut().as_mut() {
                            r.remove(&name);
                        }
                    }
                    Ok(a) => {
                        log::warn!("store: {name}: {}", reason(&a));
                        me.retry_at.set(now() + me.backoff.get());
                    }
                    Err(e) => {
                        me.online.set(Some(false));
                        *me.last_error.borrow_mut() = e;
                    }
                }
                me.busy.borrow_mut().remove(&name);
                me.wake();
            });
        }

        fn send(self: &Rc<Self>) {
            if self.online.get() == Some(false) && now() < self.retry_at.get() {
                return;
            }
            let ready: Vec<(String, Push)> = self
                .queue
                .borrow()
                .iter()
                .filter(|(n, p)| !p.stuck && !self.busy.borrow().contains(*n))
                .map(|(n, p)| (n.clone(), p.clone()))
                .collect();
            for (name, push) in ready {
                self.busy.borrow_mut().insert(name.clone());
                let me = self.clone();
                spawn_local(async move {
                    let url = format!("{}/designs/{}", me.url, encode(&name));
                    let precondition = match &push.base {
                        Some(e) => ("If-Match", format!("\"{e}\"")),
                        None => ("If-None-Match", "*".to_string()),
                    };
                    let headers = [(precondition.0, precondition.1.as_str()), ("Content-Type", "application/octet-stream")];
                    let done = match request("PUT", &url, &headers, Some(&push.bytes)).await {
                        Ok(a) if a.status == 200 || a.status == 201 => {
                            let v: serde_json::Value = serde_json::from_slice(&a.body).unwrap_or_default();
                            let etag = v.get("etag").and_then(|e| e.as_str()).map(etag_value).or(a.etag.clone()).unwrap_or_default();
                            let modified = v.get("modified").and_then(serde_json::Value::as_f64).unwrap_or_else(now);
                            Done::Saved {
                                name: name.clone(),
                                generation: push.generation,
                                remote: Remote { etag, size: push.bytes.len() as u64, modified },
                            }
                        }
                        Ok(a) if a.status == 412 => {
                            let v: serde_json::Value = serde_json::from_slice(&a.body).unwrap_or_default();
                            let current = v.get("etag").and_then(|e| e.as_str()).map(etag_value).or(a.etag.clone());
                            Done::Conflict { name: name.clone(), current }
                        }
                        Ok(a) if unreachable(a.status) => Done::Unreachable { name: name.clone(), error: reason(&a) },
                        Ok(a) => Done::Refused { name: name.clone(), error: reason(&a) },
                        Err(e) => Done::Unreachable { name: name.clone(), error: e },
                    };
                    me.done.borrow_mut().push(done);
                    me.busy.borrow_mut().remove(&name);
                    me.wake();
                });
            }
        }

        // The copy in OPFS, one operation at a time (two writers on one file fail in some browsers).

        fn cache_op(&self, op: CacheOp) {
            if self.cache.is_none() {
                return;
            }
            let mut ops = self.cache_ops.borrow_mut();
            if matches!(op, CacheOp::Index) && ops.iter().any(|o| matches!(o, CacheOp::Index)) {
                return;
            }
            ops.push_back(op);
        }

        /// Write what the cache operations queued (from the frame; they run one after another).
        pub fn flush_cache(self: &Rc<Self>) {
            if self.cache_running.get() || self.cache_ops.borrow().is_empty() {
                return;
            }
            let Some(dir) = self.cache.clone() else { return };
            self.cache_running.set(true);
            let me = self.clone();
            spawn_local(async move {
                loop {
                    let op = me.cache_ops.borrow_mut().pop_front();
                    let Some(op) = op else { break };
                    let r = match op {
                        CacheOp::Write(name, bytes) => files::dir_write(&dir, &name, &bytes).await,
                        CacheOp::Remove(name) => files::dir_remove(&dir, &name).await,
                        CacheOp::Index => files::dir_write(&dir, INDEX, me.index().as_bytes()).await,
                    };
                    if let Err(e) = r {
                        log::warn!("store: browser copy: {e}");
                    }
                }
                me.cache_running.set(false);
            });
        }

        fn index(&self) -> String {
            let known: serde_json::Map<String, serde_json::Value> = self
                .known
                .borrow()
                .iter()
                .map(|(n, r)| (n.clone(), serde_json::json!({"etag": r.etag, "size": r.size, "modified": r.modified})))
                .collect();
            let pending: serde_json::Map<String, serde_json::Value> = self
                .queue
                .borrow()
                .iter()
                .map(|(n, p)| {
                    let of = match &p.kind {
                        Kind::Conflict { of } => Some(of.clone()),
                        _ => None,
                    };
                    (n.clone(), serde_json::json!({"base": p.base, "copy": p.kind == Kind::Copy, "conflict_of": of}))
                })
                .collect();
            serde_json::json!({
                "url": self.url,
                "known": known,
                "pending": pending,
                "copied": self.copied.borrow().iter().collect::<Vec<_>>(),
                "offer_answered": self.offer_answered.get(),
            })
            .to_string()
        }

        /// Read the copy in OPFS (instant, and all there is offline).
        async fn load_cache(&self) {
            let Some(dir) = self.cache.clone() else { return };
            let files = match files::dir_files(&dir).await {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("store: browser copy: {e}");
                    return;
                }
            };
            let mut by_name: BTreeMap<String, Vec<u8>> = files.into_iter().collect();
            let index: serde_json::Value = by_name.remove(INDEX).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            if let Some(k) = index.get("known").and_then(|k| k.as_object()) {
                for (name, v) in k {
                    let Some(bytes) = by_name.get(name) else { continue };
                    let Some(etag) = v.get("etag").and_then(|e| e.as_str()) else { continue };
                    if !valid_name(name) {
                        continue;
                    }
                    let remote = Remote {
                        etag: etag.to_string(),
                        size: bytes.len() as u64,
                        modified: v.get("modified").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
                    };
                    vfs::insert(&format!("{STORE}{name}"), bytes.clone());
                    self.known.borrow_mut().insert(name.clone(), remote);
                }
            }
            if let Some(p) = index.get("pending").and_then(|p| p.as_object()) {
                for (name, v) in p {
                    let Some(bytes) = by_name.remove(&format!("{PENDING_PREFIX}{name}")) else { continue };
                    if !valid_name(name) {
                        continue;
                    }
                    let kind = match (v.get("conflict_of").and_then(|c| c.as_str()), v.get("copy").and_then(serde_json::Value::as_bool)) {
                        (Some(of), _) => Kind::Conflict { of: of.to_string() },
                        (None, Some(true)) => Kind::Copy,
                        _ => Kind::Save,
                    };
                    let generation = self.generation.get() + 1;
                    self.generation.set(generation);
                    // The page shows what it saved, not the version on disk.
                    vfs::insert(&format!("{STORE}{name}"), bytes.clone());
                    let base = v.get("base").and_then(|b| b.as_str()).map(str::to_string);
                    self.queue.borrow_mut().insert(name.clone(), Push { bytes: Rc::new(bytes), base, kind, generation, stuck: false, told: false });
                }
            }
            if let Some(c) = index.get("copied").and_then(|c| c.as_array()) {
                self.copied.borrow_mut().extend(c.iter().filter_map(|x| x.as_str()).map(str::to_string));
            }
            self.offer_answered.set(index.get("offer_answered").and_then(serde_json::Value::as_bool).unwrap_or(false));
            let n = self.queue.borrow().len();
            if n > 0 {
                self.say(format!("{n} design(s) saved in this browser are NOT on disk yet: sending them now"), true);
            }
        }

        /// One listing (`wait`: hold until it is no longer that version).
        async fn list(&self, wait: Option<&str>) -> Result<(String, BTreeMap<String, Remote>), String> {
            let url = match wait {
                Some(v) => format!("{}/designs?wait={}", self.url, encode(v)),
                None => format!("{}/designs", self.url),
            };
            let a = request("GET", &url, &[], None).await?;
            if a.status != 200 {
                return Err(reason(&a));
            }
            parse_listing(&String::from_utf8_lossy(&a.body))
        }

        fn take_listing(&self, version: String, listing: BTreeMap<String, Remote>) {
            let was_offline = self.online.get() == Some(false);
            self.online.set(Some(true));
            self.last_error.borrow_mut().clear();
            if was_offline {
                self.retry_at.set(0.0);
                self.backoff.set(RETRY_MIN_MS);
                for p in self.queue.borrow_mut().values_mut() {
                    p.told = false;
                }
            }
            *self.version.borrow_mut() = version;
            *self.remote.borrow_mut() = Some(listing);
            self.changed.set(true);
            self.wake();
        }

        /// Ask for the listing now (not waiting for a change).
        pub fn refresh(self: &Rc<Self>) {
            let me = self.clone();
            spawn_local(async move {
                match me.list(None).await {
                    Ok((v, l)) => me.take_listing(v, l),
                    Err(e) => {
                        me.online.set(Some(false));
                        *me.last_error.borrow_mut() = e;
                        me.wake();
                    }
                }
            });
        }
    }

    /// Read the browser's copy, then follow the store: a listing, then one held request after
    /// another (`wait`), so a design saved elsewhere shows up here within moments.
    pub fn start(store: &Rc<Store>) {
        let me = store.clone();
        spawn_local(async move {
            me.load_cache().await;
            me.loaded.set(true);
            me.changed.set(true);
            me.wake();
            let mut backoff = RETRY_MIN_MS;
            let mut first = true;
            loop {
                let wait = (!first).then(|| me.version.borrow().clone());
                let asked = now();
                match me.list(wait.as_deref()).await {
                    Ok((v, l)) => {
                        backoff = RETRY_MIN_MS;
                        let same = wait.as_deref() == Some(v.as_str());
                        me.take_listing(v, l);
                        // A store that doesn't hold answers at once: ask again in a while.
                        if !first && same && now() - asked < 1_000.0 {
                            sleep(POLL_MS).await;
                        }
                        first = false;
                    }
                    Err(e) => {
                        me.online.set(Some(false));
                        *me.last_error.borrow_mut() = e;
                        me.wake();
                        sleep(backoff as i32).await;
                        backoff = (backoff * 2.0).min(RETRY_MAX_MS / 2.0);
                    }
                }
            }
        });
    }
}
