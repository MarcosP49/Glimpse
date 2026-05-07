use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::OnceLock;

static LOG: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init() {
    let Some(path) = log_path() else { return };
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = LOG.set(Mutex::new(f));
    }
}

fn log_path() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|p| std::path::PathBuf::from(p).join("glimpse").join("glimpse.log"))
}

pub fn write_line(msg: &str) {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let ts = unsafe {
        let st = GetLocalTime();
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    };
    let line = format!("[{ts}] {msg}\n");
    if let Some(f) = LOG.get() {
        let _ = f.lock().write_all(line.as_bytes());
    }
}

macro_rules! clilog {
    ($($arg:tt)*) => { $crate::log::write_line(&format!($($arg)*)) };
}
pub(crate) use clilog;
