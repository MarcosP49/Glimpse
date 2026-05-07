use anyhow::Result;
use crossbeam_channel::Sender;
use crate::log::clilog;
use crate::overlay::OverlayHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey, MOD_CONTROL, MOD_SHIFT, VK_F9,
};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MSG, PostThreadMessageW, WM_APP, WM_HOTKEY,
};

const HK_SAVE:    i32 = 1;
const HK_OVERLAY: i32 = 2;

/// Posted to the hotkey thread to trigger re-registration of HK_SAVE.
const WM_REREGISTER_HK: u32 = WM_APP + 1;

pub fn start_hotkey_listener(
    save_tx: Sender<()>,
    overlay: OverlayHandle,
    hotkey: Arc<AtomicU64>,
    hk_thread_id: Arc<AtomicU32>,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            if let Err(e) = hotkey_loop(&save_tx, &overlay, &hotkey, &hk_thread_id) {
                clilog!("[hotkey] error: {e:#}");
            }
        })
        .map_err(Into::into)
}

fn hotkey_loop(
    save_tx: &Sender<()>,
    overlay: &OverlayHandle,
    hotkey: &Arc<AtomicU64>,
    hk_thread_id: &Arc<AtomicU32>,
) -> Result<()> {
    unsafe {
        hk_thread_id.store(GetCurrentThreadId(), Ordering::Relaxed);

        // Register overlay toggle first — this must succeed for the app to be usable.
        RegisterHotKey(None, HK_OVERLAY, MOD_SHIFT | MOD_CONTROL, VK_F9.0 as u32)
            .map_err(|e| anyhow::anyhow!("RegisterHotKey Shift+Ctrl+F9 failed: {e}"))?;

        let packed = hotkey.load(Ordering::Relaxed);
        let mods = HOT_KEY_MODIFIERS((packed >> 32) as u32);
        let vk   = packed as u32;

        // Save hotkey is best-effort — another app may have claimed the combo.
        if let Err(e) = RegisterHotKey(None, HK_SAVE, mods, vk) {
            clilog!("[hotkey] save hotkey unavailable ({e}), clip via tray menu only");
        }

        clilog!("[hotkey] save={}  overlay=Shift+Ctrl+F9", fmt_hotkey(packed));

        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 <= 0 { break; }

            if msg.message == WM_HOTKEY {
                match msg.wParam.0 as i32 {
                    HK_SAVE    => { clilog!("[hotkey] save clip"); let _ = save_tx.try_send(()); }
                    HK_OVERLAY => { clilog!("[hotkey] toggle overlay"); overlay.toggle(); }
                    _ => {}
                }
            } else if msg.message == WM_REREGISTER_HK {
                let packed = hotkey.load(Ordering::Relaxed);
                let mods = HOT_KEY_MODIFIERS((packed >> 32) as u32);
                let vk   = packed as u32;
                // Don't allow save hotkey to steal the overlay toggle combo.
                let is_overlay_combo = mods == (MOD_SHIFT | MOD_CONTROL)
                    && vk == VK_F9.0 as u32;
                if is_overlay_combo {
                    clilog!("[hotkey] save hotkey conflicts with overlay toggle — ignored");
                } else {
                    let _ = UnregisterHotKey(None, HK_SAVE);
                    if let Err(e) = RegisterHotKey(None, HK_SAVE, mods, vk) {
                        clilog!("[hotkey] re-register failed: {e}");
                    } else {
                        clilog!("[hotkey] save hotkey updated to {}", fmt_hotkey(packed));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Notify the hotkey thread to re-register the save hotkey.
pub fn notify_reregister(hk_thread_id: &Arc<AtomicU32>) {
    let tid = hk_thread_id.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_REREGISTER_HK, WPARAM(0), LPARAM(0));
        }
    }
}

fn fmt_hotkey(packed: u64) -> String {
    let mods = (packed >> 32) as u32;
    let vk   = packed as u32;
    let mut p: Vec<&str> = Vec::new();
    if mods & 0x0002 != 0 { p.push("Ctrl"); }
    if mods & 0x0001 != 0 { p.push("Alt"); }
    if mods & 0x0004 != 0 { p.push("Shift"); }
    if mods & 0x0008 != 0 { p.push("Win"); }
    let mut s = p.join("+");
    if !s.is_empty() { s.push('+'); }
    s.push_str(&format!("VK{vk:02X}"));
    s
}
