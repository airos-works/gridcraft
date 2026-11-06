//! The window's event loop on the interface thread: Haiku's input becomes egui's, SheetApp runs
//! its frame, and egui's output goes back to the window and the system (title, cursor,
//! clipboard, links) and, through Vulkan (`gpu`) or the software rasteriser, to the screen.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers, PointerButton, Pos2, Theme, ViewportCommand, ViewportEvent, ViewportId, ViewportInfo, pos2, vec2};
use gridcraft_ui_egui::SheetApp;

use crate::host::{self, RawEvent, WindowCommand, kind};
use crate::raster::Rasterizer;
use crate::services::{self, Dialogs};
use crate::{keys, save_prefs};

pub struct Shell {
    pub app: SheetApp,
    ctx: egui::Context,
    raster: Rasterizer,
    /// The GPU renderer; `None` draws with `raster` (no usable hardware Vulkan adapter,
    /// `GRIDCRAFT_RENDERER=cpu`, or the device was lost or failed). The rasteriser keeps egui's
    /// textures either way, so it can take over between two frames without losing anything.
    gpu: Option<crate::gpu::Gpu>,
    dialogs: Rc<Dialogs>,
    start: Instant,
    /// egui's points to device pixels, as of the last frame, and the system's own: the interface
    /// scale (the system font against 12 pt) times the desktop's density.
    ppp: f32,
    native_ppp: f32,
    ui_scale: f32,
    /// Device pixels per logical pixel ([`host::device_scale`]), and when it was last asked.
    dev: f32,
    dev_checked: Instant,
    buttons: i32,
    modifiers: Modifiers,
    focused: bool,
    events: Vec<Event>,
    close_requested: bool,
    quit: bool,
    /// When egui wants the next frame (from this thread's frames).
    repaint_at: Option<Instant>,
    /// When other threads asked for one (`request_repaint` from a worker).
    woken_for: Arc<Mutex<Option<Instant>>>,
    screenshots: Vec<egui::ScreenshotCallback>,
    title: String,
    /// Files saved (or about to be) that need their MIME type, until when to keep trying.
    to_type: Vec<(PathBuf, Instant)>,
    /// Each workbook's last saved state (its uid, path and saved copy), to notice a save.
    last_saved: Vec<(u64, Option<String>, usize)>,
}

/// egui's cursor as one of Haiku's `BCursorID`s.
fn cursor_id(icon: egui::CursorIcon) -> i32 {
    use egui::CursorIcon as C;
    match icon {
        C::None => 12,
        C::ContextMenu => 3,
        C::Help => 9,
        C::PointingHand => 6,
        C::Progress | C::Wait => 14,
        C::Cell | C::Crosshair => 5,
        C::Text => 2,
        C::VerticalText => 10,
        C::Alias => 29,
        C::Copy => 4,
        C::Move | C::AllScroll => 11,
        C::NoDrop | C::NotAllowed => 13,
        C::Grab => 7,
        C::Grabbing => 8,
        C::ResizeHorizontal | C::ResizeColumn => 24,
        C::ResizeVertical | C::ResizeRow => 23,
        C::ResizeNeSw => 25,
        C::ResizeNwSe => 26,
        C::ResizeEast => 16,
        C::ResizeSouthEast => 21,
        C::ResizeSouth => 17,
        C::ResizeSouthWest => 22,
        C::ResizeWest => 18,
        C::ResizeNorthWest => 20,
        C::ResizeNorth => 15,
        C::ResizeNorthEast => 19,
        C::ZoomIn => 27,
        C::ZoomOut => 28,
        _ => 1,
    }
}

fn button(bit: i32) -> Option<PointerButton> {
    Some(match bit {
        1 => PointerButton::Primary,
        2 => PointerButton::Secondary,
        4 => PointerButton::Middle,
        8 => PointerButton::Extra1,
        16 => PointerButton::Extra2,
        _ => return None,
    })
}

/// Runs `f`; a panic inside it (a wgpu validation failure, a driver gone wrong) becomes an
/// error, so the window falls back to the rasteriser instead of losing the open workbooks.
fn guard<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| Err(format!("{what} failed with an internal error (logged)")))
}

/// The renderer `GRIDCRAFT_RENDERER` asks for: `auto` (the default: Vulkan on a usable hardware
/// adapter, else the rasteriser), `vulkan` (Vulkan or an error) or `cpu`.
fn start_gpu() -> Result<Option<crate::gpu::Gpu>, String> {
    let mode = std::env::var("GRIDCRAFT_RENDERER").unwrap_or_else(|_| "auto".into());
    let gpu = match mode.as_str() {
        "cpu" => None,
        "auto" | "" | "vulkan" => match guard("GPU startup", crate::gpu::Gpu::new) {
            Ok(gpu) => Some(gpu),
            Err(e) if mode == "vulkan" => return Err(format!("GRIDCRAFT_RENDERER=vulkan: {e}")),
            Err(e) => {
                log::warn!("GPU startup: {e}; using the software rasteriser");
                None
            }
        },
        _ => return Err("GRIDCRAFT_RENDERER must be auto, cpu or vulkan".into()),
    };
    log::info!("air/OS renderer: {}", if gpu.is_some() { "Vulkan" } else { "CPU" });
    if gpu.is_none() {
        crate::raster_threads();
    }
    Ok(gpu)
}

impl Shell {
    pub fn new(app: SheetApp, ctx: egui::Context, dialogs: Rc<Dialogs>, ui_scale: f32) -> Result<Self, String> {
        let gpu = start_gpu()?;
        let dev = host::device_scale();
        let native_ppp = ui_scale * dev;
        let woken_for: Arc<Mutex<Option<Instant>>> = Arc::default();
        let wake = woken_for.clone();
        ctx.set_request_repaint_callback(move |info| {
            let at = Instant::now() + info.delay;
            if let Ok(mut w) = wake.lock() {
                *w = Some(w.map_or(at, |old| old.min(at)));
            }
            host::wake();
        });
        Ok(Self {
            app,
            ctx,
            raster: Rasterizer::new(),
            gpu,
            dialogs,
            start: Instant::now(),
            ppp: native_ppp,
            native_ppp,
            ui_scale,
            dev,
            dev_checked: Instant::now(),
            buttons: 0,
            modifiers: Modifiers::default(),
            focused: true,
            events: Vec::new(),
            close_requested: false,
            quit: false,
            repaint_at: Some(Instant::now()),
            woken_for,
            screenshots: Vec::new(),
            title: String::new(),
            to_type: Vec::new(),
            last_saved: Vec::new(),
        })
    }

    /// Runs until the app quits.
    pub fn run(&mut self) {
        while !self.quit {
            host::wait(self.timeout());
            let happened = self.drain();
            let due = self.take_due();
            if happened || due {
                self.frame();
            }
            self.present_chores();
        }
    }

    /// What the presenter does between frames: the last frame again where the window moved on
    /// the screen, and app_server's copy catching up with frames that went straight to the screen.
    fn present_chores(&mut self) {
        let dev = self.dev;
        let Some(gpu) = &mut self.gpu else { return };
        let result = if gpu.wants_present() {
            guard("GPU present", || gpu.present_again(dev))
        } else if gpu.sync_due().is_some_and(|at| at <= Instant::now()) {
            guard("GPU sync", || gpu.sync(dev))
        } else {
            return;
        };
        if let Err(e) = result {
            self.drop_gpu(&e);
        }
    }

    /// Back to the software rasteriser for good: the device failed (`why`). The next frame,
    /// at once, draws everything with it; the workbooks and egui's textures are untouched.
    fn drop_gpu(&mut self, why: &str) {
        if let Some(gpu) = self.gpu.take() {
            log::error!("GPU: {why}; switching to the software rasteriser");
            // Dropping waits for the device; a panic there must not take the interface down.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(gpu))).is_err() {
                log::warn!("GPU: releasing the device failed");
            }
            crate::raster_threads();
            self.raster.invalidate();
            self.repaint_at = Some(Instant::now());
        }
    }

    /// When the next frame is due, if one is.
    fn next_due(&self) -> Option<Instant> {
        let woken = self.woken_for.lock().ok().and_then(|w| *w);
        let sync = self.gpu.as_ref().and_then(|g| g.sync_due());
        [self.repaint_at, woken, sync].into_iter().flatten().min()
    }

    fn timeout(&self) -> Option<Duration> {
        self.next_due().map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// Whether a frame is due now; clears what made it due.
    fn take_due(&mut self) -> bool {
        let now = Instant::now();
        let due = self.next_due().is_some_and(|at| at <= now);
        if due {
            self.repaint_at = None;
            if let Ok(mut w) = self.woken_for.lock()
                && w.is_some_and(|at| at <= now)
            {
                *w = None;
            }
        }
        due
    }

    /// A point in the window (logical pixels) in egui's points.
    fn pos(&self, x: f32, y: f32) -> Pos2 {
        pos2(x * self.dev / self.ppp, y * self.dev / self.ppp)
    }

    /// Asks for the desktop's density again (the monitors may have changed).
    fn check_density(&mut self) {
        self.dev_checked = Instant::now();
        let dev = host::device_scale();
        if (dev - self.dev).abs() > f32::EPSILON {
            log::info!("display density {} -> {dev}", self.dev);
            self.dev = dev;
            self.native_ppp = self.ui_scale * dev;
        }
    }

    fn set_modifiers(&mut self, bits: u32) {
        let m = keys::modifiers(bits);
        if m != self.modifiers {
            self.modifiers = m;
            self.events.push(Event::ModifiersChanged(m));
        }
    }

    /// Turns the queued native events into egui's, starting with those that came while a file
    /// panel was open. Whether there were any.
    fn drain(&mut self) -> bool {
        let mut any = false;
        loop {
            let deferred = self.dialogs.deferred.borrow_mut().pop_front();
            let Some((ev, text)) = deferred.or_else(host::next_event) else { break };
            any = true;
            self.event(ev, text);
        }
        any
    }

    fn event(&mut self, ev: RawEvent, text: String) {
        match ev.kind {
            kind::MOUSE_MOVED | kind::MOUSE_DOWN | kind::MOUSE_UP => {
                self.set_modifiers(ev.modifiers);
                let pos = self.pos(ev.x, ev.y);
                self.events.push(Event::PointerMoved(pos));
                let changed = ev.buttons ^ self.buttons;
                for bit in [1, 2, 4, 8, 16] {
                    if changed & bit != 0
                        && let Some(b) = button(bit)
                    {
                        self.events.push(Event::PointerButton { pos, button: b, pressed: ev.buttons & bit != 0, modifiers: self.modifiers });
                    }
                }
                self.buttons = ev.buttons;
            }
            kind::POINTER_GONE => self.events.push(Event::PointerGone),
            kind::WHEEL => {
                self.set_modifiers(ev.modifiers);
                self.events.push(Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: vec2(-ev.dx, -ev.dy),
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Wheel,
                    modifiers: self.modifiers,
                });
            }
            kind::KEY_DOWN | kind::KEY_UP => self.key(ev.kind == kind::KEY_DOWN, ev.key, ev.raw_char, ev.modifiers, &text),
            kind::MODIFIERS => self.set_modifiers(host::modifiers()),
            kind::FOCUS => {
                self.focused = ev.x != 0.0;
                self.events.push(Event::WindowFocused(self.focused));
                if !self.focused {
                    // Keys released elsewhere never come back up.
                    self.set_modifiers(0);
                }
            }
            kind::CLOSE | kind::QUIT_REQUESTED => self.close_requested = true,
            // Tracker's Open, Open With and drops on the icon, and files dropped on the
            // window: they open as workbooks, as File › Open opens them (the desktop app
            // opens dropped files the same way). Each replaces an untouched blank Book1.
            kind::OPEN_FILES | kind::DROPPED_FILES => {
                for path in text.lines().filter(|l| !l.is_empty()) {
                    self.app.open_path(path);
                }
                host::window_command(WindowCommand::Activate);
            }
            kind::COLORS => self.check_density(),
            // The next frame picks up the new size, position or state.
            kind::RESIZED | kind::MOVED | kind::MINIMIZED => {}
            // A panel's answer after its caller stopped waiting has nobody to go to.
            _ => {}
        }
    }

    fn key(&mut self, pressed: bool, code: i32, raw_char: i32, bits: u32, text: &str) {
        self.set_modifiers(bits);
        let (logical, physical) = keys::key(code, raw_char, text.as_bytes());
        log::debug!(
            "key {} code {code:#x} raw {raw_char:#x} modifiers {bits:#x} {text:?}: {logical:?} / {physical:?}",
            if pressed { "down" } else { "up" }
        );
        let m = self.modifiers;
        if let Some(active) = logical.or(physical) {
            if pressed {
                // As egui's own integrations do: the clipboard shortcuts become clipboard events.
                if m.command && active == Key::X || m.shift && active == Key::Delete {
                    self.events.push(Event::Cut);
                    return;
                }
                if m.command && (active == Key::C || active == Key::Insert) {
                    self.events.push(Event::Copy);
                    return;
                }
                if m.command && active == Key::V || m.shift && active == Key::Insert {
                    self.paste();
                    return;
                }
            }
            self.events.push(Event::Key { key: active, physical_key: physical, pressed, repeat: false, modifiers: m });
        }
        if pressed && let Some(t) = keys::text(bits, text) {
            self.events.push(Event::Text(t.to_string()));
        }
    }

    fn viewport_info(&self, w: &host::WindowInfo, close: bool) -> ViewportInfo {
        let k = self.dev / self.ppp;
        let rect = |r: [f32; 4]| egui::Rect::from_min_max(pos2(r[0], r[1]) * k, pos2(r[2], r[3]) * k);
        ViewportInfo {
            title: Some(self.title.clone()),
            events: if close { vec![ViewportEvent::Close] } else { Vec::new() },
            native_pixels_per_point: Some(self.native_ppp),
            monitor_size: (w.screen[0] > 0.0).then(|| vec2(w.screen[0], w.screen[1]) * k),
            inner_rect: Some(rect(w.inner)),
            outer_rect: Some(rect(w.outer)),
            minimized: Some(w.minimized),
            maximized: Some(false),
            fullscreen: Some(false),
            focused: Some(w.active),
            ..Default::default()
        }
    }

    /// The desktop app's per-frame logic (`apps/gridcraft`). The window's close button asks
    /// about unsaved workbooks first (the app cancels the close and closes the window itself once
    /// they are answered) and waits for a print job being handed over.
    fn logic(&mut self, ctx: &egui::Context) {
        self.app.logic(ctx);
        if ctx.input(|i| i.viewport().close_requested()) && self.app.printing.busy() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        }
    }

    fn frame(&mut self) {
        if self.gpu.as_ref().is_some_and(|g| g.faulted()) {
            self.drop_gpu("the device was lost or failed");
        }
        if self.dev_checked.elapsed() > Duration::from_secs(2) {
            self.check_density();
        }
        let (lw, lh) = host::view_size();
        let (w, h) = ((lw * self.dev).round(), (lh * self.dev).round());
        if w < 1.0 || h < 1.0 {
            self.repaint_at = Some(Instant::now() + Duration::from_millis(50));
            return;
        }
        let close = std::mem::take(&mut self.close_requested);
        // Once a frame: each call locks the window.
        let window = host::window_info();
        let raw = egui::RawInput {
            viewport_id: ViewportId::ROOT,
            viewports: std::iter::once((ViewportId::ROOT, self.viewport_info(&window, close))).collect(),
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, vec2(w, h) / self.ppp)),
            max_texture_side: Some(self.gpu.as_ref().map_or(8192, |g| g.max_texture_side().min(16384))),
            time: Some(self.start.elapsed().as_secs_f64()),
            predicted_dt: 1.0 / 60.0,
            events: std::mem::take(&mut self.events),
            focused: self.focused,
            system_theme: Some(if host::dark() { Theme::Dark } else { Theme::Light }),
            ..Default::default()
        };
        let mut raw = raw;
        self.app.raw_input_hook(&self.ctx, &mut raw);
        let ctx = self.ctx.clone();
        let t0 = Instant::now();
        let mut out = ctx.run_ui(raw, |ui| {
            let ctx = ui.ctx().clone();
            self.logic(&ctx);
            self.app.ui(ui);
        });

        self.type_saved_files();

        // The system side of egui's output.
        host::set_cursor(cursor_id(out.platform_output.cursor_icon));
        for cmd in &out.platform_output.commands {
            log::debug!("output {cmd:?}");
            match cmd {
                egui::OutputCommand::CopyText(t) => {
                    if !host::clipboard_set(t, None) {
                        log::warn!("clipboard: couldn't copy");
                    }
                }
                egui::OutputCommand::CopyImage(img) => copy_image(img),
                egui::OutputCommand::OpenUrl(u) => {
                    if !host::open_url(&u.url) {
                        log::warn!("couldn't open {}", u.url);
                    }
                }
                // Haiku has no PRIMARY selection (X11's middle-click paste).
                egui::OutputCommand::TextSelectionSettled(_) => {}
            }
        }
        let mut delay = Duration::MAX;
        if let Some(vp) = out.viewport_output.get(&ViewportId::ROOT) {
            // A close the app didn't cancel (unsaved workbooks are asked about first) quits.
            if close && !vp.commands.iter().any(|c| matches!(c, ViewportCommand::CancelClose)) {
                self.quit = true;
            }
            delay = vp.repaint_delay;
            if let Some(t) = &vp.builder.title
                && *t != self.title
            {
                self.title.clone_from(t);
                host::set_title(t);
                services::set_title(t);
            }
            for cmd in &vp.commands {
                self.command(cmd);
            }
        }

        // Draw.
        let ppp = out.pixels_per_point;
        // The rasteriser keeps every texture even while the GPU draws, so it can take over.
        self.raster.set_textures(&out.textures_delta);
        let mut failed = None;
        if let Some(gpu) = &mut self.gpu
            && let Err(e) = guard("GPU textures", || {
                gpu.set_textures(&out.textures_delta);
                Ok(())
            })
        {
            failed = Some(e);
        }
        if let Some(e) = failed.take() {
            self.drop_gpu(&e);
        }
        if !window.minimized {
            let t1 = Instant::now();
            let prims = ctx.tessellate(out.shapes, ppp);
            let t2 = Instant::now();
            let clear = ctx.global_style().visuals.panel_fill;
            let (pw, ph) = (w as usize, h as usize);
            let dev = self.dev;
            // The presenter shows GPU frames itself; the rasteriser's are shown below.
            let dirty = match &mut self.gpu {
                Some(gpu) => {
                    let size = [u32::try_from(pw).unwrap_or(u32::MAX), u32::try_from(ph).unwrap_or(u32::MAX)];
                    if let Err(e) = guard("GPU frame", || gpu.render(size, &prims, ppp, clear, dev)) {
                        failed = Some(e);
                    }
                    None
                }
                None => self.raster.render([pw, ph], &prims, ppp, clear),
            };
            if let Some(e) = failed.take() {
                // Drawn again at once, by the rasteriser.
                self.drop_gpu(&e);
                delay = Duration::ZERO;
            }
            let t3 = Instant::now();
            if let Some(dirty) = dirty {
                host::present(self.raster.pixels(), pw, ph, dirty, self.dev);
            }
            log::debug!(
                "frame {pw}x{ph}: ui {:.1} ms, tessellate {:.1} ms, {} {:.1} ms, present {:.1} ms, dirty {dirty:?}",
                (t1 - t0).as_secs_f64() * 1e3,
                (t2 - t1).as_secs_f64() * 1e3,
                if self.gpu.is_some() { "GPU" } else { "raster" },
                (t3 - t2).as_secs_f64() * 1e3,
                t3.elapsed().as_secs_f64() * 1e3
            );
        }
        self.raster.free_textures(&out.textures_delta);
        if let Some(gpu) = &mut self.gpu {
            gpu.free_textures(&out.textures_delta);
        }
        // Applied: epaint checks (in debug builds) that no texture update is dropped unhandled.
        out.textures_delta.clear();
        for shot in std::mem::take(&mut self.screenshots) {
            let image = match self.gpu.as_mut() {
                Some(gpu) => guard("GPU screenshot", || gpu.image()).unwrap_or_else(|e| {
                    log::warn!("screenshot: {e}");
                    egui::ColorImage::default()
                }),
                None => self.raster.image(),
            };
            shot.complete(Arc::new(image));
            delay = Duration::ZERO;
        }
        if (ppp - self.ppp).abs() > f32::EPSILON {
            self.ppp = ppp;
            delay = Duration::ZERO;
        }
        if !self.events.is_empty() {
            delay = Duration::ZERO;
        }
        if !self.to_type.is_empty() {
            // Look again shortly for the files still being written.
            delay = delay.min(Duration::from_millis(250));
        }
        self.repaint_at = (delay < Duration::from_secs(3600)).then(|| Instant::now() + delay);
    }

    fn command(&mut self, cmd: &ViewportCommand) {
        // Points to logical pixels.
        let ppp = self.ppp / self.dev;
        match cmd {
            ViewportCommand::Close => self.quit = true,
            ViewportCommand::Title(t) => {
                if *t != self.title {
                    self.title.clone_from(t);
                    host::set_title(t);
                    services::set_title(t);
                }
            }
            ViewportCommand::InnerSize(s) => host::window_command(WindowCommand::ResizeTo(s.x * ppp, s.y * ppp)),
            ViewportCommand::MinInnerSize(s) => host::window_command(WindowCommand::MinSize(s.x * ppp, s.y * ppp)),
            ViewportCommand::OuterPosition(p) => host::window_command(WindowCommand::MoveTo(p.x * ppp, p.y * ppp)),
            ViewportCommand::Minimized(m) => host::window_command(WindowCommand::Minimize(*m)),
            ViewportCommand::Maximized(true) => host::window_command(WindowCommand::Zoom),
            ViewportCommand::Focus => host::window_command(WindowCommand::Activate),
            ViewportCommand::Screenshot(data) => self.screenshots.push(data.clone()),
            ViewportCommand::RequestCut => self.events.push(Event::Cut),
            ViewportCommand::RequestCopy => self.events.push(Event::Copy),
            ViewportCommand::RequestPaste => self.paste(),
            _ => {}
        }
    }

    /// Files GridCraft saves are written by the engine with plain file calls; Haiku programs'
    /// files carry their MIME type, which Tracker opens them by. A workbook whose saved copy
    /// changed with a path was just saved; a Save panel's path (an export too) may be written a
    /// frame or two later.
    fn type_saved_files(&mut self) {
        let now = Instant::now();
        let state: Vec<(u64, Option<String>, usize)> =
            self.app.session.documents().iter().map(|d| (d.uid, d.path.clone(), Arc::as_ptr(&d.saved) as usize)).collect();
        for (uid, path, saved) in &state {
            let before = self.last_saved.iter().find(|(u, ..)| u == uid);
            if let Some(path) = path
                && before.is_some_and(|(_, p, s)| s != saved || p.as_ref() != Some(path))
            {
                self.to_type.push((PathBuf::from(path), now + Duration::from_secs(10)));
            }
        }
        self.last_saved = state;
        for path in self.dialogs.saved.borrow_mut().drain(..) {
            self.to_type.push((path, now + Duration::from_secs(10)));
        }
        self.to_type.retain(|(path, until)| {
            if path.exists() {
                if !host::type_file(path) {
                    log::debug!("couldn't set the type of {}", path.display());
                }
                false
            } else {
                now < *until
            }
        });
    }

    /// The clipboard's text as a paste, or (with no text) its picture, as egui's integrations do.
    fn paste(&mut self) {
        if let Some(t) = services::clipboard_text() {
            self.events.push(Event::Paste(t.replace("\r\n", "\n")));
        } else if let Some(img) = services::clipboard_image() {
            let size = [img.width() as usize, img.height() as usize];
            self.events.push(Event::PasteImage(Arc::new(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))));
        }
    }

    /// What quitting does besides closing the window (`on_exit` in the desktop app).
    pub fn finish(&mut self) {
        save_prefs(&self.app);
    }
}

/// egui's image copy, as PNG.
fn copy_image(img: &egui::ColorImage) {
    let [w, h] = img.size;
    let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    let mut png = Vec::new();
    let encoded = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
        .ok_or_else(|| "bad size".to_string())
        .and_then(|i| i.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| e.to_string()));
    match encoded {
        Ok(()) => {
            if !host::clipboard_set_picture("image/png", &png) {
                log::warn!("clipboard: couldn't copy the picture");
            }
        }
        Err(e) => log::warn!("clipboard picture: {e}"),
    }
}
