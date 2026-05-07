use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, ShellExecuteW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, KillTimer, LoadIconW,
    PostQuitMessage, RegisterClassExW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    TrackPopupMenu, TranslateMessage,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HWND_MESSAGE,
    IDI_APPLICATION, MF_SEPARATOR, MF_STRING, MSG, SW_SHOW,
    TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WNDCLASSEXW, WM_APP,
    WM_COMMAND, WM_CREATE, WM_DESTROY, WM_LBUTTONDBLCLK, WM_RBUTTONUP, WM_TIMER,
};

use crate::log::clilog;
use crate::overlay::OverlayHandle;
use crate::segment_ring::SegmentRing;

const WM_TRAYICON: u32 = WM_APP + 1;
const IDM_SAVE: usize = 100;
const IDM_SETTINGS: usize = 101;
const IDM_EXIT: usize = 102;
const IDM_OPEN_FOLDER: usize = 103;
const TIMER_STATS: usize = 1;

const TRAY_CLASS: &str = "GlimpseTray";

struct TrayState {
    save_tx: Sender<()>,
    overlay: OverlayHandle,
    video_ring: Arc<Mutex<SegmentRing>>,
    notify_rx: Receiver<String>,
    clip_secs: Arc<AtomicU32>,
    clips_dir: PathBuf,
}

pub fn start_tray(
    save_tx: Sender<()>,
    overlay: OverlayHandle,
    video_ring: Arc<Mutex<SegmentRing>>,
    notify_rx: Receiver<String>,
    clip_secs: Arc<AtomicU32>,
    clips_dir: PathBuf,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            if let Err(e) = tray_loop(save_tx, overlay, video_ring, notify_rx, clip_secs, clips_dir) {
                clilog!("[tray] error: {e:#}");
            }
        })
        .map_err(Into::into)
}

fn tray_loop(
    save_tx: Sender<()>,
    overlay: OverlayHandle,
    video_ring: Arc<Mutex<SegmentRing>>,
    notify_rx: Receiver<String>,
    clip_secs: Arc<AtomicU32>,
    clips_dir: PathBuf,
) -> Result<()> {
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();

        let tray_class_w = wide(TRAY_CLASS);
        let wc_tray = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: hinstance,
            lpszClassName: wptr(&tray_class_w),
            ..Default::default()
        };
        RegisterClassExW(&wc_tray);

        let state = Box::new(TrayState {
            save_tx,
            overlay,
            video_ring,
            notify_rx,
            clip_secs,
            clips_dir,
        });
        let state_ptr = Box::into_raw(state);

        let hwnd = CreateWindowExW(
            Default::default(),
            wptr(&tray_class_w),
            windows::core::PCWSTR::null(),
            Default::default(),
            0, 0, 0, 0,
            HWND_MESSAGE,
            None,
            hinstance,
            Some(state_ptr as *const _),
        )?;

        clilog!("[tray] tray window created, adding icon");
        add_tray_icon(hwnd)?;
        SetTimer(hwnd, TIMER_STATS, 1000, None);

        clilog!("[tray] icon active — right-click tray icon to open menu");

        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 <= 0 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        Ok(())
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            LRESULT(0)
        }

        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }

        WM_TRAYICON => {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_LBUTTONDBLCLK {
                show_context_menu(hwnd);
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
            if !ptr.is_null() {
                let state = &mut *ptr;
                match id {
                    IDM_SAVE => {
                        let _ = state.save_tx.try_send(());
                    }
                    IDM_SETTINGS => {
                        state.overlay.toggle();
                    }
                    IDM_OPEN_FOLDER => {
                        let _ = std::fs::create_dir_all(&state.clips_dir);
                        let path_w = wide(&state.clips_dir.to_string_lossy());
                        let open_w = wide("open");
                        ShellExecuteW(
                            hwnd,
                            wptr(&open_w),
                            wptr(&path_w),
                            windows::core::PCWSTR::null(),
                            windows::core::PCWSTR::null(),
                            SW_SHOW,
                        );
                    }
                    IDM_EXIT => {
                        remove_tray_icon(hwnd);
                        KillTimer(hwnd, TIMER_STATS);
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }

        WM_TIMER => {
            if wparam.0 == TIMER_STATS {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
                if !ptr.is_null() {
                    let cs = (*ptr).clip_secs.load(Ordering::Relaxed);
                    (*ptr).video_ring.lock().update(cs);
                    while let Ok(filename) = (*ptr).notify_rx.try_recv() {
                        show_balloon_notification(hwnd, &filename);
                    }
                }
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn show_balloon_notification(hwnd: HWND, filename: &str) {
    unsafe {
        let title = wide("Clip saved");
        let body = wide(filename);
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_INFO;
        let t_len = title.len().min(nid.szInfoTitle.len());
        nid.szInfoTitle[..t_len].copy_from_slice(&title[..t_len]);
        let b_len = body.len().min(nid.szInfo.len());
        nid.szInfo[..b_len].copy_from_slice(&body[..b_len]);
        nid.dwInfoFlags = NIIF_INFO;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn add_tray_icon(hwnd: HWND) -> Result<()> {
    unsafe {
        let icon = LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap_or_default();
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = icon;
        let tip = wide("Glimpse");
        let len = tip.len().min(nid.szTip.len());
        nid.szTip[..len].copy_from_slice(&tip[..len]);
        Shell_NotifyIconW(NIM_ADD, &nid)
            .ok()
            .map_err(|e| anyhow::anyhow!("Shell_NotifyIconW failed: {e}"))?;
        Ok(())
    }
}

fn remove_tray_icon(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}


fn show_context_menu(hwnd: HWND) {
    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return,
        };
        let save_w = wide("Save Clip");
        let folder_w = wide("Open Clips Folder");
        let settings_w = wide("Settings...");
        let exit_w = wide("Exit");
        let _ = AppendMenuW(menu, MF_STRING, IDM_SAVE, wptr(&save_w));
        let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN_FOLDER, wptr(&folder_w));
        let _ = AppendMenuW(menu, MF_STRING, IDM_SETTINGS, wptr(&settings_w));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, windows::core::PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, wptr(&exit_w));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_BOTTOMALIGN | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0u16)).collect()
}

fn wptr(v: &[u16]) -> windows::core::PCWSTR {
    windows::core::PCWSTR(v.as_ptr())
}
