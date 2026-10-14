//! SolidCraft in the browser.
//!
//! Runs the same [`solidcraft_ui_egui::SolveApp`] as the desktop app through eframe's web runner
//! (wgpu: WebGPU where available, WebGL2 otherwise). Build with `trunk build --release` from this
//! directory (output in `dist/web`). URL flags: `?webgl` forces WebGL2, `?sample` opens the
//! sample part, `?store=<url>` keeps designs in a disk store on a host (this page's origin or
//! loopback; see `store` and docs/web-store.md) instead of only in the browser.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(target_arch = "wasm32")]
mod files;
#[cfg(any(target_arch = "wasm32", test))]
mod store;

#[cfg(target_arch = "wasm32")]
mod web {
    use std::rc::Rc;

    use solidcraft_engine::Session;
    use solidcraft_engine::io::vfs;
    use solidcraft_ui_egui::{Services, SolveApp};
    use wasm_bindgen::JsCast as _;

    use crate::files::{self, Files, Intent};
    use crate::store::{self, STORE, Store};

    const CANVAS_ID: &str = "solidcraft_canvas";
    const LOADING_ID: &str = "solidcraft_loading";
    const DEPTH_BITS: u8 = 24;

    /// Seconds between autosaves of unsaved work.
    const AUTOSAVE_S: f64 = 30.0;

    thread_local! {
        /// An open design has unsaved changes: leaving the page asks first.
        static UNSAVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// The browser's "leave site? changes you made may not be saved" prompt while a design has
    /// unsaved changes (the page can't show its own dialog when it is being closed).
    fn ask_before_unload(window: &web_sys::Window) {
        let on_unload = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(|e: web_sys::BeforeUnloadEvent| {
            if UNSAVED.with(std::cell::Cell::get) {
                e.prevent_default();
                e.set_return_value("unsaved");
            }
        });
        let _ = window.add_event_listener_with_callback("beforeunload", on_unload.as_ref().unchecked_ref());
        // The listener lives as long as the page.
        on_unload.forget();
    }

    const JAPANESE_FONT_PATH: &str = "./fonts/BIZUDPGothic-Regular.ttf";

    type FontLoadResult = Rc<std::cell::RefCell<Option<Result<Vec<u8>, String>>>>;

    struct Shell {
        app: SolveApp,
        files: Rc<Files>,
        /// When the last autosave was (seconds), and the revision it holds (none stored: `None`).
        autosaved_at: f64,
        autosaved: Option<u64>,
        /// A stored design being renamed: (its name, the new name).
        renaming: Option<(String, String)>,
        /// The rename field takes the keyboard when it appears.
        rename_focus: bool,
        /// The start page's recent designs were filled from the stored ones.
        recents_synced: bool,
        /// On-demand CJK font: fetched only when a CJK language is selected.
        font: solidcraft_ui_egui::fonts::CjkFontLoader,
        font_result: FontLoadResult,
        /// The disk store (`?store=`): designs live on the host, the browser keeps a copy.
        store: Option<Rc<Store>>,
        /// The active design's file, for Save As (shared with the save picker).
        active_path: Rc<std::cell::RefCell<Option<String>>>,
        /// The active design's file at the last files tick (a write to it then is a save).
        tick_path: Option<String>,
    }

    impl Shell {
        /// Files from the computer, writes to store, messages and the autosave.
        fn files_tick(&mut self, ctx: &egui::Context) {
            if self.files.ctx.borrow().is_none() {
                *self.files.ctx.borrow_mut() = Some(ctx.clone());
            }
            if let Some(st) = &self.store {
                st.set_ctx(ctx);
                // Before the writes: a design just opened is based on the version held now.
                st.track(&solidcraft_ui_egui::documents::paths(&self.app));
            }
            let saved_here = files::flush(&self.files, self.store.as_ref());
            self.adopt_into_store(&saved_here);
            for (text, error) in self.files.messages.borrow_mut().drain(..) {
                self.app.set_status(text, error);
            }
            self.store_tick();
            let incoming: Vec<_> = self.files.incoming.borrow_mut().drain(..).collect();
            for (name, bytes, intent) in incoming {
                let design = solidcraft_engine::io::is_design_path(&name);
                let path = if design && intent == Intent::Open {
                    // A design from the computer is kept (a copy: on disk with a store, else in the
                    // browser), so Save works.
                    let path = match &self.store {
                        Some(st) if store::valid_name(&name) => format!("{STORE}{}", st.free_name(&name)),
                        _ => files::free_name(&name),
                    };
                    if let Err(e) = vfs::write(&path, &bytes) {
                        self.app.set_status(e.to_string(), true);
                    }
                    files::flush(&self.files, self.store.as_ref());
                    path
                } else {
                    let path = format!("{}{name}", files::UPLOAD);
                    vfs::insert(&path, bytes);
                    path
                };
                match intent {
                    Intent::Open => self.app.open_path(&path),
                    Intent::Insert => self.app.drop_path(&path),
                }
                self.files.panel.set(false);
            }
            let store_loaded = self.store.as_ref().is_none_or(|s| s.loaded.get());
            if self.files.ready.get() && store_loaded && !self.recents_synced {
                self.recents_synced = true;
                self.sync_recents();
            }
            let now = ctx.input(|i| i.time);
            // Unsaved work is kept (at most every AUTOSAVE_S); saved work needs no autosave.
            if self.files.ready.get() && self.files.recovery.borrow().is_none() {
                let s = &self.app.session;
                let changed = s.is_dirty() && self.autosaved != Some(s.revision);
                let wait = AUTOSAVE_S - (now - self.autosaved_at);
                if changed && wait <= 0.0 {
                    let base = s.path.as_deref().and_then(|p| p.strip_prefix(STORE)).zip(self.store.as_ref()).and_then(|(n, st)| st.base_of(n));
                    files::autosave(&self.files, Some((&s.design(), s.path.as_deref(), base)));
                    self.autosaved = Some(s.revision);
                    self.autosaved_at = now;
                } else if changed {
                    // Come back for it even when nothing else happens.
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
                } else if !s.is_dirty() && self.autosaved.is_some() {
                    files::autosave(&self.files, None);
                    self.autosaved = None;
                }
            }
            self.tick_path = self.app.session.path.clone();
            if !self.files.ready.get() || !self.files.incoming.borrow().is_empty() {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }

        /// The disk store's answers: designs that now save elsewhere (a conflict), messages, and
        /// the start page following the designs on disk.
        fn store_tick(&mut self) {
            let Some(st) = self.store.clone() else { return };
            let open = solidcraft_ui_egui::documents::paths(&self.app);
            for (from, to) in st.tick(&open, self.app.file_load.busy()) {
                if self.app.session.path.as_deref() == Some(from.as_str()) {
                    self.app.session.path = Some(to.clone());
                    self.app.home.add_recent(&to);
                }
            }
            for (text, error) in st.messages.borrow_mut().drain(..) {
                // The line under the command box shows the last echo ("Save done" before this).
                self.app.session.echo(text.clone());
                self.app.set_status(text, error);
            }
            if st.changed.take() && self.recents_synced {
                self.sync_recents();
            }
            *self.active_path.borrow_mut() = self.app.session.path.clone();
        }

        /// With a disk store, a design kept only in this browser that was just saved goes to the
        /// disk too, and saves there from then on.
        fn adopt_into_store(&mut self, saved: &[String]) {
            let Some(st) = self.store.clone() else { return };
            for name in saved {
                let opfs = format!("{}{name}", files::OPFS);
                // Saved, not just opened: the design had this file before it was written.
                let saved = self.tick_path.as_deref() == Some(opfs.as_str()) && self.app.session.path.as_deref() == Some(opfs.as_str());
                if !saved || !store::valid_name(name) {
                    continue;
                }
                let Ok(bytes) = vfs::read(&opfs, solidcraft_engine::io::MAX_DESIGN_BYTES) else { continue };
                let target = st.adopt(name, bytes);
                let path = format!("{STORE}{target}");
                self.app.session.path = Some(path.clone());
                self.app.home.add_recent(&path);
            }
        }

        /// The start page's recent designs are the stored ones (with a disk store, the designs on
        /// disk): the remembered order first (those still stored), then the others, newest first.
        fn sync_recents(&mut self) {
            let mut entries: Vec<(String, f64)> = match &self.store {
                Some(st) => st.entries().into_iter().map(|e| (format!("{STORE}{}", e.name), e.modified)).collect(),
                None => self.files.entries.borrow().iter().map(|e| (format!("{}{}", files::OPFS, e.name), e.modified)).collect(),
            };
            entries.retain(|(p, _)| solidcraft_engine::io::is_design_path(p));
            entries.sort_by(|a, b| b.1.total_cmp(&a.1));
            let with_store = self.store.is_some();
            let recent = &mut self.app.home.recent;
            // With a store, designs kept only in this browser are in the file panel (to copy them).
            recent.retain(|p| vfs::exists(p) && !(with_store && p.starts_with(files::OPFS)));
            for (p, _) in entries {
                if !recent.contains(&p) {
                    recent.push(p);
                }
            }
            recent.truncate(solidcraft_ui_egui::home::MAX_RECENT);
        }

        /// What the disk store holds, and what is on its way there (the file panel's top part).
        fn store_section(st: &Rc<Store>, ui: &mut egui::Ui, act: &mut Option<(&'static str, String)>) {
            let t = solidcraft_ui_egui::theme::Tokens::get();
            let pending = st.pending();
            ui.horizontal(|ui| {
                ui.strong("On disk");
                match st.online() {
                    Some(false) => {
                        ui.colored_label(t.error, "the disk store can't be reached: showing this browser's copies").on_hover_text(st.last_error());
                    }
                    None => {
                        ui.spinner();
                    }
                    Some(true) => {}
                }
                if !pending.is_empty() {
                    let b = ui.small_button("Push now").on_hover_text("Send the designs that are not on disk yet");
                    handle("web:files:push_now", &b);
                    if b.clicked() {
                        *act = Some(("push_now", String::new()));
                    }
                }
            });
            if !pending.is_empty() {
                ui.colored_label(t.error, format!("NOT on disk yet (kept in this browser): {}", pending.join(", ")));
            }
            let entries = st.entries();
            if entries.is_empty() {
                ui.label(if st.loaded.get() {
                    "Designs you save appear here. They are kept on the host's disk."
                } else {
                    "Reading the disk store…"
                });
            }
            egui::ScrollArea::vertical().id_salt("store_files").max_height(300.0).show(ui, |ui| {
                egui::Grid::new("store_files_grid").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
                    for e in &entries {
                        if e.pending {
                            ui.colored_label(t.error, format!("{} (not on disk yet)", e.name));
                        } else {
                            ui.label(&e.name);
                        }
                        ui.label(format!("{:.1} KB", e.size as f64 / 1024.0));
                        ui.horizontal(|ui| {
                            let design = solidcraft_engine::io::is_design_path(&e.name);
                            if design {
                                let b = ui.small_button("Open");
                                handle(&format!("web:files:store_open:{}", e.name), &b);
                                if b.clicked() {
                                    *act = Some(("store_open", e.name.clone()));
                                }
                            }
                            if ui.small_button("Download").clicked() {
                                *act = Some(("store_download", e.name.clone()));
                            }
                        });
                        ui.end_row();
                    }
                });
            });
        }

        /// The stored designs: open, insert, rename, delete, download; open from the computer.
        fn file_panel(&mut self, ctx: &egui::Context) {
            if !self.files.panel.get() {
                return;
            }
            let mut open = true;
            let mut act: Option<(&'static str, String)> = None;
            let store = self.store.clone();
            egui::Window::new(if store.is_some() { "Designs" } else { "Files in this browser" })
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(520.0)
                .pivot(egui::Align2::CENTER_CENTER)
                .default_pos(ctx.content_rect().center())
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Open from computer…").clicked() {
                            files::pick_from_disk(&self.files, Intent::Open);
                        }
                        if ui.button("Insert from computer…").on_hover_text("STEP, 3MF or STL into the open design").clicked() {
                            files::pick_from_disk(&self.files, Intent::Insert);
                        }
                        if ui.button("Download open design").clicked() {
                            act = Some(("download_open", String::new()));
                        }
                    });
                    ui.separator();
                    if let Some(st) = &store {
                        Self::store_section(st, ui, &mut act);
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.strong("Only in this browser");
                            let waiting =
                                self.files.entries.borrow().iter().any(|e| solidcraft_engine::io::is_design_path(&e.name) && !st.was_copied(&e.name));
                            if waiting {
                                let b =
                                    ui.button("Copy browser designs to disk").on_hover_text("A copy of each on the host's disk; they stay here too");
                                handle("web:files:copy_all", &b);
                                if b.clicked() {
                                    act = Some(("copy_all", String::new()));
                                }
                            }
                        });
                    }
                    if !self.files.ready.get() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Reading the browser's storage…");
                        });
                        return;
                    }
                    let entries = self.files.entries.borrow().clone();
                    if entries.is_empty() {
                        ui.label(if store.is_some() {
                            "Nothing is kept only in this browser."
                        } else {
                            "Designs you save appear here. They stay in this browser."
                        });
                    }
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        egui::Grid::new("opfs_files").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
                            for e in &entries {
                                match &mut self.renaming {
                                    Some((from, to)) if *from == e.name => {
                                        let r = ui.add(egui::TextEdit::singleline(to).desired_width(200.0));
                                        if std::mem::take(&mut self.rename_focus) {
                                            r.request_focus();
                                        }
                                        if r.lost_focus() {
                                            act = Some(if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                                ("rename", to.clone())
                                            } else {
                                                ("rename_cancel", String::new())
                                            });
                                        }
                                    }
                                    _ => {
                                        match &store {
                                            Some(st) if st.was_copied(&e.name) => ui.label(format!("{} (copied to disk)", e.name)),
                                            _ => ui.label(&e.name),
                                        };
                                    }
                                }
                                ui.label(format!("{:.1} KB", e.size as f64 / 1024.0));
                                ui.horizontal(|ui| {
                                    if ui.small_button("Open").clicked() {
                                        act = Some(("open", e.name.clone()));
                                    }
                                    let design = solidcraft_engine::io::is_design_path(&e.name);
                                    if !design && ui.small_button("Insert").on_hover_text("Into the open design (STEP, 3MF, STL)").clicked() {
                                        act = Some(("insert", e.name.clone()));
                                    }
                                    if ui.small_button("Rename").clicked() {
                                        self.renaming = Some((e.name.clone(), e.name.clone()));
                                        self.rename_focus = true;
                                    }
                                    if ui.small_button("Download").clicked() {
                                        act = Some(("download", e.name.clone()));
                                    }
                                    if ui.small_button("Delete").clicked() {
                                        act = Some(("delete", e.name.clone()));
                                    }
                                });
                                ui.end_row();
                            }
                        });
                    });
                });
            if !open {
                self.files.panel.set(false);
                self.renaming = None;
            }
            let Some((what, name)) = act else { return };
            let path = format!("{}{name}", files::OPFS);
            let stored = format!("{STORE}{name}");
            match what {
                "store_open" => {
                    self.app.open_path(&stored);
                    self.files.panel.set(false);
                }
                "store_download" => match vfs::read(&stored, solidcraft_engine::io::MAX_DESIGN_BYTES) {
                    Ok(b) => {
                        if let Err(e) = files::download(&name, &b) {
                            self.app.set_status(e, true);
                        }
                    }
                    Err(e) => self.app.set_status(e.to_string(), true),
                },
                "push_now" => {
                    if let Some(st) = &self.store {
                        st.push_now();
                    }
                }
                "copy_all" => self.copy_browser_designs(),
                "open" => {
                    self.app.open_path(&path);
                    self.files.panel.set(false);
                }
                "insert" => {
                    self.app.insert_path(&path);
                    self.files.panel.set(false);
                }
                "download" => match vfs::read(&path, solidcraft_engine::io::MAX_DESIGN_BYTES) {
                    Ok(b) => {
                        if let Err(e) = files::download(&name, &b) {
                            self.app.set_status(e, true);
                        }
                    }
                    Err(e) => self.app.set_status(e.to_string(), true),
                },
                "download_open" => {
                    let d = &self.app.session.doc;
                    let result = self
                        .app
                        .session
                        .design_container(solidcraft_engine::io::DesignCompression::Fast, false, false)
                        .map_err(|e| e.to_string())
                        .and_then(|bytes| files::download(&format!("{}.{}", d.name, solidcraft_engine::io::DESIGN_EXT), &bytes));
                    if let Err(e) = result {
                        self.app.set_status(e, true);
                    }
                }
                "delete" => {
                    files::delete(&self.files, &name);
                    self.app.home.recent.retain(|r| *r != path);
                }
                "rename_cancel" => self.renaming = None,
                "rename" => {
                    if let Some((from, _)) = self.renaming.take() {
                        match files::rename(&self.files, &from, &name) {
                            Ok(()) => {
                                // Recent designs follow the new name.
                                let (old, new) = (format!("{}{from}", files::OPFS), format!("{}{name}", files::OPFS));
                                for r in &mut self.app.home.recent {
                                    if *r == old {
                                        *r = new.clone();
                                    }
                                }
                            }
                            Err(e) => self.app.set_status(e, true),
                        }
                    }
                }
                _ => {}
            }
        }

        /// Offer the autosave an earlier session left.
        fn recovery_prompt(&mut self, ctx: &egui::Context) {
            let Some((name, path, base)) = self.files.recovery.borrow().clone() else { return };
            let mut answer = None;
            egui::Window::new("Recover unsaved work?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(
                ctx,
                |ui| {
                    ui.label(format!("“{name}” had unsaved changes when this page last closed."));
                    ui.horizontal(|ui| {
                        if ui.button("Recover").clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Discard").clicked() {
                            answer = Some(false);
                        }
                    });
                },
            );
            let Some(recover) = answer else { return };
            *self.files.recovery.borrow_mut() = None;
            if recover {
                // A design from the disk store saves against the version it was based on, not
                // whatever the store holds now (a newer one makes a conflict copy, never a loss).
                if let (Some(st), Some(n)) = (&self.store, path.as_deref().and_then(|p| p.strip_prefix(STORE))) {
                    st.pin(n, base);
                }
                self.app.open_path(&files::recovery_path());
                // It belongs to its own file again (saving goes there); it is unsaved.
                self.app.session.path = path;
                self.app.session.doc_mut().name = name;
                self.app.session.mark_unsaved();
                self.app.home.open = false;
            }
            files::autosave(&self.files, None);
        }

        /// The one-time copy of the designs kept only in this browser to the disk store.
        fn copy_browser_designs(&mut self) {
            let Some(st) = self.store.clone() else { return };
            let designs: Vec<(String, Vec<u8>)> = self
                .files
                .entries
                .borrow()
                .iter()
                .filter(|e| solidcraft_engine::io::is_design_path(&e.name))
                .filter_map(|e| {
                    vfs::read(&format!("{}{}", files::OPFS, e.name), solidcraft_engine::io::MAX_DESIGN_BYTES).ok().map(|b| (e.name.clone(), b))
                })
                .collect();
            let n = st.copy_in(designs);
            let text = if n == 0 {
                "Nothing to copy: every design here is on disk already".to_string()
            } else {
                format!("Copying {n} design(s) to disk…")
            };
            self.app.set_status(text.clone(), false);
            self.app.session.echo(text);
        }

        /// With a disk store, offer once to copy the designs kept only in this browser.
        fn copy_offer(&mut self, ctx: &egui::Context) {
            let Some(st) = self.store.clone() else { return };
            if !st.loaded.get() || !self.files.ready.get() || st.offer_answered.get() || self.files.recovery.borrow().is_some() {
                return;
            }
            let names: Vec<String> = self
                .files
                .entries
                .borrow()
                .iter()
                .filter(|e| solidcraft_engine::io::is_design_path(&e.name) && !st.was_copied(&e.name))
                .map(|e| e.name.clone())
                .collect();
            if names.is_empty() {
                return;
            }
            let mut answer = None;
            egui::Window::new("Copy designs to disk?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(
                ctx,
                |ui| {
                    ui.label(format!("{} design(s) are kept only in this browser, not on disk:", names.len()));
                    ui.label(names.join(", "));
                    ui.label("Copy them to the disk store? They stay in this browser too.");
                    ui.horizontal(|ui| {
                        let copy = ui.button("Copy to disk");
                        handle("web:copy_offer:copy", &copy);
                        if copy.clicked() {
                            answer = Some(true);
                        }
                        let later = ui.button("Not now").on_hover_text("Open › Designs has the same button");
                        handle("web:copy_offer:not_now", &later);
                        if later.clicked() {
                            answer = Some(false);
                        }
                    });
                },
            );
            match answer {
                Some(true) => self.copy_browser_designs(),
                Some(false) => st.decline_offer(),
                None => {}
            }
        }

        /// While designs are not on disk (or the store can't be reached), say so where it can't
        /// be missed.
        fn store_badge(&mut self, ctx: &egui::Context) {
            let Some(st) = self.store.clone() else { return };
            let pending = st.pending();
            let offline = st.online() == Some(false);
            if pending.is_empty() && !offline {
                return;
            }
            let t = solidcraft_ui_egui::theme::Tokens::get();
            let text = match (pending.len(), offline) {
                (0, _) => "Disk store unreachable: showing this browser's copies".to_string(),
                (1, o) => format!("{} is NOT on disk yet{}", pending.join(""), if o { " (disk store unreachable)" } else { "" }),
                (n, o) => format!("{n} designs are NOT on disk yet{}", if o { " (disk store unreachable)" } else { "" }),
            };
            egui::Area::new(egui::Id::new("store_badge")).anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -40.0]).show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(t.error, text).on_hover_text(if offline { st.last_error() } else { pending.join(", ") });
                        if !pending.is_empty() {
                            let b = ui.small_button("Push now");
                            handle("web:store_badge:push_now", &b);
                            if b.clicked() {
                                st.push_now();
                            }
                        }
                    });
                });
            });
        }

        /// Fetch and install the CJK font when a CJK language is selected. If that fails, this
        /// session falls back to English (no tofu); the saved language preference is kept.
        fn tick_font(&mut self, ctx: &egui::Context) {
            let done = self.font_result.borrow_mut().take();
            if let Some(res) = done {
                let installed = match res {
                    Ok(bytes) => solidcraft_ui_egui::fonts::install_cjk_font(ctx, bytes),
                    Err(err) => {
                        log::warn!("failed to fetch CJK font from {JAPANESE_FONT_PATH}: {err}");
                        false
                    }
                };
                if installed {
                    log::info!("loaded web CJK font from {JAPANESE_FONT_PATH}");
                } else if solidcraft_ui_egui::fonts::needs_cjk_font(&self.app.preferences.language) {
                    log::warn!("CJK font unavailable; showing English for this session");
                }
                self.font.finished(installed, &mut self.app.preferences.language);
                ctx.request_repaint();
            }
            if self.font.tick(&mut self.app.preferences.language) {
                self.fetch_font(ctx);
            }
        }

        /// CJK text on screen with no CJK font yet (an English UI showing a Japanese name):
        /// fetch the font, so the text does not stay tofu (#720).
        fn watch_text_for_font(&mut self, ctx: &egui::Context) {
            if self.font.watching_text() && solidcraft_ui_egui::fonts::painted_cjk(ctx) && self.font.text_needs_font() {
                self.fetch_font(ctx);
            }
        }

        fn fetch_font(&self, ctx: &egui::Context) {
            let slot = self.font_result.clone();
            let ctx = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let res = fetch_font_bytes(JAPANESE_FONT_PATH).await;
                *slot.borrow_mut() = Some(res);
                ctx.request_repaint();
            });
        }
    }

    impl eframe::App for Shell {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.tick_font(ctx);
            self.app.logic(ctx);
            self.files_tick(ctx);
            let unsent = self.store.as_ref().is_some_and(|s| !s.pending().is_empty());
            UNSAVED.with(|u| u.set(solidcraft_ui_egui::documents::any_dirty(&self.app) || unsent));
        }
        fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
            files::take_drops(&self.files, raw);
            self.app.raw_input_hook(raw);
        }
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            self.app.ui(ui);
            let ctx = ui.ctx().clone();
            // A save from this frame's clicks reaches the store (and the status line) at once.
            self.files_tick(&ctx);
            self.file_panel(&ctx);
            self.recovery_prompt(&ctx);
            self.copy_offer(&ctx);
            self.store_badge(&ctx);
            self.watch_text_for_font(&ctx);
        }
        fn save(&mut self, storage: &mut dyn eframe::Storage) {
            // Persist the user's own language, not a session-only English fallback.
            let saved = self.font.persisted_language(&self.app.preferences.language).to_owned();
            let live = std::mem::replace(&mut self.app.preferences.language, saved);
            storage.set_string(solidcraft_ui_egui::prefs::STORAGE_KEY, self.app.prefs());
            self.app.preferences.language = live;
        }
    }

    /// Where a button was drawn, for `ui.handles` / `ui.click` (agents and tests drive the page).
    fn handle(name: &str, r: &egui::Response) {
        solidcraft_ui_egui::scenario::publish_handle(name, r.rect.center());
    }

    fn query() -> String {
        web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
    }

    /// Fetch font bytes asynchronously through the browser.
    async fn fetch_font_bytes(url: &str) -> Result<Vec<u8>, String> {
        let window = web_sys::window().ok_or("no window")?;
        let resp_val = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
        let resp: web_sys::Response = resp_val.dyn_into().map_err(|_| "response cast failed")?;
        if !resp.ok() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let ab_val = wasm_bindgen_futures::JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?).await.map_err(|e| format!("{e:?}"))?;
        let uint8_arr = js_sys::Uint8Array::new(&ab_val);
        Ok(uint8_arr.to_vec())
    }

    pub fn start() {
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        wasm_bindgen_futures::spawn_local(async {
            let Some(window) = web_sys::window() else { return };
            ask_before_unload(&window);
            let Some(document) = window.document() else { return };
            let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
                log::error!("missing <canvas id=\"{CANVAS_ID}\">");
                return;
            };
            let mut options = eframe::WebOptions { depth_buffer: DEPTH_BITS, ..Default::default() };
            if query().contains("webgl")
                && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
            {
                create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
            }
            let result = eframe::WebRunner::new()
                .start(
                    canvas,
                    options,
                    Box::new(move |cc| {
                        solidcraft_ui_egui::fonts::setup_fonts(&cc.egui_ctx);
                        let files = Rc::new(Files::default());
                        files::start(&files);
                        let store = store::url_from_query(&query()).map(|url| Rc::new(Store::new(url)));
                        if let Some(st) = &store {
                            store::start(st);
                        }
                        let active_path: Rc<std::cell::RefCell<Option<String>>> = Rc::default();
                        let (save_store, save_active) = (store.clone(), active_path.clone());
                        let panel = files.clone();
                        let services = Services {
                            // Open shows the browser's files (and the computer's, from there).
                            pick_open: Some(Box::new(move || {
                                panel.panel.set(true);
                                None
                            })),
                            // Designs are kept on disk (with a store) or in the browser; everything
                            // else is downloaded. A new name never replaces another design on disk.
                            pick_save: Some(Box::new(move |name: &str, exts: &[&str]| {
                                let design = exts.contains(&solidcraft_engine::io::DESIGN_EXT);
                                match &save_store {
                                    Some(st) if design && store::valid_name(name) => {
                                        let own = format!("{STORE}{name}");
                                        if save_active.borrow().as_deref() == Some(own.as_str()) {
                                            Some(own)
                                        } else {
                                            Some(format!("{STORE}{}", st.free_name(name)))
                                        }
                                    }
                                    _ => Some(format!("{}{name}", if design { files::OPFS } else { files::DOWNLOADS })),
                                }
                            })),
                            ..Default::default()
                        };
                        let mut app = SolveApp::new(Session::default(), services);
                        if let Some(p) = cc.storage.and_then(|s| solidcraft_ui_egui::prefs::stored(|k| s.get_string(k))) {
                            app.load_prefs(&p);
                        }
                        if let Some(rs) = &cc.wgpu_render_state {
                            // eframe's web painter has no stencil buffer.
                            app.set_wgpu(rs, DEPTH_BITS, 0, 1);
                        }
                        if query().contains("sample") {
                            if let Err(e) = app.session.run_script(&solidcraft_engine::sample::script()) {
                                app.set_status(format!("sample: {e}"), true);
                            }
                            app.session.doc_mut().name = "Sample Plate".into();
                            app.session.mark_saved();
                        }
                        if !query().contains("sample") {
                            let rev = app.session.revision;
                            app.home.show(rev);
                        }
                        Ok(Box::new(Shell {
                            app,
                            files,
                            autosaved_at: f64::NEG_INFINITY,
                            autosaved: None,
                            renaming: None,
                            rename_focus: false,
                            recents_synced: false,
                            font: Default::default(),
                            font_result: Rc::new(std::cell::RefCell::new(None)),
                            store,
                            active_path,
                            tick_path: None,
                        }))
                    }),
                )
                .await;
            if let Some(el) = document.get_element_by_id(LOADING_ID) {
                match result {
                    Ok(()) => el.remove(),
                    Err(e) => {
                        el.set_inner_html(&format!("<p>SolidCraft failed to start: {e:?}</p><p>A browser with WebGPU or WebGL2 is required.</p>"))
                    }
                }
            }
        });
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("solidcraft-web only runs in the browser: build it with `trunk build --release` in apps/solidcraft-web");
}
