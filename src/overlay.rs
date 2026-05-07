use anyhow::Result;
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState,
    VK_CONTROL, VK_LCONTROL, VK_RCONTROL,
    VK_MENU, VK_LMENU, VK_RMENU,
    VK_SHIFT, VK_LSHIFT, VK_RSHIFT,
    VK_LWIN, VK_RWIN,
    VK_ESCAPE, VK_F1, VK_F2, VK_F3, VK_F4, VK_F5, VK_F6,
    VK_F7, VK_F8, VK_F9, VK_F10, VK_F11, VK_F12,
    VK_SPACE, VK_RETURN, VK_TAB, VK_BACK,
    VK_DELETE, VK_INSERT, VK_HOME, VK_END, VK_PRIOR, VK_NEXT,
    VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use windows::Win32::Graphics::Imaging::{
    IWICFormatConverter, IWICImagingFactory,
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeMedianCut,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OPENFILENAMEW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST};
use crate::hotkey::notify_reregister;
use crate::log::clilog;
use crate::settings::{set_startup_registry, Settings};

// ── palette ───────────────────────────────────────────────────────────────────
const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}
const C_BG:       COLORREF = rgb(13,  13,  13 );
const C_CARD:     COLORREF = rgb(22,  22,  22 );
const C_ICON_BG:  COLORREF = rgb(42,  42,  42 );
const C_WHITE:    COLORREF = rgb(172, 172, 172);
const C_GREY:     COLORREF = rgb(88,  88,  88 );
const C_GREY_LT:  COLORREF = rgb(140, 140, 140);
const C_TRACK:    COLORREF = rgb(30,  30,  30 );
const C_CLOSE_BG: COLORREF = rgb(35,  35,  35 );
const C_TOG_OFF:  COLORREF = rgb(60,  60,  60 );
const C_TL_RED:   COLORREF = rgb(255, 95,  87 );
const C_TL_YLW:   COLORREF = rgb(254, 188, 46 );
const C_TL_GRN:   COLORREF = rgb(40,  200, 64 );

// ── monitor enumeration ───────────────────────────────────────────────────────
#[derive(Clone)]
struct MonitorInfo { w: u32, h: u32 }

fn enumerate_monitors() -> Vec<MonitorInfo> {
    unsafe {
        let Ok(factory): windows::core::Result<IDXGIFactory1> = CreateDXGIFactory1() else {
            return Vec::new();
        };
        let Ok(adapter) = factory.EnumAdapters(0) else { return Vec::new() };
        let mut v = Vec::new();
        let mut i = 0u32;
        loop {
            let Ok(output) = adapter.EnumOutputs(i) else { break };
            let Ok(desc) = output.GetDesc() else { i += 1; continue };
            let r = &desc.DesktopCoordinates;
            let w = (r.right  - r.left).max(0) as u32;
            let h = (r.bottom - r.top ).max(0) as u32;
            if w > 0 && h > 0 { v.push(MonitorInfo { w, h }); }
            i += 1;
        }
        v
    }
}

// ── slider descriptors ────────────────────────────────────────────────────────
struct Slider { label: &'static str, unit: &'static str, min: u32, max: u32 }
const SLIDERS: &[Slider] = &[
    Slider { label: "clip_length", unit: "s",    min: 10, max: 120 },
    Slider { label: "frame_rate",  unit: "fps",  min: 1,  max: 120 },
    Slider { label: "bitrate",     unit: "Mbps", min: 1,  max: 100 },
];
const COLOR_DRAG: usize = 99;

// ── DPI-aware layout ──────────────────────────────────────────────────────────
struct Lay {
    ow: i32, oh: i32,
    pad: i32, corner: i32,
    header_h: i32, tab_start: i32, tab_btn_w: i32, tab_btn_h: i32, tab_btn_y: i32,
    card_gap: i32, card_h: i32, dir_h: i32, tog_h: i32, color_h: i32, card_r: i32,
    content_x: i32,
    track_right: i32, track_h: i32, track_offset: i32, col_track_offset: i32,
    close_sz: i32, close_x: i32, close_y: i32,
    dpi: u32,
}
impl Lay {
    fn from_dpi(dpi: u32) -> Self {
        let f = move |n: i32| -> i32 { (n as f64 * dpi as f64 / 96.0).round() as i32 };
        let pad       = f(14);
        let content_x = pad + f(10);
        let ow        = f(340);
        let header_h  = f(48);
        let tab_btn_w = f(80);
        let tab_btn_h = f(26);
        let tab_btn_y = f(11);
        let card_gap  = f(5);
        let card_h    = f(56);
        let dir_h     = f(70); // taller card for output_dir
        let tog_h     = f(48);
        let color_h   = f(56);
        let tab_start = header_h + card_gap;
        // oh: 3 sliders + hotkey + output_dir(dir_h) + startup + footer
        let oh = tab_start
               + 3 * (card_h + card_gap)
               + (tog_h + card_gap)
               + (dir_h + card_gap)
               + (tog_h + card_gap)
               + f(52);
        // close_x/y/sz point to the red traffic-light dot (it IS the close button)
        let dot_d   = f(10); // dot diameter
        let close_x = pad;
        let close_y = (header_h - dot_d) / 2;
        let close_sz = dot_d;
        Lay {
            ow, oh, pad, corner: f(14),
            header_h, tab_start, tab_btn_w, tab_btn_h, tab_btn_y,
            card_gap, card_h, dir_h, tog_h, color_h, card_r: f(12),
            content_x,
            track_right: ow - pad - f(10),
            track_h: f(3), track_offset: f(38), col_track_offset: f(36),
            close_sz, close_x, close_y,
            dpi,
        }
    }
    fn s(&self, n: i32) -> i32 { (n as f64 * self.dpi as f64 / 96.0).round() as i32 }

    // Main tab: 0-2=sliders(card_h), 3=hotkey(tog_h), 4=output_dir(dir_h), 5=startup(tog_h)
    fn main_card_y(&self, i: usize) -> i32 {
        let s3 = self.tab_start + 3 * (self.card_h + self.card_gap);
        match i {
            0 => self.tab_start,
            1 => self.tab_start + self.card_h + self.card_gap,
            2 => self.tab_start + 2 * (self.card_h + self.card_gap),
            3 => s3,
            4 => s3 + self.tog_h + self.card_gap,
            5 => s3 + self.tog_h + self.card_gap + self.dir_h + self.card_gap,
            _ => 0,
        }
    }
    // Advanced tab: Display(card_h), Color(color_h), BG(tog_h)
    fn adv_card_y(&self, i: usize) -> i32 {
        match i {
            0 => self.tab_start,
            1 => self.tab_start + self.card_h + self.card_gap,
            2 => self.tab_start + self.card_h + self.card_gap + self.color_h + self.card_gap,
            _ => 0,
        }
    }
    fn footer_y(&self)          -> i32 { self.oh - self.s(52) }
    fn track_y(&self, i: usize) -> i32 { self.main_card_y(i) + self.track_offset }
    fn col_track_y(&self)       -> i32 { self.adv_card_y(1) + self.col_track_offset }
    fn tab_x(&self) -> [i32; 2] {
        // Centre the two tab buttons in the header with a comfortable gap
        let gap    = self.s(24);
        let center = self.ow / 2;
        [center - self.tab_btn_w - gap / 2, center + gap / 2]
    }

    fn thumb_x(&self, val: u32, sl: &Slider) -> i32 {
        let t = val.saturating_sub(sl.min) as f32 / (sl.max - sl.min) as f32;
        self.content_x + (t * (self.track_right - self.content_x) as f32) as i32
    }
    fn val_of(&self, x: i32, sl: &Slider) -> u32 {
        let t = ((x - self.content_x) as f32 / (self.track_right - self.content_x) as f32)
            .clamp(0.0, 1.0);
        (sl.min as f32 + t * (sl.max - sl.min) as f32).round() as u32
    }
    fn hue_thumb_x(&self, hue: f32) -> i32 {
        self.content_x + (hue / 360.0 * (self.track_right - self.content_x) as f32) as i32
    }
    fn hue_of(&self, x: i32) -> f32 {
        ((x - self.content_x) as f32 / (self.track_right - self.content_x) as f32)
            .clamp(0.0, 1.0) * 360.0
    }
}

// ── timers / messages ─────────────────────────────────────────────────────────
const TIMER_ANIM: usize = 2;
const TIMER_TOG:  usize = 3;
const TIMER_TAB:  usize = 4;
const WM_TOGGLE: u32 = WM_APP + 10;
const WM_HOTKEY_CAPTURED: u32 = WM_APP + 11;
const ANIM_STEPS: i32 = 6;

static LL_HOOK_HWND:   AtomicIsize = AtomicIsize::new(0);
static LL_HOOK_HANDLE: AtomicIsize = AtomicIsize::new(0);

// ── public handle ─────────────────────────────────────────────────────────────
#[derive(Clone)]
pub struct OverlayHandle { raw: isize }
unsafe impl Send for OverlayHandle {}
impl OverlayHandle {
    pub fn toggle(&self) {
        unsafe { let _ = PostMessageW(HWND(self.raw as *mut _), WM_TOGGLE, WPARAM(0), LPARAM(0)); }
    }
}

// ── GDI resource bundle ───────────────────────────────────────────────────────
struct Gdi {
    f_title: isize, f_label: isize, f_val: isize, f_hint: isize,
    br_bg: isize, br_card: isize, br_icon_bg: isize,
    br_white: isize, br_track: isize, br_tog_off: isize, br_close: isize,
    pen_null: isize, pen_white: isize,
    mem_dc: isize, mem_bm: isize,
    grad_dc: isize, grad_bm: isize,
}
impl Gdi {
    unsafe fn new(dpi: u32) -> Self {
        let pt = move |p: i32| p * dpi as i32 / 72;
        let pw = ((2.0 * dpi as f64 / 96.0).round() as i32).max(1);
        Gdi {
            f_title:    make_font(pt(18), FW_BOLD.0 as i32),
            f_label:    make_font(pt(10), FW_SEMIBOLD.0 as i32),
            f_val:      make_font(pt(10), FW_NORMAL.0 as i32),
            f_hint:     make_font(pt(8),  FW_NORMAL.0 as i32),
            br_bg:      CreateSolidBrush(C_BG).0 as isize,
            br_card:    CreateSolidBrush(C_CARD).0 as isize,
            br_icon_bg: CreateSolidBrush(C_ICON_BG).0 as isize,
            br_white:   CreateSolidBrush(C_WHITE).0 as isize,
            br_track:   CreateSolidBrush(C_TRACK).0 as isize,
            br_tog_off: CreateSolidBrush(C_TOG_OFF).0 as isize,
            br_close:   CreateSolidBrush(C_CLOSE_BG).0 as isize,
            pen_null:   CreatePen(PS_NULL,  0,  COLORREF(0)).0 as isize,
            pen_white:  CreatePen(PS_SOLID, pw, C_WHITE).0 as isize,
            mem_dc: 0, mem_bm: 0, grad_dc: 0, grad_bm: 0,
        }
    }
    unsafe fn init_backbuf(&mut self, hwnd: HWND, ow: i32, oh: i32) {
        let sdc = GetDC(hwnd);
        let mdc = CreateCompatibleDC(sdc);
        let mbm = CreateCompatibleBitmap(sdc, ow, oh);
        SelectObject(mdc, HGDIOBJ(mbm.0));
        ReleaseDC(hwnd, sdc);
        self.mem_dc = mdc.0 as isize;
        self.mem_bm = mbm.0 as isize;
    }
    unsafe fn init_gradient(&mut self, hwnd: HWND, tw: i32, th: i32) {
        let sdc = GetDC(hwnd);
        let gdc = CreateCompatibleDC(sdc);
        let gbm = CreateCompatibleBitmap(sdc, tw, th);
        SelectObject(gdc, HGDIOBJ(gbm.0));
        ReleaseDC(hwnd, sdc);
        for x in 0..tw {
            let h = 360.0 * x as f32 / tw as f32;
            let c = hsv_to_rgb(h, 0.70, 0.95);
            let br = CreateSolidBrush(c);
            let r = RECT { left: x, top: 0, right: x + 1, bottom: th };
            FillRect(gdc, &r, br);
            let _ = DeleteObject(HGDIOBJ(br.0));
        }
        self.grad_dc = gdc.0 as isize;
        self.grad_bm = gbm.0 as isize;
    }
    unsafe fn free(&self) {
        for h in [
            self.f_title, self.f_label, self.f_val, self.f_hint,
            self.br_bg, self.br_card, self.br_icon_bg,
            self.br_white, self.br_track, self.br_tog_off, self.br_close,
            self.pen_null, self.pen_white, self.mem_bm, self.grad_bm,
        ] {
            if h != 0 { let _ = DeleteObject(HGDIOBJ(h as *mut _)); }
        }
        if self.mem_dc  != 0 { let _ = DeleteDC(HDC(self.mem_dc  as *mut _)); }
        if self.grad_dc != 0 { let _ = DeleteDC(HDC(self.grad_dc as *mut _)); }
    }
    fn mem(&self)  -> HDC   { HDC(self.mem_dc  as *mut _) }
    fn grad(&self) -> HDC   { HDC(self.grad_dc as *mut _) }
    fn font(&self, f: isize)  -> HFONT  { HFONT(f as *mut _) }
    fn brush(&self, b: isize) -> HBRUSH { HBRUSH(b as *mut _) }
    fn pen(&self)  -> HPEN  { HPEN(self.pen_null  as *mut _) }
}

// ── per-window state ──────────────────────────────────────────────────────────
struct State {
    settings: Arc<Mutex<Settings>>,
    clip_secs: Arc<AtomicU32>, fps: Arc<AtomicU32>, bitrate_mbps: Arc<AtomicU32>,
    restart_tx: Sender<()>,
    save_tx: Sender<()>,
    monitor_index: Arc<AtomicU32>,
    monitors: Vec<MonitorInfo>,
    hotkey: Arc<AtomicU64>,
    hk_thread_id: Arc<AtomicU32>,
    capturing_hotkey: bool,
    bg_bitmap: Option<(HBITMAP, i32, i32)>,
    bg_path: String,
    val: [u32; 3], startup: bool, tog_anim: f32, hue: f32,
    active_tab: u8, tab_anim: f32,
    visible: bool, anim_t: i32, anim_dir: i32,
    drag: Option<usize>,
    sw: i32, sh: i32,
    lay: Lay, gdi: Gdi,
}
unsafe impl Send for State {}

// ── entry point ───────────────────────────────────────────────────────────────
pub fn start_overlay(
    settings: Arc<Mutex<Settings>>,
    clip_secs: Arc<AtomicU32>, fps: Arc<AtomicU32>, bitrate_mbps: Arc<AtomicU32>,
    restart_tx: Sender<()>,
    save_tx: Sender<()>,
    monitor_index: Arc<AtomicU32>,
    hotkey: Arc<AtomicU64>,
    hk_thread_id: Arc<AtomicU32>,
) -> Result<OverlayHandle> {
    let (tx, rx) = crossbeam_channel::bounded::<isize>(1);
    std::thread::Builder::new().name("overlay".into()).spawn(move || {
        if let Err(e) = run(settings, clip_secs, fps, bitrate_mbps, restart_tx, save_tx,
                            monitor_index, hotkey, hk_thread_id, tx) {
            clilog!("[overlay] {e:#}");
        }
    })?;
    let raw = rx.recv().map_err(|_| anyhow::anyhow!("overlay thread died on startup"))?;
    Ok(OverlayHandle { raw })
}

fn run(
    settings: Arc<Mutex<Settings>>,
    clip_secs: Arc<AtomicU32>, fps: Arc<AtomicU32>, bitrate_mbps: Arc<AtomicU32>,
    restart_tx: Sender<()>,
    save_tx: Sender<()>,
    monitor_index: Arc<AtomicU32>,
    hotkey: Arc<AtomicU64>,
    hk_thread_id: Arc<AtomicU32>,
    ready: crossbeam_channel::Sender<isize>,
) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dpi = GetDpiForSystem();
        let lay = Lay::from_dpi(dpi);

        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
        let cls = wv("GlimpseOverlay6");

        let class_bg = CreateSolidBrush(C_BG);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: class_bg,
            lpszClassName: pw(&cls),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let cur = settings.lock().clone();
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);

        let monitors = enumerate_monitors();

        let state = Box::new(State {
            settings, clip_secs, fps, bitrate_mbps, restart_tx, save_tx,
            monitor_index, monitors, hotkey, hk_thread_id,
            capturing_hotkey: false,
            bg_bitmap: None,
            bg_path: cur.bg_image_path.clone(),
            val: [cur.clip_secs, cur.fps, cur.bitrate_mbps],
            startup: cur.start_with_windows,
            tog_anim: if cur.start_with_windows { 1.0 } else { 0.0 },
            hue: cur.hue,
            active_tab: 0, tab_anim: 0.0,
            visible: false, anim_t: 0, anim_dir: 0,
            drag: None, sw, sh,
            gdi: Gdi::new(dpi), lay,
        });
        let sp = Box::into_raw(state);

        let (ow, oh, _corner) = ((*sp).lay.ow, (*sp).lay.oh, (*sp).lay.corner);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            pw(&cls), windows::core::PCWSTR::null(), WS_POPUP,
            sw, (sh - oh) / 2, ow, oh,
            None, None, hinstance, Some(sp as *const _),
        )?;

        let corner_pref = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd, DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner_pref as *const _ as *const _,
            std::mem::size_of_val(&corner_pref) as u32,
        );

        let backdrop = DWMSBT_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd, DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as *const _, std::mem::size_of_val(&backdrop) as u32,
        );
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 218, LWA_COLORKEY | LWA_ALPHA);

        ready.send(hwnd.0 as isize).ok();

        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 { break; }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        CoUninitialize();
        Ok(())
    }
}

// ── window procedure ──────────────────────────────────────────────────────────
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            let s  = sref(hwnd);
            let ow = s.lay.ow; let oh = s.lay.oh;
            let tw = s.lay.track_right - s.lay.content_x;
            let th = s.lay.track_h;
            s.gdi.init_backbuf(hwnd, ow, oh);
            s.gdi.init_gradient(hwnd, tw, th);
            let initial_path = s.bg_path.clone();
            if !initial_path.is_empty() {
                s.bg_bitmap = load_bg_bitmap_from_path(&initial_path);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
            if !p.is_null() {
                if let Some((hbm, _, _)) = (*p).bg_bitmap.take() {
                    let _ = DeleteObject(HGDIOBJ(hbm.0));
                }
                (*p).gdi.free();
                drop(Box::from_raw(p));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_NCHITTEST => {
            let s  = sref(hwnd);
            let mx = (lp.0 & 0xFFFF) as i16 as i32;
            let my = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = POINT { x: mx, y: my };
            let _ = ScreenToClient(hwnd, &mut pt);
            if in_rounded_rect(pt.x, pt.y, s.lay.ow, s.lay.oh, s.lay.corner) {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            }
        }
        WM_TOGGLE => {
            let s = sref(hwnd);
            if s.visible || s.anim_dir > 0 { do_close(hwnd, s); }
            else { do_open(hwnd, s); }
            LRESULT(0)
        }
        WM_PAINT => {
            let s  = sref(hwnd);
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            paint(dc, s);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_TIMER => {
            let s = sref(hwnd);
            if wp.0 == TIMER_ANIM {
                s.anim_t = (s.anim_t + s.anim_dir).clamp(0, ANIM_STEPS);
                let alpha = (218 * ease_i(s.anim_t, ANIM_STEPS) / ANIM_STEPS) as u8;
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_COLORKEY | LWA_ALPHA);
                let dc = GetDC(hwnd);
                paint(dc, s);
                ReleaseDC(hwnd, dc);
                if s.anim_dir > 0 && s.anim_t >= ANIM_STEPS {
                    let _ = KillTimer(hwnd, TIMER_ANIM);
                    s.visible = true; s.anim_dir = 0;
                } else if s.anim_dir < 0 && s.anim_t <= 0 {
                    let _ = KillTimer(hwnd, TIMER_ANIM);
                    s.visible = false; s.anim_dir = 0;
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            } else if wp.0 == TIMER_TOG {
                let target = if s.startup { 1.0f32 } else { 0.0f32 };
                let step   = 1.0 / 8.0;
                let diff   = target - s.tog_anim;
                if diff.abs() < step {
                    s.tog_anim = target;
                    let _ = KillTimer(hwnd, TIMER_TOG);
                } else {
                    s.tog_anim += diff.signum() * step;
                }
                let _ = InvalidateRect(hwnd, None, false);
            } else if wp.0 == TIMER_TAB {
                let target = s.active_tab as f32;
                let step   = 1.0 / 7.0;
                let diff   = target - s.tab_anim;
                if diff.abs() < step {
                    s.tab_anim = target;
                    let _ = KillTimer(hwnd, TIMER_TAB);
                } else {
                    s.tab_anim += diff.signum() * step;
                }
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let s = sref(hwnd);
            on_down(hwnd, s,
                (lp.0 & 0xFFFF) as i16 as i32,
                ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if wp.0 & 0x0001 != 0 {
                let s = sref(hwnd);
                on_drag(hwnd, s, (lp.0 & 0xFFFF) as i16 as i32);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let s = sref(hwnd);
            if s.drag.is_some() { s.drag = None; commit(s); }
            LRESULT(0)
        }
        WM_HOTKEY_CAPTURED => {
            let s = sref(hwnd);
            let packed = wp.0 as u64;
            if packed != 0 {
                s.hotkey.store(packed, Ordering::Relaxed);
                notify_reregister(&s.hk_thread_id);
            }
            cancel_capture(hwnd, s);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ── open / close / commit ─────────────────────────────────────────────────────
unsafe fn do_open(hwnd: HWND, s: &mut State) {
    let cur = s.settings.lock().clone();
    s.val      = [cur.clip_secs, cur.fps, cur.bitrate_mbps];
    s.startup  = cur.start_with_windows;
    s.tog_anim = if s.startup { 1.0 } else { 0.0 };
    s.tab_anim = s.active_tab as f32;
    s.hue      = cur.hue;
    // monitor_index and hotkey: don't reset — they reflect live state
    cancel_capture(hwnd, s);
    s.anim_dir = 1;
    if s.anim_t == 0 {
        let x = (s.sw - s.lay.ow) / 2;
        let y = (s.sh - s.lay.oh) / 2;
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y,
                             s.lay.ow, s.lay.oh, SWP_NOACTIVATE);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_COLORKEY | LWA_ALPHA);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    SetTimer(hwnd, TIMER_ANIM, 16, None);
}

unsafe fn do_close(hwnd: HWND, s: &mut State) {
    cancel_capture(hwnd, s);
    commit(s);
    s.anim_dir = -1;
    s.visible  = false;
    SetTimer(hwnd, TIMER_ANIM, 16, None);
}

fn commit(s: &mut State) {
    let old_fps = s.fps.load(Ordering::Relaxed);
    let old_bps = s.bitrate_mbps.load(Ordering::Relaxed);
    s.clip_secs.store(s.val[0], Ordering::Relaxed);
    s.fps.store(s.val[1], Ordering::Relaxed);
    s.bitrate_mbps.store(s.val[2], Ordering::Relaxed);
    let packed = s.hotkey.load(Ordering::Relaxed);
    let cfg = Settings {
        monitor_index: s.monitor_index.load(Ordering::Relaxed),
        clip_secs: s.val[0], fps: s.val[1], bitrate_mbps: s.val[2],
        start_with_windows: s.startup, hue: s.hue,
        hotkey_mods: (packed >> 32) as u32,
        hotkey_vk:   packed as u32,
        bg_image_path: s.bg_path.clone(),
    };
    set_startup_registry(cfg.start_with_windows);
    if let Err(e) = cfg.save() { clilog!("[overlay] save: {e}"); }
    *s.settings.lock() = cfg;
    if s.val[1] != old_fps || s.val[2] != old_bps {
        clilog!("[overlay] fps/bitrate changed — restarting encoder");
        let _ = s.restart_tx.try_send(());
    }
}

// ── painting ──────────────────────────────────────────────────────────────────
unsafe fn paint(dc: HDC, s: &State) {
    let g   = &s.gdi;
    let lay = &s.lay;
    let mem = g.mem();

    let accent    = hsv_to_rgb(s.hue, 0.70, 0.95);
    let br_accent = CreateSolidBrush(accent);

    // Fill corners with black (the LWA_COLORKEY transparency color) before clipping.
    // Any pixel left as pure black after painting becomes transparent in the compositor.
    let full = RECT { left: 0, top: 0, right: lay.ow, bottom: lay.oh };
    FillRect(mem, &full, HBRUSH(GetStockObject(BLACK_BRUSH).0));

    // Clip background fill and image to the panel's rounded shape
    let panel_rgn = CreateRoundRectRgn(0, 0, lay.ow, lay.oh, lay.corner * 2, lay.corner * 2);
    SelectClipRgn(mem, panel_rgn);

    frect(mem, 0, 0, lay.ow, lay.oh, g.brush(g.br_bg));

    if let Some((hbm, img_w, img_h)) = &s.bg_bitmap {
        let tmp = CreateCompatibleDC(mem);
        SelectObject(tmp, HGDIOBJ(hbm.0));
        let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 90, AlphaFormat: 0 };
        let _ = AlphaBlend(mem, 0, 0, lay.ow, lay.oh, tmp, 0, 0, *img_w, *img_h, blend);
        let _ = DeleteDC(tmp);
    }

    SelectClipRgn(mem, HRGN::default());
    let _ = DeleteObject(HGDIOBJ(panel_rgn.0));

    // ── header: traffic lights (red = close) + centred tabs ──────────────
    let br_tl_red = CreateSolidBrush(C_TL_RED);
    let br_tl_ylw = CreateSolidBrush(C_TL_YLW);
    let br_tl_grn = CreateSolidBrush(C_TL_GRN);

    // Dots — red dot is the close button (lay.close_x/y/sz match its position)
    let dot_r   = lay.s(5);
    let dot_gap = lay.s(6);
    let dot_y   = (lay.header_h - dot_r * 2) / 2;
    fell_aa(mem, lay.pad,                         dot_y, dot_r*2, dot_r*2, br_tl_red, g.pen(), C_BG);
    fell_aa(mem, lay.pad + dot_r*2 + dot_gap,     dot_y, dot_r*2, dot_r*2, br_tl_ylw, g.pen(), C_BG);
    fell_aa(mem, lay.pad + (dot_r*2 + dot_gap)*2, dot_y, dot_r*2, dot_r*2, br_tl_grn, g.pen(), C_BG);

    // Centred tab buttons
    let tab_xs     = lay.tab_x();
    let tab_labels = ["settings", "advanced"];
    for (i, (&tx, &label)) in tab_xs.iter().zip(tab_labels.iter()).enumerate() {
        let col = if s.active_tab == i as u8 { C_WHITE } else { C_GREY };
        dtext_rect(mem, tx, lay.tab_btn_y, tx + lay.tab_btn_w, lay.tab_btn_y + lay.tab_btn_h,
                   label, g.font(g.f_label), col, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    }
    // Single animated underline that slides between tabs
    let ul_x0 = tab_xs[0] + lay.s(6);
    let ul_x1 = tab_xs[1] + lay.s(6);
    let ul_x  = ul_x0 + (s.tab_anim * (ul_x1 - ul_x0) as f32) as i32;
    frect(mem, ul_x, lay.tab_btn_y + lay.tab_btn_h - lay.s(2),
          lay.tab_btn_w - lay.s(12), lay.s(2), br_accent);

    // Separator line below header
    frect(mem, 0, lay.header_h - 1, lay.ow, 1, g.brush(g.br_close));

    if s.active_tab == 0 {
        // ── Settings tab: sliders ───────────────────────────────────────────
        for (i, sl) in SLIDERS.iter().enumerate() {
            let cy = lay.main_card_y(i);
            let ty = lay.track_y(i);
            let tx = lay.thumb_x(s.val[i], sl);

            rrect_alpha(mem, lay.pad, cy, lay.ow - lay.pad*2, lay.card_h, lay.card_r, C_CARD, 200);

            dtext_rect(mem, lay.pad + lay.s(10), cy + lay.s(9), lay.ow / 2, cy + lay.s(9) + lay.s(20),
                       sl.label, g.font(g.f_label), C_WHITE, DT_LEFT | DT_TOP | DT_SINGLELINE);
            let vs = format!("{}{}", s.val[i], sl.unit);
            dtext_rect(mem, lay.content_x, cy + lay.s(11),
                       lay.ow - lay.pad - lay.s(10), cy + lay.s(29),
                       &vs, g.font(g.f_val), accent, DT_RIGHT | DT_SINGLELINE | DT_VCENTER);

            frect(mem, lay.content_x, ty, lay.track_right - lay.content_x, lay.track_h, g.brush(g.br_track));
            let fill_w = (tx - lay.content_x).max(0);
            let accent_dim = hsv_to_rgb(s.hue, 0.70, 0.60); // darker at the left end
            hfill_gradient(mem, lay.content_x, ty, fill_w, lay.track_h, accent_dim, accent);

            // Soft glow ring behind thumb
            let gr = lay.s(11);
            fell_glow(mem, tx - gr, ty - gr, gr*2, gr*2, accent, 48);
            // Thumb — small solid dot
            let tr = lay.s(5);
            fell_aa(mem, tx - tr, ty - tr, tr*2, tr*2, br_accent, g.pen(), C_CARD);
        }

        // ── Settings tab: hotkey card ───────────────────────────────────────
        let hkcy = lay.main_card_y(3);
        rrect_alpha(mem, lay.pad, hkcy, lay.ow - lay.pad*2, lay.tog_h, lay.card_r, C_CARD, 200);

        let hk_label_col = if s.capturing_hotkey { accent } else { C_WHITE };
        dtext_rect(mem, lay.pad + lay.s(10), hkcy + lay.s(9), lay.ow / 2, hkcy + lay.s(9) + lay.s(20),
                   "hotkey", g.font(g.f_label), hk_label_col, DT_LEFT | DT_TOP | DT_SINGLELINE);

        if s.capturing_hotkey {
            dtext_rect(mem, lay.content_x, hkcy, lay.track_right, hkcy + lay.tog_h,
                       "press a key\u{2026}", g.font(g.f_hint), C_GREY, DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
        } else {
            let parts  = hotkey_parts(s.hotkey.load(Ordering::Relaxed));
            let badge_h = lay.s(20);
            let badge_y = hkcy + (lay.tog_h - badge_h) / 2;
            let char_w  = lay.s(7);
            let h_pad   = lay.s(8);
            let sep_w   = lay.s(4);
            let widths: Vec<i32> = parts.iter()
                .map(|p| (p.len() as i32 * char_w).max(lay.s(20)) + h_pad * 2)
                .collect();
            let total_w: i32 = widths.iter().sum::<i32>()
                + sep_w * (widths.len() as i32 - 1).max(0);
            let mut bx = lay.ow - lay.pad - lay.s(10) - total_w;
            for (badge, &w) in parts.iter().zip(widths.iter()) {
                rrect(mem, bx, badge_y, w, badge_h, lay.s(3), g.brush(g.br_close), g.pen());
                dtext_rect(mem, bx, badge_y, bx + w, badge_y + badge_h,
                           badge, g.font(g.f_hint), C_WHITE, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
                bx += w + sep_w;
            }
        }

        // ── Settings tab: output_dir card ──────────────────────────────────
        let dcy = lay.main_card_y(4);
        rrect_alpha(mem, lay.pad, dcy, lay.ow - lay.pad*2, lay.dir_h, lay.card_r, C_CARD, 200);
        dtext_rect(mem, lay.pad + lay.s(10), dcy + lay.s(9), lay.ow / 2, dcy + lay.s(9) + lay.s(20),
                   "output_dir", g.font(g.f_label), C_WHITE, DT_LEFT | DT_TOP | DT_SINGLELINE);
        let field_x = lay.pad + lay.s(10);
        let field_y = dcy + lay.s(38);
        let field_w = lay.ow - lay.pad * 2 - lay.s(20);
        let field_h = lay.s(22);
        rrect(mem, field_x, field_y, field_w, field_h, lay.s(3), g.brush(g.br_track), g.pen());
        let dir_path = {
            let vids = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\user".to_string());
            format!("{}\\Videos\\Glimpse", vids)
        };
        dtext_rect(mem, field_x + lay.s(8), field_y, field_x + field_w - lay.s(8), field_y + field_h,
                   &dir_path, g.font(g.f_hint), C_GREY_LT,
                   DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);

        // ── Settings tab: start_with_windows toggle ─────────────────────────
        let scy = lay.main_card_y(5);
        rrect_alpha(mem, lay.pad, scy, lay.ow - lay.pad*2, lay.tog_h, lay.card_r, C_CARD, 200);
        dtext_rect(mem, lay.pad + lay.s(10), scy + lay.s(9),
                   lay.ow - lay.s(70), scy + lay.s(9) + lay.s(20),
                   "start_with_windows", g.font(g.f_label), C_WHITE, DT_LEFT | DT_TOP | DT_SINGLELINE);
        let tog_x = lay.ow - lay.pad - lay.s(10) - lay.s(52);
        let tog_y = scy + (lay.tog_h - lay.s(30)) / 2;
        draw_toggle(mem, g, lay, tog_x, tog_y, s.tog_anim, accent, br_accent);

    } else {
        // ── Advanced tab: display card ──────────────────────────────────────
        let mcy = lay.adv_card_y(0);
        rrect_alpha(mem, lay.pad, mcy, lay.ow - lay.pad*2, lay.card_h, lay.card_r, C_CARD, 200);
        dtext_rect(mem, lay.pad + lay.s(10), mcy + lay.s(9), lay.ow / 2, mcy + lay.s(9) + lay.s(20),
                   "display", g.font(g.f_label), C_WHITE, DT_LEFT | DT_TOP | DT_SINGLELINE);
        let mon_idx = s.monitor_index.load(Ordering::Relaxed) as usize;
        let (mw, mh) = s.monitors.get(mon_idx).map(|m| (m.w, m.h)).unwrap_or((0, 0));
        dtext_rect(mem, lay.content_x, mcy + lay.s(11), lay.track_right, mcy + lay.s(29),
                   &format!("{mw} \u{00D7} {mh}"), g.font(g.f_val), accent,
                   DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
        let nty = mcy + lay.track_offset;
        if s.monitors.len() > 1 {
            let nav = format!("\u{25C0}  {} of {}  \u{25B6}", mon_idx + 1, s.monitors.len());
            dtext_rect(mem, lay.content_x, nty - lay.s(8), lay.track_right, nty + lay.s(8),
                       &nav, g.font(g.f_hint), C_GREY, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
        } else {
            dtext_rect(mem, lay.content_x, nty - lay.s(8), lay.track_right, nty + lay.s(8),
                       "only display", g.font(g.f_hint), C_GREY, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
        }

        // ── Advanced tab: accent color ──────────────────────────────────────
        let ccy = lay.adv_card_y(1);
        rrect_alpha(mem, lay.pad, ccy, lay.ow - lay.pad*2, lay.color_h, lay.card_r, C_CARD, 200);
        dtext_rect(mem, lay.pad + lay.s(10), ccy + lay.s(9), lay.ow / 2, ccy + lay.s(9) + lay.s(20),
                   "accent_color", g.font(g.f_label), C_WHITE, DT_LEFT | DT_TOP | DT_SINGLELINE);
        let cty = lay.col_track_y();
        let ctw = lay.track_right - lay.content_x;
        let _ = BitBlt(mem, lay.content_x, cty, ctw, lay.track_h, g.grad(), 0, 0, SRCCOPY);
        let ctx = lay.hue_thumb_x(s.hue);
        let cr  = lay.s(6);
        fell_aa(mem, ctx - cr, cty - cr, cr*2, cr*2, br_accent, g.pen(), C_CARD);
        fell_aa(mem, ctx - lay.s(3), cty - lay.s(3), lay.s(5), lay.s(5),
                g.brush(g.br_white), g.pen(), accent);

        // ── Advanced tab: background image ──────────────────────────────────
        let icy = lay.adv_card_y(2);
        rrect_alpha(mem, lay.pad, icy, lay.ow - lay.pad*2, lay.tog_h, lay.card_r, C_CARD, 200);
        dtext_rect(mem, lay.pad + lay.s(10), icy + lay.s(9), lay.ow / 2, icy + lay.s(9) + lay.s(20),
                   "background", g.font(g.f_label), C_WHITE, DT_LEFT | DT_TOP | DT_SINGLELINE);
        if s.bg_bitmap.is_some() {
            let fname = std::path::Path::new(&s.bg_path)
                .file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let text_right = lay.ow - lay.pad - lay.close_sz - lay.s(6);
            dtext_rect(mem, lay.content_x, icy, text_right, icy + lay.tog_h,
                       &fname, g.font(g.f_hint), accent,
                       DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
            let cx      = lay.ow - lay.pad - lay.close_sz;
            let cy_btn  = icy + (lay.tog_h - lay.close_sz) / 2;
            rrect(mem, cx, cy_btn, lay.close_sz, lay.close_sz, lay.s(4), g.brush(g.br_close), g.pen());
            dtext_rect(mem, cx, cy_btn, cx + lay.close_sz, cy_btn + lay.close_sz,
                       "\u{00D7}", g.font(g.f_hint), C_GREY, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
        } else {
            dtext_rect(mem, lay.content_x, icy, lay.track_right, icy + lay.tog_h,
                       "click to browse\u{2026}", g.font(g.f_hint), C_GREY,
                       DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
        }
    }

    // ── footer ─────────────────────────────────────────────────────────────
    let fy = lay.footer_y();
    frect(mem, lay.pad, fy, lay.ow - lay.pad*2, 1, g.brush(g.br_close));

    // Status dot + "running"
    let dot_fy = fy + (lay.oh - fy - lay.s(10)) / 2;
    fell_aa(mem, lay.pad + lay.s(2), dot_fy, lay.s(10), lay.s(10), br_tl_grn, g.pen(), C_BG);
    dtext(mem, lay.pad + lay.s(16), fy + (lay.oh - fy - lay.s(18)) / 2,
          "running", g.font(g.f_hint), C_GREY, DT_LEFT);

    // Save clip button
    let btn_w = lay.s(118);
    let btn_h = lay.s(32);
    let btn_x = lay.ow - lay.pad - btn_w;
    let btn_y = fy + (lay.oh - fy - btn_h) / 2;
    rrect(mem, btn_x, btn_y, btn_w, btn_h, lay.s(5), br_accent, g.pen());
    dtext_rect(mem, btn_x, btn_y, btn_x + btn_w, btn_y + btn_h,
               "> save clip", g.font(g.f_label), C_BG,
               DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOCLIP);


    let _ = BitBlt(dc, 0, 0, lay.ow, lay.oh, mem, 0, 0, SRCCOPY);
    let _ = DeleteObject(HGDIOBJ(br_accent.0));
    let _ = DeleteObject(HGDIOBJ(br_tl_red.0));
    let _ = DeleteObject(HGDIOBJ(br_tl_ylw.0));
    let _ = DeleteObject(HGDIOBJ(br_tl_grn.0));
}


unsafe fn draw_toggle(dc: HDC, g: &Gdi, lay: &Lay, x: i32, y: i32, anim: f32,
                      _accent: COLORREF, br_accent: HBRUSH) {
    let tw = lay.s(52); let th = lay.s(30);
    // AA pill track
    let bg_br = if anim > 0.5 { br_accent } else { g.brush(g.br_tog_off) };
    rrect_aa(dc, x, y, tw, th, th / 2, bg_br);
    // AA sliding knob — dark, slightly smaller than the pill height
    let kd     = th - lay.s(10);
    let margin = (th - kd) / 2;
    let off_kx = x + margin;
    let on_kx  = x + tw - margin - kd;
    let kx     = off_kx + (anim * (on_kx - off_kx) as f32) as i32;
    let knob_br = CreateSolidBrush(C_BG);
    fell_aa(dc, kx, y + margin, kd, kd, knob_br, g.pen(), C_TOG_OFF);
    let _ = DeleteObject(HGDIOBJ(knob_br.0));
}

// ── GDI helpers ───────────────────────────────────────────────────────────────
unsafe fn frect(dc: HDC, x: i32, y: i32, w: i32, h: i32, br: HBRUSH) {
    let r = RECT { left: x, top: y, right: x+w, bottom: y+h };
    FillRect(dc, &r, br);
}

// Horizontal gradient fill using GDI GradientFill.
unsafe fn hfill_gradient(dc: HDC, x: i32, y: i32, w: i32, h: i32,
                          col_l: COLORREF, col_r: COLORREF) {
    if w <= 0 || h <= 0 { return; }
    let ch = |c: COLORREF, sh: u32| -> u16 { (((c.0 >> sh) & 0xFF) as u16) << 8 };
    let verts = [
        TRIVERTEX { x,     y,     Red: ch(col_l,0), Green: ch(col_l,8), Blue: ch(col_l,16), Alpha: 0 },
        TRIVERTEX { x:x+w, y:y+h, Red: ch(col_r,0), Green: ch(col_r,8), Blue: ch(col_r,16), Alpha: 0 },
    ];
    let mesh = GRADIENT_RECT { UpperLeft: 0, LowerRight: 1 };
    let _ = GradientFill(dc, &verts,
                         &mesh as *const GRADIENT_RECT as *const _,
                         1, GRADIENT_FILL_RECT_H);
}

// 4× supersampled filled rounded rectangle.
// Seeds from destination so AA edges blend correctly with whatever is under it.
unsafe fn rrect_aa(dc: HDC, x: i32, y: i32, w: i32, h: i32, r: i32, br: HBRUSH) {
    if w <= 0 || h <= 0 { return; }
    let scale = 4i32;
    let tmp   = CreateCompatibleDC(dc);
    let bmp   = CreateCompatibleBitmap(dc, w * scale, h * scale);
    let ob    = SelectObject(tmp, HGDIOBJ(bmp.0));
    SetStretchBltMode(tmp, HALFTONE);
    let _ = SetBrushOrgEx(tmp, 0, 0, None);
    let _ = StretchBlt(tmp, 0, 0, w * scale, h * scale, dc, x, y, w, h, SRCCOPY);
    let ob2 = SelectObject(tmp, HGDIOBJ(br.0));
    let op2 = SelectObject(tmp, GetStockObject(NULL_PEN));
    let _ = RoundRect(tmp, 0, 0, w * scale, h * scale, r * scale * 2, r * scale * 2);
    SelectObject(tmp, ob2); SelectObject(tmp, op2);
    SetStretchBltMode(dc, HALFTONE);
    let _ = SetBrushOrgEx(dc, 0, 0, None);
    let _ = StretchBlt(dc, x, y, w, h, tmp, 0, 0, w * scale, h * scale, SRCCOPY);
    SelectObject(tmp, ob);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(tmp);
}

// 4× supersampled accent border drawn over the full panel.
// Seeds from the already-painted mem buffer so non-border pixels are preserved.

// Soft glow ring — 8× supersampled ellipse blended at low opacity.
// Seeds from destination so only the ellipse pixels pick up colour; everything
// outside is completely unaffected by the AlphaBlend.
unsafe fn fell_glow(dc: HDC, x: i32, y: i32, w: i32, h: i32, col: COLORREF, alpha: u8) {
    if w <= 0 || h <= 0 { return; }
    let scale = 8i32;
    // ── render AA ellipse at 8× ──────────────────────────────────────────
    let tmp = CreateCompatibleDC(dc);
    let bmp = CreateCompatibleBitmap(dc, w * scale, h * scale);
    let ob  = SelectObject(tmp, HGDIOBJ(bmp.0));
    SetStretchBltMode(tmp, HALFTONE);
    let _ = SetBrushOrgEx(tmp, 0, 0, None);
    let _ = StretchBlt(tmp, 0, 0, w*scale, h*scale, dc, x, y, w, h, SRCCOPY);
    let br  = CreateSolidBrush(col);
    let ob2 = SelectObject(tmp, HGDIOBJ(br.0));
    let op2 = SelectObject(tmp, GetStockObject(NULL_PEN));
    let _ = Ellipse(tmp, 0, 0, w*scale, h*scale);
    SelectObject(tmp, ob2); SelectObject(tmp, op2);
    let _ = DeleteObject(HGDIOBJ(br.0));
    // ── downscale to 1× intermediate ────────────────────────────────────
    let mid    = CreateCompatibleDC(dc);
    let mid_bm = CreateCompatibleBitmap(dc, w, h);
    let mob    = SelectObject(mid, HGDIOBJ(mid_bm.0));
    SetStretchBltMode(mid, HALFTONE);
    let _ = SetBrushOrgEx(mid, 0, 0, None);
    let _ = StretchBlt(mid, 0, 0, w, h, tmp, 0, 0, w*scale, h*scale, SRCCOPY);
    // ── AlphaBlend onto destination ──────────────────────────────────────
    // Non-ellipse pixels in mid == destination pixels, so they cancel out:
    // result = src*a + dst*(1-a) = dst*a + dst*(1-a) = dst  (no change outside)
    let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: 0 };
    let _ = AlphaBlend(dc, x, y, w, h, mid, 0, 0, w, h, blend);
    SelectObject(mid, mob);
    let _ = DeleteObject(HGDIOBJ(mid_bm.0));
    let _ = DeleteDC(mid);
    SelectObject(tmp, ob);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(tmp);
}

// 8× supersampled ellipse — seeds from destination so AA blends correctly.
unsafe fn fell_aa(dc: HDC, x: i32, y: i32, w: i32, h: i32,
                  br: HBRUSH, pn: HPEN, _bg: COLORREF) {
    let scale = 8i32;
    let tmp_dc = CreateCompatibleDC(dc);
    let tmp_bm = CreateCompatibleBitmap(dc, w * scale, h * scale);
    SelectObject(tmp_dc, HGDIOBJ(tmp_bm.0));
    // Seed with actual destination content so AA blends correctly.
    SetStretchBltMode(tmp_dc, HALFTONE);
    let _ = SetBrushOrgEx(tmp_dc, 0, 0, None);
    let _ = StretchBlt(tmp_dc, 0, 0, w * scale, h * scale, dc, x, y, w, h, SRCCOPY);
    let ob = SelectObject(tmp_dc, HGDIOBJ(br.0));
    let op = SelectObject(tmp_dc, HGDIOBJ(pn.0));
    let _ = Ellipse(tmp_dc, 0, 0, w * scale, h * scale);
    SelectObject(tmp_dc, ob);
    SelectObject(tmp_dc, op);
    SetStretchBltMode(dc, HALFTONE);
    let _ = SetBrushOrgEx(dc, 0, 0, None);
    let _ = StretchBlt(dc, x, y, w, h, tmp_dc, 0, 0, w * scale, h * scale, SRCCOPY);
    let _ = DeleteObject(HGDIOBJ(tmp_bm.0));
    let _ = DeleteDC(tmp_dc);
}

unsafe fn rrect(dc: HDC, x: i32, y: i32, w: i32, h: i32, r: i32, br: HBRUSH, pn: HPEN) {
    let ob = SelectObject(dc, HGDIOBJ(br.0));
    let op = SelectObject(dc, HGDIOBJ(pn.0));
    let _ = RoundRect(dc, x, y, x+w, y+h, r*2, r*2);
    SelectObject(dc, ob); SelectObject(dc, op);
}

unsafe fn rrect_alpha(dc: HDC, x: i32, y: i32, w: i32, h: i32, r: i32, color: COLORREF, alpha: u8) {
    if w <= 0 || h <= 0 { return; }
    let tmp = CreateCompatibleDC(dc);
    let bmp = CreateCompatibleBitmap(dc, w, h);
    let ob = SelectObject(tmp, HGDIOBJ(bmp.0));
    let br = CreateSolidBrush(color);
    let rct = RECT { left: 0, top: 0, right: w, bottom: h };
    FillRect(tmp, &rct, br);
    let _ = DeleteObject(HGDIOBJ(br.0));
    let rgn = CreateRoundRectRgn(x, y, x + w, y + h, r * 2, r * 2);
    SelectClipRgn(dc, rgn);
    let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: 0 };
    let _ = AlphaBlend(dc, x, y, w, h, tmp, 0, 0, w, h, blend);
    SelectClipRgn(dc, HRGN::default());
    let _ = DeleteObject(HGDIOBJ(rgn.0));
    SelectObject(tmp, ob);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(tmp);
}
unsafe fn dtext(dc: HDC, x: i32, y: i32, text: &str,
                font: HFONT, col: COLORREF, flags: DRAW_TEXT_FORMAT) {
    let old = SelectObject(dc, HGDIOBJ(font.0));
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, col);
    let mut txt: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT { left: x, top: y, right: 4096, bottom: y + 32 };
    DrawTextW(dc, &mut txt, &mut r, flags | DT_SINGLELINE | DT_VCENTER | DT_NOCLIP);
    SelectObject(dc, old);
}
unsafe fn dtext_rect(dc: HDC, left: i32, top: i32, right: i32, bottom: i32,
                     text: &str, font: HFONT, col: COLORREF, flags: DRAW_TEXT_FORMAT) {
    let old = SelectObject(dc, HGDIOBJ(font.0));
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, col);
    let mut txt: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT { left, top, right, bottom };
    DrawTextW(dc, &mut txt, &mut r, flags);
    SelectObject(dc, old);
}

// ── mouse ─────────────────────────────────────────────────────────────────────
unsafe fn on_down(hwnd: HWND, s: &mut State, mx: i32, my: i32) {
    let lay = &s.lay;

    // Close button (red traffic-light dot)
    if mx >= lay.close_x && mx <= lay.close_x + lay.close_sz
    && my >= lay.close_y && my <= lay.close_y + lay.close_sz {
        do_close(hwnd, s); return;
    }

    // Save clip button (footer)
    let fy    = lay.footer_y();
    let btn_w = lay.s(118);
    let btn_h = lay.s(32);
    let btn_x = lay.ow - lay.pad - btn_w;
    let btn_y = fy + (lay.oh - fy - btn_h) / 2;
    if mx >= btn_x && mx <= btn_x + btn_w && my >= btn_y && my <= btn_y + btn_h {
        let _ = s.save_tx.try_send(());
        return;
    }

    // Tab buttons
    if my >= lay.tab_btn_y && my <= lay.tab_btn_y + lay.tab_btn_h {
        let tab_x = lay.tab_x();
        for (i, &tx) in tab_x.iter().enumerate() {
            if mx >= tx && mx <= tx + lay.tab_btn_w {
                let new_tab = i as u8;
                if s.active_tab != new_tab {
                    s.active_tab = new_tab;
                    cancel_capture(hwnd, s);
                    SetTimer(hwnd, TIMER_TAB, 16, None);
                    let _ = InvalidateRect(hwnd, None, false);
                }
                return;
            }
        }
    }

    if s.active_tab == 0 {
        // ── Settings tab ───────────────────────────────────────────────────

        // Slider tracks
        for i in 0..SLIDERS.len() {
            let ty = lay.track_y(i);
            let tx = lay.thumb_x(s.val[i], &SLIDERS[i]);
            let hit = (mx - tx).abs() <= lay.s(12) && (my - ty).abs() <= lay.s(12)
                   || mx >= lay.content_x && mx <= lay.track_right && (my - ty).abs() <= lay.s(12);
            if hit {
                s.drag = Some(i);
                s.val[i] = lay.val_of(mx, &SLIDERS[i]);
                let _ = InvalidateRect(hwnd, None, false);
                return;
            }
        }

        // Hotkey card
        let hcy = lay.main_card_y(3);
        if mx >= lay.pad && mx <= lay.ow - lay.pad
        && my >= hcy && my <= hcy + lay.tog_h {
            if s.capturing_hotkey { cancel_capture(hwnd, s); }
            else { start_capture(hwnd, s); }
            return;
        }

        // Startup toggle (main_card_y 5)
        let scy   = lay.main_card_y(5);
        let tog_x = lay.ow - lay.pad - lay.s(10) - lay.s(52);
        let tog_y = scy + (lay.tog_h - lay.s(30)) / 2;
        if mx >= tog_x && mx <= tog_x + lay.s(52) && my >= tog_y && my <= tog_y + lay.s(30) {
            s.startup = !s.startup;
            SetTimer(hwnd, TIMER_TOG, 16, None);
            return;
        }

    } else {
        // ── Advanced tab ───────────────────────────────────────────────────

        // Monitor card
        if s.monitors.len() > 1 {
            let mcy = lay.adv_card_y(0);
            if mx >= lay.pad && mx <= lay.ow - lay.pad
            && my >= mcy && my <= mcy + lay.card_h {
                let n   = s.monitors.len() as u32;
                let cur = s.monitor_index.load(Ordering::Relaxed);
                let mid = (lay.content_x + lay.track_right) / 2;
                let new_idx = if mx < mid {
                    if cur == 0 { n - 1 } else { cur - 1 }
                } else {
                    if cur + 1 >= n { 0 } else { cur + 1 }
                };
                s.monitor_index.store(new_idx, Ordering::Relaxed);
                clilog!("[overlay] switched to display {}", new_idx);
                let _ = InvalidateRect(hwnd, None, false);
                return;
            }
        }

        // Image card (adv_card_y 2)
        let icy = lay.adv_card_y(2);
        if mx >= lay.pad && mx <= lay.ow - lay.pad
        && my >= icy && my <= icy + lay.tog_h {
            let clear_x = lay.ow - lay.pad - lay.close_sz;
            if s.bg_bitmap.is_some() && mx >= clear_x {
                reload_bg_bitmap(hwnd, s, "");
            } else if let Some(path) = browse_image(hwnd) {
                reload_bg_bitmap(hwnd, s, &path);
            }
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }

        // Color slider (adv_card_y 1)
        let cty = lay.col_track_y();
        let ctx = lay.hue_thumb_x(s.hue);
        let hit = (mx - ctx).abs() <= lay.s(12) && (my - cty).abs() <= lay.s(12)
               || mx >= lay.content_x && mx <= lay.track_right && (my - cty).abs() <= lay.s(12);
        if hit {
            s.drag = Some(COLOR_DRAG);
            s.hue  = lay.hue_of(mx);
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

unsafe fn on_drag(hwnd: HWND, s: &mut State, mx: i32) {
    if let Some(i) = s.drag {
        if i < SLIDERS.len() {
            s.val[i] = s.lay.val_of(mx, &SLIDERS[i]);
            if i == 0 { s.clip_secs.store(s.val[0], Ordering::Relaxed); }
        } else {
            s.hue = s.lay.hue_of(mx);
        }
        let _ = InvalidateRect(hwnd, None, false);
    }
}

// ── animation math ────────────────────────────────────────────────────────────
fn ease_i(t: i32, steps: i32) -> i32 {
    let u = 1.0 - t as f32 / steps as f32;
    ((1.0 - u * u) * steps as f32) as i32
}

// ── GDI font creation ─────────────────────────────────────────────────────────
unsafe fn make_font(px: i32, weight: i32) -> isize {
    let name: Vec<u16> = "Consolas".encode_utf16().chain(Some(0)).collect();
    let mut lf = LOGFONTW::default();
    lf.lfHeight = -px;
    lf.lfWeight = weight;
    lf.lfQuality = CLEARTYPE_QUALITY;
    let n = name.len().min(lf.lfFaceName.len());
    lf.lfFaceName[..n].copy_from_slice(&name[..n]);
    CreateFontIndirectW(&lf).0 as isize
}

// ── state accessor ────────────────────────────────────────────────────────────
unsafe fn sref(hwnd: HWND) -> &'static mut State {
    &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State)
}

// ── wide-string helpers ───────────────────────────────────────────────────────
fn wv(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
fn pw(v: &[u16]) -> windows::core::PCWSTR { windows::core::PCWSTR(v.as_ptr()) }

// ── hit-test for rounded rectangle ───────────────────────────────────────────
fn in_rounded_rect(x: i32, y: i32, w: i32, h: i32, r: i32) -> bool {
    if x < 0 || y < 0 || x >= w || y >= h { return false; }
    let r2 = (r * r) as f32;
    if x < r && y < r {
        let dx = (r - x) as f32; let dy = (r - y) as f32;
        return dx*dx + dy*dy <= r2;
    }
    if x >= w - r && y < r {
        let dx = (x - (w - r - 1)) as f32; let dy = (r - y) as f32;
        return dx*dx + dy*dy <= r2;
    }
    if x < r && y >= h - r {
        let dx = (r - x) as f32; let dy = (y - (h - r - 1)) as f32;
        return dx*dx + dy*dy <= r2;
    }
    if x >= w - r && y >= h - r {
        let dx = (x - (w - r - 1)) as f32; let dy = (y - (h - r - 1)) as f32;
        return dx*dx + dy*dy <= r2;
    }
    true
}

// ── background image ──────────────────────────────────────────────────────────
fn load_wic_bgra(path: &str) -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;

        let path_w: Vec<u16> = path.encode_utf16().chain(Some(0u16)).collect();
        let decoder = factory.CreateDecoderFromFilename(
            windows::core::PCWSTR(path_w.as_ptr()),
            Some(std::ptr::null()),
            GENERIC_ACCESS_RIGHTS(0x80000000u32),
            WICDecodeMetadataCacheOnDemand,
        ).ok()?;

        let frame = decoder.GetFrame(0).ok()?;
        let converter: IWICFormatConverter = factory.CreateFormatConverter().ok()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        ).ok()?;

        let (mut w, mut h) = (0u32, 0u32);
        converter.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 { return None; }

        let stride = w * 4;
        let mut pixels = vec![0u8; (stride * h) as usize];
        converter.CopyPixels(std::ptr::null(), stride, &mut pixels).ok()?;

        Some((pixels, w, h))
    }
}

unsafe fn load_bg_bitmap_from_path(path: &str) -> Option<(HBITMAP, i32, i32)> {
    let (pixels, w, h) = load_wic_bgra(path)?;

    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader.biSize        = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth       = w as i32;
    info.bmiHeader.biHeight      = -(h as i32); // top-down
    info.bmiHeader.biPlanes      = 1;
    info.bmiHeader.biBitCount    = 32;
    info.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let hbm = CreateDIBSection(
        HDC::default(), &info, DIB_RGB_COLORS,
        &mut bits, HANDLE::default(), 0,
    ).ok()?;

    if !bits.is_null() {
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
    }

    Some((hbm, w as i32, h as i32))
}

unsafe fn reload_bg_bitmap(hwnd: HWND, s: &mut State, path: &str) {
    if let Some((hbm, _, _)) = s.bg_bitmap.take() {
        let _ = DeleteObject(HGDIOBJ(hbm.0));
    }
    s.bg_path = path.to_string();
    if !path.is_empty() {
        s.bg_bitmap = load_bg_bitmap_from_path(path);
        if s.bg_bitmap.is_none() {
            clilog!("[overlay] failed to load image: {path}");
        }
    }
    let _ = InvalidateRect(hwnd, None, false);
}

unsafe fn browse_image(hwnd: HWND) -> Option<String> {
    let filter_w: Vec<u16> =
        "Images\0*.png;*.jpg;*.jpeg;*.bmp\0All Files\0*.*\0\0"
        .encode_utf16().chain(Some(0u16)).collect();
    let title_w: Vec<u16> =
        "Select Background Image\0".encode_utf16().chain(Some(0u16)).collect();

    let mut buf = [0u16; 1024];
    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize  = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner    = hwnd;
    ofn.lpstrFilter  = windows::core::PCWSTR(filter_w.as_ptr());
    ofn.lpstrFile    = windows::core::PWSTR(buf.as_mut_ptr());
    ofn.nMaxFile     = buf.len() as u32;
    ofn.lpstrTitle   = windows::core::PCWSTR(title_w.as_ptr());
    ofn.Flags        = OFN_PATHMUSTEXIST | OFN_FILEMUSTEXIST;

    if GetOpenFileNameW(&mut ofn).as_bool() {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    } else {
        None
    }
}


// ── hotkey capture ────────────────────────────────────────────────────────────
unsafe fn start_capture(hwnd: HWND, s: &mut State) {
    LL_HOOK_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    let hmod: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
    match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_key_hook), hmod, 0) {
        Ok(hook) => {
            LL_HOOK_HANDLE.store(hook.0 as isize, Ordering::Relaxed);
            s.capturing_hotkey = true;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Err(e) => clilog!("[overlay] hook install failed: {e}"),
    }
}

unsafe fn cancel_capture(hwnd: HWND, s: &mut State) {
    let h = LL_HOOK_HANDLE.swap(0, Ordering::Relaxed);
    if h != 0 { let _ = UnhookWindowsHookEx(HHOOK(h as *mut _)); }
    LL_HOOK_HWND.store(0, Ordering::Relaxed);
    if s.capturing_hotkey {
        s.capturing_hotkey = false;
        let _ = InvalidateRect(hwnd, None, false);
    }
}

unsafe extern "system" fn ll_key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let event = wp.0 as u32;
        if event == WM_KEYDOWN || event == WM_SYSKEYDOWN {
            let kb = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            let vk = kb.vkCode;
            if !is_modifier_vk(vk) {
                let hwnd_raw = LL_HOOK_HWND.load(Ordering::Relaxed);
                if hwnd_raw != 0 {
                    let hwnd = HWND(hwnd_raw as *mut _);
                    if vk == VK_ESCAPE.0 as u32 {
                        let _ = PostMessageW(hwnd, WM_HOTKEY_CAPTURED, WPARAM(0), LPARAM(0));
                    } else {
                        let mods = current_modifiers();
                        let packed = ((mods as u64) << 32) | vk as u64;
                        let _ = PostMessageW(hwnd, WM_HOTKEY_CAPTURED, WPARAM(packed as usize), LPARAM(0));
                    }
                }
                return LRESULT(1); // consume the key
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

unsafe fn current_modifiers() -> u32 {
    let mut m = 0u32;
    if (GetKeyState(VK_CONTROL.0 as i32) as i16) < 0 { m |= 0x0002; }
    if (GetKeyState(VK_MENU.0    as i32) as i16) < 0 { m |= 0x0001; }
    if (GetKeyState(VK_SHIFT.0   as i32) as i16) < 0 { m |= 0x0004; }
    if (GetKeyState(VK_LWIN.0    as i32) as i16) < 0 { m |= 0x0008; }
    if (GetKeyState(VK_RWIN.0    as i32) as i16) < 0 { m |= 0x0008; }
    m
}

fn is_modifier_vk(vk: u32) -> bool {
    let mods: &[u32] = &[
        VK_SHIFT.0 as u32, VK_LSHIFT.0 as u32, VK_RSHIFT.0 as u32,
        VK_CONTROL.0 as u32, VK_LCONTROL.0 as u32, VK_RCONTROL.0 as u32,
        VK_MENU.0 as u32, VK_LMENU.0 as u32, VK_RMENU.0 as u32,
        VK_LWIN.0 as u32, VK_RWIN.0 as u32,
    ];
    mods.contains(&vk)
}

fn vk_name(vk: u32) -> String {
    let v = vk as u16;
    if v == VK_F1.0  { return "F1".into(); }
    if v == VK_F2.0  { return "F2".into(); }
    if v == VK_F3.0  { return "F3".into(); }
    if v == VK_F4.0  { return "F4".into(); }
    if v == VK_F5.0  { return "F5".into(); }
    if v == VK_F6.0  { return "F6".into(); }
    if v == VK_F7.0  { return "F7".into(); }
    if v == VK_F8.0  { return "F8".into(); }
    if v == VK_F9.0  { return "F9".into(); }
    if v == VK_F10.0 { return "F10".into(); }
    if v == VK_F11.0 { return "F11".into(); }
    if v == VK_F12.0 { return "F12".into(); }
    if v == VK_SPACE.0  { return "Space".into(); }
    if v == VK_RETURN.0 { return "Enter".into(); }
    if v == VK_TAB.0    { return "Tab".into(); }
    if v == VK_BACK.0   { return "Backspace".into(); }
    if v == VK_DELETE.0 { return "Del".into(); }
    if v == VK_INSERT.0 { return "Ins".into(); }
    if v == VK_HOME.0   { return "Home".into(); }
    if v == VK_END.0    { return "End".into(); }
    if v == VK_PRIOR.0  { return "PgUp".into(); }
    if v == VK_NEXT.0   { return "PgDn".into(); }
    if v == VK_LEFT.0   { return "Left".into(); }
    if v == VK_RIGHT.0  { return "Right".into(); }
    if v == VK_UP.0     { return "Up".into(); }
    if v == VK_DOWN.0   { return "Down".into(); }
    if vk >= 0x30 && vk <= 0x39 { return format!("{}", (b'0' + (vk - 0x30) as u8) as char); }
    if vk >= 0x41 && vk <= 0x5A { return format!("{}", (b'A' + (vk - 0x41) as u8) as char); }
    format!("VK{vk:02X}")
}

fn hotkey_parts(packed: u64) -> Vec<String> {
    let mods = (packed >> 32) as u32;
    let vk   = packed as u32;
    let mut v = Vec::new();
    if mods & 0x0002 != 0 { v.push("Ctrl".to_string()); }
    if mods & 0x0001 != 0 { v.push("Alt".to_string()); }
    if mods & 0x0004 != 0 { v.push("Shift".to_string()); }
    if mods & 0x0008 != 0 { v.push("Win".to_string()); }
    if vk != 0 { v.push(vk_name(vk)); }
    v
}



// ── color conversion ──────────────────────────────────────────────────────────
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> COLORREF {
    let h = h.rem_euclid(360.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = if      h < 60.0  { (c, x, 0.0) }
                       else if h < 120.0 { (x, c, 0.0) }
                       else if h < 180.0 { (0.0, c, x) }
                       else if h < 240.0 { (0.0, x, c) }
                       else if h < 300.0 { (x, 0.0, c) }
                       else              { (c, 0.0, x) };
    rgb(
        ((r1 + m) * 255.0) as u8,
        ((g1 + m) * 255.0) as u8,
        ((b1 + m) * 255.0) as u8,
    )
}
