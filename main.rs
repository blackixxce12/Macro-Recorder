#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! # Macro Recorder 1.2
//!
//! A DPI-aware macro recorder for Windows.
//!
//! Thread layout (unchanged from 1.1, but hardened):
//!   * UI thread          - eframe/egui
//!   * hook thread        - WH_KEYBOARD_LL + WH_MOUSE_LL + RegisterHotKey + message loop
//!   * collector thread   - crossbeam channel -> Vec<MacroEvent>
//!   * playback thread    - spin_sleep + timeBeginPeriod(1) + SendInput
//!
//! Everything shared lives in `Arc<AppState>` (atomics + parking_lot).
//!
//! Notes for maintainers:
//!   * `panic = "abort"` in release, so `catch_unwind` is useless -> the hook callbacks are
//!     written to be panic-free (no unwrap, no indexing, null-pointer guards).
//!   * The hook callbacks must stay cheap. No COM, no window enumeration, no blocking locks
//!     in the hot path: Windows silently unhooks a callback that exceeds LowLevelHooksTimeout.

use anyhow::{Context as _, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[cfg(windows)]
mod win32 {
    pub use windows::Win32::Foundation::*;
    pub use windows::Win32::Globalization::GetUserDefaultUILanguage;
    pub use windows::Win32::Graphics::Dwm::*;
    pub use windows::Win32::Graphics::Gdi::HRGN;
    pub use windows::Win32::Media::*;
    pub use windows::Win32::Security::*;
    pub use windows::Win32::System::Com::*;
    pub use windows::Win32::System::LibraryLoader::*;
    pub use windows::Win32::System::Power::*;
    pub use windows::Win32::System::Registry::*;
    pub use windows::Win32::System::Shutdown::*;
    pub use windows::Win32::System::Threading::*;
    pub use windows::Win32::UI::HiDpi::*;
    pub use windows::Win32::UI::Input::KeyboardAndMouse::*;
    pub use windows::Win32::UI::Shell::*;
    pub use windows::Win32::UI::WindowsAndMessaging::*;
    pub use windows::core::{PCSTR, PCWSTR, w};
}

// ============================================================================
// Constants
// ============================================================================

const APP_TITLE: &str = "Macro Recorder";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hotkey ids passed to RegisterHotKey.
const HK_ID_RECORD: i32 = 1;
const HK_ID_PLAY: i32 = 2;
const HK_ID_STOP: i32 = 3;

/// Custom thread messages for the hook thread.
const WM_APP_REHOTKEY: u32 = 0x8001; // WM_APP + 1
const WM_HOTKEY_ID: u32 = 0x0312; // WM_HOTKEY

/// Maximum time a single sleep chunk may last inside the playback loop (B2).
/// Bounds the worst-case reaction time to Stop / Pause.
const SLEEP_CHUNK_US: u64 = 15_000;
/// Below this threshold we busy-wait instead of sleeping.
const SPIN_THRESHOLD_US: u64 = 2_000;
/// Refresh interval for the cached virtual-screen metrics.
const METRICS_TTL_US: u64 = 500_000;
/// Refresh interval for the cached "are we on the active virtual desktop" answer.
const DESKTOP_TTL_US: u64 = 200_000;
/// Sanity limit for a loaded macro.
const MAX_EVENTS: usize = 4_000_000;

// ============================================================================
// Small utilities
// ============================================================================

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn init_epoch() {
    let _ = EPOCH.set(Instant::now());
}

fn now_us() -> u64 {
    EPOCH.get().map(|e| e.elapsed().as_micros() as u64).unwrap_or(0)
}

fn format_us(us: u64) -> String {
    let secs = us / 1_000_000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 { format!("{h:02}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

/// Tiny xorshift64* PRNG.
///
/// Deliberately dependency-free: the only randomness we need is timing jitter,
/// and a self-contained generator keeps this testable and reproducible.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let seed = now_us() ^ 0x9E37_79B9_7F4A_7C15 ^ (std::process::id() as u64) << 32;
        Self(if seed == 0 { 0x2545_F491_4F6C_DD1D } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform value in `0..bound` (returns 0 when `bound == 0`).
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next_u64() % bound }
    }
}

// ============================================================================
// Paths (B5) - portable next to the exe, otherwise %APPDATA%
// ============================================================================

mod paths {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

    fn is_writable(dir: &Path) -> bool {
        let probe = dir.join(".macro_recorder_write_test");
        match std::fs::File::create(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }

    fn exe_dir() -> Option<PathBuf> {
        Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
    }

    #[cfg(windows)]
    fn roaming_dir() -> Option<PathBuf> {
        // VERIFY: known-folders 1.3 exposes `get_known_folder_path(KnownFolder) -> Option<PathBuf>`.
        known_folders::get_known_folder_path(known_folders::KnownFolder::RoamingAppData)
            .map(|p| p.join("MacroRecorder"))
    }

    #[cfg(not(windows))]
    fn roaming_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/macro-recorder"))
    }

    /// Directory that holds config, macros and logs.
    ///
    /// Order: portable (exe directory, if writable) -> %APPDATA%\MacroRecorder -> cwd.
    pub fn data_dir() -> &'static Path {
        DATA_DIR.get_or_init(|| {
            if let Some(dir) = exe_dir() {
                if is_writable(&dir) {
                    return dir;
                }
            }
            if let Some(dir) = roaming_dir() {
                if std::fs::create_dir_all(&dir).is_ok() && is_writable(&dir) {
                    return dir;
                }
            }
            PathBuf::from(".")
        })
    }

    pub fn config_path() -> PathBuf {
        data_dir().join("config.json")
    }

    pub fn default_macro_path() -> PathBuf {
        data_dir().join("macro.json")
    }

    pub fn log_dir() -> PathBuf {
        let dir = data_dir().join("logs");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}

// ============================================================================
// Hotkeys
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub vk: u32,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
}

impl Hotkey {
    const fn plain(vk: u32) -> Self {
        Self { vk, ctrl: false, alt: false, shift: false }
    }

    fn label(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(vk_name(self.vk));
        s
    }
}

/// Keys offered in the hotkey pickers. Deliberately conservative: keys that are
/// rarely typed inside a macro and rarely stolen by other applications.
const HOTKEY_CHOICES: [(&str, u32); 22] = [
    ("F1", 0x70),
    ("F2", 0x71),
    ("F3", 0x72),
    ("F4", 0x73),
    ("F5", 0x74),
    ("F6", 0x75),
    ("F7", 0x76),
    ("F8", 0x77),
    ("F9", 0x78),
    ("F10", 0x79),
    ("F11", 0x7A),
    ("F12", 0x7B),
    ("Pause", 0x13),
    ("ScrollLock", 0x91),
    ("Insert", 0x2D),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Num *", 0x6A),
    ("Num -", 0x6D),
    ("Num +", 0x6B),
];

fn vk_name(vk: u32) -> &'static str {
    HOTKEY_CHOICES.iter().find(|(_, v)| *v == vk).map(|(n, _)| *n).unwrap_or("?")
}

/// Hot-path copies of the hotkey virtual-key codes.
///
/// The keyboard hook fires *before* Windows dispatches WM_HOTKEY, so without this
/// filter every F8/F9 press would end up inside the recording (B9).
static HK_VK: [AtomicU32; 3] =
    [AtomicU32::new(0x77), AtomicU32::new(0x78), AtomicU32::new(0x13)];

/// Bit mask of hotkeys that failed to register (R2). Bit 0 = record, 1 = play, 2 = stop.
static HK_FAILED: AtomicU32 = AtomicU32::new(0);

/// Thread id of the hook thread, used to post re-registration requests.
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

fn publish_hotkeys(cfg: &AppConfig) {
    HK_VK[0].store(cfg.hotkey_record.vk, Ordering::Relaxed);
    HK_VK[1].store(cfg.hotkey_play.vk, Ordering::Relaxed);
    HK_VK[2].store(cfg.hotkey_stop.vk, Ordering::Relaxed);
    *PENDING_HOTKEYS.lock() = [cfg.hotkey_record, cfg.hotkey_play, cfg.hotkey_stop];
}

static PENDING_HOTKEYS: Mutex<[Hotkey; 3]> = Mutex::new([
    Hotkey::plain(0x77),
    Hotkey::plain(0x78),
    Hotkey::plain(0x13),
]);

fn is_hotkey_vk(vk: u32) -> bool {
    HK_VK.iter().any(|a| a.load(Ordering::Relaxed) == vk)
}

/// Ask the hook thread to re-register the hotkeys with the current configuration.
fn request_hotkey_refresh() {
    #[cfg(windows)]
    {
        let tid = HOOK_THREAD_ID.load(Ordering::Relaxed);
        if tid != 0 {
            unsafe {
                let _ = win32::PostThreadMessageW(
                    tid,
                    WM_APP_REHOTKEY,
                    win32::WPARAM(0),
                    win32::LPARAM(0),
                );
            }
        }
    }
}

// ============================================================================
// Configuration & persistence
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AppConfig {
    // appearance
    pub default_lang: usize,
    pub default_theme: usize,
    pub transparent_ui: bool,
    pub always_on_top: bool,

    // playback
    pub loop_play: bool,
    pub play_count_limit: u64,
    pub speed: f64,
    pub absolute_mouse: bool,
    pub repeat_delay_ms: u64,
    pub jitter_pct: u64,

    // recording
    pub capture_mouse_moves: bool,
    pub mouse_sample_ms: u64,

    // time limit
    pub time_limit_enabled: bool,
    pub time_limit_h: u64,
    pub time_limit_m: u64,
    pub time_limit_s: u64,
    pub action_on_completion: usize,
    pub shutdown_delay_s: u64,

    // hotkeys
    pub hotkey_record: Hotkey,
    pub hotkey_play: Hotkey,
    pub hotkey_stop: Hotkey,

    // files
    pub recent_files: Vec<String>,
    pub compress_on_save: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_lang: 0,
            default_theme: 0,
            transparent_ui: true,
            always_on_top: true,

            loop_play: true,
            play_count_limit: 1,
            speed: 1.0,
            absolute_mouse: true,
            repeat_delay_ms: 0,
            jitter_pct: 0,

            capture_mouse_moves: true,
            mouse_sample_ms: 5,

            time_limit_enabled: false,
            time_limit_h: 0,
            time_limit_m: 0,
            time_limit_s: 0,
            action_on_completion: 0,
            shutdown_delay_s: 60,

            hotkey_record: Hotkey::plain(0x77), // F8
            hotkey_play: Hotkey::plain(0x78),   // F9
            hotkey_stop: Hotkey::plain(0x13),   // Pause/Break

            recent_files: Vec::new(),
            compress_on_save: false,
        }
    }
}

impl AppConfig {
    fn sanitize(&mut self) {
        self.default_lang = self.default_lang.min(6);
        self.default_theme = self.default_theme.min(THEME_NAMES.len() - 1);
        self.play_count_limit = self.play_count_limit.clamp(1, 9999);
        self.speed = if self.speed.is_finite() { self.speed.clamp(0.05, 10.0) } else { 1.0 };
        self.repeat_delay_ms = self.repeat_delay_ms.min(600_000);
        self.jitter_pct = self.jitter_pct.min(50);
        self.mouse_sample_ms = self.mouse_sample_ms.clamp(1, 100);
        self.time_limit_h = self.time_limit_h.min(240);
        self.time_limit_m = self.time_limit_m.min(59);
        self.time_limit_s = self.time_limit_s.min(59);
        self.action_on_completion = self.action_on_completion.min(EndAction::COUNT - 1);
        self.shutdown_delay_s = self.shutdown_delay_s.min(600);
        self.recent_files.truncate(8);
    }

    fn time_limit_us(&self) -> u64 {
        (self.time_limit_h * 3600 + self.time_limit_m * 60 + self.time_limit_s) * 1_000_000
    }

    fn push_recent(&mut self, path: &Path) {
        let s = path.to_string_lossy().to_string();
        self.recent_files.retain(|p| p != &s);
        self.recent_files.insert(0, s);
        self.recent_files.truncate(8);
    }
}

fn load_config() -> AppConfig {
    let mut cfg = std::fs::read_to_string(paths::config_path())
        .ok()
        .and_then(|s| serde_json::from_str::<AppConfig>(&s).ok())
        .unwrap_or_default();
    cfg.sanitize();
    cfg
}

fn save_config(cfg: &AppConfig) -> Result<()> {
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(paths::config_path(), json)
        .with_context(|| format!("writing {}", paths::config_path().display()))?;
    Ok(())
}

// ============================================================================
// Macro event model & storage
// ============================================================================

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum InputEventKind {
    Key { vk: u16, scan: u16, down: bool, extended: bool },
    MouseMove { x: i32, y: i32, dx: i32, dy: i32 },
    MouseButton { button: MouseButton, down: bool, x: i32, y: i32 },
    MouseWheel { delta: i32, x: i32, y: i32, horizontal: bool },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MacroEvent {
    pub t_us: u64,
    pub kind: InputEventKind,
}

/// Macro container, format version 2.
///
/// v1 files were a bare `[MacroEvent, ...]` array and are still accepted on load;
/// the only thing they lose is the trailing pause of the recording (B8).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct MacroData {
    #[serde(default = "format_version")]
    pub version: u32,
    /// Full length of the recording, including any trailing idle time.
    #[serde(default)]
    pub duration_us: u64,
    pub events: Vec<MacroEvent>,
}

fn format_version() -> u32 {
    2
}

impl MacroData {
    fn new(events: Vec<MacroEvent>, duration_us: u64) -> Self {
        Self { version: 2, duration_us, events }
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    fn last_t(&self) -> u64 {
        self.events.last().map(|e| e.t_us).unwrap_or(0)
    }

    /// Length of one playback cycle in recorded (un-scaled) microseconds.
    fn cycle_len_us(&self) -> u64 {
        self.duration_us.max(self.last_t()).max(1)
    }

    /// Sorts non-monotonic timestamps and rejects obviously broken files (R5).
    fn normalize(&mut self) -> Result<()> {
        if self.events.is_empty() {
            anyhow::bail!("macro contains no events");
        }
        if self.events.len() > MAX_EVENTS {
            anyhow::bail!("macro contains {} events (limit {MAX_EVENTS})", self.events.len());
        }
        let monotonic = self.events.windows(2).all(|w| w[0].t_us <= w[1].t_us);
        if !monotonic {
            warn!("macro timestamps are not monotonic - sorting");
            self.events.sort_by_key(|e| e.t_us);
        }
        let last = self.last_t();
        if self.duration_us < last {
            self.duration_us = last;
        }
        Ok(())
    }
}

/// `.mrz` / `.gz` are gzipped compact JSON, everything else is plain JSON.
fn is_compressed_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("mrz") | Some("gz")
    )
}

fn save_macro(path: &Path, data: &MacroData) -> Result<()> {
    if is_compressed_path(path) {
        use std::io::Write as _;
        let raw = serde_json::to_vec(data)?;
        let mut enc =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&raw)?;
        let compressed = enc.finish()?;
        std::fs::write(path, compressed)
    } else {
        std::fs::write(path, serde_json::to_vec_pretty(data)?)
    }
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn load_macro(path: &Path) -> Result<MacroData> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;

    let text = if is_compressed_path(path) {
        use std::io::Read as _;
        let mut out = String::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_string(&mut out)?;
        out
    } else {
        String::from_utf8(bytes).context("macro file is not valid UTF-8")?
    };

    // v2 object first, then fall back to the v1 bare array.
    let mut data = match serde_json::from_str::<MacroData>(&text) {
        Ok(d) => d,
        Err(obj_err) => match serde_json::from_str::<Vec<MacroEvent>>(&text) {
            Ok(events) => {
                let dur = events.last().map(|e| e.t_us).unwrap_or(0);
                info!("loaded legacy v1 macro ({} events)", events.len());
                MacroData::new(events, dur)
            }
            Err(_) => return Err(anyhow::Error::new(obj_err).context("unrecognised macro format")),
        },
    };
    data.normalize()?;
    Ok(data)
}

// ============================================================================
// End-of-run actions
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndAction {
    Stop,
    Shutdown,
    Reboot,
    Sleep,
    Hibernate,
    LogOff,
}

impl EndAction {
    const COUNT: usize = 6;

    fn from_index(i: usize) -> Self {
        match i {
            1 => Self::Shutdown,
            2 => Self::Reboot,
            3 => Self::Sleep,
            4 => Self::Hibernate,
            5 => Self::LogOff,
            _ => Self::Stop,
        }
    }
}

// ============================================================================
// Virtual Desktop isolation (Windows 11)
// ============================================================================

#[cfg(windows)]
mod virtual_desktop {
    use super::win32::*;
    use super::{DESKTOP_TTL_US, now_us};
    use std::cell::RefCell;

    thread_local! {
        static VDM: RefCell<Option<IVirtualDesktopManager>> = const { RefCell::new(None) };
        /// (last check timestamp, cached answer) - see B4.
        static CACHE: RefCell<(u64, bool)> = const { RefCell::new((0, true)) };
    }

    pub fn init_thread() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if let Ok(vdm) = CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL) {
                VDM.with(|v| *v.borrow_mut() = Some(vdm));
            }
        }
    }

    fn query(hwnd: HWND) -> bool {
        if hwnd.0.is_null() {
            return true;
        }
        VDM.with(|v| match v.borrow().as_ref() {
            Some(vdm) => unsafe {
                vdm.IsWindowOnCurrentVirtualDesktop(hwnd).unwrap_or_default().as_bool()
            },
            None => true,
        })
    }

    /// Throttled variant used by the hooks and the playback loop.
    ///
    /// A COM round-trip per keystroke is exactly the kind of thing that gets a
    /// low-level hook killed by the system, so the answer is cached.
    pub fn is_app_on_active_desktop_cached(hwnd: HWND) -> bool {
        let now = now_us();
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if now.saturating_sub(c.0) >= DESKTOP_TTL_US || c.0 == 0 {
                c.0 = now;
                c.1 = query(hwnd);
            }
            c.1
        })
    }
}

#[cfg(not(windows))]
mod virtual_desktop {
    pub fn init_thread() {}
    pub fn is_app_on_active_desktop_cached(_: ()) -> bool {
        true
    }
}

// ============================================================================
// Platform layer
// ============================================================================

#[cfg(windows)]
mod platform {
    use super::win32::*;
    use super::{APP_TITLE, EndAction, METRICS_TTL_US, now_us};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicI32, AtomicIsize, AtomicU64, Ordering};

    /// Cached main-window handle (B4). Resolved at most once per second.
    static HWND_CACHE: AtomicIsize = AtomicIsize::new(0);
    static HWND_LAST_TRY: AtomicU64 = AtomicU64::new(0);

    /// Cached virtual-screen metrics (R6).
    static VS: [AtomicI32; 4] =
        [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(1), AtomicI32::new(1)];
    static VS_LAST: AtomicU64 = AtomicU64::new(0);

    /// Finds (and caches) our own top-level window.
    ///
    /// Verified against the process id so a foreign window with the same caption
    /// can never be picked up.
    pub fn app_hwnd() -> HWND {
        let cached = HWND_CACHE.load(Ordering::Relaxed);
        if cached != 0 {
            let hwnd = HWND(cached as *mut c_void);
            if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return hwnd;
            }
            HWND_CACHE.store(0, Ordering::Relaxed);
        }

        let now = now_us();
        let last = HWND_LAST_TRY.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < 1_000_000 {
            return HWND::default();
        }
        HWND_LAST_TRY.store(now, Ordering::Relaxed);

        unsafe {
            let title: Vec<u16> = APP_TITLE.encode_utf16().chain(std::iter::once(0)).collect();
            if let Ok(hwnd) = FindWindowW(None, PCWSTR(title.as_ptr())) {
                if !hwnd.0.is_null() {
                    let mut pid = 0u32;
                    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
                    if pid == GetCurrentProcessId() {
                        HWND_CACHE.store(hwnd.0 as isize, Ordering::Relaxed);
                        return hwnd;
                    }
                }
            }
        }
        HWND::default()
    }

    pub fn apply_system_backdrop(hwnd: HWND, backdrop: i32) {
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            let dark_mode: i32 = 1;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark_mode as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            );
            // DWMWA_SYSTEMBACKDROP_TYPE = 38: 1 = none, 2 = Mica, 3 = Acrylic, 4 = Tabbed.
            let backdrop_type: i32 = backdrop;
            let result = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(38),
                &backdrop_type as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            );
            if result.is_err() && backdrop > 1 {
                // Windows 10 fallback.
                let bb = DWM_BLURBEHIND {
                    dwFlags: DWM_BB_ENABLE,
                    fEnable: true.into(),
                    hRgnBlur: HRGN::default(),
                    fTransitionOnMaximized: false.into(),
                };
                let _ = DwmEnableBlurBehindWindow(hwnd, &bb);
            }
        }
    }

    fn virtual_screen() -> (i32, i32, i32, i32) {
        let now = now_us();
        let last = VS_LAST.load(Ordering::Relaxed);
        if last == 0 || now.saturating_sub(last) >= METRICS_TTL_US {
            unsafe {
                VS[0].store(GetSystemMetrics(SM_XVIRTUALSCREEN), Ordering::Relaxed);
                VS[1].store(GetSystemMetrics(SM_YVIRTUALSCREEN), Ordering::Relaxed);
                VS[2].store(GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1), Ordering::Relaxed);
                VS[3].store(GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1), Ordering::Relaxed);
            }
            VS_LAST.store(now, Ordering::Relaxed);
        }
        (
            VS[0].load(Ordering::Relaxed),
            VS[1].load(Ordering::Relaxed),
            VS[2].load(Ordering::Relaxed),
            VS[3].load(Ordering::Relaxed),
        )
    }

    /// Normalizes a screen pixel to the 0..=65535 range SendInput expects.
    ///
    /// Uses `w - 1` as the denominator (R6) so the right/bottom-most pixel is
    /// actually reachable; the classic `/ w` formula can never emit 65535.
    pub fn normalize_abs(x: i32, y: i32, vx: i32, vy: i32, vw: i32, vh: i32) -> (i32, i32) {
        let dx = (vw - 1).max(1) as f64;
        let dy = (vh - 1).max(1) as f64;
        let nx = (((x - vx) as f64 / dx) * 65535.0).round().clamp(0.0, 65535.0) as i32;
        let ny = (((y - vy) as f64 / dy) * 65535.0).round().clamp(0.0, 65535.0) as i32;
        (nx, ny)
    }

    pub unsafe fn send_absolute_mouse_move(x: i32, y: i32) {
        unsafe {
            let (vx, vy, vw, vh) = virtual_screen();
            let (nx, ny) = normalize_abs(x, y, vx, vy, vw, vh);
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: nx,
                        dy: ny,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE
                            | MOUSEEVENTF_ABSOLUTE
                            | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    pub fn begin_high_res_timer() {
        unsafe {
            let _ = timeBeginPeriod(1);
        }
    }

    pub fn end_high_res_timer() {
        unsafe {
            let _ = timeEndPeriod(1);
        }
    }

    /// Enables SeShutdownPrivilege for the current process.
    unsafe fn enable_shutdown_privilege() {
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
            .is_err()
            {
                return;
            }
            let mut luid = LUID::default();
            if LookupPrivilegeValueW(None, w!("SeShutdownPrivilege"), &mut luid).is_ok() {
                let mut tp = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
                };
                let _ = AdjustTokenPrivileges(token, false, Some(&mut tp), 0, None, None);
            }
            let _ = CloseHandle(token);
        }
    }

    /// Executes the configured end-of-run action.
    ///
    /// Shutdown / reboot use a visible countdown, so `shutdown /a` can still abort it.
    pub fn run_end_action(action: EndAction, delay_s: u32, reason: &str) -> anyhow::Result<()> {
        unsafe {
            match action {
                EndAction::Stop => Ok(()),
                EndAction::Shutdown | EndAction::Reboot => {
                    enable_shutdown_privilege();
                    let msg: Vec<u16> =
                        reason.encode_utf16().chain(std::iter::once(0)).collect();
                    let reboot = matches!(action, EndAction::Reboot);
                    let res = InitiateSystemShutdownExW(
                        PCWSTR::null(),
                        PCWSTR(msg.as_ptr()),
                        delay_s,
                        true.into(),   // force apps closed
                        reboot.into(), // reboot afterwards
                        SHTDN_REASON_MAJOR_OTHER
                            | SHTDN_REASON_MINOR_OTHER
                            | SHTDN_REASON_FLAG_PLANNED,
                    );
                    res.map_err(|e| anyhow::anyhow!("InitiateSystemShutdownExW failed: {e}"))
                }
                EndAction::LogOff => {
                    enable_shutdown_privilege();
                    ExitWindowsEx(
                        EWX_LOGOFF,
                        SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_MINOR_OTHER,
                    )
                    .map_err(|e| anyhow::anyhow!("ExitWindowsEx failed: {e}"))
                }
                EndAction::Sleep | EndAction::Hibernate => {
                    enable_shutdown_privilege();
                    let hibernate = matches!(action, EndAction::Hibernate);
                    // windows 0.62: fn SetSuspendState(bhibernate: bool, bforce: bool,
                    //                                  bwakeupeventsdisabled: bool) -> bool
                    if SetSuspendState(hibernate, true, false) {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!(
                            "SetSuspendState failed (hibernation may be disabled on this system)"
                        ))
                    }
                }
            }
        }
    }

    /// Returns false when another instance already holds the mutex (R1).
    pub fn acquire_single_instance() -> bool {
        unsafe {
            match CreateMutexW(None, true, w!("Local\\MacroRecorder_SingleInstance_v1")) {
                Ok(handle) => {
                    if GetLastError() == ERROR_ALREADY_EXISTS {
                        let _ = CloseHandle(handle);
                        false
                    } else {
                        // The handle is intentionally never closed: the mutex has to stay
                        // owned for the whole process lifetime. `HANDLE` is a Copy wrapper
                        // around a raw handle with no Drop impl, so simply letting it go out
                        // of scope leaves the kernel object open until the process exits -
                        // `mem::forget` would have been a no-op here.
                        true
                    }
                }
                Err(_) => true,
            }
        }
    }

    pub fn focus_existing_instance() {
        unsafe {
            let title: Vec<u16> = APP_TITLE.encode_utf16().chain(std::iter::once(0)).collect();
            if let Ok(hwnd) = FindWindowW(None, PCWSTR(title.as_ptr())) {
                if !hwnd.0.is_null() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                }
            }
        }
    }

    /// Attaches to the parent console so `--help` / `--no-gui` can print something
    /// even though the release build is a GUI subsystem binary.
    ///
    /// `AttachConsole` is resolved at runtime instead of being linked directly: that
    /// keeps the crate free of the `Win32_System_Console` feature, so this file builds
    /// against any feature set that already covers the rest of the app.
    ///
    /// Must run before the first `println!` - Rust caches the std handles on first use,
    /// and `AttachConsole` is what populates them for a GUI-subsystem process.
    pub fn attach_parent_console() {
        const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
        unsafe {
            let Ok(kernel32) = GetModuleHandleW(w!("kernel32.dll")) else {
                return;
            };
            let Some(sym) =
                GetProcAddress(kernel32, PCSTR(b"AttachConsole\0".as_ptr()))
            else {
                return;
            };
            // FARPROC is `Option<unsafe extern "system" fn() -> isize>`; re-type it to the
            // real AttachConsole signature: BOOL AttachConsole(DWORD dwProcessId).
            let attach: unsafe extern "system" fn(u32) -> i32 = std::mem::transmute(sym);
            let _ = attach(ATTACH_PARENT_PROCESS);
        }
    }

    pub fn set_dpi_awareness() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::EndAction;

    pub fn app_hwnd() {}
    pub fn apply_system_backdrop(_: (), _: i32) {}
    pub unsafe fn send_absolute_mouse_move(_: i32, _: i32) {}
    pub fn begin_high_res_timer() {}
    pub fn end_high_res_timer() {}
    pub fn acquire_single_instance() -> bool {
        true
    }
    pub fn focus_existing_instance() {}
    pub fn attach_parent_console() {}
    pub fn set_dpi_awareness() {}
    pub fn normalize_abs(_: i32, _: i32, _: i32, _: i32, _: i32, _: i32) -> (i32, i32) {
        (0, 0)
    }
    pub fn run_end_action(_: EndAction, _: u32, _: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("power actions are only supported on Windows"))
    }
}

// ============================================================================
// Shared application state
// ============================================================================

pub struct AppState {
    // lifecycle
    pub recording: AtomicBool,
    pub playing: AtomicBool,
    pub paused: AtomicBool,
    pub stop_play: AtomicBool,
    /// Incremented on every playback start; an older thread sees the mismatch and exits (B6).
    pub play_generation: AtomicU64,
    /// Set while playback is held back by the virtual-desktop gate (UI feedback).
    pub held_by_desktop: AtomicBool,

    // playback settings
    pub loop_play: AtomicBool,
    pub play_count: AtomicU64,
    pub play_count_limit: AtomicU64,
    pub absolute_mouse: AtomicBool,
    pub repeat_delay_ms: AtomicU64,
    pub jitter_pct: AtomicU64,
    pub speed: Mutex<f64>,

    // recording settings
    pub capture_mouse_moves: AtomicBool,
    pub mouse_sample_us: AtomicU64,

    // time limit
    pub time_limit_enabled: AtomicBool,
    pub time_limit_us: AtomicU64,
    pub action_on_completion: AtomicU64,
    pub shutdown_delay_s: AtomicU64,

    // recording bookkeeping
    pub rec_start_us: AtomicU64,
    pub last_move_us: AtomicU64,
    pub recorded_time_us: AtomicU64,
    pub last_x: Mutex<i32>,
    pub last_y: Mutex<i32>,

    // data
    pub macro_data: Mutex<MacroData>,
    pub current_path: Mutex<Option<PathBuf>>,
    pub event_tx: Option<Sender<MacroEvent>>,
}

impl AppState {
    fn new(tx: Sender<MacroEvent>) -> Arc<Self> {
        Arc::new(Self {
            recording: AtomicBool::new(false),
            playing: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            stop_play: AtomicBool::new(false),
            play_generation: AtomicU64::new(0),
            held_by_desktop: AtomicBool::new(false),

            loop_play: AtomicBool::new(true),
            play_count: AtomicU64::new(0),
            play_count_limit: AtomicU64::new(1),
            absolute_mouse: AtomicBool::new(true),
            repeat_delay_ms: AtomicU64::new(0),
            jitter_pct: AtomicU64::new(0),
            speed: Mutex::new(1.0),

            capture_mouse_moves: AtomicBool::new(true),
            mouse_sample_us: AtomicU64::new(5_000),

            time_limit_enabled: AtomicBool::new(false),
            time_limit_us: AtomicU64::new(0),
            action_on_completion: AtomicU64::new(0),
            shutdown_delay_s: AtomicU64::new(60),

            rec_start_us: AtomicU64::new(0),
            last_move_us: AtomicU64::new(0),
            recorded_time_us: AtomicU64::new(0),
            last_x: Mutex::new(i32::MIN),
            last_y: Mutex::new(i32::MIN),

            macro_data: Mutex::new(MacroData::default()),
            current_path: Mutex::new(None),
            event_tx: Some(tx),
        })
    }
}

/// Applies every persisted setting to the live state (B1).
///
/// This used to happen only inside `if checkbox(..).changed()`, which meant a saved
/// time limit or `loop_play = false` was silently ignored until the user toggled
/// the widget by hand.
fn apply_config_to_state(cfg: &AppConfig, state: &AppState) {
    state.loop_play.store(cfg.loop_play, Ordering::Relaxed);
    state.play_count_limit.store(cfg.play_count_limit, Ordering::Relaxed);
    state.absolute_mouse.store(cfg.absolute_mouse, Ordering::Relaxed);
    state.repeat_delay_ms.store(cfg.repeat_delay_ms, Ordering::Relaxed);
    state.jitter_pct.store(cfg.jitter_pct, Ordering::Relaxed);
    *state.speed.lock() = cfg.speed;

    state.capture_mouse_moves.store(cfg.capture_mouse_moves, Ordering::Relaxed);
    state.mouse_sample_us.store(cfg.mouse_sample_ms * 1_000, Ordering::Relaxed);

    state.time_limit_enabled.store(cfg.time_limit_enabled, Ordering::Relaxed);
    state.time_limit_us.store(cfg.time_limit_us(), Ordering::Relaxed);
    state.action_on_completion.store(cfg.action_on_completion as u64, Ordering::Relaxed);
    state.shutdown_delay_s.store(cfg.shutdown_delay_s, Ordering::Relaxed);
}

fn current_rec_time_us(state: &AppState) -> u64 {
    now_us().saturating_sub(state.rec_start_us.load(Ordering::Relaxed))
}

// ============================================================================
// Playback engine
// ============================================================================

/// Tracks what playback currently holds down, so nothing stays stuck (B3).
#[derive(Default)]
struct PressedInputs {
    keys: Vec<(u16, u16, bool)>,
    buttons: Vec<MouseButton>,
}

impl PressedInputs {
    fn note_key(&mut self, vk: u16, scan: u16, extended: bool, down: bool) {
        let key = (vk, scan, extended);
        if down {
            if !self.keys.contains(&key) {
                self.keys.push(key);
            }
        } else {
            self.keys.retain(|k| *k != key);
        }
    }

    fn note_button(&mut self, button: MouseButton, down: bool) {
        if down {
            if !self.buttons.contains(&button) {
                self.buttons.push(button);
            }
        } else {
            self.buttons.retain(|b| *b != button);
        }
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Releases everything still held, newest first.
    #[cfg(windows)]
    fn release_all(&mut self, state: &AppState) {
        while let Some((vk, scan, extended)) = self.keys.pop() {
            unsafe {
                send_input_event(
                    &InputEventKind::Key { vk, scan, down: false, extended },
                    state,
                    &mut PressedInputs::default(),
                );
            }
        }
        while let Some(button) = self.buttons.pop() {
            unsafe {
                send_input_event(
                    &InputEventKind::MouseButton { button, down: false, x: 0, y: 0 },
                    state,
                    &mut PressedInputs::default(),
                );
            }
        }
    }

    #[cfg(not(windows))]
    fn release_all(&mut self, _state: &AppState) {
        self.keys.clear();
        self.buttons.clear();
    }
}

/// Sleeps until `due_us` on the playback clock, waking up often enough to notice
/// Stop and Pause (B2). Returns false if playback should abort.
fn wait_until(
    state: &AppState,
    generation: u64,
    due_us: u64,
    elapsed_us: &dyn Fn() -> u64,
) -> bool {
    loop {
        if state.stop_play.load(Ordering::Relaxed)
            || state.play_generation.load(Ordering::Relaxed) != generation
        {
            return false;
        }
        if state.paused.load(Ordering::Relaxed) {
            return true; // let the main loop handle the pause bookkeeping
        }

        let now = elapsed_us();
        if now >= due_us {
            return true;
        }
        let remaining = due_us - now;

        if remaining > SPIN_THRESHOLD_US {
            let chunk = remaining.saturating_sub(1_000).min(SLEEP_CHUNK_US);
            std::thread::sleep(Duration::from_micros(chunk.max(1)));
        } else {
            spin_sleep::sleep(Duration::from_micros(remaining));
            return true;
        }
    }
}

fn playback_loop(state: Arc<AppState>, data: MacroData, generation: u64) {
    if data.is_empty() {
        state.playing.store(false, Ordering::Relaxed);
        return;
    }

    virtual_desktop::init_thread();
    platform::begin_high_res_timer();

    let speed = (*state.speed.lock()).clamp(0.05, 10.0);
    let repeat_delay_us = state.repeat_delay_ms.load(Ordering::Relaxed) * 1_000;
    let jitter_pct = state.jitter_pct.load(Ordering::Relaxed);
    let cycle_us = ((data.cycle_len_us() as f64 / speed) as u64) + repeat_delay_us;

    let loop_play = state.loop_play.load(Ordering::Relaxed);
    let max_count = if loop_play {
        u64::MAX
    } else {
        state.play_count_limit.load(Ordering::Relaxed).max(1)
    };

    let start = Instant::now();
    let mut paused_us: u64 = 0;
    let mut pause_started: Option<Instant> = None;
    let mut pressed = PressedInputs::default();
    let mut rng = Rng::new();

    let mut cycle_start_us: u64 = 0;
    let mut index: usize = 0;
    let mut count: u64 = 0;
    let mut prev_scaled_t: u64 = 0;

    // Monotonic playback clock that excludes paused time (F3 / fixes the
    // virtual-desktop fast-forward: holding no longer accumulates schedule debt).
    macro_rules! elapsed_us {
        () => {
            (start.elapsed().as_micros() as u64).saturating_sub(paused_us)
        };
    }

    loop {
        if state.stop_play.load(Ordering::Relaxed)
            || state.play_generation.load(Ordering::Relaxed) != generation
        {
            break;
        }

        // ---- pause / virtual-desktop gate ----------------------------------
        let on_desktop = virtual_desktop::is_app_on_active_desktop_cached(platform::app_hwnd());
        state.held_by_desktop.store(!on_desktop, Ordering::Relaxed);
        let should_hold = state.paused.load(Ordering::Relaxed) || !on_desktop;

        if should_hold {
            if pause_started.is_none() {
                // Never leave a key or a button held down while suspended.
                if !pressed.is_empty() {
                    pressed.release_all(&state);
                }
                pause_started = Some(Instant::now());
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        } else if let Some(p) = pause_started.take() {
            paused_us = paused_us.saturating_add(p.elapsed().as_micros() as u64);
        }

        // ---- time limit ----------------------------------------------------
        if state.time_limit_enabled.load(Ordering::Relaxed) {
            let limit = state.time_limit_us.load(Ordering::Relaxed);
            if limit > 0 && elapsed_us!() >= limit {
                let action =
                    EndAction::from_index(state.action_on_completion.load(Ordering::Relaxed) as usize);
                pressed.release_all(&state);
                // The loop breaks right after, so this can only ever fire once.
                if action != EndAction::Stop {
                    let delay = state.shutdown_delay_s.load(Ordering::Relaxed) as u32;
                    match platform::run_end_action(action, delay, "Macro Recorder: time limit reached.")
                    {
                        Ok(()) => info!("end action {action:?} requested"),
                        Err(e) => warn!("end action failed: {e}"),
                    }
                }
                break;
            }
        }

        // ---- end of cycle --------------------------------------------------
        if index >= data.events.len() {
            count += 1;
            state.play_count.store(count, Ordering::Relaxed);
            if count >= max_count {
                break;
            }
            cycle_start_us = cycle_start_us.saturating_add(cycle_us);
            index = 0;
            prev_scaled_t = 0;
            continue;
        }

        // ---- schedule the next event ---------------------------------------
        let ev = data.events[index];
        let scaled_t = (ev.t_us as f64 / speed) as u64;
        let mut due = cycle_start_us + scaled_t;

        if jitter_pct > 0 {
            // Positive-only jitter keeps the sequence ordered and never fires early.
            let gap = scaled_t.saturating_sub(prev_scaled_t);
            let max_off = gap.saturating_mul(jitter_pct) / 100;
            due = due.saturating_add(rng.below(max_off.min(250_000) + 1));
        }

        if !wait_until(&state, generation, due, &|| elapsed_us!()) {
            break;
        }
        if state.paused.load(Ordering::Relaxed) {
            continue; // handled at the top of the loop
        }
        if elapsed_us!() < due {
            continue; // woke up early (pause toggled) - re-evaluate
        }

        #[cfg(windows)]
        unsafe {
            send_input_event(&ev.kind, &state, &mut pressed);
        }

        prev_scaled_t = scaled_t;
        index += 1;
    }

    pressed.release_all(&state);
    platform::end_high_res_timer();
    state.held_by_desktop.store(false, Ordering::Relaxed);

    // Only the current generation is allowed to clear the flag (B6).
    if state.play_generation.load(Ordering::Relaxed) == generation {
        state.paused.store(false, Ordering::Relaxed);
        state.playing.store(false, Ordering::Relaxed);
    }
    info!("playback finished after {count} cycle(s)");
}

#[cfg(windows)]
unsafe fn send_input_event(kind: &InputEventKind, state: &AppState, pressed: &mut PressedInputs) {
    use win32::*;
    unsafe {
        match kind {
            InputEventKind::Key { vk, scan, down, extended } => {
                let mut flags = KEYBD_EVENT_FLAGS(0);
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                if *extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                let ki = if *scan != 0 {
                    flags |= KEYEVENTF_SCANCODE;
                    KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: *scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    }
                } else {
                    KEYBDINPUT {
                        wVk: VIRTUAL_KEY(*vk),
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    }
                };
                let input = INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                pressed.note_key(*vk, *scan, *extended, *down);
            }
            InputEventKind::MouseMove { x, y, dx, dy } => {
                if state.absolute_mouse.load(Ordering::Relaxed) {
                    platform::send_absolute_mouse_move(*x, *y);
                } else {
                    let input = INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: *dx,
                                dy: *dy,
                                mouseData: 0,
                                dwFlags: MOUSEEVENTF_MOVE,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    };
                    SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                }
            }
            InputEventKind::MouseButton { button, down, x, y } => {
                if state.absolute_mouse.load(Ordering::Relaxed) && (*x != 0 || *y != 0) {
                    platform::send_absolute_mouse_move(*x, *y);
                }
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, 1),
                    (MouseButton::X1, false) => (MOUSEEVENTF_XUP, 1),
                    (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, 2),
                    (MouseButton::X2, false) => (MOUSEEVENTF_XUP, 2),
                };
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: data,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                pressed.note_button(*button, *down);
            }
            InputEventKind::MouseWheel { delta, horizontal, .. } => {
                let flags = if *horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: *delta as u32,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
        }
    }
}

// ============================================================================
// Transport controls
// ============================================================================

fn emit_event(state: &AppState, kind: InputEventKind) {
    let event = MacroEvent { t_us: current_rec_time_us(state), kind };
    if let Some(tx) = state.event_tx.as_ref() {
        let _ = tx.send(event);
    }
}

fn stop_recording(state: &AppState) {
    if state.recording.swap(false, Ordering::Relaxed) {
        let dur = current_rec_time_us(state);
        state.recorded_time_us.store(dur, Ordering::Relaxed);
        // Give the collector a moment to drain, then stamp the true duration (B8).
        std::thread::sleep(Duration::from_millis(30));
        let mut data = state.macro_data.lock();
        data.duration_us = dur.max(data.last_t());
        data.version = 2;
        info!("recording stopped: {} events, {} us", data.events.len(), dur);
    }
}

fn start_recording(state: &Arc<AppState>) {
    if state.playing.load(Ordering::Relaxed) {
        return;
    }
    {
        let mut data = state.macro_data.lock();
        data.events.clear();
        data.duration_us = 0;
        data.version = 2;
    }
    *state.last_x.lock() = i32::MIN;
    *state.last_y.lock() = i32::MIN;
    state.last_move_us.store(0, Ordering::Relaxed);
    state.recorded_time_us.store(0, Ordering::Relaxed);
    state.rec_start_us.store(now_us(), Ordering::Relaxed);
    state.recording.store(true, Ordering::Relaxed);
    info!("recording started");
}

fn toggle_recording(state: &Arc<AppState>) {
    if state.recording.load(Ordering::Relaxed) {
        stop_recording(state);
    } else {
        start_recording(state);
    }
}

fn start_playback(state: &Arc<AppState>) {
    if state.recording.load(Ordering::Relaxed) || state.playing.load(Ordering::Relaxed) {
        return;
    }
    let data = state.macro_data.lock().clone();
    if data.is_empty() {
        return;
    }
    let generation = state.play_generation.fetch_add(1, Ordering::SeqCst) + 1;
    state.play_count.store(0, Ordering::Relaxed);
    state.stop_play.store(false, Ordering::Relaxed);
    state.paused.store(false, Ordering::Relaxed);
    state.playing.store(true, Ordering::Relaxed);

    let s = state.clone();
    std::thread::Builder::new()
        .name("playback".into())
        .spawn(move || playback_loop(s, data, generation))
        .map(|_| ())
        .unwrap_or_else(|e| {
            warn!("failed to spawn playback thread: {e}");
            state.playing.store(false, Ordering::Relaxed);
        });
    info!("playback started (generation {generation})");
}

fn stop_playback(state: &AppState) {
    if state.playing.load(Ordering::Relaxed) {
        state.paused.store(false, Ordering::Relaxed);
        state.stop_play.store(true, Ordering::Relaxed);
        info!("playback stop requested");
    }
}

fn toggle_playback(state: &Arc<AppState>) {
    if state.playing.load(Ordering::Relaxed) {
        stop_playback(state);
    } else {
        start_playback(state);
    }
}

fn toggle_pause(state: &AppState) {
    if state.playing.load(Ordering::Relaxed) {
        let now = !state.paused.load(Ordering::Relaxed);
        state.paused.store(now, Ordering::Relaxed);
        info!("playback {}", if now { "paused" } else { "resumed" });
    }
}

/// Emergency stop: kills both recording and playback.
fn stop_everything(state: &Arc<AppState>) {
    stop_playback(state);
    stop_recording(state);
}

fn collector_thread(rx: Receiver<MacroEvent>, state: Arc<AppState>) {
    while let Ok(event) = rx.recv() {
        if state.recording.load(Ordering::Relaxed) {
            let mut data = state.macro_data.lock();
            if data.events.len() < MAX_EVENTS {
                data.events.push(event);
            }
        }
    }
}

// ============================================================================
// Input hooks
// ============================================================================

#[cfg(windows)]
static GLOBAL_STATE: OnceLock<Arc<AppState>> = OnceLock::new();

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HookMode {
    /// Full recorder: low-level hooks + hotkeys.
    Full,
    /// Headless playback: hotkeys only, no input capture.
    HotkeysOnly,
}

#[cfg(windows)]
unsafe fn register_hotkeys() {
    use win32::*;
    let hk = *PENDING_HOTKEYS.lock();
    let mut failed = 0u32;
    unsafe {
        for (idx, id) in [HK_ID_RECORD, HK_ID_PLAY, HK_ID_STOP].into_iter().enumerate() {
            let _ = UnregisterHotKey(None, id);
            let key = hk[idx];
            let mut mods = MOD_NOREPEAT;
            if key.ctrl {
                mods |= MOD_CONTROL;
            }
            if key.alt {
                mods |= MOD_ALT;
            }
            if key.shift {
                mods |= MOD_SHIFT;
            }
            if RegisterHotKey(None, id, mods, key.vk).is_err() {
                failed |= 1 << idx;
                warn!("RegisterHotKey failed for {}", key.label());
            }
        }
    }
    HK_FAILED.store(failed, Ordering::Relaxed);
}

#[cfg(windows)]
fn input_hook_thread(state: Arc<AppState>, mode: HookMode) {
    use win32::*;

    virtual_desktop::init_thread();
    let _ = GLOBAL_STATE.set(state.clone());
    HOOK_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);

    unsafe {
        let hmod = GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default();
        let (kb_hook, ms_hook) = if mode == HookMode::Full {
            (
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), Some(hmod), 0).ok(),
                SetWindowsHookExW(WH_MOUSE_LL, Some(ms_proc), Some(hmod), 0).ok(),
            )
        } else {
            (None, None)
        };

        register_hotkeys();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            match msg.message {
                WM_HOTKEY_ID => match msg.wParam.0 as i32 {
                    HK_ID_RECORD => toggle_recording(&state),
                    HK_ID_PLAY => toggle_playback(&state),
                    HK_ID_STOP => stop_everything(&state),
                    _ => {}
                },
                WM_APP_REHOTKEY => register_hotkeys(),
                _ => {
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
            }
        }

        for id in [HK_ID_RECORD, HK_ID_PLAY, HK_ID_STOP] {
            let _ = UnregisterHotKey(None, id);
        }
        if let Some(h) = kb_hook {
            let _ = UnhookWindowsHookEx(h);
        }
        if let Some(h) = ms_hook {
            let _ = UnhookWindowsHookEx(h);
        }
    }
    info!("hook thread exited");
}

/// True when the hooks should currently record.
///
/// Cheap by construction: one atomic load plus a cached desktop check (B4).
#[cfg(windows)]
fn should_record() -> Option<&'static Arc<AppState>> {
    let state = GLOBAL_STATE.get()?;
    if !state.recording.load(Ordering::Relaxed) {
        return None;
    }
    if !virtual_desktop::is_app_on_active_desktop_cached(platform::app_hwnd()) {
        return None;
    }
    Some(state)
}

#[cfg(windows)]
unsafe extern "system" fn kb_proc(code: i32, wp: win32::WPARAM, lp: win32::LPARAM) -> win32::LRESULT {
    use win32::*;
    if code == 0 && lp.0 != 0 {
        if let Some(state) = should_record() {
            unsafe {
                let data = &*(lp.0 as *const KBDLLHOOKSTRUCT);
                if data.flags.0 & LLKHF_INJECTED.0 == 0 && !is_hotkey_vk(data.vkCode) {
                    let (down, valid) = match wp.0 as u32 {
                        0x0100 | 0x0104 => (true, true),  // WM_KEYDOWN / WM_SYSKEYDOWN
                        0x0101 | 0x0105 => (false, true), // WM_KEYUP / WM_SYSKEYUP
                        _ => (false, false),
                    };
                    if valid {
                        emit_event(
                            state,
                            InputEventKind::Key {
                                vk: data.vkCode as u16,
                                scan: data.scanCode as u16,
                                down,
                                extended: data.flags.0 & LLKHF_EXTENDED.0 != 0,
                            },
                        );
                    }
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

#[cfg(windows)]
unsafe extern "system" fn ms_proc(code: i32, wp: win32::WPARAM, lp: win32::LPARAM) -> win32::LRESULT {
    use win32::*;
    if code == 0 && lp.0 != 0 {
        if let Some(state) = should_record() {
            unsafe {
                let data = &*(lp.0 as *const MSLLHOOKSTRUCT);
                if data.flags & LLMHF_INJECTED == 0 {
                    let (x, y) = (data.pt.x, data.pt.y);
                    let kind = match wp.0 as u32 {
                        0x0200 => {
                            // WM_MOUSEMOVE - throttled to the configured sample interval.
                            if !state.capture_mouse_moves.load(Ordering::Relaxed) {
                                None
                            } else {
                                let now = current_rec_time_us(state);
                                let last = state.last_move_us.load(Ordering::Relaxed);
                                let step = state.mouse_sample_us.load(Ordering::Relaxed);
                                if last == 0 || now.saturating_sub(last) >= step {
                                    state.last_move_us.store(now, Ordering::Relaxed);
                                    let mut lx = state.last_x.lock();
                                    let mut ly = state.last_y.lock();
                                    let (dx, dy) =
                                        if *lx == i32::MIN { (0, 0) } else { (x - *lx, y - *ly) };
                                    *lx = x;
                                    *ly = y;
                                    Some(InputEventKind::MouseMove { x, y, dx, dy })
                                } else {
                                    None
                                }
                            }
                        }
                        0x0201 => Some(InputEventKind::MouseButton {
                            button: MouseButton::Left,
                            down: true,
                            x,
                            y,
                        }),
                        0x0202 => Some(InputEventKind::MouseButton {
                            button: MouseButton::Left,
                            down: false,
                            x,
                            y,
                        }),
                        0x0204 => Some(InputEventKind::MouseButton {
                            button: MouseButton::Right,
                            down: true,
                            x,
                            y,
                        }),
                        0x0205 => Some(InputEventKind::MouseButton {
                            button: MouseButton::Right,
                            down: false,
                            x,
                            y,
                        }),
                        0x0207 => Some(InputEventKind::MouseButton {
                            button: MouseButton::Middle,
                            down: true,
                            x,
                            y,
                        }),
                        0x0208 => Some(InputEventKind::MouseButton {
                            button: MouseButton::Middle,
                            down: false,
                            x,
                            y,
                        }),
                        // WM_XBUTTONDOWN / WM_XBUTTONUP (F1): the button index lives
                        // in the high word of mouseData.
                        0x020B | 0x020C => {
                            let down = wp.0 as u32 == 0x020B;
                            let which = (data.mouseData >> 16) & 0xFFFF;
                            let button = if which == 2 { MouseButton::X2 } else { MouseButton::X1 };
                            Some(InputEventKind::MouseButton { button, down, x, y })
                        }
                        0x020A => {
                            let delta = ((data.mouseData >> 16) & 0xFFFF) as i16 as i32;
                            Some(InputEventKind::MouseWheel { delta, x, y, horizontal: false })
                        }
                        0x020E => {
                            let delta = ((data.mouseData >> 16) & 0xFFFF) as i16 as i32;
                            Some(InputEventKind::MouseWheel { delta, x, y, horizontal: true })
                        }
                        _ => None,
                    };
                    if let Some(k) = kind {
                        emit_event(state, k);
                    }
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

// ============================================================================
// Localization
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    En,
    Ru,
    Uk,
    Pt,
    Es,
    Zh,
}

struct Strings {
    // transport
    record: &'static str,
    stop_rec: &'static str,
    play: &'static str,
    pause: &'static str,
    resume: &'static str,
    stop_play: &'static str,
    // status
    rec_time: &'static str,
    rec_done: &'static str,
    play_inf: &'static str,
    play_lim: &'static str,
    events: &'static str,
    duration: &'static str,
    status_ready: &'static str,
    status_rec: &'static str,
    status_play: &'static str,
    status_paused: &'static str,
    status_held: &'static str,
    // sections
    sec_playback: &'static str,
    sec_recording: &'static str,
    sec_limit: &'static str,
    sec_appearance: &'static str,
    sec_hotkeys: &'static str,
    sec_files: &'static str,
    // playback settings
    loop_cb: &'static str,
    play_count: &'static str,
    speed: &'static str,
    repeat_delay: &'static str,
    jitter: &'static str,
    abs_mouse: &'static str,
    // recording settings
    capture_moves: &'static str,
    sample_rate: &'static str,
    // limit
    time_limit_cb: &'static str,
    time_limit_h: &'static str,
    time_limit_m: &'static str,
    time_limit_s: &'static str,
    action_on_limit: &'static str,
    action_stop: &'static str,
    action_shutdown: &'static str,
    action_reboot: &'static str,
    action_sleep: &'static str,
    action_hibernate: &'static str,
    action_logoff: &'static str,
    shutdown_delay: &'static str,
    // appearance
    theme: &'static str,
    language: &'static str,
    lang_auto: &'static str,
    transparent_ui: &'static str,
    on_top: &'static str,
    // hotkeys
    hk_record: &'static str,
    hk_play: &'static str,
    hk_stop: &'static str,
    hk_failed: &'static str,
    // files
    save: &'static str,
    save_as: &'static str,
    load: &'static str,
    open_file: &'static str,
    clear: &'static str,
    recent: &'static str,
    compress: &'static str,
    data_dir: &'static str,
    save_settings: &'static str,
    // messages
    saved: &'static str,
    loaded: &'static str,
    cleared: &'static str,
    settings_saved: &'static str,
    save_err: &'static str,
    load_err: &'static str,
    no_macro: &'static str,
}

const EN: Strings = Strings {
    record: "🔴 Record",
    stop_rec: "⏹ Stop Rec",
    play: "▶ Play",
    pause: "⏸ Pause",
    resume: "⏵ Resume",
    stop_play: "⏹ Stop Play",
    rec_time: "⏱ Recording: {}…",
    rec_done: "⏱ Recorded: {} (done)",
    play_inf: "🔄 Plays: {} (∞)",
    play_lim: "🔄 Plays: {} / {}",
    events: "📦 Events: {}",
    duration: "⏳ Length: {}",
    status_ready: "Ready",
    status_rec: "Recording…",
    status_play: "Playing…",
    status_paused: "Paused",
    status_held: "Held — app is on another virtual desktop",
    sec_playback: "▶ Playback",
    sec_recording: "🎬 Recording",
    sec_limit: "⏱ Time limit",
    sec_appearance: "🎨 Appearance",
    sec_hotkeys: "⌨ Hotkeys",
    sec_files: "📁 Files",
    loop_cb: "Loop playback",
    play_count: "Play count:",
    speed: "Speed",
    repeat_delay: "Delay between loops (ms)",
    jitter: "Timing jitter (%)",
    abs_mouse: "Absolute mouse (DPI fix)",
    capture_moves: "Capture mouse movement",
    sample_rate: "Movement sampling (ms)",
    time_limit_cb: "Stop after time limit",
    time_limit_h: "H",
    time_limit_m: "M",
    time_limit_s: "S",
    action_on_limit: "Action on limit:",
    action_stop: "Stop",
    action_shutdown: "Shut down",
    action_reboot: "Restart",
    action_sleep: "Sleep",
    action_hibernate: "Hibernate",
    action_logoff: "Log off",
    shutdown_delay: "Shutdown countdown (s)",
    theme: "Theme:",
    language: "Language:",
    lang_auto: "Auto (system)",
    transparent_ui: "🪟 Transparent UI",
    on_top: "📌 Always on Top",
    hk_record: "Record:",
    hk_play: "Play / Stop:",
    hk_stop: "Emergency stop:",
    hk_failed: "⚠ Some hotkeys are taken by another app",
    save: "💾 Save",
    save_as: "💾 Save as…",
    load: "📂 Load",
    open_file: "📂 Open…",
    clear: "🗑 Clear",
    recent: "Recent:",
    compress: "Compress saved macros (.mrz)",
    data_dir: "📁 Data folder:",
    save_settings: "💾 Save Settings",
    saved: "Saved: {}",
    loaded: "Loaded: {}",
    cleared: "Macro cleared",
    settings_saved: "Settings saved",
    save_err: "Save error: {}",
    load_err: "Load error: {}",
    no_macro: "No macro loaded",
};

const RU: Strings = Strings {
    record: "🔴 Запись",
    stop_rec: "⏹ Стоп запись",
    play: "▶ Воспроизвести",
    pause: "⏸ Пауза",
    resume: "⏵ Продолжить",
    stop_play: "⏹ Стоп",
    rec_time: "⏱ Запись: {}…",
    rec_done: "⏱ Записано: {} (готово)",
    play_inf: "🔄 Проигрываний: {} (∞)",
    play_lim: "🔄 Проигрываний: {} / {}",
    events: "📦 Событий: {}",
    duration: "⏳ Длительность: {}",
    status_ready: "Готов",
    status_rec: "Идёт запись…",
    status_play: "Воспроизведение…",
    status_paused: "Пауза",
    status_held: "Удержание — окно на другом рабочем столе",
    sec_playback: "▶ Воспроизведение",
    sec_recording: "🎬 Запись",
    sec_limit: "⏱ Лимит времени",
    sec_appearance: "🎨 Оформление",
    sec_hotkeys: "⌨ Горячие клавиши",
    sec_files: "📁 Файлы",
    loop_cb: "Циклическое воспроизведение",
    play_count: "Проигрываний:",
    speed: "Скорость",
    repeat_delay: "Пауза между циклами (мс)",
    jitter: "Джиттер таймингов (%)",
    abs_mouse: "Абсолютная мышь (фикс DPI)",
    capture_moves: "Записывать движения мыши",
    sample_rate: "Шаг выборки движений (мс)",
    time_limit_cb: "Остановиться по таймеру",
    time_limit_h: "Ч",
    time_limit_m: "М",
    time_limit_s: "С",
    action_on_limit: "Действие по таймеру:",
    action_stop: "Остановить",
    action_shutdown: "Выключить",
    action_reboot: "Перезагрузить",
    action_sleep: "Сон",
    action_hibernate: "Гибернация",
    action_logoff: "Выйти из системы",
    shutdown_delay: "Отсчёт до выключения (с)",
    theme: "Тема:",
    language: "Язык:",
    lang_auto: "Авто (система)",
    transparent_ui: "🪟 Прозрачный интерфейс",
    on_top: "📌 Поверх всех окон",
    hk_record: "Запись:",
    hk_play: "Плей / стоп:",
    hk_stop: "Аварийный стоп:",
    hk_failed: "⚠ Часть клавиш занята другой программой",
    save: "💾 Сохранить",
    save_as: "💾 Сохранить как…",
    load: "📂 Загрузить",
    open_file: "📂 Открыть…",
    clear: "🗑 Очистить",
    recent: "Недавние:",
    compress: "Сжимать макросы (.mrz)",
    data_dir: "📁 Папка данных:",
    save_settings: "💾 Сохранить настройки",
    saved: "Сохранено: {}",
    loaded: "Загружено: {}",
    cleared: "Макрос очищен",
    settings_saved: "Настройки сохранены",
    save_err: "Ошибка сохранения: {}",
    load_err: "Ошибка загрузки: {}",
    no_macro: "Макрос не загружен",
};

const UK: Strings = Strings {
    record: "🔴 Запис",
    stop_rec: "⏹ Стоп запис",
    play: "▶ Відтворити",
    pause: "⏸ Пауза",
    resume: "⏵ Продовжити",
    stop_play: "⏹ Стоп",
    rec_time: "⏱ Запис: {}…",
    rec_done: "⏱ Записано: {} (готово)",
    play_inf: "🔄 Відтворень: {} (∞)",
    play_lim: "🔄 Відтворень: {} / {}",
    events: "📦 Подій: {}",
    duration: "⏳ Тривалість: {}",
    status_ready: "Готово",
    status_rec: "Триває запис…",
    status_play: "Відтворення…",
    status_paused: "Пауза",
    status_held: "Утримання — вікно на іншому робочому столі",
    sec_playback: "▶ Відтворення",
    sec_recording: "🎬 Запис",
    sec_limit: "⏱ Ліміт часу",
    sec_appearance: "🎨 Оформлення",
    sec_hotkeys: "⌨ Гарячі клавіші",
    sec_files: "📁 Файли",
    loop_cb: "Циклічне відтворення",
    play_count: "Відтворень:",
    speed: "Швидкість",
    repeat_delay: "Пауза між циклами (мс)",
    jitter: "Джитер таймінгів (%)",
    abs_mouse: "Абсолютна миша (фікс DPI)",
    capture_moves: "Записувати рухи миші",
    sample_rate: "Крок вибірки рухів (мс)",
    time_limit_cb: "Зупинитися за таймером",
    time_limit_h: "Г",
    time_limit_m: "Х",
    time_limit_s: "С",
    action_on_limit: "Дія за таймером:",
    action_stop: "Зупинити",
    action_shutdown: "Вимкнути",
    action_reboot: "Перезавантажити",
    action_sleep: "Сон",
    action_hibernate: "Гібернація",
    action_logoff: "Вийти з системи",
    shutdown_delay: "Відлік до вимкнення (с)",
    theme: "Тема:",
    language: "Мова:",
    lang_auto: "Авто (система)",
    transparent_ui: "🪟 Прозорий інтерфейс",
    on_top: "📌 Завжди поверх вікон",
    hk_record: "Запис:",
    hk_play: "Плей / стоп:",
    hk_stop: "Аварійний стоп:",
    hk_failed: "⚠ Частину клавіш зайнято іншою програмою",
    save: "💾 Зберегти",
    save_as: "💾 Зберегти як…",
    load: "📂 Завантажити",
    open_file: "📂 Відкрити…",
    clear: "🗑 Очистити",
    recent: "Нещодавні:",
    compress: "Стискати макроси (.mrz)",
    data_dir: "📁 Тека даних:",
    save_settings: "💾 Зберегти налаштування",
    saved: "Збережено: {}",
    loaded: "Завантажено: {}",
    cleared: "Макрос очищено",
    settings_saved: "Налаштування збережено",
    save_err: "Помилка збереження: {}",
    load_err: "Помилка завантаження: {}",
    no_macro: "Макрос не завантажено",
};

const PT: Strings = Strings {
    record: "🔴 Gravar",
    stop_rec: "⏹ Parar Grav",
    play: "▶ Tocar",
    pause: "⏸ Pausar",
    resume: "⏵ Retomar",
    stop_play: "⏹ Parar",
    rec_time: "⏱ Gravando: {}…",
    rec_done: "⏱ Gravado: {} (pronto)",
    play_inf: "🔄 Reproduções: {} (∞)",
    play_lim: "🔄 Reproduções: {} / {}",
    events: "📦 Eventos: {}",
    duration: "⏳ Duração: {}",
    status_ready: "Pronto",
    status_rec: "Gravando…",
    status_play: "Reproduzindo…",
    status_paused: "Pausado",
    status_held: "Em espera — janela em outra área de trabalho",
    sec_playback: "▶ Reprodução",
    sec_recording: "🎬 Gravação",
    sec_limit: "⏱ Limite de tempo",
    sec_appearance: "🎨 Aparência",
    sec_hotkeys: "⌨ Atalhos",
    sec_files: "📁 Arquivos",
    loop_cb: "Reprodução em loop",
    play_count: "Contagem:",
    speed: "Velocidade",
    repeat_delay: "Pausa entre loops (ms)",
    jitter: "Variação de tempo (%)",
    abs_mouse: "Mouse absoluto (fix DPI)",
    capture_moves: "Gravar movimento do mouse",
    sample_rate: "Amostragem de movimento (ms)",
    time_limit_cb: "Parar após o limite",
    time_limit_h: "H",
    time_limit_m: "M",
    time_limit_s: "S",
    action_on_limit: "Ação no limite:",
    action_stop: "Parar",
    action_shutdown: "Desligar",
    action_reboot: "Reiniciar",
    action_sleep: "Suspender",
    action_hibernate: "Hibernar",
    action_logoff: "Sair da sessão",
    shutdown_delay: "Contagem para desligar (s)",
    theme: "Tema:",
    language: "Idioma:",
    lang_auto: "Auto (sistema)",
    transparent_ui: "🪟 Interface transparente",
    on_top: "📌 Sempre no topo",
    hk_record: "Gravar:",
    hk_play: "Tocar / parar:",
    hk_stop: "Parada de emergência:",
    hk_failed: "⚠ Alguns atalhos estão ocupados",
    save: "💾 Salvar",
    save_as: "💾 Salvar como…",
    load: "📂 Carregar",
    open_file: "📂 Abrir…",
    clear: "🗑 Limpar",
    recent: "Recentes:",
    compress: "Comprimir macros (.mrz)",
    data_dir: "📁 Pasta de dados:",
    save_settings: "💾 Salvar configurações",
    saved: "Salvo: {}",
    loaded: "Carregado: {}",
    cleared: "Macro limpo",
    settings_saved: "Configurações salvas",
    save_err: "Erro ao salvar: {}",
    load_err: "Erro ao carregar: {}",
    no_macro: "Nenhum macro carregado",
};

const ES: Strings = Strings {
    record: "🔴 Grabar",
    stop_rec: "⏹ Detener grab",
    play: "▶ Reproducir",
    pause: "⏸ Pausar",
    resume: "⏵ Reanudar",
    stop_play: "⏹ Detener",
    rec_time: "⏱ Grabando: {}…",
    rec_done: "⏱ Grabado: {} (listo)",
    play_inf: "🔄 Reproducciones: {} (∞)",
    play_lim: "🔄 Reproducciones: {} / {}",
    events: "📦 Eventos: {}",
    duration: "⏳ Duración: {}",
    status_ready: "Listo",
    status_rec: "Grabando…",
    status_play: "Reproduciendo…",
    status_paused: "En pausa",
    status_held: "En espera — ventana en otro escritorio",
    sec_playback: "▶ Reproducción",
    sec_recording: "🎬 Grabación",
    sec_limit: "⏱ Límite de tiempo",
    sec_appearance: "🎨 Apariencia",
    sec_hotkeys: "⌨ Atajos",
    sec_files: "📁 Archivos",
    loop_cb: "Reproducción en bucle",
    play_count: "Repeticiones:",
    speed: "Velocidad",
    repeat_delay: "Pausa entre bucles (ms)",
    jitter: "Variación de tiempo (%)",
    abs_mouse: "Ratón absoluto (fijo DPI)",
    capture_moves: "Grabar movimiento del ratón",
    sample_rate: "Muestreo de movimiento (ms)",
    time_limit_cb: "Detener tras el límite",
    time_limit_h: "H",
    time_limit_m: "M",
    time_limit_s: "S",
    action_on_limit: "Acción al límite:",
    action_stop: "Detener",
    action_shutdown: "Apagar",
    action_reboot: "Reiniciar",
    action_sleep: "Suspender",
    action_hibernate: "Hibernar",
    action_logoff: "Cerrar sesión",
    shutdown_delay: "Cuenta atrás de apagado (s)",
    theme: "Tema:",
    language: "Idioma:",
    lang_auto: "Auto (sistema)",
    transparent_ui: "🪟 Interfaz transparente",
    on_top: "📌 Siempre encima",
    hk_record: "Grabar:",
    hk_play: "Reproducir / detener:",
    hk_stop: "Parada de emergencia:",
    hk_failed: "⚠ Algunos atajos están ocupados",
    save: "💾 Guardar",
    save_as: "💾 Guardar como…",
    load: "📂 Cargar",
    open_file: "📂 Abrir…",
    clear: "🗑 Limpiar",
    recent: "Recientes:",
    compress: "Comprimir macros (.mrz)",
    data_dir: "📁 Carpeta de datos:",
    save_settings: "💾 Guardar ajustes",
    saved: "Guardado: {}",
    loaded: "Cargado: {}",
    cleared: "Macro borrado",
    settings_saved: "Ajustes guardados",
    save_err: "Error al guardar: {}",
    load_err: "Error al cargar: {}",
    no_macro: "Ningún macro cargado",
};

const ZH: Strings = Strings {
    record: "🔴 录制",
    stop_rec: "⏹ 停止录制",
    play: "▶ 播放",
    pause: "⏸ 暂停",
    resume: "⏵ 继续",
    stop_play: "⏹ 停止",
    rec_time: "⏱ 录制中: {}…",
    rec_done: "⏱ 已录制: {} (完成)",
    play_inf: "🔄 播放次数: {} (∞)",
    play_lim: "🔄 播放次数: {} / {}",
    events: "📦 事件: {}",
    duration: "⏳ 时长: {}",
    status_ready: "就绪",
    status_rec: "录制中…",
    status_play: "播放中…",
    status_paused: "已暂停",
    status_held: "已挂起 — 窗口在其他虚拟桌面",
    sec_playback: "▶ 播放",
    sec_recording: "🎬 录制",
    sec_limit: "⏱ 时间限制",
    sec_appearance: "🎨 外观",
    sec_hotkeys: "⌨ 快捷键",
    sec_files: "📁 文件",
    loop_cb: "循环播放",
    play_count: "播放次数:",
    speed: "速度",
    repeat_delay: "循环间隔 (毫秒)",
    jitter: "时间抖动 (%)",
    abs_mouse: "绝对鼠标 (修复DPI)",
    capture_moves: "记录鼠标移动",
    sample_rate: "移动采样 (毫秒)",
    time_limit_cb: "到达时限后停止",
    time_limit_h: "时",
    time_limit_m: "分",
    time_limit_s: "秒",
    action_on_limit: "到达时限操作:",
    action_stop: "停止",
    action_shutdown: "关机",
    action_reboot: "重启",
    action_sleep: "睡眠",
    action_hibernate: "休眠",
    action_logoff: "注销",
    shutdown_delay: "关机倒计时 (秒)",
    theme: "主题:",
    language: "语言:",
    lang_auto: "自动 (系统)",
    transparent_ui: "🪟 透明界面",
    on_top: "📌 始终置顶",
    hk_record: "录制:",
    hk_play: "播放 / 停止:",
    hk_stop: "紧急停止:",
    hk_failed: "⚠ 部分快捷键被其他程序占用",
    save: "💾 保存",
    save_as: "💾 另存为…",
    load: "📂 加载",
    open_file: "📂 打开…",
    clear: "🗑 清空",
    recent: "最近:",
    compress: "压缩保存 (.mrz)",
    data_dir: "📁 数据目录:",
    save_settings: "💾 保存设置",
    saved: "已保存: {}",
    loaded: "已加载: {}",
    cleared: "宏已清空",
    settings_saved: "设置已保存",
    save_err: "保存错误: {}",
    load_err: "加载错误: {}",
    no_macro: "未加载宏",
};

fn detect_system_lang() -> Lang {
    #[cfg(windows)]
    unsafe {
        let lang = win32::GetUserDefaultUILanguage() as u32;
        match lang & 0x3FF {
            0x19 => Lang::Ru,
            0x22 => Lang::Uk,
            0x16 => Lang::Pt,
            0x0A => Lang::Es,
            0x04 => Lang::Zh,
            _ => Lang::En,
        }
    }
    #[cfg(not(windows))]
    Lang::En
}

fn get_strings(lang_mode: usize, system_lang: Lang) -> &'static Strings {
    let lang = match lang_mode {
        1 => Lang::En,
        2 => Lang::Ru,
        3 => Lang::Uk,
        4 => Lang::Pt,
        5 => Lang::Es,
        6 => Lang::Zh,
        _ => system_lang,
    };
    match lang {
        Lang::Ru => &RU,
        Lang::Uk => &UK,
        Lang::Pt => &PT,
        Lang::Es => &ES,
        Lang::Zh => &ZH,
        Lang::En => &EN,
    }
}

// ============================================================================
// Theme system
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum Theme {
    Dark,
    Oled,
    Material3,
    Catppuccin,
    Nord,
    Dracula,
    Glass,
    Neumorphism,
    Fluent,
}

const THEMES: [Theme; 9] = [
    Theme::Dark,
    Theme::Oled,
    Theme::Material3,
    Theme::Catppuccin,
    Theme::Nord,
    Theme::Dracula,
    Theme::Glass,
    Theme::Neumorphism,
    Theme::Fluent,
];

const THEME_NAMES: [&str; 9] = [
    "Dark (default)",
    "OLED (Pure Black)",
    "Material Design 3",
    "Catppuccin Mocha",
    "Nord",
    "Dracula",
    "Glassmorphism (Acrylic)",
    "Neumorphism",
    "Fluent (Mica)",
];

fn theme_at(index: usize) -> Theme {
    THEMES.get(index).copied().unwrap_or(Theme::Dark)
}

struct Palette {
    dark: bool,
    bg: egui::Color32,
    panel: egui::Color32,
    widget: egui::Color32,
    widget_hover: egui::Color32,
    widget_active: egui::Color32,
    active_fg: egui::Color32,
    border: egui::Color32,
    hover_border: egui::Color32,
    text: egui::Color32,
    faint: egui::Color32,
    accent: egui::Color32,
    focus_border: egui::Color32,
    widget_round: f32,
    shadow_blur: u8,
    shadow_offset: i8,
    shadow_alpha: u8,
    item_spacing_y: f32,
    button_padding: f32,
    animation_time: f32,
    /// DWMWA_SYSTEMBACKDROP_TYPE: 1 = none, 2 = Mica, 3 = Acrylic.
    backdrop: i32,
}

fn rgb(r: u8, g: u8, b: u8) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

#[cfg(windows)]
fn get_system_accent_color() -> Option<egui::Color32> {
    use win32::*;
    unsafe {
        let mut key = HKEY::default();
        let path = windows::core::w!("Software\\Microsoft\\Windows\\DWM");
        if RegOpenKeyExW(HKEY_CURRENT_USER, path, Some(0), KEY_READ, &mut key).is_err() {
            return None;
        }
        let mut data: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let res = RegQueryValueExW(
            key,
            windows::core::w!("AccentColor"),
            None,
            None,
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if res.is_ok() {
            // AccentColor is stored as ABGR.
            return Some(egui::Color32::from_rgb(
                (data & 0xFF) as u8,
                ((data >> 8) & 0xFF) as u8,
                ((data >> 16) & 0xFF) as u8,
            ));
        }
        None
    }
}

#[cfg(not(windows))]
fn get_system_accent_color() -> Option<egui::Color32> {
    None
}

fn get_palette(theme: Theme) -> Palette {
    match theme {
        Theme::Dark => Palette {
            dark: true, bg: rgb(16, 16, 16), panel: rgb(24, 24, 24), widget: rgb(42, 42, 42),
            widget_hover: rgb(58, 58, 58), widget_active: rgb(75, 75, 75), active_fg: rgb(255, 255, 255),
            border: rgb(70, 70, 70), hover_border: rgb(95, 95, 95), text: rgb(230, 230, 230),
            faint: rgb(130, 130, 130), accent: rgb(70, 130, 255), focus_border: rgb(0, 200, 255),
            widget_round: 4.0, shadow_blur: 4, shadow_offset: 1, shadow_alpha: 60,
            item_spacing_y: 5.0, button_padding: 3.0, animation_time: 0.15, backdrop: 1,
        },
        Theme::Oled => Palette {
            dark: true, bg: rgb(0, 0, 0), panel: rgb(0, 0, 0), widget: rgb(20, 20, 20),
            widget_hover: rgb(35, 35, 35), widget_active: rgb(50, 50, 50), active_fg: rgb(255, 255, 255),
            border: rgb(40, 40, 40), hover_border: rgb(80, 80, 80), text: rgb(240, 240, 240),
            faint: rgb(120, 120, 120), accent: rgb(0, 122, 204), focus_border: rgb(0, 255, 255),
            widget_round: 2.0, shadow_blur: 0, shadow_offset: 0, shadow_alpha: 0,
            item_spacing_y: 5.0, button_padding: 3.0, animation_time: 0.1, backdrop: 1,
        },
        Theme::Material3 => {
            let accent = get_system_accent_color().unwrap_or_else(|| rgb(208, 188, 255));
            Palette {
                dark: true, bg: rgb(18, 17, 24), panel: rgb(24, 23, 31), widget: rgb(32, 31, 42),
                widget_hover: rgb(40, 39, 52), widget_active: accent, active_fg: rgb(255, 255, 255),
                border: rgb(73, 69, 82), hover_border: accent, text: rgb(230, 224, 233),
                faint: rgb(147, 143, 153), accent, focus_border: rgb(255, 255, 0),
                widget_round: 20.0, shadow_blur: 0, shadow_offset: 0, shadow_alpha: 0,
                item_spacing_y: 7.0, button_padding: 6.0, animation_time: 0.4, backdrop: 1,
            }
        }
        Theme::Catppuccin => Palette {
            dark: true, bg: rgb(17, 17, 27), panel: rgb(30, 30, 46), widget: rgb(49, 50, 68),
            widget_hover: rgb(69, 71, 90), widget_active: rgb(203, 166, 247), active_fg: rgb(17, 17, 27),
            border: rgb(88, 91, 112), hover_border: rgb(203, 166, 247), text: rgb(205, 214, 244),
            faint: rgb(166, 172, 200), accent: rgb(203, 166, 247), focus_border: rgb(250, 178, 102),
            widget_round: 10.0, shadow_blur: 6, shadow_offset: 2, shadow_alpha: 90,
            item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.25, backdrop: 1,
        },
        Theme::Nord => Palette {
            dark: true, bg: rgb(46, 52, 64), panel: rgb(46, 52, 64), widget: rgb(59, 66, 82),
            widget_hover: rgb(67, 76, 94), widget_active: rgb(136, 192, 208), active_fg: rgb(46, 52, 64),
            border: rgb(76, 86, 106), hover_border: rgb(136, 192, 208), text: rgb(216, 222, 233),
            faint: rgb(148, 155, 168), accent: rgb(136, 192, 208), focus_border: rgb(143, 188, 187),
            widget_round: 6.0, shadow_blur: 5, shadow_offset: 1, shadow_alpha: 80,
            item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.2, backdrop: 1,
        },
        Theme::Dracula => Palette {
            dark: true, bg: rgb(40, 42, 54), panel: rgb(40, 42, 54), widget: rgb(68, 71, 90),
            widget_hover: rgb(80, 83, 105), widget_active: rgb(255, 121, 198), active_fg: rgb(40, 42, 54),
            border: rgb(98, 114, 164), hover_border: rgb(255, 121, 198), text: rgb(248, 248, 242),
            faint: rgb(135, 140, 160), accent: rgb(255, 121, 198), focus_border: rgb(189, 147, 249),
            widget_round: 8.0, shadow_blur: 6, shadow_offset: 2, shadow_alpha: 90,
            item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.25, backdrop: 1,
        },
        Theme::Glass => Palette {
            dark: true, bg: rgb(24, 28, 40), panel: rgba(40, 46, 64, 110), widget: rgba(255, 255, 255, 45),
            widget_hover: rgba(255, 255, 255, 75), widget_active: rgba(120, 180, 255, 200),
            active_fg: rgb(255, 255, 255), border: rgba(255, 255, 255, 110),
            hover_border: rgba(255, 255, 255, 170), text: rgb(240, 245, 255), faint: rgb(190, 200, 220),
            accent: rgb(120, 180, 255), focus_border: rgb(255, 255, 255),
            widget_round: 14.0, shadow_blur: 12, shadow_offset: 3, shadow_alpha: 100,
            item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.3, backdrop: 3,
        },
        Theme::Neumorphism => Palette {
            dark: false, bg: rgb(224, 229, 236), panel: rgb(224, 229, 236), widget: rgb(224, 229, 236),
            widget_hover: rgb(231, 236, 243), widget_active: rgb(93, 120, 255), active_fg: rgb(255, 255, 255),
            border: rgb(224, 229, 236), hover_border: rgb(224, 229, 236), text: rgb(60, 70, 90),
            faint: rgb(120, 130, 150), accent: rgb(93, 120, 255), focus_border: rgb(255, 100, 100),
            widget_round: 12.0, shadow_blur: 10, shadow_offset: 5, shadow_alpha: 110,
            item_spacing_y: 6.0, button_padding: 5.0, animation_time: 0.25, backdrop: 1,
        },
        // Mica finally wired up to a theme of its own.
        Theme::Fluent => {
            let accent = get_system_accent_color().unwrap_or_else(|| rgb(76, 156, 255));
            Palette {
                dark: true, bg: rgb(32, 32, 32), panel: rgba(43, 43, 43, 150), widget: rgba(255, 255, 255, 22),
                widget_hover: rgba(255, 255, 255, 38), widget_active: accent, active_fg: rgb(255, 255, 255),
                border: rgba(255, 255, 255, 40), hover_border: accent, text: rgb(240, 240, 240),
                faint: rgb(165, 165, 165), accent, focus_border: accent,
                widget_round: 7.0, shadow_blur: 0, shadow_offset: 0, shadow_alpha: 0,
                item_spacing_y: 6.0, button_padding: 5.0, animation_time: 0.2, backdrop: 2,
            }
        }
    }
}

fn make_shadow(p: &Palette) -> egui::Shadow {
    egui::Shadow {
        offset: [p.shadow_offset, p.shadow_offset],
        blur: p.shadow_blur,
        spread: 0,
        color: egui::Color32::from_black_alpha(p.shadow_alpha),
    }
}

fn apply_theme(ctx: &egui::Context, theme: Theme, transparent_ui: bool) {
    let p = get_palette(theme);
    let mut visuals = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };

    visuals.window_fill = p.panel;
    visuals.panel_fill = p.panel;
    visuals.extreme_bg_color = p.bg;
    visuals.window_shadow = make_shadow(&p);
    visuals.popup_shadow = make_shadow(&p);
    visuals.hyperlink_color = p.accent;
    visuals.selection.bg_fill = p.accent;
    visuals.selection.stroke = egui::Stroke::new(2.0, p.focus_border);

    let states = [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
    ];
    for w in states {
        w.corner_radius = p.widget_round.into();
        w.bg_stroke = egui::Stroke::new(1.0, p.border);
        w.fg_stroke = egui::Stroke::new(1.0, p.text);
    }
    visuals.widgets.noninteractive.bg_fill = p.panel;
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.faint);
    visuals.widgets.inactive.bg_fill = p.widget;
    visuals.widgets.hovered.bg_fill = p.widget_hover;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.5, p.hover_border);
    visuals.widgets.active.bg_fill = p.widget_active;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, p.active_fg);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(2.0, p.focus_border);

    let translucent = transparent_ui || p.backdrop > 1;
    if translucent {
        visuals.window_fill = egui::Color32::TRANSPARENT;
        visuals.panel_fill = if p.backdrop > 1 { p.panel } else { rgba(30, 30, 30, 140) };
        visuals.extreme_bg_color = egui::Color32::TRANSPARENT;
        visuals.window_shadow = egui::Shadow::NONE;
        visuals.popup_shadow = egui::Shadow::NONE;
    }

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = visuals;
    style.animation_time = p.animation_time;
    style.spacing.item_spacing = egui::vec2(8.0, p.item_spacing_y);
    style.spacing.button_padding = egui::vec2(p.button_padding, p.button_padding);

    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);

    // Always push the backdrop (1 = none), so switching *away* from Glass/Fluent
    // actually removes the system effect instead of leaving it behind.
    #[cfg(windows)]
    platform::apply_system_backdrop(platform::app_hwnd(), p.backdrop);
}

// ============================================================================
// Application
// ============================================================================

struct MacroApp {
    state: Arc<AppState>,
    config: AppConfig,
    system_lang: Lang,
    status_msg: String,
    theme_dirty: bool,
}

impl MacroApp {
    fn new(cc: &eframe::CreationContext<'_>, state: Arc<AppState>, config: AppConfig) -> Self {
        setup_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, theme_at(config.default_theme), config.transparent_ui);
        Self {
            state,
            config,
            system_lang: detect_system_lang(),
            status_msg: String::new(),
            theme_dirty: true,
        }
    }

    fn strs(&self) -> &'static Strings {
        get_strings(self.config.default_lang, self.system_lang)
    }

    fn sync(&self) {
        apply_config_to_state(&self.config, &self.state);
    }

    fn do_save(&mut self, path: PathBuf) {
        let data = self.state.macro_data.lock().clone();
        let s = self.strs();
        if data.is_empty() {
            self.status_msg = s.no_macro.to_string();
            return;
        }
        match save_macro(&path, &data) {
            Ok(()) => {
                self.config.push_recent(&path);
                *self.state.current_path.lock() = Some(path.clone());
                self.status_msg =
                    s.saved.replace("{}", &path.file_name().unwrap_or_default().to_string_lossy());
            }
            Err(e) => self.status_msg = s.save_err.replace("{}", &e.to_string()),
        }
    }

    fn do_load(&mut self, path: PathBuf) {
        let s = self.strs();
        match load_macro(&path) {
            Ok(data) => {
                self.state.recorded_time_us.store(data.duration_us, Ordering::Relaxed);
                *self.state.macro_data.lock() = data;
                *self.state.current_path.lock() = Some(path.clone());
                self.config.push_recent(&path);
                self.status_msg =
                    s.loaded.replace("{}", &path.file_name().unwrap_or_default().to_string_lossy());
            }
            Err(e) => self.status_msg = s.load_err.replace("{}", &e.to_string()),
        }
    }

    fn default_save_path(&self) -> PathBuf {
        self.state.current_path.lock().clone().unwrap_or_else(|| {
            if self.config.compress_on_save {
                paths::data_dir().join("macro.mrz")
            } else {
                paths::default_macro_path()
            }
        })
    }

    fn pick_open(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Macro", &["json", "mrz", "gz"])
            .set_directory(paths::data_dir())
            .pick_file()
        {
            self.do_load(path);
        }
    }

    fn pick_save(&mut self) {
        let ext = if self.config.compress_on_save { "mrz" } else { "json" };
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Macro", &["json", "mrz"])
            .set_directory(paths::data_dir())
            .set_file_name(format!("macro.{ext}"))
            .save_file()
        {
            self.do_save(path);
        }
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\meiryo.ttc",
    ];
    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert("cjk".into(), egui::FontData::from_owned(data).into());
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                f.push("cjk".into());
            }
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                f.push("cjk".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

impl eframe::App for MacroApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let s = self.strs();
        let recording = self.state.recording.load(Ordering::Relaxed);
        let playing = self.state.playing.load(Ordering::Relaxed);
        let paused = self.state.paused.load(Ordering::Relaxed);

        // The window only exists after the first frame, so the backdrop is applied once here.
        if self.theme_dirty {
            apply_theme(ui.ctx(), theme_at(self.config.default_theme), self.config.transparent_ui);
            self.theme_dirty = false;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(APP_TITLE);
                    ui.label(egui::RichText::new(format!("v{APP_VERSION}")).weak());
                });
                ui.separator();

                // ---- transport ------------------------------------------------
                ui.horizontal(|ui| {
                    let rec_label =
                        if recording { s.stop_rec } else { s.record };
                    if ui
                        .add_enabled(!playing, egui::Button::new(rec_label))
                        .clicked()
                    {
                        toggle_recording(&self.state);
                    }
                    ui.label(
                        egui::RichText::new(self.config.hotkey_record.label()).weak(),
                    );
                });

                ui.horizontal(|ui| {
                    let play_label = if playing { s.stop_play } else { s.play };
                    if ui.add_enabled(!recording, egui::Button::new(play_label)).clicked() {
                        toggle_playback(&self.state);
                    }
                    let pause_label = if paused { s.resume } else { s.pause };
                    if ui.add_enabled(playing, egui::Button::new(pause_label)).clicked() {
                        toggle_pause(&self.state);
                    }
                    ui.label(egui::RichText::new(self.config.hotkey_play.label()).weak());
                });

                // ---- live status ----------------------------------------------
                if recording {
                    ui.label(
                        s.rec_time.replace("{}", &format_us(current_rec_time_us(&self.state))),
                    );
                } else {
                    let rt = self.state.recorded_time_us.load(Ordering::Relaxed);
                    if rt > 0 {
                        ui.label(s.rec_done.replace("{}", &format_us(rt)));
                    }
                }

                if playing || self.state.play_count.load(Ordering::Relaxed) > 0 {
                    let c = self.state.play_count.load(Ordering::Relaxed);
                    if self.state.loop_play.load(Ordering::Relaxed) {
                        ui.label(s.play_inf.replace("{}", &c.to_string()));
                    } else {
                        let l = self.state.play_count_limit.load(Ordering::Relaxed);
                        ui.label(
                            s.play_lim
                                .replacen("{}", &c.to_string(), 1)
                                .replacen("{}", &l.to_string(), 1),
                        );
                    }
                }

                ui.separator();

                // ---- playback settings ----------------------------------------
                egui::CollapsingHeader::new(s.sec_playback).default_open(true).show(ui, |ui| {
                    ui.checkbox(&mut self.config.loop_play, s.loop_cb);
                    if !self.config.loop_play {
                        ui.horizontal(|ui| {
                            ui.label(s.play_count);
                            ui.add(
                                egui::DragValue::new(&mut self.config.play_count_limit)
                                    .range(1..=9999),
                            );
                        });
                    }
                    ui.add(egui::Slider::new(&mut self.config.speed, 0.1..=3.0).text(s.speed));
                    ui.horizontal(|ui| {
                        ui.label(s.repeat_delay);
                        ui.add(
                            egui::DragValue::new(&mut self.config.repeat_delay_ms)
                                .range(0..=600_000)
                                .speed(10.0),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(s.jitter);
                        ui.add(egui::DragValue::new(&mut self.config.jitter_pct).range(0..=50));
                    });
                    ui.checkbox(&mut self.config.absolute_mouse, s.abs_mouse);
                });

                // ---- recording settings ---------------------------------------
                egui::CollapsingHeader::new(s.sec_recording).show(ui, |ui| {
                    ui.checkbox(&mut self.config.capture_mouse_moves, s.capture_moves);
                    ui.horizontal(|ui| {
                        ui.label(s.sample_rate);
                        ui.add(
                            egui::DragValue::new(&mut self.config.mouse_sample_ms).range(1..=100),
                        );
                    });
                });

                // ---- time limit -----------------------------------------------
                egui::CollapsingHeader::new(s.sec_limit).show(ui, |ui| {
                    ui.checkbox(&mut self.config.time_limit_enabled, s.time_limit_cb);
                    if self.config.time_limit_enabled {
                        ui.horizontal(|ui| {
                            ui.label(s.time_limit_h);
                            ui.add(
                                egui::DragValue::new(&mut self.config.time_limit_h).range(0..=240),
                            );
                            ui.label(s.time_limit_m);
                            ui.add(
                                egui::DragValue::new(&mut self.config.time_limit_m).range(0..=59),
                            );
                            ui.label(s.time_limit_s);
                            ui.add(
                                egui::DragValue::new(&mut self.config.time_limit_s).range(0..=59),
                            );
                        });

                        let actions = [
                            s.action_stop,
                            s.action_shutdown,
                            s.action_reboot,
                            s.action_sleep,
                            s.action_hibernate,
                            s.action_logoff,
                        ];
                        ui.horizontal_wrapped(|ui| {
                            ui.label(s.action_on_limit);
                            for (i, name) in actions.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.config.action_on_completion,
                                    i,
                                    *name,
                                );
                            }
                        });
                        if matches!(self.config.action_on_completion, 1 | 2) {
                            ui.horizontal(|ui| {
                                ui.label(s.shutdown_delay);
                                ui.add(
                                    egui::DragValue::new(&mut self.config.shutdown_delay_s)
                                        .range(0..=600),
                                );
                            });
                        }
                    }
                });

                // ---- hotkeys ---------------------------------------------------
                egui::CollapsingHeader::new(s.sec_hotkeys).show(ui, |ui| {
                    let mut changed = false;
                    changed |= hotkey_row(ui, s.hk_record, "hk_rec", &mut self.config.hotkey_record);
                    changed |= hotkey_row(ui, s.hk_play, "hk_play", &mut self.config.hotkey_play);
                    changed |= hotkey_row(ui, s.hk_stop, "hk_stop", &mut self.config.hotkey_stop);
                    if changed {
                        publish_hotkeys(&self.config);
                        request_hotkey_refresh();
                    }
                    if HK_FAILED.load(Ordering::Relaxed) != 0 {
                        ui.colored_label(egui::Color32::from_rgb(255, 170, 60), s.hk_failed);
                    }
                });

                // ---- appearance ------------------------------------------------
                egui::CollapsingHeader::new(s.sec_appearance).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(s.theme);
                        egui::ComboBox::from_id_salt("theme")
                            .selected_text(THEME_NAMES[self.config.default_theme])
                            .show_ui(ui, |ui| {
                                for (i, name) in THEME_NAMES.iter().enumerate() {
                                    if ui
                                        .selectable_label(self.config.default_theme == i, *name)
                                        .clicked()
                                    {
                                        self.config.default_theme = i;
                                        self.theme_dirty = true;
                                    }
                                }
                            });
                    });

                    if ui.checkbox(&mut self.config.transparent_ui, s.transparent_ui).changed() {
                        self.theme_dirty = true;
                    }

                    if ui.checkbox(&mut self.config.always_on_top, s.on_top).changed() {
                        let level = if self.config.always_on_top {
                            egui::viewport::WindowLevel::AlwaysOnTop
                        } else {
                            egui::viewport::WindowLevel::Normal
                        };
                        ui.ctx()
                            .send_viewport_cmd(egui::viewport::ViewportCommand::WindowLevel(level));
                    }

                    ui.horizontal(|ui| {
                        ui.label(s.language);
                        egui::ComboBox::from_id_salt("lang")
                            .selected_text(match self.config.default_lang {
                                1 => "English",
                                2 => "Русский",
                                3 => "Українська",
                                4 => "Português",
                                5 => "Español",
                                6 => "中文",
                                _ => s.lang_auto,
                            })
                            .show_ui(ui, |ui| {
                                let names = [
                                    s.lang_auto,
                                    "English",
                                    "Русский",
                                    "Українська",
                                    "Português",
                                    "Español",
                                    "中文",
                                ];
                                for (i, name) in names.iter().enumerate() {
                                    if ui
                                        .selectable_label(self.config.default_lang == i, *name)
                                        .clicked()
                                    {
                                        self.config.default_lang = i;
                                    }
                                }
                            });
                    });
                });

                // ---- files ------------------------------------------------------
                egui::CollapsingHeader::new(s.sec_files).default_open(true).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button(s.save).clicked() {
                            let path = self.default_save_path();
                            self.do_save(path);
                        }
                        if ui.button(s.save_as).clicked() {
                            self.pick_save();
                        }
                        if ui.button(s.load).clicked() {
                            let path = self
                                .state
                                .current_path
                                .lock()
                                .clone()
                                .unwrap_or_else(paths::default_macro_path);
                            self.do_load(path);
                        }
                        if ui.button(s.open_file).clicked() {
                            self.pick_open();
                        }
                        if ui.add_enabled(!recording && !playing, egui::Button::new(s.clear)).clicked()
                        {
                            *self.state.macro_data.lock() = MacroData::default();
                            self.state.recorded_time_us.store(0, Ordering::Relaxed);
                            *self.state.current_path.lock() = None;
                            self.status_msg = s.cleared.to_string();
                        }
                    });

                    ui.checkbox(&mut self.config.compress_on_save, s.compress);

                    if !self.config.recent_files.is_empty() {
                        let recent = self.config.recent_files.clone();
                        ui.horizontal_wrapped(|ui| {
                            ui.label(s.recent);
                            for path in recent {
                                let name = Path::new(&path)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| path.clone());
                                if ui.small_button(name).on_hover_text(&path).clicked() {
                                    self.do_load(PathBuf::from(path.clone()));
                                }
                            }
                        });
                    }

                    ui.label(
                        egui::RichText::new(format!(
                            "{} {}",
                            s.data_dir,
                            paths::data_dir().display()
                        ))
                        .weak()
                        .small(),
                    );
                });

                if ui.button(s.save_settings).clicked() {
                    self.config.sanitize();
                    match save_config(&self.config) {
                        Ok(()) => self.status_msg = s.settings_saved.to_string(),
                        Err(e) => self.status_msg = s.save_err.replace("{}", &e.to_string()),
                    }
                }

                ui.separator();

                // ---- footer -----------------------------------------------------
                let (count, dur) = {
                    let d = self.state.macro_data.lock();
                    (d.events.len(), d.duration_us)
                };
                ui.horizontal(|ui| {
                    ui.label(s.events.replace("{}", &count.to_string()));
                    if dur > 0 {
                        ui.label(s.duration.replace("{}", &format_us(dur)));
                    }
                });

                let status = if !self.status_msg.is_empty() {
                    self.status_msg.clone()
                } else if recording {
                    s.status_rec.to_string()
                } else if self.state.held_by_desktop.load(Ordering::Relaxed) {
                    s.status_held.to_string()
                } else if paused {
                    s.status_paused.to_string()
                } else if playing {
                    s.status_play.to_string()
                } else {
                    format!(
                        "{} [{}: rec | {}: play | {}: stop]",
                        s.status_ready,
                        self.config.hotkey_record.label(),
                        self.config.hotkey_play.label(),
                        self.config.hotkey_stop.label()
                    )
                };
                ui.label(format!("ℹ {status}"));
            });
        });

        // Idempotent every frame (B1): the live state can never drift from the UI.
        self.config.sanitize();
        self.sync();
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.state.stop_play.store(true, Ordering::Relaxed);
        self.state.paused.store(false, Ordering::Relaxed);
        stop_recording(&self.state);

        // Give the playback thread a moment to release any held keys (B3).
        let deadline = Instant::now() + Duration::from_millis(400);
        while self.state.playing.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        // Autosave (R4).
        self.config.sanitize();
        if let Err(e) = save_config(&self.config) {
            warn!("could not autosave config: {e}");
        }

        #[cfg(windows)]
        {
            let tid = HOOK_THREAD_ID.load(Ordering::Relaxed);
            if tid != 0 {
                unsafe {
                    let _ = win32::PostThreadMessageW(
                        tid,
                        win32::WM_QUIT,
                        win32::WPARAM(0),
                        win32::LPARAM(0),
                    );
                }
            }
        }
        info!("application exiting gracefully");
    }
}

/// One row of the hotkey editor. Returns true when the binding changed.
fn hotkey_row(ui: &mut egui::Ui, label: &str, salt: &str, hk: &mut Hotkey) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(salt)
            .selected_text(vk_name(hk.vk))
            .width(110.0)
            .show_ui(ui, |ui| {
                for (name, vk) in HOTKEY_CHOICES {
                    if ui.selectable_label(hk.vk == vk, name).clicked() && hk.vk != vk {
                        hk.vk = vk;
                        changed = true;
                    }
                }
            });
        changed |= ui.checkbox(&mut hk.ctrl, "Ctrl").changed();
        changed |= ui.checkbox(&mut hk.alt, "Alt").changed();
        changed |= ui.checkbox(&mut hk.shift, "Shift").changed();
    });
    changed
}

// ============================================================================
// Command line
// ============================================================================

struct CliArgs {
    play: Option<PathBuf>,
    loops: Option<u64>,
    speed: Option<f64>,
    no_gui: bool,
    help: bool,
    version: bool,
}

fn parse_cli() -> CliArgs {
    let mut args = CliArgs {
        play: None,
        loops: None,
        speed: None,
        no_gui: false,
        help: false,
        version: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--play" | "-p" => args.play = it.next().map(PathBuf::from),
            "--loops" | "-n" => args.loops = it.next().and_then(|v| v.parse().ok()),
            "--speed" | "-s" => args.speed = it.next().and_then(|v| v.parse().ok()),
            "--no-gui" => args.no_gui = true,
            "--help" | "-h" => args.help = true,
            "--version" | "-V" => args.version = true,
            _ => {}
        }
    }
    args
}

const HELP_TEXT: &str = "\
Macro Recorder - record and replay mouse & keyboard on Windows.

USAGE:
    macro-recorder [OPTIONS]

OPTIONS:
    -p, --play <FILE>    Load a macro (.json / .mrz) on start
    -n, --loops <N>      Repeat count (0 = infinite)
    -s, --speed <X>      Playback speed multiplier (0.05 - 10.0)
        --no-gui         Play the macro headless and exit
    -h, --help           Show this help
    -V, --version        Show the version

Without --no-gui the options simply pre-load the GUI.
";

/// Headless playback (F7).
fn run_headless(args: &CliArgs, cfg: &AppConfig) -> Result<()> {
    let path = args.play.clone().context("--no-gui requires --play <FILE>")?;
    let data = load_macro(&path)?;

    let (tx, rx) = unbounded();
    let state = AppState::new(tx);
    apply_config_to_state(cfg, &state);

    if let Some(n) = args.loops {
        state.loop_play.store(n == 0, Ordering::Relaxed);
        state.play_count_limit.store(n.max(1), Ordering::Relaxed);
    }
    if let Some(sp) = args.speed {
        *state.speed.lock() = sp.clamp(0.05, 10.0);
    }

    // Drain the (unused) channel so senders never block.
    std::thread::spawn(move || while rx.recv().is_ok() {});

    #[cfg(windows)]
    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("hooks".into())
            .spawn(move || input_hook_thread(st, HookMode::HotkeysOnly))?;
    }

    println!("Playing {} ({} events)…", path.display(), data.events.len());
    let generation = state.play_generation.fetch_add(1, Ordering::SeqCst) + 1;
    state.playing.store(true, Ordering::Relaxed);
    playback_loop(state, data, generation);
    println!("Done.");
    Ok(())
}

// ============================================================================
// Entry point
// ============================================================================

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let appender = tracing_appender::rolling::daily(paths::log_dir(), "macro-recorder.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Some(guard)
}

fn main() -> Result<()> {
    init_epoch();
    let _log_guard = init_logging();
    let args = parse_cli();

    if args.help || args.version {
        platform::attach_parent_console();
        if args.version {
            println!("macro-recorder {APP_VERSION}");
        } else {
            print!("{HELP_TEXT}");
        }
        return Ok(());
    }

    platform::set_dpi_awareness();

    let mut config = load_config();
    publish_hotkeys(&config);
    info!("data directory: {}", paths::data_dir().display());

    if args.no_gui {
        platform::attach_parent_console();
        return run_headless(&args, &config);
    }

    // Single instance (R1).
    if !platform::acquire_single_instance() {
        platform::focus_existing_instance();
        info!("another instance is already running - exiting");
        return Ok(());
    }

    let (tx, rx) = unbounded();
    let state = AppState::new(tx);
    apply_config_to_state(&config, &state);

    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("collector".into())
            .spawn(move || collector_thread(rx, st))?;
    }

    #[cfg(windows)]
    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("hooks".into())
            .spawn(move || input_hook_thread(st, HookMode::Full))?;
    }

    // Optional macro preloaded from the command line.
    if let Some(path) = args.play.clone() {
        match load_macro(&path) {
            Ok(data) => {
                state.recorded_time_us.store(data.duration_us, Ordering::Relaxed);
                *state.macro_data.lock() = data;
                *state.current_path.lock() = Some(path.clone());
                config.push_recent(&path);
            }
            Err(e) => warn!("could not preload {}: {e}", path.display()),
        }
    }
    if let Some(n) = args.loops {
        config.loop_play = n == 0;
        config.play_count_limit = n.max(1);
    }
    if let Some(sp) = args.speed {
        config.speed = sp;
    }
    config.sanitize();
    apply_config_to_state(&config, &state);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([440.0, 680.0])
        .with_min_inner_size([360.0, 400.0])
        .with_transparent(true);
    if config.always_on_top {
        viewport = viewport.with_always_on_top();
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let st = state.clone();

    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(MacroApp::new(cc, st, config)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

// ============================================================================
// Tests (I2)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t_us: u64) -> MacroEvent {
        MacroEvent { t_us, kind: InputEventKind::MouseMove { x: 1, y: 2, dx: 0, dy: 0 } }
    }

    #[test]
    fn roundtrip_v2() {
        let data = MacroData::new(vec![ev(0), ev(1000)], 5000);
        let json = serde_json::to_string(&data).unwrap();
        let back: MacroData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.events.len(), 2);
        assert_eq!(back.duration_us, 5000);
        assert_eq!(back.version, 2);
    }

    #[test]
    fn accepts_legacy_v1_array() {
        let json = serde_json::to_string(&vec![ev(0), ev(42)]).unwrap();
        assert!(serde_json::from_str::<MacroData>(&json).is_err());
        let events: Vec<MacroEvent> = serde_json::from_str(&json).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn normalize_sorts_and_fixes_duration() {
        let mut data = MacroData::new(vec![ev(500), ev(100)], 0);
        data.normalize().unwrap();
        assert_eq!(data.events[0].t_us, 100);
        assert_eq!(data.duration_us, 500);
    }

    #[test]
    fn normalize_rejects_empty() {
        let mut data = MacroData::default();
        assert!(data.normalize().is_err());
    }

    #[test]
    fn cycle_length_keeps_trailing_pause() {
        // 3 s of events followed by 2 s of idle time must replay as a 5 s cycle (B8).
        let data = MacroData::new(vec![ev(0), ev(3_000_000)], 5_000_000);
        assert_eq!(data.cycle_len_us(), 5_000_000);
    }

    #[test]
    fn config_sanitize_clamps() {
        let mut cfg = AppConfig {
            speed: f64::NAN,
            play_count_limit: 100_000,
            jitter_pct: 900,
            mouse_sample_ms: 0,
            default_theme: 99,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.speed, 1.0);
        assert_eq!(cfg.play_count_limit, 9999);
        assert_eq!(cfg.jitter_pct, 50);
        assert_eq!(cfg.mouse_sample_ms, 1);
        assert_eq!(cfg.default_theme, THEME_NAMES.len() - 1);
    }

    #[test]
    fn time_limit_math() {
        let cfg = AppConfig { time_limit_h: 1, time_limit_m: 2, time_limit_s: 3, ..Default::default() };
        assert_eq!(cfg.time_limit_us(), 3_723_000_000);
    }

    #[test]
    fn pressed_inputs_track_and_release() {
        let mut p = PressedInputs::default();
        p.note_key(65, 30, false, true);
        p.note_button(MouseButton::Left, true);
        assert!(!p.is_empty());
        p.note_key(65, 30, false, false);
        p.note_button(MouseButton::Left, false);
        assert!(p.is_empty());
    }

    #[test]
    fn absolute_normalization_hits_both_edges() {
        let (nx, ny) = platform::normalize_abs(0, 0, 0, 0, 1920, 1080);
        assert_eq!((nx, ny), (0, 0));
        let (nx, ny) = platform::normalize_abs(1919, 1079, 0, 0, 1920, 1080);
        assert_eq!((nx, ny), (65535, 65535));
    }

    #[test]
    fn rng_is_bounded() {
        let mut rng = Rng::new();
        for _ in 0..1000 {
            assert!(rng.below(10) < 10);
        }
        assert_eq!(rng.below(0), 0);
    }

    #[test]
    fn compressed_extension_detection() {
        assert!(is_compressed_path(Path::new("a.mrz")));
        assert!(is_compressed_path(Path::new("a.MRZ")));
        assert!(!is_compressed_path(Path::new("a.json")));
    }

    #[test]
    fn recent_list_dedupes_and_caps() {
        let mut cfg = AppConfig::default();
        for i in 0..12 {
            cfg.push_recent(Path::new(&format!("m{i}.json")));
        }
        cfg.push_recent(Path::new("m11.json"));
        assert_eq!(cfg.recent_files.len(), 8);
        assert!(cfg.recent_files[0].ends_with("m11.json"));
    }
}

