//! Minimal PDF viewer: pdfium (rendering) + winit (window) + softbuffer (blit).
//!
//! Design for low memory + smoothness:
//!  * Pages are rendered in 512x512 tiles, only for what is on screen (+1 tile margin),
//!    so memory is independent of page count and zoom level.
//!  * Tiles live in an LRU-ish cache with a fixed byte budget.
//!  * Rendering happens on a worker thread; the UI thread only blits cached tiles,
//!    so scrolling never blocks. Missing tiles show as white paper until they arrive.
//!  * Event-driven: no redraws when idle (0% CPU).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use pdfium_render::prelude::*;
use softbuffer::{Context, Surface};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

const TILE: i32 = 512;
const GAP: f32 = 8.0; // points: gap between pages and side margin
const CACHE_BUDGET: usize = 64 * 1024 * 1024;
const MAX_WANTED: usize = 48;
const BG: u32 = 0x0038_3838;
const PAPER: u32 = 0x00FF_FFFF;

// ───────────────────────── shared types ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct TileKey {
    doc: u32,
    page: u32,
    scale: u32, // pixels-per-point * 1000
    tx: u32,
    ty: u32,
}

enum Msg {
    Loaded { doc: u32, sizes: Vec<(f32, f32)> },
    Failed { doc: u32, err: String },
    Fatal(String),
    Tile { key: TileKey, w: i32, h: i32, data: Vec<u32> },
}

#[derive(Debug)]
struct Wake;

#[derive(Default)]
struct State {
    wanted: Vec<TileKey>, // priority order, most important first
    current: Option<TileKey>,
    open: Option<(u32, PathBuf)>,
    quit: bool,
}

struct Shared {
    m: Mutex<State>,
    cv: Condvar,
}

#[cfg(windows)]
fn msgbox(title: &str, text: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(h: *mut core::ffi::c_void, t: *const u16, c: *const u16, f: u32) -> i32;
    }
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (t, c) = (w(text), w(title));
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), 0x10);
    }
}
#[cfg(not(windows))]
fn msgbox(title: &str, text: &str) {
    eprintln!("{title}: {text}");
}

// ───────────────────────── worker thread (owns pdfium) ─────────────────────────

fn load_pdfium() -> Result<Pdfium, String> {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_default();
    let bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir))
        .or_else(|_| Pdfium::bind_to_system_library())
        .map_err(|e| format!("Could not load pdfium.dll.\nPut it next to the executable.\n\n{e}"))?;
    Ok(Pdfium::new(bindings))
}

fn wait_open(sh: &Shared) -> Option<(u32, PathBuf)> {
    let mut g = sh.m.lock().unwrap();
    loop {
        if g.quit {
            return None;
        }
        if let Some(o) = g.open.take() {
            return Some(o);
        }
        g = sh.cv.wait(g).unwrap();
    }
}

fn worker(sh: Arc<Shared>, tx: Sender<Msg>, proxy: EventLoopProxy<Wake>) {
    let send = |m: Msg| {
        let _ = tx.send(m);
        let _ = proxy.send_event(Wake);
    };
    let pdfium = match load_pdfium() {
        Ok(p) => p,
        Err(e) => return send(Msg::Fatal(e)),
    };
    let mut next = wait_open(&sh);
    while let Some((id, path)) = next {
        next = serve(&pdfium, id, &path, &sh, &send);
    }
}

/// Serves one document until a new one is requested (returns it) or we quit (None).
fn serve(
    pdfium: &Pdfium,
    doc_id: u32,
    path: &Path,
    sh: &Shared,
    send: &dyn Fn(Msg),
) -> Option<(u32, PathBuf)> {
    let doc = match pdfium.load_pdf_from_file(path, None) {
        Ok(d) => d,
        Err(e) => {
            send(Msg::Failed { doc: doc_id, err: format!("{}\n\n{e}", path.display()) });
            return wait_open(sh);
        }
    };
    let pages = doc.pages();
    let sizes: Vec<(f32, f32)> = (0..pages.len())
        .map(|i| match pages.get(i) {
            Ok(p) => (p.width().value.max(1.0), p.height().value.max(1.0)),
            Err(_) => (595.0, 842.0),
        })
        .collect();
    send(Msg::Loaded { doc: doc_id, sizes });

    // Keep a few parsed pages around so neighbouring tiles don't re-parse.
    let mut open_pages: Vec<(u32, PdfPage)> = Vec::new();

    loop {
        let key = {
            let mut g = sh.m.lock().unwrap();
            loop {
                if g.quit {
                    return None;
                }
                if let Some(o) = g.open.take() {
                    return Some(o);
                }
                if !g.wanted.is_empty() {
                    let k = g.wanted.remove(0);
                    g.current = Some(k);
                    break k;
                }
                g = sh.cv.wait(g).unwrap();
            }
        };

        if key.doc == doc_id {
            let idx = match open_pages.iter().position(|(p, _)| *p == key.page) {
                Some(i) => Some(i),
                None => match pages.get(key.page as i32) {
                    Ok(p) => {
                        if open_pages.len() >= 4 {
                            open_pages.remove(0);
                        }
                        open_pages.push((key.page, p));
                        Some(open_pages.len() - 1)
                    }
                    Err(_) => None,
                },
            };
            if let Some(i) = idx {
                if let Some((w, h, data)) = render_tile(&open_pages[i].1, key) {
                    send(Msg::Tile { key, w, h, data });
                }
            }
        }
        sh.m.lock().unwrap().current = None;
    }
}

fn render_tile(page: &PdfPage, key: TileKey) -> Option<(i32, i32, Vec<u32>)> {
    let s = key.scale as f32 / 1000.0;
    let pw = (page.width().value * s).round().max(1.0) as i32;
    let ph = (page.height().value * s).round().max(1.0) as i32;
    let (x0, y0) = (key.tx as i32 * TILE, key.ty as i32 * TILE);
    let (tw, th) = ((pw - x0).min(TILE), (ph - y0).min(TILE));
    if tw <= 0 || th <= 0 {
        return None;
    }
    let mut bmp = PdfBitmap::empty(tw as _, th as _, PdfBitmapFormat::BGRx).ok()?;
    let cfg = PdfRenderConfig::new()
        .set_format(PdfBitmapFormat::BGRx)
        .set_reverse_byte_order(false) // keep native B,G,R,x -> 0x00RRGGBB as u32
        .set_fixed_size(pw as _, ph as _) // whole page size in pixels...
        .set_origin(-x0 as _, -y0 as _); // ...but only this tile lands in the small bitmap
    page.render_into_bitmap_with_config(&mut bmp, &cfg).ok()?;

    let raw = bmp.as_raw_bytes();
    let (tw_u, th_u) = (tw as usize, th as usize);
    if raw.len() < tw_u * 4 * th_u {
        return None;
    }
    let stride = raw.len() / th_u;
    let mut data = Vec::with_capacity(tw_u * th_u);
    for row in 0..th_u {
        let r = &raw[row * stride..row * stride + tw_u * 4];
        data.extend(r.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], 0])));
    }
    Some((tw, th, data))
}

// ───────────────────────── blitting helpers ─────────────────────────

fn fill_rect(dst: &mut [u32], dw: i32, dh: i32, x: i32, y: i32, w: i32, h: i32, c: u32) {
    let (x0, x1, y0, y1) = (x.max(0), (x + w).min(dw), y.max(0), (y + h).min(dh));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for row in y0..y1 {
        let s = (row * dw + x0) as usize;
        dst[s..s + (x1 - x0) as usize].fill(c);
    }
}

fn blit(dst: &mut [u32], dw: i32, dh: i32, src: &[u32], sw: i32, sh: i32, x: i32, y: i32) {
    let (x0, x1, y0, y1) = (x.max(0), (x + sw).min(dw), y.max(0), (y + sh).min(dh));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let n = (x1 - x0) as usize;
    for row in y0..y1 {
        let s = ((row - y) * sw + (x0 - x)) as usize;
        let d = (row * dw + x0) as usize;
        dst[d..d + n].copy_from_slice(&src[s..s + n]);
    }
}

// ───────────────────────── application ─────────────────────────

struct PageInfo {
    w: f32,
    h: f32,
    y: f32, // top, in points, from document top
}

struct Tile {
    w: i32,
    h: i32,
    data: Vec<u32>,
    used: u64,
}

struct App {
    shared: Arc<Shared>,
    rx: Receiver<Msg>,
    initial: Option<PathBuf>,

    window: Option<Arc<Window>>,
    _ctx: Option<Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,

    doc: u32,
    name: String,
    pages: Vec<PageInfo>,
    max_w: f32,
    total_h: f32,
    fit_pending: bool,

    scale: f32, // pixels per point, quantized to 1/1000
    sf: f64,    // monitor DPI scale factor
    scroll_x: f32,
    scroll_y: f32,
    target_y: f32,
    animating: bool,
    last_frame: Instant,
    vw: u32,
    vh: u32,

    cache: HashMap<TileKey, Tile>,
    cache_bytes: usize,
    last_skey: u32,
    frame: u64,

    mods: ModifiersState,
    cursor: (f32, f32),
    drag: Option<(f32, f32)>,
    last_title: String,
    fatal: bool,
}

fn q(s: f32) -> f32 {
    (s * 1000.0).round() / 1000.0
}

impl App {
    fn new(shared: Arc<Shared>, rx: Receiver<Msg>, initial: Option<PathBuf>) -> Self {
        App {
            shared, rx, initial,
            window: None, _ctx: None, surface: None,
            doc: 0, name: String::new(), pages: Vec::new(), max_w: 0.0, total_h: 0.0,
            fit_pending: false,
            scale: 1.0, sf: 1.0, scroll_x: 0.0, scroll_y: 0.0, target_y: 0.0,
            animating: false, last_frame: Instant::now(), vw: 0, vh: 0,
            cache: HashMap::new(), cache_bytes: 0, last_skey: 0, frame: 0,
            mods: ModifiersState::empty(), cursor: (0.0, 0.0), drag: None,
            last_title: String::new(), fatal: false,
        }
    }

    // ----- geometry -----
    fn base(&self) -> f32 {
        96.0 / 72.0 * self.sf as f32 // 100% zoom = 96 dpi, DPI-aware
    }
    fn skey(&self) -> u32 {
        (self.scale * 1000.0).round() as u32
    }
    fn page_px(&self, i: usize) -> (i32, i32) {
        let p = &self.pages[i];
        ((p.w * self.scale).round().max(1.0) as i32, (p.h * self.scale).round().max(1.0) as i32)
    }
    fn content_w(&self) -> f32 {
        (self.max_w + 2.0 * GAP) * self.scale
    }
    fn content_h(&self) -> f32 {
        self.total_h * self.scale
    }
    fn origin_x(&self) -> f32 {
        let (cw, vw) = (self.content_w(), self.vw as f32);
        if cw < vw { (vw - cw) / 2.0 } else { -self.scroll_x }
    }
    fn page_screen_x(&self, i: usize) -> i32 {
        let (w, _) = self.page_px(i);
        (self.origin_x() + self.content_w() / 2.0 - w as f32 / 2.0).round() as i32
    }
    fn page_screen_y(&self, i: usize) -> i32 {
        (self.pages[i].y * self.scale).round() as i32 - self.scroll_y.round() as i32
    }
    fn clamp_scroll(&mut self) {
        let mh = (self.content_h() - self.vh as f32).max(0.0);
        let mw = (self.content_w() - self.vw as f32).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, mh);
        self.target_y = self.target_y.clamp(0.0, mh);
        self.scroll_x = self.scroll_x.clamp(0.0, mw);
    }
    fn default_scale(&self) -> f32 {
        let base = self.base();
        let fit = self.vw as f32 / (self.pages[0].w + 2.0 * GAP);
        q(fit.min(1.5 * base).max(0.2 * base))
    }

    // ----- actions -----
    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn open(&mut self, path: PathBuf) {
        self.doc += 1;
        self.cache.clear();
        self.cache_bytes = 0;
        self.pages.clear();
        self.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        {
            let mut g = self.shared.m.lock().unwrap();
            g.open = Some((self.doc, path));
            g.wanted.clear();
        }
        self.shared.cv.notify_all();
        self.request_redraw();
    }

    fn shutdown(&self) {
        self.shared.m.lock().unwrap().quit = true;
        self.shared.cv.notify_all();
    }

    fn set_pages(&mut self, sizes: Vec<(f32, f32)>) {
        let mut y = GAP;
        self.pages = sizes
            .iter()
            .map(|&(w, h)| {
                let p = PageInfo { w, h, y };
                y += h + GAP;
                p
            })
            .collect();
        self.total_h = y;
        self.max_w = sizes.iter().fold(0.0f32, |a, s| a.max(s.0));
        self.fit_pending = true;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.target_y = 0.0;
    }

    fn scroll_by(&mut self, dy: f32) {
        if !self.animating {
            self.last_frame = Instant::now();
        }
        self.target_y += dy;
        self.request_redraw();
    }

    /// Zoom keeping the document point under (ax, ay) fixed.
    fn zoom_at(&mut self, factor: f32, ax: f32, ay: f32) {
        if self.pages.is_empty() {
            return;
        }
        let base = self.base();
        let ns = q((self.scale * factor).clamp(0.2 * base, 8.0 * base));
        if (ns - self.scale).abs() < 1e-6 {
            return;
        }
        let r = ns / self.scale;
        let ox = self.origin_x();
        self.scroll_y = (self.scroll_y + ay) * r - ay;
        self.scroll_x = (ax - ox) * r - ax;
        self.target_y = self.scroll_y;
        self.scale = ns;
        self.request_redraw();
    }

    fn zoom_center(&mut self, factor: f32) {
        self.zoom_at(factor, self.vw as f32 / 2.0, self.vh as f32 / 2.0);
    }

    fn reset_zoom(&mut self) {
        if !self.pages.is_empty() && self.vw > 0 {
            let f = self.default_scale() / self.scale;
            self.zoom_center(f);
        }
    }

    fn key(&mut self, k: Key) {
        let (sf, vh) = (self.sf as f32, self.vh as f32);
        let shift = self.mods.shift_key();
        match k.as_ref() {
            Key::Character("+") | Key::Character("=") => self.zoom_center(1.25),
            Key::Character("-") | Key::Character("_") => self.zoom_center(0.8),
            Key::Character("0") => self.reset_zoom(),
            Key::Named(NamedKey::ArrowDown) => self.scroll_by(80.0 * sf),
            Key::Named(NamedKey::ArrowUp) => self.scroll_by(-80.0 * sf),
            Key::Named(NamedKey::PageDown) => self.scroll_by(vh * 0.9),
            Key::Named(NamedKey::PageUp) => self.scroll_by(-vh * 0.9),
            Key::Named(NamedKey::Space) => self.scroll_by(if shift { -vh * 0.9 } else { vh * 0.9 }),
            Key::Named(NamedKey::Home) => self.scroll_by(-1e9),
            Key::Named(NamedKey::End) => self.scroll_by(1e9),
            Key::Named(NamedKey::ArrowLeft) => {
                self.scroll_x -= 80.0 * sf;
                self.request_redraw();
            }
            Key::Named(NamedKey::ArrowRight) => {
                self.scroll_x += 80.0 * sf;
                self.request_redraw();
            }
            _ => {}
        }
    }

    // ----- messages from worker -----
    fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Loaded { doc, sizes } if doc == self.doc => {
                    self.set_pages(sizes);
                    changed = true;
                }
                Msg::Failed { doc, err } if doc == self.doc => {
                    msgbox("Cannot open PDF", &err);
                    self.name.clear();
                    changed = true;
                }
                Msg::Fatal(e) => {
                    msgbox("PDF Viewer", &e);
                    self.fatal = true;
                }
                Msg::Tile { key, w, h, data } if key.doc == self.doc && key.scale == self.skey() => {
                    self.cache_bytes += data.len() * 4;
                    let t = Tile { w, h, data, used: self.frame };
                    if let Some(old) = self.cache.insert(key, t) {
                        self.cache_bytes -= old.data.len() * 4;
                    }
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    fn evict(&mut self) {
        while self.cache_bytes > CACHE_BUDGET {
            let victim = self.cache.iter().min_by_key(|(_, t)| t.used).map(|(k, t)| (*k, t.used));
            match victim {
                Some((k, used)) if used < self.frame => {
                    if let Some(t) = self.cache.remove(&k) {
                        self.cache_bytes -= t.data.len() * 4;
                    }
                }
                _ => break, // everything left is on screen right now
            }
        }
    }

    // ----- drawing -----
    fn redraw(&mut self) {
        self.drain();
        let Some(window) = self.window.clone() else { return };
        if self.fit_pending && self.vw > 0 && !self.pages.is_empty() {
            self.scale = self.default_scale();
            self.fit_pending = false;
        }
        if self.vw == 0 || self.vh == 0 {
            return;
        }

        // smooth scrolling: ease scroll_y toward target_y
        self.clamp_scroll();
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;
        let diff = self.target_y - self.scroll_y;
        if diff.abs() > 0.5 {
            self.scroll_y += diff * (1.0 - (-dt * 22.0).exp());
            self.animating = true;
        } else {
            self.scroll_y = self.target_y;
            self.animating = false;
        }

        // drop tiles of an old zoom level right away
        let skey = self.skey();
        if skey != self.last_skey {
            self.cache.retain(|k, _| k.scale == skey);
            self.cache_bytes = self.cache.values().map(|t| t.data.len() * 4).sum();
            self.last_skey = skey;
        }

        let (vw, vh) = (self.vw as i32, self.vh as i32);
        self.frame += 1;
        let scale = self.scale;
        let (top, bottom) = (self.scroll_y, self.scroll_y + vh as f32);
        let margin = TILE; // prefetch one tile beyond the viewport

        // visible pages: (index, screen x, screen y, width px, height px)
        let mut vis = Vec::new();
        let first = self.pages.partition_point(|p| (p.y + p.h) * scale < top - margin as f32);
        for i in first..self.pages.len() {
            if self.pages[i].y * scale > bottom + margin as f32 {
                break;
            }
            let (w, h) = self.page_px(i);
            vis.push((i, self.page_screen_x(i), self.page_screen_y(i), w, h));
        }

        let Some(surface) = self.surface.as_mut() else { return };
        let _ = surface.resize(NonZeroU32::new(vw as u32).unwrap(), NonZeroU32::new(vh as u32).unwrap());
        let Ok(mut buf) = surface.buffer_mut() else { return };
        buf.fill(BG);

        let (cx, cy) = (vw as f32 / 2.0, vh as f32 / 2.0);
        let mut missing: Vec<(f32, TileKey)> = Vec::new();
        for &(i, px, py, w, h) in &vis {
            fill_rect(&mut buf, vw, vh, px, py, w, h, PAPER);
            let (cols, rows) = ((w + TILE - 1) / TILE, (h + TILE - 1) / TILE);
            let tx0 = (-px - margin).max(0) / TILE;
            let tx1 = cols.min(((vw - px + margin).max(0) + TILE - 1) / TILE);
            let ty0 = (-py - margin).max(0) / TILE;
            let ty1 = rows.min(((vh - py + margin).max(0) + TILE - 1) / TILE);
            for ty in ty0..ty1 {
                for tx in tx0..tx1 {
                    let key = TileKey { doc: self.doc, page: i as u32, scale: skey, tx: tx as u32, ty: ty as u32 };
                    let (dx, dy) = (px + tx * TILE, py + ty * TILE);
                    if let Some(t) = self.cache.get_mut(&key) {
                        t.used = self.frame;
                        blit(&mut buf, vw, vh, &t.data, t.w, t.h, dx, dy);
                    } else {
                        let (tw, th) = (TILE.min(w - tx * TILE), TILE.min(h - ty * TILE));
                        let d = ((dx + tw / 2) as f32 - cx).powi(2) + ((dy + th / 2) as f32 - cy).powi(2);
                        missing.push((d, key));
                    }
                }
            }
        }
        let _ = buf.present();

        // ask the worker for what's missing, nearest-to-center first
        missing.sort_by(|a, b| a.0.total_cmp(&b.0));
        {
            let mut g = self.shared.m.lock().unwrap();
            let list: Vec<TileKey> = missing
                .into_iter()
                .map(|m| m.1)
                .filter(|k| Some(*k) != g.current)
                .take(MAX_WANTED)
                .collect();
            if g.wanted != list {
                g.wanted = list;
                self.shared.cv.notify_one();
            }
        }
        self.evict();

        // window title: file - page/total - zoom
        let title = if self.pages.is_empty() {
            "PDF Viewer - drop a PDF file here".to_string()
        } else {
            let mid = self.scroll_y + vh as f32 / 2.0;
            let cur = self.pages.partition_point(|p| (p.y + p.h) * scale < mid).min(self.pages.len() - 1);
            let pct = (scale / self.base() * 100.0).round();
            format!("{} - {}/{} - {}%", self.name, cur + 1, self.pages.len(), pct)
        };
        if title != self.last_title {
            window.set_title(&title);
            self.last_title = title;
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("PDF Viewer - drop a PDF file here")
            .with_inner_size(LogicalSize::new(900.0, 1000.0))
            .with_window_icon(load_window_icon());
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        let ctx = Context::new(window.clone()).expect("softbuffer context");
        let surface = Surface::new(&ctx, window.clone()).expect("softbuffer surface");
        let sz = window.inner_size();
        self.vw = sz.width;
        self.vh = sz.height;
        self.sf = window.scale_factor();
        self.window = Some(window);
        self._ctx = Some(ctx);
        self.surface = Some(surface);
        if let Some(p) = self.initial.take() {
            self.open(p);
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _e: Wake) {
        if self.drain() {
            self.request_redraw();
        }
    }

    fn new_events(&mut self, _el: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. }) && self.animating {
            self.request_redraw();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.fatal {
            self.shutdown();
            el.exit();
            return;
        }
        if self.animating {
            el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(7)));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => {
                self.shutdown();
                el.exit();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(sz) => {
                self.vw = sz.width;
                self.vh = sz.height;
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let r = (scale_factor / self.sf) as f32;
                self.sf = scale_factor;
                self.scale = q(self.scale * r);
                self.scroll_x *= r;
                self.scroll_y *= r;
                self.target_y *= r;
                self.request_redraw();
            }
            WindowEvent::DroppedFile(p) => self.open(p),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                if let Some((lx, ly)) = self.drag {
                    self.scroll_x -= x - lx;
                    self.scroll_y -= y - ly;
                    self.target_y = self.scroll_y;
                    self.drag = Some((x, y));
                    self.request_redraw();
                }
                self.cursor = (x, y);
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                self.drag = (state == ElementState::Pressed).then_some(self.cursor);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let line = 90.0 * self.sf as f32;
                let (dx, dy, steps) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * line, y * line, y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32, p.y as f32 / 60.0),
                };
                if self.mods.control_key() {
                    let (cx, cy) = self.cursor;
                    self.zoom_at(1.1f32.powf(steps), cx, cy);
                } else if self.mods.shift_key() {
                    self.scroll_x -= if dx != 0.0 { dx } else { dy };
                    self.request_redraw();
                } else {
                    self.scroll_x -= dx;
                    self.scroll_by(-dy);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.key(event.logical_key);
            }
            _ => {}
        }
    }
}

fn load_window_icon() -> Option<winit::window::Icon> {
    let bytes = include_bytes!("../icon.ico");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}


fn main() {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let el = EventLoop::<Wake>::with_user_event().build().expect("event loop");
    let proxy = el.create_proxy();
    let shared = Arc::new(Shared { m: Mutex::new(State::default()), cv: Condvar::new() });
    let (tx, rx) = channel();
    {
        let sh = shared.clone();
        std::thread::spawn(move || worker(sh, tx, proxy));
    }
    let mut app = App::new(shared, rx, path);
    el.run_app(&mut app).expect("run");
}
