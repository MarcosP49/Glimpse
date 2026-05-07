use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Settings {
    pub monitor_index: u32,
    pub clip_secs: u32,
    pub fps: u32,
    pub bitrate_mbps: u32,
    pub start_with_windows: bool,
    pub hue: f32,
    pub hotkey_mods: u32, // MOD_SHIFT|MOD_CONTROL etc.
    pub hotkey_vk: u32,   // virtual key code
    pub bg_image_path: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            monitor_index: 0, clip_secs: 30, fps: 30, bitrate_mbps: 30,
            start_with_windows: false, hue: 45.0,
            hotkey_mods: 0x0006, // MOD_CONTROL | MOD_SHIFT
            hotkey_vk:   0x77,   // VK_F8
            bg_image_path: String::new(),
        }
    }
}

impl Settings {
    fn config_path() -> Option<PathBuf> {
        std::env::var("APPDATA")
            .ok()
            .map(|p| PathBuf::from(p).join("glimpse").join("config.cfg"))
    }

    pub fn load() -> Self {
        let mut s = Self::default();
        s.start_with_windows = is_startup_registered();

        let path = match Self::config_path() { Some(p) => p, None => return s };
        let text = match std::fs::read_to_string(&path) { Ok(t) => t, Err(_) => return s };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let mut parts = line.splitn(2, '=');
            let key = parts.next().unwrap_or("").trim();
            let val = parts.next().unwrap_or("").trim();
            match key {
                "monitor_index" => { if let Ok(v) = val.parse() { s.monitor_index = v; } }
                "clip_secs"     => { if let Ok(v) = val.parse::<u32>() { s.clip_secs = v.clamp(10, 120); } }
                "fps"           => { if let Ok(v) = val.parse::<u32>() { s.fps = v.clamp(1, 120); } }
                "bitrate_mbps"  => { if let Ok(v) = val.parse::<u32>() { s.bitrate_mbps = v.clamp(1, 100); } }
                "hue"           => { if let Ok(v) = val.parse::<f32>() { s.hue = v.clamp(0.0, 360.0); } }
                "hotkey_mods"   => { if let Ok(v) = val.parse() { s.hotkey_mods = v; } }
                "hotkey_vk"     => { if let Ok(v) = val.parse() { s.hotkey_vk = v; } }
                "bg_image_path" => { s.bg_image_path = val.to_string(); }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        set_startup_registry(self.start_with_windows);

        let path = Self::config_path()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no APPDATA"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            format!(
                "monitor_index={}\nclip_secs={}\nfps={}\nbitrate_mbps={}\nhue={:.1}\nhotkey_mods={}\nhotkey_vk={}\nbg_image_path={}\n",
                self.monitor_index, self.clip_secs, self.fps, self.bitrate_mbps,
                self.hue, self.hotkey_mods, self.hotkey_vk, self.bg_image_path,
            ),
        )
    }
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Glimpse";

pub fn is_startup_registered() -> bool {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::*;
    use windows::core::PCWSTR;
    unsafe {
        let key_w: Vec<u16> = format!("{RUN_KEY}\0").encode_utf16().collect();
        let val_w: Vec<u16> = format!("{RUN_VALUE}\0").encode_utf16().collect();
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key_w.as_ptr()), 0, KEY_READ, &mut hkey)
            != ERROR_SUCCESS
        {
            return false;
        }
        let found = RegQueryValueExW(hkey, PCWSTR(val_w.as_ptr()), None, None, None, None)
            == ERROR_SUCCESS;
        let _ = RegCloseKey(hkey);
        found
    }
}

pub fn set_startup_registry(enabled: bool) {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::*;
    use windows::core::PCWSTR;
    unsafe {
        let key_w: Vec<u16> = format!("{RUN_KEY}\0").encode_utf16().collect();
        let val_w: Vec<u16> = format!("{RUN_VALUE}\0").encode_utf16().collect();
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key_w.as_ptr()), 0, KEY_WRITE, &mut hkey)
            != ERROR_SUCCESS
        {
            return;
        }
        if enabled {
            if let Ok(exe) = std::env::current_exe() {
                let data_str = format!("\"{}\"\0", exe.to_string_lossy());
                let data_w: Vec<u16> = data_str.encode_utf16().collect();
                let bytes = std::slice::from_raw_parts(
                    data_w.as_ptr() as *const u8,
                    data_w.len() * 2,
                );
                let _ = RegSetValueExW(hkey, PCWSTR(val_w.as_ptr()), 0, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR(val_w.as_ptr()));
        }
        let _ = RegCloseKey(hkey);
    }
}
