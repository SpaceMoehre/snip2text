#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod config;
mod ocr;

use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::Result;
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::HotKey,
};
use softbuffer::{Context, Surface};
use tray_icon::{
    TrayIcon, TrayIconBuilder, TrayIconEvent,
    MouseButton as TrayBtn, MouseButtonState as TrayBtnState,
    Icon,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::{Cursor, CursorIcon, Fullscreen, Window, WindowAttributes, WindowId, WindowLevel},
};

use config::Config;

// ── user events ───────────────────────────────────────────────────────────────

#[derive(Debug)]
enum UserEvent {
    TrayEvent(TrayIconEvent),
    MenuEvent(MenuEvent),
    HotkeyActivated,
    OcrDone(String),
    OcrError(String),
}

// ── selection state ───────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy)]
enum Phase {
    #[default]
    Idle,
    Dragging { sx: f64, sy: f64, cx: f64, cy: f64 },
}

// ── overlay window (one per monitor) ─────────────────────────────────────────

struct Overlay {
    window: Arc<Window>,
    _ctx: Context<Arc<Window>>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    background: Vec<u32>,
    width: u32,
    height: u32,
    phase: Phase,
}

// ── app ───────────────────────────────────────────────────────────────────────

struct App {
    config: Config,
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    hk_manager: GlobalHotKeyManager,
    hotkey: Option<HotKey>,
    overlays: Vec<Overlay>,
    ocr_busy: bool,
    id_configure: Option<tray_icon::menu::MenuId>,
    id_quit: Option<tray_icon::menu::MenuId>,
    id_langs: Vec<(tray_icon::menu::MenuId, String)>,
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            config: Config::load(),
            proxy,
            tray: None,
            hk_manager: GlobalHotKeyManager::new().expect("hotkey manager"),
            hotkey: None,
            overlays: Vec::new(),
            ocr_busy: false,
            id_configure: None,
            id_quit: None,
            id_langs: Vec::new(),
        }
    }

    fn activate(&mut self, event_loop: &ActiveEventLoop) {
        if !self.overlays.is_empty() || self.ocr_busy {
            return;
        }
        let monitors: Vec<_> = event_loop.available_monitors().collect();
        let monitors: Box<dyn Iterator<Item = winit::monitor::MonitorHandle>> =
            if monitors.is_empty() {
                eprintln!("activate: available_monitors empty, falling back to primary");
                Box::new(event_loop.primary_monitor().into_iter())
            } else {
                Box::new(monitors.into_iter())
            };
        for m in monitors {
            let pos = m.position();
            let sz = m.size();
            eprintln!("activate: monitor pos={:?} size={:?}", pos, sz);
            let background = match capture::capture_region(pos.x, pos.y, sz.width, sz.height) {
                Ok(px) => px,
                Err(e) => { show_error(&format!("capture failed: {e}")); continue; }
            };
            let attrs = WindowAttributes::default()
                .with_title("snip2text")
                .with_decorations(false)
                .with_visible(false)
                .with_fullscreen(Some(Fullscreen::Borderless(Some(m))))
                .with_window_level(WindowLevel::AlwaysOnTop);
            let window = match event_loop.create_window(attrs) {
                Ok(w) => Arc::new(w),
                Err(e) => { show_error(&format!("window creation failed: {e}")); continue; }
            };
            window.set_cursor(Cursor::Icon(CursorIcon::Crosshair));
            let ctx = Context::new(window.clone()).expect("softbuffer context");
            let mut surface = Surface::new(&ctx, window.clone()).expect("softbuffer surface");
            // Paint before revealing the window to avoid a white flash.
            if surface
                .resize(NonZeroU32::new(sz.width).unwrap(), NonZeroU32::new(sz.height).unwrap())
                .is_ok()
            {
                if let Ok(mut buf) = surface.buffer_mut() {
                    render_overlay(&mut buf, &background, sz.width, sz.height, Phase::Idle);
                    let _ = buf.present();
                }
            }
            window.set_visible(true);
            self.overlays.push(Overlay {
                window,
                _ctx: ctx,
                surface,
                background,
                width: sz.width,
                height: sz.height,
                phase: Phase::Idle,
            });
        }
    }

    fn close_overlays(&mut self) {
        self.overlays.clear();
    }

    fn finish_selection(&mut self, idx: usize, x1: u32, y1: u32, x2: u32, y2: u32) {
        eprintln!("finish_selection: overlay={idx} region=({x1},{y1})-({x2},{y2}) size={}x{}", x2-x1, y2-y1);
        let (background, full_width) = {
            let ov = &self.overlays[idx];
            (ov.background.clone(), ov.width)
        };
        let rw = x2 - x1;
        let rh = y2 - y1;
        let rgba = capture::extract_rgba(&background, full_width, x1, y1, rw, rh);
        let language = self.config.ocr_language.clone();
        let proxy = self.proxy.clone();
        self.ocr_busy = true;
        self.close_overlays();
        std::thread::spawn(move || {
            match ocr::run_ocr(&rgba, &language) {
                Ok(t)  => { let _ = proxy.send_event(UserEvent::OcrDone(t)); }
                Err(e) => { let _ = proxy.send_event(UserEvent::OcrError(e.to_string())); }
            }
        });
    }

    fn register_hotkey(&mut self) {
        if let Some(old) = self.hotkey.take() {
            let _ = self.hk_manager.unregister(old);
        }
        match self.config.hotkey.parse::<HotKey>() {
            Ok(hk) => {
                if self.hk_manager.register(hk).is_ok() {
                    self.hotkey = Some(hk);
                }
            }
            Err(e) => eprintln!("invalid hotkey '{}': {e}", self.config.hotkey),
        }
    }

    fn setup_tray(&mut self) {
        let languages = ocr::available_languages();
        let configure_item = MenuItem::new("Configure", true, None);
        let quit_item = MenuItem::new("Quit", true, None);

        let lang_items: Vec<MenuItem> = languages
            .iter()
            .map(|(display, _)| MenuItem::new(display.as_str(), true, None))
            .collect();
        let lang_refs: Vec<&dyn tray_icon::menu::IsMenuItem> =
            lang_items.iter().map(|i| i as _).collect();
        let lang_sub = Submenu::with_items("OCR Language", true, &lang_refs)
            .expect("lang submenu");

        let menu = Menu::new();
        menu.append_items(&[
            &lang_sub as &dyn tray_icon::menu::IsMenuItem,
            &PredefinedMenuItem::separator(),
            &configure_item,
            &PredefinedMenuItem::separator(),
            &quit_item,
        ])
        .expect("menu items");

        self.id_configure = Some(configure_item.id().clone());
        self.id_quit = Some(quit_item.id().clone());
        self.id_langs = lang_items
            .iter()
            .zip(languages.iter())
            .map(|(item, (_, tag))| (item.id().clone(), tag.clone()))
            .collect();

        self.tray = Some(
            TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_icon(make_icon())
                .with_tooltip("snip2text — click or Ctrl+Alt+S to snip")
                .build()
                .expect("tray icon"),
        );
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        self.register_hotkey();
        self.setup_tray();

        let proxy = self.proxy.clone();
        let hotkey_id = self.hotkey.map(|hk| hk.id());
        std::thread::spawn(move || {
            let recv = GlobalHotKeyEvent::receiver();
            loop {
                if let Ok(ev) = recv.recv() {
                    if hotkey_id == Some(ev.id) && ev.state == HotKeyState::Pressed {
                        let _ = proxy.send_event(UserEvent::HotkeyActivated);
                    }
                }
            }
        });
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(idx) = self.overlays.iter().position(|o| o.window.id() == window_id) else {
            return;
        };

        match event {
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed =>
            {
                use winit::keyboard::{Key, NamedKey};
                if let Key::Named(NamedKey::Escape) = event.logical_key {
                    self.close_overlays();
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if matches!(self.overlays[idx].phase, Phase::Idle) {
                    self.overlays[idx].phase =
                        Phase::Dragging { sx: 0.0, sy: 0.0, cx: 0.0, cy: 0.0 };
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                let needs_redraw =
                    if let Phase::Dragging { sx, sy, cx, cy } = &mut self.overlays[idx].phase {
                        if *sx == 0.0 && *sy == 0.0 {
                            *sx = position.x;
                            *sy = position.y;
                        }
                        *cx = position.x;
                        *cy = position.y;
                        true
                    } else {
                        false
                    };
                if needs_redraw {
                    self.overlays[idx].window.request_redraw();
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                let info = if let Phase::Dragging { sx, sy, cx, cy } = self.overlays[idx].phase {
                    let (w, h) = (self.overlays[idx].width, self.overlays[idx].height);
                    Some((sx, sy, cx, cy, w, h))
                } else {
                    None
                };
                if let Some((sx, sy, cx, cy, w, h)) = info {
                    let clamp =
                        |v: f64, max: u32| (v.max(0.0) as u32).min(max.saturating_sub(1));
                    let x1 = clamp(sx.min(cx), w);
                    let y1 = clamp(sy.min(cy), h);
                    let x2 = clamp(sx.max(cx), w);
                    let y2 = clamp(sy.max(cy), h);
                    if x2 > x1 && y2 > y1 {
                        self.finish_selection(idx, x1, y1, x2, y2);
                    } else {
                        self.close_overlays();
                    }
                }
            }

            WindowEvent::RedrawRequested => {
                let ov = &mut self.overlays[idx];
                let w = ov.width;
                let h = ov.height;
                if ov
                    .surface
                    .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
                    .is_ok()
                {
                    if let Ok(mut buf) = ov.surface.buffer_mut() {
                        render_overlay(&mut buf, &ov.background, w, h, ov.phase);
                        let _ = buf.present();
                    }
                }
            }

            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::HotkeyActivated => self.activate(event_loop),

            UserEvent::TrayEvent(TrayIconEvent::Click {
                button: TrayBtn::Left,
                button_state: TrayBtnState::Up,
                ..
            }) => self.activate(event_loop),

            UserEvent::MenuEvent(ev) => {
                if Some(&ev.id) == self.id_quit.as_ref() {
                    event_loop.exit();
                } else if Some(&ev.id) == self.id_configure.as_ref() {
                    if let Some(path) = Config::path() {
                        self.config.save();
                        let _ = open_in_editor(&path);
                    }
                } else if let Some((_, tag)) = self.id_langs.iter().find(|(id, _)| id == &ev.id) {
                    self.config.ocr_language = tag.clone();
                    self.config.save();
                }
            }

            UserEvent::OcrDone(text) => {
                self.ocr_busy = false;
                eprintln!("OCR done: {:?}", &text[..text.len().min(80)]);
                if !text.trim().is_empty() {
                    if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                        show_error(&format!("Clipboard write failed: {e}"));
                    }
                } else {
                    eprintln!("OCR: empty result");
                }
            }

            UserEvent::OcrError(e) => {
                self.ocr_busy = false;
                show_error(&format!("OCR failed: {e}"));
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {}
}

// ── rendering ─────────────────────────────────────────────────────────────────

fn render_overlay(buf: &mut [u32], bg: &[u32], w: u32, h: u32, phase: Phase) {
    match phase {
        Phase::Idle => {
            for (dst, &src) in buf.iter_mut().zip(bg.iter()) {
                *dst = dim(src);
            }
        }
        Phase::Dragging { sx, sy, cx, cy } => {
            let x1 = sx.min(cx).max(0.0) as u32;
            let y1 = sy.min(cy).max(0.0) as u32;
            let x2 = (sx.max(cx) as u32).min(w.saturating_sub(1));
            let y2 = (sy.max(cy) as u32).min(h.saturating_sub(1));
            for idx in 0..(w * h) as usize {
                let x = idx as u32 % w;
                let y = idx as u32 / w;
                let inside = x >= x1 && x <= x2 && y >= y1 && y <= y2;
                let border = inside && (x == x1 || x == x2 || y == y1 || y == y2);
                buf[idx] = if border {
                    0x00FFFFFF
                } else if inside {
                    bg[idx]
                } else {
                    dim(bg[idx])
                };
            }
        }
    }
}

#[inline]
fn dim(px: u32) -> u32 {
    (px >> 1) & 0x007F7F7F
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_icon() -> Icon {
    const S: u32 = 32;
    let mut rgba = vec![0u8; (S * S * 4) as usize];
    for i in (0..rgba.len()).step_by(4) {
        rgba[i] = 32;
        rgba[i + 1] = 178;
        rgba[i + 2] = 170;
        rgba[i + 3] = 255;
    }
    Icon::from_rgba(rgba, S, S).expect("icon")
}

fn show_error(msg: &str) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let text = windows::core::HSTRING::from(msg);
    unsafe {
        MessageBoxW(
            HWND(std::ptr::null_mut()),
            &text,
            windows::core::w!("snip2text"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn open_in_editor(path: &std::path::Path) -> Result<()> {
    std::process::Command::new("cmd")
        .args(["/c", "start", "", &path.to_string_lossy()])
        .spawn()?;
    Ok(())
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    {
        let p = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::TrayEvent(e));
        }));
    }
    {
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::MenuEvent(e));
        }));
    }
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}
