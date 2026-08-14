#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! # Macro Recorder
//!
//! A DPI-aware macro recorder for Windows.
//!
//! Threads:
//!   * UI          - eframe/egui
//!   * hooks       - WH_KEYBOARD_LL + WH_MOUSE_LL + hotkeys + tray window + message loop
//!   * collector   - crossbeam channel -> Vec<MacroEvent>
//!   * playback    - spin_sleep + timeBeginPeriod(1) + SendInput
//!
//! Everything shared lives in `Arc<AppState>` (atomics + parking_lot).
//!
//! Notes for maintainers:
//!   * `panic = "abort"` in release, so the hook callbacks are written to be panic-free
//!     (no unwrap, no indexing, null-pointer guards) rather than relying on catch_unwind.
//!   * The hook callbacks must stay cheap: Windows silently unhooks a callback that
//!     exceeds LowLevelHooksTimeout. No COM, no window enumeration in the hot path.

use anyhow::{Context as _, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[cfg(windows)]
mod win32 {
    pub use windows::Win32::Foundation::*;
    pub use windows::Win32::Globalization::GetUserDefaultUILanguage;
    pub use windows::Win32::Graphics::Dwm::*;
    // Explicit imports instead of a glob: several Gdi names collide with
    // WindowsAndMessaging and would become ambiguous at the use site.
    pub use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, HRGN, ReleaseDC};
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

/// Window icon as raw 128x128 RGBA (no PNG decoder needed at runtime).
/// See `assets/README.md` to regenerate.
const ICON_RGBA: &[u8] = include_bytes!("../assets/icon.rgba");
const ICON_SIZE: u32 = 128;

const HK_ID_RECORD: i32 = 1;
const HK_ID_PLAY: i32 = 2;
const HK_ID_STOP: i32 = 3;
const HK_ID_PAUSE: i32 = 4;

const WM_HOTKEY_ID: u32 = 0x0312;
const WM_APP_REHOTKEY: u32 = 0x8001;
const WM_APP_TRAY: u32 = 0x8002;
/// Temporarily drops all global hotkeys so the key being bound can reach the window.
const WM_APP_HK_OFF: u32 = 0x8003;

const TRAY_ID_SHOW: u32 = 101;
const TRAY_ID_RECORD: u32 = 102;
const TRAY_ID_PLAY: u32 = 103;
const TRAY_ID_STOP: u32 = 104;
const TRAY_ID_EXIT: u32 = 105;

/// Longest single sleep inside the playback loop: bounds Stop/Pause latency.
const SLEEP_CHUNK_US: u64 = 15_000;
const SPIN_THRESHOLD_US: u64 = 2_000;
const METRICS_TTL_US: u64 = 500_000;
const DESKTOP_TTL_US: u64 = 200_000;
const PIXEL_CHECK_TTL_US: u64 = 250_000;
const MAX_EVENTS: usize = 4_000_000;

/// Footer magic for macros appended to a copy of this executable.
const PAYLOAD_MAGIC: &[u8; 8] = b"MRPAYLD1";

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

/// xorshift64* - the only randomness we need is playback jitter, so no dependency.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let seed = now_us() ^ 0x9E37_79B9_7F4A_7C15 ^ ((std::process::id() as u64) << 32);
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
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next_u64() % bound }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ============================================================================
// Paths
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
        known_folders::get_known_folder_path(known_folders::KnownFolder::RoamingAppData)
            .map(|p| p.join("MacroRecorder"))
    }

    #[cfg(not(windows))]
    fn roaming_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/macro-recorder"))
    }

    /// Portable (next to the exe) when possible, otherwise %APPDATA%.
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
    pub fn sub_dir(name: &str) -> PathBuf {
        let dir = data_dir().join(name);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
    pub fn log_dir() -> PathBuf {
        sub_dir("logs")
    }
    pub fn profiles_dir() -> PathBuf {
        sub_dir("profiles")
    }
    pub fn lang_dir() -> PathBuf {
        sub_dir("lang")
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
        if self.vk == 0 {
            return "—".into();
        }
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
        s.push_str(&vk_name(self.vk));
        s
    }
}

/// Human name for a virtual-key code.
fn vk_name(vk: u32) -> String {
    match vk {
        0x00 => "—".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x10 => "Shift".into(),
        0x11 => "Ctrl".into(),
        0x12 => "Alt".into(),
        0x13 => "Pause".into(),
        0x14 => "CapsLock".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2C => "PrintScreen".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x30..=0x39 => char::from(b'0' + (vk - 0x30) as u8).to_string(),
        0x41..=0x5A => char::from(b'A' + (vk - 0x41) as u8).to_string(),
        0x5B => "LWin".into(),
        0x5C => "RWin".into(),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x6A => "Num*".into(),
        0x6B => "Num+".into(),
        0x6D => "Num-".into(),
        0x6E => "Num.".into(),
        0x6F => "Num/".into(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x90 => "NumLock".into(),
        0x91 => "ScrollLock".into(),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xC0 => "`".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        _ => format!("VK 0x{vk:02X}"),
    }
}

/// Hot-path copies of the hotkey virtual keys.
///
/// The keyboard hook fires *before* WM_HOTKEY is dispatched, so without this filter
/// every hotkey press would be recorded into the macro.
static HK_VK: [AtomicU32; 4] = [
    AtomicU32::new(0x75), // F6 record
    AtomicU32::new(0x76), // F7 play
    AtomicU32::new(0x78), // F9 emergency stop
    AtomicU32::new(0x77), // F8 pause
];

/// Bit mask of hotkeys that failed to register.
static HK_FAILED: AtomicU32 = AtomicU32::new(0);
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

static PENDING_HOTKEYS: Mutex<[Hotkey; 4]> = Mutex::new([
    Hotkey::plain(0x75),
    Hotkey::plain(0x76),
    Hotkey::plain(0x78),
    Hotkey::plain(0x77),
]);

/// 0 = idle, 1..=4 = capture the next key press for that hotkey slot.
static CAPTURE_SLOT: AtomicU32 = AtomicU32::new(0);
static CAPTURED_KEY: Mutex<Option<Hotkey>> = Mutex::new(None);

/// Keys offered in the dropdown next to each binding.
///
/// "Press a key" covers everything, but a plain list is guaranteed to work even if
/// the window never sees the key press - including keys egui does not report at all
/// (Pause, ScrollLock, NumPad).
const HOTKEY_CHOICES: [(&str, u32); 26] = [
    ("—", 0x00),
    ("F1", 0x70), ("F2", 0x71), ("F3", 0x72), ("F4", 0x73),
    ("F5", 0x74), ("F6", 0x75), ("F7", 0x76), ("F8", 0x77),
    ("F9", 0x78), ("F10", 0x79), ("F11", 0x7A), ("F12", 0x7B),
    ("Pause", 0x13), ("ScrollLock", 0x91), ("Insert", 0x2D), ("Delete", 0x2E),
    ("Home", 0x24), ("End", 0x23), ("PageUp", 0x21), ("PageDown", 0x22),
    ("Num0", 0x60), ("Num1", 0x61), ("Num*", 0x6A), ("Num-", 0x6D), ("Num+", 0x6B),
];

/// Starts binding mode for one hotkey slot.
fn begin_capture(slot: u32) {
    CAPTURE_SLOT.store(slot, Ordering::Relaxed);
    *CAPTURED_KEY.lock() = None;
    // Without this the currently bound keys are eaten by RegisterHotKey and would
    // never arrive as window input, so F6-F9 could not be rebound onto each other.
    request_hotkey_message(WM_APP_HK_OFF);
}

/// Leaves binding mode and puts the global hotkeys back.
fn end_capture() {
    CAPTURE_SLOT.store(0, Ordering::Relaxed);
    request_hotkey_message(WM_APP_REHOTKEY);
}

/// Maps an egui key to a Windows virtual-key code.
///
/// This is the capture path used while the window has focus; the low-level hook
/// covers the rest (and keys egui never reports).
fn egui_key_to_vk(key: egui::Key) -> Option<u32> {
    use egui::Key as K;
    Some(match key {
        K::A => 0x41, K::B => 0x42, K::C => 0x43, K::D => 0x44, K::E => 0x45,
        K::F => 0x46, K::G => 0x47, K::H => 0x48, K::I => 0x49, K::J => 0x4A,
        K::K => 0x4B, K::L => 0x4C, K::M => 0x4D, K::N => 0x4E, K::O => 0x4F,
        K::P => 0x50, K::Q => 0x51, K::R => 0x52, K::S => 0x53, K::T => 0x54,
        K::U => 0x55, K::V => 0x56, K::W => 0x57, K::X => 0x58, K::Y => 0x59,
        K::Z => 0x5A,
        K::Num0 => 0x30, K::Num1 => 0x31, K::Num2 => 0x32, K::Num3 => 0x33,
        K::Num4 => 0x34, K::Num5 => 0x35, K::Num6 => 0x36, K::Num7 => 0x37,
        K::Num8 => 0x38, K::Num9 => 0x39,
        K::F1 => 0x70, K::F2 => 0x71, K::F3 => 0x72, K::F4 => 0x73,
        K::F5 => 0x74, K::F6 => 0x75, K::F7 => 0x76, K::F8 => 0x77,
        K::F9 => 0x78, K::F10 => 0x79, K::F11 => 0x7A, K::F12 => 0x7B,
        K::Tab => 0x09, K::Backspace => 0x08, K::Enter => 0x0D, K::Space => 0x20,
        K::Insert => 0x2D, K::Delete => 0x2E, K::Home => 0x24, K::End => 0x23,
        K::PageUp => 0x21, K::PageDown => 0x22,
        K::ArrowUp => 0x26, K::ArrowDown => 0x28,
        K::ArrowLeft => 0x25, K::ArrowRight => 0x27,
        _ => return None,
    })
}

/// Pulls a binding out of this frame's window input, if binding mode is active.
fn capture_from_window(ctx: &egui::Context) -> Option<Hotkey> {
    ctx.input(|i| {
        for ev in &i.events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                if *key == egui::Key::Escape {
                    return Some(Hotkey::plain(0)); // treated as "cancel" by the caller
                }
                if let Some(vk) = egui_key_to_vk(*key) {
                    return Some(Hotkey {
                        vk,
                        ctrl: modifiers.ctrl,
                        alt: modifiers.alt,
                        shift: modifiers.shift,
                    });
                }
            }
        }
        None
    })
}

fn publish_hotkeys(cfg: &AppConfig) {
    let hk = [cfg.hotkey_record, cfg.hotkey_play, cfg.hotkey_stop, cfg.hotkey_pause];
    for (i, k) in hk.iter().enumerate() {
        HK_VK[i].store(k.vk, Ordering::Relaxed);
    }
    *PENDING_HOTKEYS.lock() = hk;
}

fn is_hotkey_vk(vk: u32) -> bool {
    HK_VK.iter().any(|a| {
        let v = a.load(Ordering::Relaxed);
        v != 0 && v == vk
    })
}

fn request_hotkey_message(msg: u32) {
    #[cfg(windows)]
    {
        let tid = HOOK_THREAD_ID.load(Ordering::Relaxed);
        if tid != 0 {
            unsafe {
                let _ = win32::PostThreadMessageW(tid, msg, win32::WPARAM(0), win32::LPARAM(0));
            }
        }
    }
    #[cfg(not(windows))]
    let _ = msg;
}

fn request_hotkey_refresh() {
    request_hotkey_message(WM_APP_REHOTKEY);
}

// ============================================================================
// Configuration
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AppConfig {
    // appearance
    pub default_lang: usize,
    pub default_theme: usize,
    pub transparent_ui: bool,
    pub always_on_top: bool,
    pub tray_enabled: bool,
    pub close_to_tray: bool,

    // playback
    pub loop_play: bool,
    pub play_count_limit: u64,
    pub speed: f64,
    pub absolute_mouse: bool,
    pub repeat_delay_ms: u64,
    pub jitter_pct: u64,
    pub use_window_anchor: bool,

    // recording
    pub capture_mouse_moves: bool,
    pub mouse_sample_ms: u64,
    pub record_window_anchor: bool,

    // time limit
    pub time_limit_enabled: bool,
    pub time_limit_h: u64,
    pub time_limit_m: u64,
    pub time_limit_s: u64,
    pub action_on_completion: usize,
    pub shutdown_delay_s: u64,

    // pixel stop condition
    pub pixel_enabled: bool,
    pub pixel_x: i32,
    pub pixel_y: i32,
    pub pixel_r: u8,
    pub pixel_g: u8,
    pub pixel_b: u8,
    pub pixel_tolerance: u32,
    /// 0 = stop when the pixel matches, 1 = stop when it stops matching.
    pub pixel_mode: usize,

    // hotkeys
    pub hotkey_record: Hotkey,
    pub hotkey_play: Hotkey,
    pub hotkey_stop: Hotkey,
    pub hotkey_pause: Hotkey,

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
            tray_enabled: true,
            close_to_tray: true,

            loop_play: true,
            play_count_limit: 1,
            speed: 1.0,
            absolute_mouse: true,
            repeat_delay_ms: 0,
            jitter_pct: 0,
            use_window_anchor: false,

            capture_mouse_moves: true,
            mouse_sample_ms: 5,
            record_window_anchor: false,

            time_limit_enabled: false,
            time_limit_h: 0,
            time_limit_m: 0,
            time_limit_s: 0,
            action_on_completion: 0,
            shutdown_delay_s: 60,

            pixel_enabled: false,
            pixel_x: 0,
            pixel_y: 0,
            pixel_r: 255,
            pixel_g: 0,
            pixel_b: 0,
            pixel_tolerance: 20,
            pixel_mode: 0,

            hotkey_record: Hotkey::plain(0x75), // F6
            hotkey_play: Hotkey::plain(0x76),   // F7
            hotkey_pause: Hotkey::plain(0x77),  // F8
            hotkey_stop: Hotkey::plain(0x78),   // F9

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
        self.pixel_tolerance = self.pixel_tolerance.min(255);
        self.pixel_mode = self.pixel_mode.min(1);
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

fn load_config_from(path: &Path) -> AppConfig {
    let mut cfg = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<AppConfig>(&s).ok())
        .unwrap_or_default();
    cfg.sanitize();
    cfg
}

fn load_config() -> AppConfig {
    load_config_from(&paths::config_path())
}

fn save_config_to(path: &Path, cfg: &AppConfig) -> Result<()> {
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn save_config(cfg: &AppConfig) -> Result<()> {
    save_config_to(&paths::config_path(), cfg)
}

/// Named setting profiles stored in `<data>/profiles/<name>.json`.
fn list_profiles() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths::profiles_dir()) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                if let Some(stem) = p.file_stem().and_then(|x| x.to_str()) {
                    out.push(stem.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

fn profile_path(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '_' })
        .collect();
    paths::profiles_dir().join(format!("{}.json", safe.trim()))
}

// ============================================================================
// Macro model & storage
// ============================================================================

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl MouseButton {
    fn from_index(i: usize) -> Self {
        match i {
            1 => Self::Right,
            2 => Self::Middle,
            3 => Self::X1,
            4 => Self::X2,
            _ => Self::Left,
        }
    }
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

/// Position of the window that was in the foreground when recording started.
///
/// Lets playback re-anchor absolute coordinates if that window has since moved.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct WindowAnchor {
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Macro container, format version 2.
///
/// v1 files were a bare `[MacroEvent, ...]` array and are still accepted on load.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct MacroData {
    #[serde(default = "format_version")]
    pub version: u32,
    /// Full recording length, including trailing idle time.
    #[serde(default)]
    pub duration_us: u64,
    #[serde(default)]
    pub anchor: Option<WindowAnchor>,
    pub events: Vec<MacroEvent>,
}

fn format_version() -> u32 {
    2
}

impl MacroData {
    fn new(events: Vec<MacroEvent>, duration_us: u64) -> Self {
        Self { version: 2, duration_us, anchor: None, events }
    }
    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
    fn last_t(&self) -> u64 {
        self.events.last().map(|e| e.t_us).unwrap_or(0)
    }
    fn cycle_len_us(&self) -> u64 {
        self.duration_us.max(self.last_t()).max(1)
    }
    /// Sorts non-monotonic timestamps and rejects obviously broken files.
    fn normalize(&mut self) -> Result<()> {
        if self.events.is_empty() {
            anyhow::bail!("macro contains no events");
        }
        if self.events.len() > MAX_EVENTS {
            anyhow::bail!("macro contains {} events (limit {MAX_EVENTS})", self.events.len());
        }
        if !self.events.windows(2).all(|w| w[0].t_us <= w[1].t_us) {
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

fn is_compressed_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("mrz") | Some("gz")
    )
}

fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write as _;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(bytes)?;
    Ok(enc.finish()?)
}

fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
}

fn save_macro(path: &Path, data: &MacroData) -> Result<()> {
    let bytes = if is_compressed_path(path) {
        gzip(&serde_json::to_vec(data)?)?
    } else {
        serde_json::to_vec_pretty(data)?
    };
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn parse_macro(text: &str) -> Result<MacroData> {
    let mut data = match serde_json::from_str::<MacroData>(text) {
        Ok(d) => d,
        Err(obj_err) => match serde_json::from_str::<Vec<MacroEvent>>(text) {
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

fn load_macro(path: &Path) -> Result<MacroData> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = if is_compressed_path(path) {
        String::from_utf8(gunzip(&bytes)?)?
    } else {
        String::from_utf8(bytes).context("macro file is not valid UTF-8")?
    };
    parse_macro(&text)
}

// ---------------------------------------------------------------------------
// AutoHotkey export
// ---------------------------------------------------------------------------

/// Writes an AutoHotkey v2 script that reproduces the macro.
///
/// Coordinates are emitted in screen space (`CoordMode "Mouse", "Screen"`), and the
/// gaps between events become `Sleep` calls, so the timing survives the trip.
fn export_ahk(path: &Path, data: &MacroData, loops: u64) -> Result<()> {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(data.events.len() * 48);
    s.push_str("#Requires AutoHotkey v2.0\n");
    s.push_str("; Generated by Macro Recorder\n");
    s.push_str("CoordMode \"Mouse\", \"Screen\"\n");
    s.push_str("SetKeyDelay -1, -1\nSetMouseDelay -1\n\n");
    s.push_str("Esc::ExitApp\n\n");
    if loops == 0 {
        s.push_str("Loop {\n");
    } else {
        let _ = writeln!(s, "Loop {loops} {{");
    }

    let mut prev = 0u64;
    for ev in &data.events {
        let gap_ms = (ev.t_us.saturating_sub(prev)) / 1000;
        if gap_ms > 0 {
            let _ = writeln!(s, "    Sleep {gap_ms}");
        }
        prev = ev.t_us;
        match ev.kind {
            InputEventKind::MouseMove { x, y, .. } => {
                let _ = writeln!(s, "    MouseMove {x}, {y}, 0");
            }
            InputEventKind::MouseButton { button, down, x, y } => {
                let name = match button {
                    MouseButton::Left => "Left",
                    MouseButton::Right => "Right",
                    MouseButton::Middle => "Middle",
                    MouseButton::X1 => "X1",
                    MouseButton::X2 => "X2",
                };
                let state = if down { "D" } else { "U" };
                let _ = writeln!(s, "    Click {x}, {y}, \"{name}\", \"{state}\"");
            }
            InputEventKind::MouseWheel { delta, horizontal, .. } => {
                let dir = match (horizontal, delta >= 0) {
                    (false, true) => "WheelUp",
                    (false, false) => "WheelDown",
                    (true, true) => "WheelRight",
                    (true, false) => "WheelLeft",
                };
                let n = (delta.abs() / 120).max(1);
                let _ = writeln!(s, "    Click \"{dir}\", {n}");
            }
            InputEventKind::Key { vk, down, .. } => {
                let state = if down { "down" } else { "up" };
                // vk<hex> is always valid in AHK and sidesteps name mapping entirely.
                let _ = writeln!(s, "    Send \"{{vk{vk:02X} {state}}}\"");
            }
        }
    }

    let tail_ms = data.duration_us.saturating_sub(prev) / 1000;
    if tail_ms > 0 {
        let _ = writeln!(s, "    Sleep {tail_ms}");
    }
    s.push_str("}\n");

    std::fs::write(path, s).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Self-running executable export
// ---------------------------------------------------------------------------

/// Playback settings baked into an exported executable.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Payload {
    #[serde(default)]
    loops: u64,
    #[serde(default = "one_f64")]
    speed: f64,
    #[serde(default)]
    absolute_mouse: bool,
    #[serde(default)]
    repeat_delay_ms: u64,
    #[serde(default)]
    macro_data: MacroData,
}

fn one_f64() -> f64 {
    1.0
}

/// Returns the offset where an appended payload starts, if this image has one.
fn payload_offset(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 16 {
        return None;
    }
    let tail = &bytes[bytes.len() - 8..];
    if tail != PAYLOAD_MAGIC {
        return None;
    }
    let mut len = [0u8; 8];
    len.copy_from_slice(&bytes[bytes.len() - 16..bytes.len() - 8]);
    let len = u64::from_le_bytes(len) as usize;
    let start = bytes.len().checked_sub(16 + len)?;
    Some(start)
}

/// Copies this executable and appends the macro, producing a standalone player.
///
/// A PE image ignores trailing bytes, which is the same trick self-extracting
/// archives use - no compiler or linker is involved.
fn export_self_running_exe(dest: &Path, payload: &Payload) -> Result<()> {
    let exe = std::env::current_exe().context("locating the current executable")?;
    let mut bytes = std::fs::read(&exe).with_context(|| format!("reading {}", exe.display()))?;
    if let Some(off) = payload_offset(&bytes) {
        bytes.truncate(off); // never nest payloads
    }
    let blob = gzip(&serde_json::to_vec(payload)?)?;
    bytes.extend_from_slice(&blob);
    bytes.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    bytes.extend_from_slice(PAYLOAD_MAGIC);
    std::fs::write(dest, bytes).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// Reads a payload appended to our own image, if any.
///
/// Only the 16-byte footer is read on a normal launch, so the usual startup path
/// never pulls the whole multi-megabyte image off disk.
fn read_self_payload() -> Option<Payload> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let exe = std::env::current_exe().ok()?;
    let mut file = std::fs::File::open(&exe).ok()?;
    let size = file.metadata().ok()?.len();
    if size < 16 {
        return None;
    }

    let mut footer = [0u8; 16];
    file.seek(SeekFrom::End(-16)).ok()?;
    file.read_exact(&mut footer).ok()?;
    if &footer[8..] != PAYLOAD_MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(footer[..8].try_into().ok()?);
    let start = size.checked_sub(16 + len)?;

    let mut blob = vec![0u8; len as usize];
    file.seek(SeekFrom::Start(start)).ok()?;
    file.read_exact(&mut blob).ok()?;

    let json = gunzip(&blob).ok()?;
    serde_json::from_slice::<Payload>(&json).ok()
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
// Virtual desktop isolation
// ============================================================================

#[cfg(windows)]
mod virtual_desktop {
    use super::win32::*;
    use super::{DESKTOP_TTL_US, now_us};
    use std::cell::RefCell;

    thread_local! {
        static VDM: RefCell<Option<IVirtualDesktopManager>> = const { RefCell::new(None) };
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

    /// Throttled: a COM round-trip per keystroke would get the hook killed.
    pub fn is_app_on_active_desktop_cached(hwnd: HWND) -> bool {
        let now = now_us();
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.0 == 0 || now.saturating_sub(c.0) >= DESKTOP_TTL_US {
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
    use super::{APP_TITLE, EndAction, METRICS_TTL_US, WindowAnchor, now_us, wide};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicI32, AtomicIsize, AtomicU64, Ordering};

    static HWND_CACHE: AtomicIsize = AtomicIsize::new(0);
    static HWND_LAST_TRY: AtomicU64 = AtomicU64::new(0);
    static VS: [AtomicI32; 4] =
        [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(1), AtomicI32::new(1)];
    static VS_LAST: AtomicU64 = AtomicU64::new(0);

    /// Our own top-level window, cached and validated against the process id.
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
            let title = wide(APP_TITLE);
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
            // DWMWA_SYSTEMBACKDROP_TYPE: 1 = none, 2 = Mica, 3 = Acrylic, 4 = Tabbed.
            let backdrop_type: i32 = backdrop;
            let result = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(38),
                &backdrop_type as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            );
            if result.is_err() && backdrop > 1 {
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

    /// `w - 1` as the denominator so the right/bottom-most pixel stays reachable.
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

    /// Colour of a screen pixel, or None if the read failed.
    pub fn screen_pixel(x: i32, y: i32) -> Option<(u8, u8, u8)> {
        unsafe {
            let hdc = GetDC(None);
            if hdc.is_invalid() {
                return None;
            }
            let c = GetPixel(hdc, x, y);
            ReleaseDC(None, hdc);
            if c.0 == 0xFFFF_FFFF {
                return None;
            }
            Some(((c.0 & 0xFF) as u8, ((c.0 >> 8) & 0xFF) as u8, ((c.0 >> 16) & 0xFF) as u8))
        }
    }

    pub fn cursor_pos() -> (i32, i32) {
        unsafe {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            (p.x, p.y)
        }
    }

    /// Title + rect of the foreground window, skipping our own.
    pub fn foreground_anchor() -> Option<WindowAnchor> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() || hwnd == app_hwnd() {
                return None;
            }
            let mut buf = [0u16; 256];
            let len = GetWindowTextW(hwnd, &mut buf);
            if len <= 0 {
                return None;
            }
            let title = String::from_utf16_lossy(&buf[..len as usize]);
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() {
                return None;
            }
            Some(WindowAnchor {
                title,
                x: r.left,
                y: r.top,
                w: r.right - r.left,
                h: r.bottom - r.top,
            })
        }
    }

    /// Current position of the anchored window, if it is still around.
    pub fn find_window_rect(title: &str) -> Option<(i32, i32)> {
        unsafe {
            let w = wide(title);
            let hwnd = FindWindowW(None, PCWSTR(w.as_ptr())).ok()?;
            if hwnd.0.is_null() {
                return None;
            }
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() {
                return None;
            }
            Some((r.left, r.top))
        }
    }

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
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: luid,
                        Attributes: SE_PRIVILEGE_ENABLED,
                    }],
                };
                let _ = AdjustTokenPrivileges(token, false, Some(&mut tp), 0, None, None);
            }
            let _ = CloseHandle(token);
        }
    }

    pub fn run_end_action(action: EndAction, delay_s: u32, reason: &str) -> anyhow::Result<()> {
        unsafe {
            match action {
                EndAction::Stop => Ok(()),
                EndAction::Shutdown | EndAction::Reboot => {
                    enable_shutdown_privilege();
                    let msg = wide(reason);
                    let reboot = matches!(action, EndAction::Reboot);
                    InitiateSystemShutdownExW(
                        PCWSTR::null(),
                        PCWSTR(msg.as_ptr()),
                        delay_s,
                        true.into(),
                        reboot.into(),
                        SHTDN_REASON_MAJOR_OTHER
                            | SHTDN_REASON_MINOR_OTHER
                            | SHTDN_REASON_FLAG_PLANNED,
                    )
                    .map_err(|e| anyhow::anyhow!("InitiateSystemShutdownExW failed: {e}"))
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
                    if SetSuspendState(hibernate, true, false) {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!(
                            "SetSuspendState failed (hibernation may be disabled)"
                        ))
                    }
                }
            }
        }
    }

    pub fn acquire_single_instance() -> bool {
        unsafe {
            match CreateMutexW(None, true, w!("Local\\MacroRecorder_SingleInstance_v1")) {
                Ok(handle) => {
                    if GetLastError() == ERROR_ALREADY_EXISTS {
                        let _ = CloseHandle(handle);
                        false
                    } else {
                        // Never closed on purpose: the mutex must live as long as the
                        // process. HANDLE has no Drop, so letting it fall out of scope
                        // leaves the kernel object open.
                        true
                    }
                }
                Err(_) => true,
            }
        }
    }

    /// Hides or restores our own top-level window.
    pub fn set_window_hidden(hidden: bool) {
        unsafe {
            let hwnd = app_hwnd();
            if hwnd.0.is_null() {
                return;
            }
            if hidden {
                let _ = ShowWindow(hwnd, SW_HIDE);
            } else {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }

    /// Asks the main window to close, exactly the way the ✕ button does.
    pub fn request_app_close() {
        unsafe {
            let hwnd = app_hwnd();
            if !hwnd.0.is_null() {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn focus_existing_instance() {
        unsafe {
            let title = wide(APP_TITLE);
            if let Ok(hwnd) = FindWindowW(None, PCWSTR(title.as_ptr())) {
                if !hwnd.0.is_null() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                }
            }
        }
    }

    /// Resolved dynamically so the crate needs no Win32_System_Console feature.
    pub fn attach_parent_console() {
        const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
        unsafe {
            let Ok(kernel32) = GetModuleHandleW(w!("kernel32.dll")) else {
                return;
            };
            let Some(sym) = GetProcAddress(kernel32, PCSTR(b"AttachConsole\0".as_ptr())) else {
                return;
            };
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
    use super::{EndAction, WindowAnchor};

    pub fn app_hwnd() {}
    pub fn apply_system_backdrop(_: (), _: i32) {}
    pub unsafe fn send_absolute_mouse_move(_: i32, _: i32) {}
    pub fn begin_high_res_timer() {}
    pub fn end_high_res_timer() {}
    pub fn screen_pixel(_: i32, _: i32) -> Option<(u8, u8, u8)> {
        None
    }
    pub fn cursor_pos() -> (i32, i32) {
        (0, 0)
    }
    pub fn foreground_anchor() -> Option<WindowAnchor> {
        None
    }
    pub fn find_window_rect(_: &str) -> Option<(i32, i32)> {
        None
    }
    pub fn acquire_single_instance() -> bool {
        true
    }
    pub fn set_window_hidden(_: bool) {}
    pub fn request_app_close() {}
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
// Shared state
// ============================================================================

/// Mirrors the main window visibility so the tray menu can label itself correctly.
static WINDOW_VISIBLE: AtomicBool = AtomicBool::new(true);
/// Set by "Exit" so the close-to-tray rule lets that one close through.
static ALLOW_CLOSE: AtomicBool = AtomicBool::new(false);

/// Shows or hides the main window.
///
/// Uses `ShowWindow` rather than a viewport command: the tray lives on the hook
/// thread, and a hidden window stops painting, so anything routed through the UI
/// thread would never bring it back.
fn set_window_visible(visible: bool) {
    WINDOW_VISIBLE.store(visible, Ordering::Relaxed);
    platform::set_window_hidden(!visible);
}

fn toggle_main_window() {
    set_window_visible(!WINDOW_VISIBLE.load(Ordering::Relaxed));
}

/// Quits for real, even when the close button is set to minimize to tray.
fn quit_application() {
    ALLOW_CLOSE.store(true, Ordering::Relaxed);
    // The window has to be up for winit to deliver the close, and it doubles as the
    // only visible sign that Exit was registered.
    set_window_visible(true);
    platform::request_app_close();
}

pub struct AppState {
    // lifecycle
    pub recording: AtomicBool,
    pub playing: AtomicBool,
    pub paused: AtomicBool,
    pub stop_play: AtomicBool,
    pub play_generation: AtomicU64,
    pub held_by_desktop: AtomicBool,
    /// Set when playback ended because the pixel condition fired.
    pub pixel_triggered: AtomicBool,

    // playback settings
    pub loop_play: AtomicBool,
    pub play_count: AtomicU64,
    pub play_count_limit: AtomicU64,
    pub absolute_mouse: AtomicBool,
    pub repeat_delay_ms: AtomicU64,
    pub jitter_pct: AtomicU64,
    pub use_window_anchor: AtomicBool,
    pub speed: Mutex<f64>,

    // recording settings
    pub capture_mouse_moves: AtomicBool,
    pub mouse_sample_us: AtomicU64,
    pub record_window_anchor: AtomicBool,

    // time limit
    pub time_limit_enabled: AtomicBool,
    pub time_limit_us: AtomicU64,
    pub action_on_completion: AtomicU64,
    pub shutdown_delay_s: AtomicU64,

    // pixel condition
    pub pixel_enabled: AtomicBool,
    pub pixel_x: AtomicI32,
    pub pixel_y: AtomicI32,
    pub pixel_rgb: AtomicU32,
    pub pixel_tolerance: AtomicU32,
    pub pixel_mode: AtomicU32,

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
            pixel_triggered: AtomicBool::new(false),

            loop_play: AtomicBool::new(true),
            play_count: AtomicU64::new(0),
            play_count_limit: AtomicU64::new(1),
            absolute_mouse: AtomicBool::new(true),
            repeat_delay_ms: AtomicU64::new(0),
            jitter_pct: AtomicU64::new(0),
            use_window_anchor: AtomicBool::new(false),
            speed: Mutex::new(1.0),

            capture_mouse_moves: AtomicBool::new(true),
            mouse_sample_us: AtomicU64::new(5_000),
            record_window_anchor: AtomicBool::new(true),

            time_limit_enabled: AtomicBool::new(false),
            time_limit_us: AtomicU64::new(0),
            action_on_completion: AtomicU64::new(0),
            shutdown_delay_s: AtomicU64::new(60),

            pixel_enabled: AtomicBool::new(false),
            pixel_x: AtomicI32::new(0),
            pixel_y: AtomicI32::new(0),
            pixel_rgb: AtomicU32::new(0xFF_0000),
            pixel_tolerance: AtomicU32::new(20),
            pixel_mode: AtomicU32::new(0),

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

/// Pushes every persisted setting into the live state.
///
/// Called at startup and once per UI frame, so the running engine can never drift
/// from what the user sees.
fn apply_config_to_state(cfg: &AppConfig, state: &AppState) {
    state.loop_play.store(cfg.loop_play, Ordering::Relaxed);
    state.play_count_limit.store(cfg.play_count_limit, Ordering::Relaxed);
    state.absolute_mouse.store(cfg.absolute_mouse, Ordering::Relaxed);
    state.repeat_delay_ms.store(cfg.repeat_delay_ms, Ordering::Relaxed);
    state.jitter_pct.store(cfg.jitter_pct, Ordering::Relaxed);
    state.use_window_anchor.store(cfg.use_window_anchor, Ordering::Relaxed);
    *state.speed.lock() = cfg.speed;

    state.capture_mouse_moves.store(cfg.capture_mouse_moves, Ordering::Relaxed);
    state.mouse_sample_us.store(cfg.mouse_sample_ms * 1_000, Ordering::Relaxed);
    state.record_window_anchor.store(cfg.record_window_anchor, Ordering::Relaxed);

    state.time_limit_enabled.store(cfg.time_limit_enabled, Ordering::Relaxed);
    state.time_limit_us.store(cfg.time_limit_us(), Ordering::Relaxed);
    state.action_on_completion.store(cfg.action_on_completion as u64, Ordering::Relaxed);
    state.shutdown_delay_s.store(cfg.shutdown_delay_s, Ordering::Relaxed);

    state.pixel_enabled.store(cfg.pixel_enabled, Ordering::Relaxed);
    state.pixel_x.store(cfg.pixel_x, Ordering::Relaxed);
    state.pixel_y.store(cfg.pixel_y, Ordering::Relaxed);
    let rgb = ((cfg.pixel_r as u32) << 16) | ((cfg.pixel_g as u32) << 8) | cfg.pixel_b as u32;
    state.pixel_rgb.store(rgb, Ordering::Relaxed);
    state.pixel_tolerance.store(cfg.pixel_tolerance, Ordering::Relaxed);
    state.pixel_mode.store(cfg.pixel_mode as u32, Ordering::Relaxed);
}

fn current_rec_time_us(state: &AppState) -> u64 {
    now_us().saturating_sub(state.rec_start_us.load(Ordering::Relaxed))
}

// ============================================================================
// Playback engine
// ============================================================================

/// Tracks what playback is holding down so nothing can stay stuck.
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

    #[cfg(windows)]
    fn release_all(&mut self, state: &AppState) {
        while let Some((vk, scan, extended)) = self.keys.pop() {
            unsafe {
                send_input_event(
                    &InputEventKind::Key { vk, scan, down: false, extended },
                    state,
                    &mut PressedInputs::default(),
                    (0, 0),
                );
            }
        }
        while let Some(button) = self.buttons.pop() {
            unsafe {
                send_input_event(
                    &InputEventKind::MouseButton { button, down: false, x: 0, y: 0 },
                    state,
                    &mut PressedInputs::default(),
                    (0, 0),
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

/// True when the configured pixel condition currently says "stop".
fn pixel_condition_met(state: &AppState) -> bool {
    if !state.pixel_enabled.load(Ordering::Relaxed) {
        return false;
    }
    let (x, y) = (state.pixel_x.load(Ordering::Relaxed), state.pixel_y.load(Ordering::Relaxed));
    let Some((r, g, b)) = platform::screen_pixel(x, y) else {
        return false;
    };
    let want = state.pixel_rgb.load(Ordering::Relaxed);
    let (wr, wg, wb) = (((want >> 16) & 0xFF) as i32, ((want >> 8) & 0xFF) as i32, (want & 0xFF) as i32);
    let tol = state.pixel_tolerance.load(Ordering::Relaxed) as i32;
    let matches = (r as i32 - wr).abs() <= tol
        && (g as i32 - wg).abs() <= tol
        && (b as i32 - wb).abs() <= tol;
    if state.pixel_mode.load(Ordering::Relaxed) == 0 { matches } else { !matches }
}

/// Sleeps until `due_us`, waking often enough to notice Stop and Pause.
/// Returns false when playback should abort.
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
            return true;
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
    state.pixel_triggered.store(false, Ordering::Relaxed);

    let speed = (*state.speed.lock()).clamp(0.05, 10.0);
    let repeat_delay_us = state.repeat_delay_ms.load(Ordering::Relaxed) * 1_000;
    let jitter_pct = state.jitter_pct.load(Ordering::Relaxed);
    let cycle_us = ((data.cycle_len_us() as f64 / speed) as u64) + repeat_delay_us;

    // Re-anchor absolute coordinates if the target window has moved since recording.
    let offset = match (state.use_window_anchor.load(Ordering::Relaxed), data.anchor.as_ref()) {
        (true, Some(anchor)) => match platform::find_window_rect(&anchor.title) {
            Some((x, y)) => {
                let off = (x - anchor.x, y - anchor.y);
                info!("anchored to '{}': offset {:?}", anchor.title, off);
                off
            }
            None => {
                warn!("anchor window '{}' not found - playing unshifted", anchor.title);
                (0, 0)
            }
        },
        _ => (0, 0),
    };

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
    let mut last_pixel_check: u64 = 0;

    // Playback clock that excludes paused time.
    macro_rules! elapsed_us {
        () => {
            (start.elapsed().as_micros() as u64).saturating_sub(paused_us)
        };
    }

    let mut finish_action: Option<EndAction> = None;

    loop {
        if state.stop_play.load(Ordering::Relaxed)
            || state.play_generation.load(Ordering::Relaxed) != generation
        {
            break;
        }

        // ---- pause / virtual-desktop gate ---------------------------------
        let on_desktop = virtual_desktop::is_app_on_active_desktop_cached(platform::app_hwnd());
        state.held_by_desktop.store(!on_desktop, Ordering::Relaxed);
        if state.paused.load(Ordering::Relaxed) || !on_desktop {
            if pause_started.is_none() {
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

        let now_running = elapsed_us!();

        // ---- pixel stop condition -----------------------------------------
        if state.pixel_enabled.load(Ordering::Relaxed)
            && now_running.saturating_sub(last_pixel_check) >= PIXEL_CHECK_TTL_US
        {
            last_pixel_check = now_running;
            if pixel_condition_met(&state) {
                info!("pixel condition met - stopping");
                state.pixel_triggered.store(true, Ordering::Relaxed);
                finish_action = Some(EndAction::from_index(
                    state.action_on_completion.load(Ordering::Relaxed) as usize,
                ));
                break;
            }
        }

        // ---- time limit ----------------------------------------------------
        if state.time_limit_enabled.load(Ordering::Relaxed) {
            let limit = state.time_limit_us.load(Ordering::Relaxed);
            if limit > 0 && now_running >= limit {
                finish_action = Some(EndAction::from_index(
                    state.action_on_completion.load(Ordering::Relaxed) as usize,
                ));
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
            // Positive-only jitter keeps the order intact and never fires early.
            let gap = scaled_t.saturating_sub(prev_scaled_t);
            let max_off = gap.saturating_mul(jitter_pct) / 100;
            due = due.saturating_add(rng.below(max_off.min(250_000) + 1));
        }

        if !wait_until(&state, generation, due, &|| elapsed_us!()) {
            break;
        }
        if state.paused.load(Ordering::Relaxed) || elapsed_us!() < due {
            continue;
        }

        #[cfg(windows)]
        unsafe {
            send_input_event(&ev.kind, &state, &mut pressed, offset);
        }

        prev_scaled_t = scaled_t;
        index += 1;
    }

    pressed.release_all(&state);
    platform::end_high_res_timer();
    state.held_by_desktop.store(false, Ordering::Relaxed);

    if let Some(action) = finish_action {
        if action != EndAction::Stop {
            let delay = state.shutdown_delay_s.load(Ordering::Relaxed) as u32;
            match platform::run_end_action(action, delay, "Macro Recorder: run finished.") {
                Ok(()) => info!("end action {action:?} requested"),
                Err(e) => warn!("end action failed: {e}"),
            }
        }
    }

    if state.play_generation.load(Ordering::Relaxed) == generation {
        state.paused.store(false, Ordering::Relaxed);
        state.playing.store(false, Ordering::Relaxed);
    }
    info!("playback finished after {count} cycle(s)");
}

#[cfg(windows)]
unsafe fn send_input_event(
    kind: &InputEventKind,
    state: &AppState,
    pressed: &mut PressedInputs,
    offset: (i32, i32),
) {
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
                    platform::send_absolute_mouse_move(*x + offset.0, *y + offset.1);
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
                    platform::send_absolute_mouse_move(*x + offset.0, *y + offset.1);
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
        std::thread::sleep(Duration::from_millis(30)); // let the collector drain
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
    let anchor = if state.record_window_anchor.load(Ordering::Relaxed) {
        platform::foreground_anchor()
    } else {
        None
    };
    {
        let mut data = state.macro_data.lock();
        data.events.clear();
        data.duration_us = 0;
        data.version = 2;
        data.anchor = anchor;
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
    match std::thread::Builder::new()
        .name("playback".into())
        .spawn(move || playback_loop(s, data, generation))
    {
        Ok(_) => info!("playback started (generation {generation})"),
        Err(e) => {
            warn!("failed to spawn playback thread: {e}");
            state.playing.store(false, Ordering::Relaxed);
        }
    }
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
// Tray icon (Windows)
// ============================================================================

#[cfg(windows)]
mod tray {
    use super::win32::*;
    use super::*;
    use std::ffi::c_void;
    use std::sync::atomic::AtomicIsize;

    static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
    static TRAY_ADDED: AtomicBool = AtomicBool::new(false);

    fn icon_handle(hinst: HINSTANCE) -> HICON {
        unsafe {
            // Resource id 1 is what winresource assigns to the embedded icon.
            if let Ok(icon) = LoadIconW(Some(hinst), PCWSTR(1 as *const u16)) {
                if !icon.is_invalid() {
                    return icon;
                }
            }
            LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
        }
    }

    fn notify_data(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        let tip = wide(APP_TITLE);
        let n = tip.len().min(nid.szTip.len());
        nid.szTip[..n].copy_from_slice(&tip[..n]);
        nid
    }

    /// Creates the message-only window that owns the tray icon.
    pub fn init() {
        unsafe {
            let hinst = GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default();
            let class = w!("MacroRecorderTrayWnd");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("Macro Recorder Tray"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(hinst),
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    warn!("tray window could not be created: {e}");
                    return;
                }
            };
            TRAY_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

            let nid = notify_data(hwnd, icon_handle(hinst));
            if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                TRAY_ADDED.store(true, Ordering::Relaxed);
                info!("tray icon added");
            } else {
                warn!("Shell_NotifyIconW(NIM_ADD) failed");
            }
        }
    }

    /// True once the icon is actually in the notification area.
    pub fn is_active() -> bool {
        TRAY_ADDED.load(Ordering::Relaxed)
    }

    pub fn shutdown() {
        if !TRAY_ADDED.swap(false, Ordering::Relaxed) {
            return;
        }
        unsafe {
            let hwnd = HWND(TRAY_HWND.load(Ordering::Relaxed) as *mut c_void);
            let nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            let _ = DestroyWindow(hwnd);
        }
    }

    unsafe fn show_menu(hwnd: HWND) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            let show_label = if WINDOW_VISIBLE.load(Ordering::Relaxed) {
                w!("Hide window")
            } else {
                w!("Show window")
            };
            let _ = AppendMenuW(menu, MF_STRING, TRAY_ID_SHOW as usize, show_label);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, TRAY_ID_RECORD as usize, w!("Record / stop"));
            let _ = AppendMenuW(menu, MF_STRING, TRAY_ID_PLAY as usize, w!("Play / stop"));
            let _ = AppendMenuW(menu, MF_STRING, TRAY_ID_STOP as usize, w!("Emergency stop"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, TRAY_ID_EXIT as usize, w!("Exit"));

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu closes when the user clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                Some(0),
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);

            let state = GLOBAL_STATE.get();
            match cmd.0 as u32 {
                TRAY_ID_SHOW => toggle_main_window(),
                TRAY_ID_RECORD => {
                    if let Some(s) = state {
                        toggle_recording(s);
                    }
                }
                TRAY_ID_PLAY => {
                    if let Some(s) = state {
                        toggle_playback(s);
                    }
                }
                TRAY_ID_STOP => {
                    if let Some(s) = state {
                        stop_everything(s);
                    }
                }
                TRAY_ID_EXIT => quit_application(),
                _ => {}
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        unsafe {
            if msg == WM_APP_TRAY {
                match lp.0 as u32 {
                    0x0202 => toggle_main_window(), // WM_LBUTTONUP
                    0x0205 | 0x007B => show_menu(hwnd), // WM_RBUTTONUP / WM_CONTEXTMENU
                    _ => {}
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

#[cfg(not(windows))]
mod tray {
    pub fn init() {}
    pub fn shutdown() {}
    pub fn is_active() -> bool {
        false
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
    /// Headless playback: hotkeys only.
    HotkeysOnly,
}

#[cfg(windows)]
unsafe fn register_hotkeys() {
    use win32::*;
    let hk = *PENDING_HOTKEYS.lock();
    let ids = [HK_ID_RECORD, HK_ID_PLAY, HK_ID_STOP, HK_ID_PAUSE];
    let mut failed = 0u32;
    unsafe {
        for (idx, id) in ids.into_iter().enumerate() {
            let _ = UnregisterHotKey(None, id);
            let key = hk[idx];
            if key.vk == 0 {
                continue; // unbound
            }
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
fn input_hook_thread(state: Arc<AppState>, mode: HookMode, with_tray: bool) {
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
        if with_tray {
            tray::init();
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            match msg.message {
                WM_HOTKEY_ID => match msg.wParam.0 as i32 {
                    HK_ID_RECORD => toggle_recording(&state),
                    HK_ID_PLAY => toggle_playback(&state),
                    HK_ID_STOP => stop_everything(&state),
                    HK_ID_PAUSE => toggle_pause(&state),
                    _ => {}
                },
                WM_APP_REHOTKEY => register_hotkeys(),
                WM_APP_HK_OFF => {
                    for id in [HK_ID_RECORD, HK_ID_PLAY, HK_ID_STOP, HK_ID_PAUSE] {
                        let _ = UnregisterHotKey(None, id);
                    }
                    HK_FAILED.store(0, Ordering::Relaxed);
                }
                _ => {
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
            }
        }

        tray::shutdown();
        for id in [HK_ID_RECORD, HK_ID_PLAY, HK_ID_STOP, HK_ID_PAUSE] {
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

/// Cheap by construction: one atomic load plus a cached desktop answer.
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

/// Handles "press any key to bind". Returns true if the key was consumed.
#[cfg(windows)]
unsafe fn handle_key_capture(vk: u32, down: bool) -> bool {
    use win32::*;
    if CAPTURE_SLOT.load(Ordering::Relaxed) == 0 {
        return false;
    }
    if !down {
        return true; // swallow the matching key-up too
    }
    // Modifiers alone are not a binding.
    if matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C) {
        return true;
    }
    if vk == 0x1B {
        CAPTURE_SLOT.store(0, Ordering::Relaxed); // Esc cancels
        return true;
    }
    unsafe {
        // GetAsyncKeyState, not GetKeyState: the hook thread has no input queue of
        // its own, so the synchronous variant would always report "not pressed".
        let down_state = |k: i32| (GetAsyncKeyState(k) as u16 & 0x8000) != 0;
        *CAPTURED_KEY.lock() = Some(Hotkey {
            vk,
            ctrl: down_state(0x11),
            alt: down_state(0x12),
            shift: down_state(0x10),
        });
    }
    true
}

#[cfg(windows)]
unsafe extern "system" fn kb_proc(
    code: i32,
    wp: win32::WPARAM,
    lp: win32::LPARAM,
) -> win32::LRESULT {
    use win32::*;
    if code == 0 && lp.0 != 0 {
        unsafe {
            let data = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            if data.flags.0 & LLKHF_INJECTED.0 == 0 {
                let wm = wp.0 as u32;
                let (down, valid) = match wm {
                    0x0100 | 0x0104 => (true, true),
                    0x0101 | 0x0105 => (false, true),
                    _ => (false, false),
                };
                if valid {
                    // Binding mode swallows the key so it never reaches the app below.
                    if handle_key_capture(data.vkCode, down) {
                        return LRESULT(1);
                    }
                    if !is_hotkey_vk(data.vkCode) {
                        if let Some(state) = should_record() {
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
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

#[cfg(windows)]
unsafe extern "system" fn ms_proc(
    code: i32,
    wp: win32::WPARAM,
    lp: win32::LPARAM,
) -> win32::LRESULT {
    use win32::*;
    if code == 0 && lp.0 != 0 {
        if let Some(state) = should_record() {
            unsafe {
                let data = &*(lp.0 as *const MSLLHOOKSTRUCT);
                if data.flags & LLMHF_INJECTED == 0 {
                    let (x, y) = (data.pt.x, data.pt.y);
                    let kind = match wp.0 as u32 {
                        0x0200 => {
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
                        // WM_XBUTTONDOWN / WM_XBUTTONUP: index is the high word.
                        0x020B | 0x020C => {
                            let down = wp.0 as u32 == 0x020B;
                            let which = (data.mouseData >> 16) & 0xFFFF;
                            let button =
                                if which == 2 { MouseButton::X2 } else { MouseButton::X1 };
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

/// Generates the `Strings` struct plus the plumbing for external translation files,
/// so adding a field costs exactly one line here.
macro_rules! define_strings {
    ($($field:ident),* $(,)?) => {
        #[derive(Clone, Copy)]
        pub struct Strings { $(pub $field: &'static str),* }

        impl Strings {
            /// Applies key/value overrides loaded from `lang/<code>.json`.
            fn with_overrides(mut self, map: &BTreeMap<String, String>) -> Self {
                $(
                    if let Some(v) = map.get(stringify!($field)) {
                        if !v.is_empty() {
                            // Leaked on purpose: language tables live for the whole process.
                            self.$field = Box::leak(v.clone().into_boxed_str());
                        }
                    }
                )*
                self
            }
            /// Dumps the table so users can start a translation from a filled template.
            fn to_map(&self) -> BTreeMap<&'static str, &'static str> {
                let mut m = BTreeMap::new();
                $( m.insert(stringify!($field), self.$field); )*
                m
            }
        }
    };
}

define_strings!(
    record, stop_rec, play, pause, resume, stop_play,
    rec_time, rec_done, play_inf, play_lim, events, duration,
    status_ready, status_rec, status_play, status_paused, status_held, status_pixel,
    sec_playback, sec_recording, sec_limit, sec_pixel, sec_appearance, sec_hotkeys,
    sec_files, sec_editor, sec_profiles,
    loop_cb, play_count, speed, repeat_delay, jitter, abs_mouse, anchor_use,
    capture_moves, sample_rate, anchor_rec, anchor_of, anchor_none,
    time_limit_cb, time_limit_h, time_limit_m, time_limit_s, action_on_limit,
    action_stop, action_shutdown, action_reboot, action_sleep, action_hibernate, action_logoff,
    shutdown_delay,
    pixel_cb, pixel_pick, pixel_picking, pixel_tol, pixel_match, pixel_differ,
    theme, language, lang_auto, transparent_ui, on_top, tray_cb, close_tray_cb, lang_template,
    hk_record, hk_play, hk_pause, hk_stop, hk_failed, hk_bind, hk_press, hk_clear,
    save, save_as, load, open_file, clear, recent, compress, data_dir, save_settings,
    export_exe, export_ahk,
    ed_from, ed_to, ed_delete, ed_crop, ed_insert, ed_scale, ed_drop_moves, ed_zero, ed_undo,
    prof_name, prof_save, prof_load, prof_delete,
    saved, loaded, cleared, settings_saved, save_err, load_err, no_macro, exported, done,
    ed_open, ed_title, ed_human, ed_raw, ed_selected, ed_steps,
    step_wait, step_move, step_click, step_dbl, step_drag, step_scroll, step_type,
    step_key, step_hold,
    dir_up, dir_down, dir_left, dir_right, btn_l, btn_r, btn_m,
    insp_title, insp_none, insp_time, insp_key, insp_delta, insp_horiz, insp_extended,
    insp_dup, insp_del_one, st_down, st_up, bulk_replace, bulk_shift,
);

const EN: Strings = Strings {
    record: "🔴 Record", stop_rec: "⏹ Stop rec", play: "▶ Play", pause: "⏸ Pause",
    resume: "▶ Resume", stop_play: "⏹ Stop",
    rec_time: "⏱ Recording: {}…", rec_done: "⏱ Recorded: {}", play_inf: "🔄 Plays: {} (∞)",
    play_lim: "🔄 Plays: {} / {}", events: "📦 Events: {}", duration: "⏳ Length: {}",
    status_ready: "Ready", status_rec: "Recording…", status_play: "Playing…",
    status_paused: "Paused", status_held: "Held — another virtual desktop",
    status_pixel: "Stopped by the pixel condition",
    sec_playback: "▶ Playback", sec_recording: "🎬 Recording", sec_limit: "⏱ Time limit",
    sec_pixel: "🎯 Pixel condition", sec_appearance: "🎨 Appearance", sec_hotkeys: "⌨ Hotkeys",
    sec_files: "📁 Files", sec_editor: "✂ Editor", sec_profiles: "📋 Profiles",
    loop_cb: "Loop playback", play_count: "Play count:", speed: "Speed",
    repeat_delay: "Delay between loops (ms)", jitter: "Timing jitter (%)",
    abs_mouse: "Absolute mouse (DPI fix)", anchor_use: "Follow the anchored window",
    capture_moves: "Capture mouse movement", sample_rate: "Movement sampling (ms)",
    anchor_rec: "Remember the target window", anchor_of: "⚓ Anchor: {}", anchor_none: "none",
    time_limit_cb: "Stop after time limit", time_limit_h: "H", time_limit_m: "M",
    time_limit_s: "S", action_on_limit: "Then:",
    action_stop: "Stop", action_shutdown: "Shut down", action_reboot: "Restart",
    action_sleep: "Sleep", action_hibernate: "Hibernate", action_logoff: "Log off",
    shutdown_delay: "Shutdown countdown (s)",
    pixel_cb: "Stop on a screen pixel", pixel_pick: "🎯 Pick in 3 s",
    pixel_picking: "Hover the target… {} s", pixel_tol: "Tolerance",
    pixel_match: "when it matches", pixel_differ: "when it differs",
    theme: "Theme:", language: "Language:", lang_auto: "Auto (system)",
    transparent_ui: "🌓 Transparent UI", on_top: "📌 Always on Top",
    tray_cb: "Tray icon", close_tray_cb: "Close button minimizes to tray",
    lang_template: "🌍 Export language template",
    hk_record: "Record:", hk_play: "Play / stop:", hk_pause: "Pause:",
    hk_stop: "Emergency stop:", hk_failed: "⚠ Some hotkeys are taken by another app",
    hk_bind: "Bind", hk_press: "press a key… (Esc cancels)", hk_clear: "Clear",
    save: "💾 Save", save_as: "💾 Save as…", load: "📂 Load", open_file: "📂 Open…",
    clear: "🗑 Clear", recent: "Recent:", compress: "Compress macros (.mrz)",
    data_dir: "📁 Data folder:", save_settings: "💾 Save settings",
    export_exe: "⚙ Export .exe", export_ahk: "📜 Export .ahk",
    ed_from: "from", ed_to: "to", ed_delete: "Delete", ed_crop: "Keep only",
    ed_insert: "Insert pause (ms)", ed_scale: "Scale time ×", ed_drop_moves: "Drop moves",
    ed_zero: "Trim lead-in", ed_undo: "⏪ Undo",
    prof_name: "Name:", prof_save: "Save", prof_load: "Load", prof_delete: "Delete",
    saved: "Saved: {}", loaded: "Loaded: {}", cleared: "Macro cleared",
    settings_saved: "Settings saved", save_err: "Save error: {}", load_err: "Load error: {}",
    no_macro: "No macro loaded", exported: "Exported: {}", done: "Done",
    ed_open: "✂ Open editor", ed_title: "Macro editor",
    ed_human: "Story", ed_raw: "Raw events",
    ed_selected: "Selected: {} … {}", ed_steps: "{} steps",
    step_wait: "Waited {}", step_move: "Moved the cursor to {}",
    step_click: "{} click at {}", step_dbl: "{} double click at {}",
    step_drag: "Dragged with {} from {} to {}", step_scroll: "Scrolled {} — {} notches",
    step_type: "Typed \u{201c}{}\u{201d}", step_key: "Pressed {}", step_hold: "Held {} for {}",
    dir_up: "up", dir_down: "down", dir_left: "left", dir_right: "right",
    btn_l: "Left", btn_r: "Right", btn_m: "Middle",
    insp_title: "Selected action", insp_none: "Click a line to edit it",
    insp_time: "At (ms)", insp_key: "Key", insp_delta: "Delta", insp_horiz: "Horizontal",
    insp_extended: "Extended", insp_dup: "Duplicate", insp_del_one: "Delete action",
    st_down: "press", st_up: "release",
    bulk_replace: "Replace in selection", bulk_shift: "Shift coordinates",
};

const RU: Strings = Strings {
    record: "🔴 Запись", stop_rec: "⏹ Стоп запись", play: "▶ Воспроизвести", pause: "⏸ Пауза",
    resume: "▶ Продолжить", stop_play: "⏹ Стоп",
    rec_time: "⏱ Запись: {}…", rec_done: "⏱ Записано: {}", play_inf: "🔄 Проигрываний: {} (∞)",
    play_lim: "🔄 Проигрываний: {} / {}", events: "📦 Событий: {}", duration: "⏳ Длительность: {}",
    status_ready: "Готов", status_rec: "Идёт запись…", status_play: "Воспроизведение…",
    status_paused: "Пауза", status_held: "Удержание — другой рабочий стол",
    status_pixel: "Остановлено по условию пикселя",
    sec_playback: "▶ Воспроизведение", sec_recording: "🎬 Запись", sec_limit: "⏱ Лимит времени",
    sec_pixel: "🎯 Условие по пикселю", sec_appearance: "🎨 Оформление",
    sec_hotkeys: "⌨ Горячие клавиши", sec_files: "📁 Файлы", sec_editor: "✂ Редактор",
    sec_profiles: "📋 Профили",
    loop_cb: "Циклическое воспроизведение", play_count: "Проигрываний:", speed: "Скорость",
    repeat_delay: "Пауза между циклами (мс)", jitter: "Джиттер таймингов (%)",
    abs_mouse: "Абсолютная мышь (фикс DPI)", anchor_use: "Следовать за окном привязки",
    capture_moves: "Записывать движения мыши", sample_rate: "Шаг выборки движений (мс)",
    anchor_rec: "Запоминать целевое окно", anchor_of: "⚓ Привязка: {}", anchor_none: "нет",
    time_limit_cb: "Остановиться по таймеру", time_limit_h: "Ч", time_limit_m: "М",
    time_limit_s: "С", action_on_limit: "Затем:",
    action_stop: "Остановить", action_shutdown: "Выключить", action_reboot: "Перезагрузить",
    action_sleep: "Сон", action_hibernate: "Гибернация", action_logoff: "Выйти из системы",
    shutdown_delay: "Отсчёт до выключения (с)",
    pixel_cb: "Останавливаться по пикселю экрана", pixel_pick: "🎯 Взять через 3 с",
    pixel_picking: "Наведите курсор… {} с", pixel_tol: "Допуск",
    pixel_match: "когда совпадает", pixel_differ: "когда отличается",
    theme: "Тема:", language: "Язык:", lang_auto: "Авто (система)",
    transparent_ui: "🌓 Прозрачный интерфейс", on_top: "📌 Поверх всех окон",
    tray_cb: "Значок в трее", close_tray_cb: "Крестик сворачивает в трей",
    lang_template: "🌍 Выгрузить шаблон перевода",
    hk_record: "Запись:", hk_play: "Плей / стоп:", hk_pause: "Пауза:",
    hk_stop: "Аварийный стоп:", hk_failed: "⚠ Часть клавиш занята другой программой",
    hk_bind: "Задать", hk_press: "нажмите клавишу… (Esc — отмена)", hk_clear: "Сбросить",
    save: "💾 Сохранить", save_as: "💾 Сохранить как…", load: "📂 Загрузить",
    open_file: "📂 Открыть…", clear: "🗑 Очистить", recent: "Недавние:",
    compress: "Сжимать макросы (.mrz)", data_dir: "📁 Папка данных:",
    save_settings: "💾 Сохранить настройки",
    export_exe: "⚙ Экспорт в .exe", export_ahk: "📜 Экспорт в .ahk",
    ed_from: "с", ed_to: "по", ed_delete: "Удалить", ed_crop: "Оставить только",
    ed_insert: "Вставить паузу (мс)", ed_scale: "Масштаб времени ×",
    ed_drop_moves: "Убрать движения", ed_zero: "Обрезать начало", ed_undo: "⏪ Отменить",
    prof_name: "Имя:", prof_save: "Сохранить", prof_load: "Загрузить", prof_delete: "Удалить",
    saved: "Сохранено: {}", loaded: "Загружено: {}", cleared: "Макрос очищен",
    settings_saved: "Настройки сохранены", save_err: "Ошибка сохранения: {}",
    load_err: "Ошибка загрузки: {}", no_macro: "Макрос не загружен",
    exported: "Экспортировано: {}", done: "Готово",
    ed_open: "✂ Открыть редактор", ed_title: "Редактор макроса",
    ed_human: "Рассказ", ed_raw: "Сырые события",
    ed_selected: "Выбрано: {} … {}", ed_steps: "шагов: {}",
    step_wait: "Пауза {}", step_move: "Курсор переехал в {}",
    step_click: "{} клик в {}", step_dbl: "Двойной {} клик в {}",
    step_drag: "Протащил {} из {} в {}", step_scroll: "Прокрутка {} — {} щелчков",
    step_type: "Набрано \u{201c}{}\u{201d}", step_key: "Нажата {}", step_hold: "{} удерживалась {}",
    dir_up: "вверх", dir_down: "вниз", dir_left: "влево", dir_right: "вправо",
    btn_l: "ЛКМ", btn_r: "ПКМ", btn_m: "СКМ",
    insp_title: "Выбранное действие", insp_none: "Кликните по строке, чтобы её изменить",
    insp_time: "Момент (мс)", insp_key: "Клавиша", insp_delta: "Дельта", insp_horiz: "Горизонтально",
    insp_extended: "Расширенная", insp_dup: "Дублировать", insp_del_one: "Удалить действие",
    st_down: "нажатие", st_up: "отпускание",
    bulk_replace: "Заменить в выделении", bulk_shift: "Сдвинуть координаты",
};

const UK: Strings = Strings {
    record: "🔴 Запис", stop_rec: "⏹ Стоп запис", play: "▶ Відтворити", pause: "⏸ Пауза",
    resume: "▶ Продовжити", stop_play: "⏹ Стоп",
    rec_time: "⏱ Запис: {}…", rec_done: "⏱ Записано: {}", play_inf: "🔄 Відтворень: {} (∞)",
    play_lim: "🔄 Відтворень: {} / {}", events: "📦 Подій: {}", duration: "⏳ Тривалість: {}",
    status_ready: "Готово", status_rec: "Триває запис…", status_play: "Відтворення…",
    status_paused: "Пауза", status_held: "Утримання — інший робочий стіл",
    status_pixel: "Зупинено за умовою пікселя",
    sec_playback: "▶ Відтворення", sec_recording: "🎬 Запис", sec_limit: "⏱ Ліміт часу",
    sec_pixel: "🎯 Умова за пікселем", sec_appearance: "🎨 Оформлення",
    sec_hotkeys: "⌨ Гарячі клавіші", sec_files: "📁 Файли", sec_editor: "✂ Редактор",
    sec_profiles: "📋 Профілі",
    loop_cb: "Циклічне відтворення", play_count: "Відтворень:", speed: "Швидкість",
    repeat_delay: "Пауза між циклами (мс)", jitter: "Джитер таймінгів (%)",
    abs_mouse: "Абсолютна миша (фікс DPI)", anchor_use: "Слідувати за вікном прив'язки",
    capture_moves: "Записувати рухи миші", sample_rate: "Крок вибірки рухів (мс)",
    anchor_rec: "Запам'ятовувати цільове вікно", anchor_of: "⚓ Прив'язка: {}",
    anchor_none: "немає",
    time_limit_cb: "Зупинитися за таймером", time_limit_h: "Г", time_limit_m: "Х",
    time_limit_s: "С", action_on_limit: "Потім:",
    action_stop: "Зупинити", action_shutdown: "Вимкнути", action_reboot: "Перезавантажити",
    action_sleep: "Сон", action_hibernate: "Гібернація", action_logoff: "Вийти з системи",
    shutdown_delay: "Відлік до вимкнення (с)",
    pixel_cb: "Зупинятися за пікселем екрана", pixel_pick: "🎯 Взяти через 3 с",
    pixel_picking: "Наведіть курсор… {} с", pixel_tol: "Допуск",
    pixel_match: "коли збігається", pixel_differ: "коли відрізняється",
    theme: "Тема:", language: "Мова:", lang_auto: "Авто (система)",
    transparent_ui: "🌓 Прозорий інтерфейс", on_top: "📌 Завжди поверх вікон",
    tray_cb: "Значок у треї", close_tray_cb: "Хрестик згортає у трей",
    lang_template: "🌍 Вивантажити шаблон перекладу",
    hk_record: "Запис:", hk_play: "Плей / стоп:", hk_pause: "Пауза:",
    hk_stop: "Аварійний стоп:", hk_failed: "⚠ Частину клавіш зайнято іншою програмою",
    hk_bind: "Задати", hk_press: "натисніть клавішу… (Esc — скасувати)", hk_clear: "Скинути",
    save: "💾 Зберегти", save_as: "💾 Зберегти як…", load: "📂 Завантажити",
    open_file: "📂 Відкрити…", clear: "🗑 Очистити", recent: "Нещодавні:",
    compress: "Стискати макроси (.mrz)", data_dir: "📁 Тека даних:",
    save_settings: "💾 Зберегти налаштування",
    export_exe: "⚙ Експорт у .exe", export_ahk: "📜 Експорт у .ahk",
    ed_from: "з", ed_to: "по", ed_delete: "Видалити", ed_crop: "Залишити лише",
    ed_insert: "Вставити паузу (мс)", ed_scale: "Масштаб часу ×",
    ed_drop_moves: "Прибрати рухи", ed_zero: "Обрізати початок", ed_undo: "⏪ Скасувати",
    prof_name: "Ім'я:", prof_save: "Зберегти", prof_load: "Завантажити", prof_delete: "Видалити",
    saved: "Збережено: {}", loaded: "Завантажено: {}", cleared: "Макрос очищено",
    settings_saved: "Налаштування збережено", save_err: "Помилка збереження: {}",
    load_err: "Помилка завантаження: {}", no_macro: "Макрос не завантажено",
    exported: "Експортовано: {}", done: "Готово",
    ed_open: "✂ Відкрити редактор", ed_title: "Редактор макроса",
    ed_human: "Розповідь", ed_raw: "Сирі події",
    ed_selected: "Вибрано: {} … {}", ed_steps: "кроків: {}",
    step_wait: "Пауза {}", step_move: "Курсор переїхав у {}",
    step_click: "{} клік у {}", step_dbl: "Подвійний {} клік у {}",
    step_drag: "Протягнув {} з {} у {}", step_scroll: "Прокрутка {} — {} клацань",
    step_type: "Набрано \u{201c}{}\u{201d}", step_key: "Натиснуто {}", step_hold: "{} утримувалась {}",
    dir_up: "вгору", dir_down: "вниз", dir_left: "вліво", dir_right: "вправо",
    btn_l: "ЛКМ", btn_r: "ПКМ", btn_m: "СКМ",
    insp_title: "Вибрана дія", insp_none: "Клацніть рядок, щоб змінити його",
    insp_time: "Момент (мс)", insp_key: "Клавіша", insp_delta: "Дельта", insp_horiz: "Горизонтально",
    insp_extended: "Розширена", insp_dup: "Дублювати", insp_del_one: "Видалити дію",
    st_down: "натискання", st_up: "відпускання",
    bulk_replace: "Замінити у виділенні", bulk_shift: "Зсунути координати",
};

const PT: Strings = Strings {
    record: "🔴 Gravar", stop_rec: "⏹ Parar grav", play: "▶ Tocar", pause: "⏸ Pausar",
    resume: "▶ Retomar", stop_play: "⏹ Parar",
    rec_time: "⏱ Gravando: {}…", rec_done: "⏱ Gravado: {}", play_inf: "🔄 Reproduções: {} (∞)",
    play_lim: "🔄 Reproduções: {} / {}", events: "📦 Eventos: {}", duration: "⏳ Duração: {}",
    status_ready: "Pronto", status_rec: "Gravando…", status_play: "Reproduzindo…",
    status_paused: "Pausado", status_held: "Em espera — outra área de trabalho",
    status_pixel: "Parado pela condição de pixel",
    sec_playback: "▶ Reprodução", sec_recording: "🎬 Gravação", sec_limit: "⏱ Limite de tempo",
    sec_pixel: "🎯 Condição de pixel", sec_appearance: "🎨 Aparência", sec_hotkeys: "⌨ Atalhos",
    sec_files: "📁 Arquivos", sec_editor: "✂ Editor", sec_profiles: "📋 Perfis",
    loop_cb: "Reprodução em loop", play_count: "Contagem:", speed: "Velocidade",
    repeat_delay: "Pausa entre loops (ms)", jitter: "Variação de tempo (%)",
    abs_mouse: "Mouse absoluto (fix DPI)", anchor_use: "Seguir a janela ancorada",
    capture_moves: "Gravar movimento do mouse", sample_rate: "Amostragem (ms)",
    anchor_rec: "Lembrar a janela alvo", anchor_of: "⚓ Âncora: {}", anchor_none: "nenhuma",
    time_limit_cb: "Parar após o limite", time_limit_h: "H", time_limit_m: "M",
    time_limit_s: "S", action_on_limit: "Depois:",
    action_stop: "Parar", action_shutdown: "Desligar", action_reboot: "Reiniciar",
    action_sleep: "Suspender", action_hibernate: "Hibernar", action_logoff: "Sair da sessão",
    shutdown_delay: "Contagem para desligar (s)",
    pixel_cb: "Parar por um pixel da tela", pixel_pick: "🎯 Capturar em 3 s",
    pixel_picking: "Aponte o cursor… {} s", pixel_tol: "Tolerância",
    pixel_match: "quando coincidir", pixel_differ: "quando diferir",
    theme: "Tema:", language: "Idioma:", lang_auto: "Auto (sistema)",
    transparent_ui: "🌓 Interface transparente", on_top: "📌 Sempre no topo",
    tray_cb: "Ícone na bandeja", close_tray_cb: "Fechar minimiza para a bandeja",
    lang_template: "🌍 Exportar modelo de idioma",
    hk_record: "Gravar:", hk_play: "Tocar / parar:", hk_pause: "Pausar:",
    hk_stop: "Parada de emergência:", hk_failed: "⚠ Alguns atalhos estão ocupados",
    hk_bind: "Definir", hk_press: "pressione uma tecla… (Esc cancela)", hk_clear: "Limpar",
    save: "💾 Salvar", save_as: "💾 Salvar como…", load: "📂 Carregar", open_file: "📂 Abrir…",
    clear: "🗑 Limpar", recent: "Recentes:", compress: "Comprimir macros (.mrz)",
    data_dir: "📁 Pasta de dados:", save_settings: "💾 Salvar configurações",
    export_exe: "⚙ Exportar .exe", export_ahk: "📜 Exportar .ahk",
    ed_from: "de", ed_to: "até", ed_delete: "Excluir", ed_crop: "Manter só",
    ed_insert: "Inserir pausa (ms)", ed_scale: "Escalar tempo ×",
    ed_drop_moves: "Remover movimentos", ed_zero: "Cortar início", ed_undo: "⏪ Desfazer",
    prof_name: "Nome:", prof_save: "Salvar", prof_load: "Carregar", prof_delete: "Excluir",
    saved: "Salvo: {}", loaded: "Carregado: {}", cleared: "Macro limpo",
    settings_saved: "Configurações salvas", save_err: "Erro ao salvar: {}",
    load_err: "Erro ao carregar: {}", no_macro: "Nenhum macro carregado",
    exported: "Exportado: {}", done: "Pronto",
    ed_open: "✂ Abrir editor", ed_title: "Editor de macro",
    ed_human: "Narrativa", ed_raw: "Eventos brutos",
    ed_selected: "Selecionado: {} … {}", ed_steps: "{} passos",
    step_wait: "Esperou {}", step_move: "Moveu o cursor para {}",
    step_click: "Clique {} em {}", step_dbl: "Clique duplo {} em {}",
    step_drag: "Arrastou com {} de {} para {}", step_scroll: "Rolou {} — {} entalhes",
    step_type: "Digitou \u{201c}{}\u{201d}", step_key: "Pressionou {}", step_hold: "Segurou {} por {}",
    dir_up: "para cima", dir_down: "para baixo", dir_left: "à esquerda", dir_right: "à direita",
    btn_l: "Esquerdo", btn_r: "Direito", btn_m: "Meio",
    insp_title: "Ação selecionada", insp_none: "Clique numa linha para editá-la",
    insp_time: "Momento (ms)", insp_key: "Tecla", insp_delta: "Delta", insp_horiz: "Horizontal",
    insp_extended: "Estendida", insp_dup: "Duplicar", insp_del_one: "Excluir ação",
    st_down: "pressionar", st_up: "soltar",
    bulk_replace: "Substituir na seleção", bulk_shift: "Deslocar coordenadas",
};

const ES: Strings = Strings {
    record: "🔴 Grabar", stop_rec: "⏹ Detener grab", play: "▶ Reproducir", pause: "⏸ Pausar",
    resume: "▶ Reanudar", stop_play: "⏹ Detener",
    rec_time: "⏱ Grabando: {}…", rec_done: "⏱ Grabado: {}",
    play_inf: "🔄 Reproducciones: {} (∞)", play_lim: "🔄 Reproducciones: {} / {}",
    events: "📦 Eventos: {}", duration: "⏳ Duración: {}",
    status_ready: "Listo", status_rec: "Grabando…", status_play: "Reproduciendo…",
    status_paused: "En pausa", status_held: "En espera — otro escritorio",
    status_pixel: "Detenido por la condición de píxel",
    sec_playback: "▶ Reproducción", sec_recording: "🎬 Grabación",
    sec_limit: "⏱ Límite de tiempo", sec_pixel: "🎯 Condición de píxel",
    sec_appearance: "🎨 Apariencia", sec_hotkeys: "⌨ Atajos", sec_files: "📁 Archivos",
    sec_editor: "✂ Editor", sec_profiles: "📋 Perfiles",
    loop_cb: "Reproducción en bucle", play_count: "Repeticiones:", speed: "Velocidad",
    repeat_delay: "Pausa entre bucles (ms)", jitter: "Variación de tiempo (%)",
    abs_mouse: "Ratón absoluto (fijo DPI)", anchor_use: "Seguir la ventana anclada",
    capture_moves: "Grabar movimiento del ratón", sample_rate: "Muestreo (ms)",
    anchor_rec: "Recordar la ventana objetivo", anchor_of: "⚓ Ancla: {}", anchor_none: "ninguna",
    time_limit_cb: "Detener tras el límite", time_limit_h: "H", time_limit_m: "M",
    time_limit_s: "S", action_on_limit: "Luego:",
    action_stop: "Detener", action_shutdown: "Apagar", action_reboot: "Reiniciar",
    action_sleep: "Suspender", action_hibernate: "Hibernar", action_logoff: "Cerrar sesión",
    shutdown_delay: "Cuenta atrás de apagado (s)",
    pixel_cb: "Detener por un píxel de pantalla", pixel_pick: "🎯 Capturar en 3 s",
    pixel_picking: "Apunta el cursor… {} s", pixel_tol: "Tolerancia",
    pixel_match: "cuando coincida", pixel_differ: "cuando difiera",
    theme: "Tema:", language: "Idioma:", lang_auto: "Auto (sistema)",
    transparent_ui: "🌓 Interfaz transparente", on_top: "📌 Siempre encima",
    tray_cb: "Icono en la bandeja", close_tray_cb: "Cerrar minimiza a la bandeja",
    lang_template: "🌍 Exportar plantilla de idioma",
    hk_record: "Grabar:", hk_play: "Reproducir / detener:", hk_pause: "Pausar:",
    hk_stop: "Parada de emergencia:", hk_failed: "⚠ Algunos atajos están ocupados",
    hk_bind: "Asignar", hk_press: "pulsa una tecla… (Esc cancela)", hk_clear: "Borrar",
    save: "💾 Guardar", save_as: "💾 Guardar como…", load: "📂 Cargar", open_file: "📂 Abrir…",
    clear: "🗑 Limpiar", recent: "Recientes:", compress: "Comprimir macros (.mrz)",
    data_dir: "📁 Carpeta de datos:", save_settings: "💾 Guardar ajustes",
    export_exe: "⚙ Exportar .exe", export_ahk: "📜 Exportar .ahk",
    ed_from: "de", ed_to: "a", ed_delete: "Eliminar", ed_crop: "Conservar sólo",
    ed_insert: "Insertar pausa (ms)", ed_scale: "Escalar tiempo ×",
    ed_drop_moves: "Quitar movimientos", ed_zero: "Recortar inicio", ed_undo: "⏪ Deshacer",
    prof_name: "Nombre:", prof_save: "Guardar", prof_load: "Cargar", prof_delete: "Eliminar",
    saved: "Guardado: {}", loaded: "Cargado: {}", cleared: "Macro borrado",
    settings_saved: "Ajustes guardados", save_err: "Error al guardar: {}",
    load_err: "Error al cargar: {}", no_macro: "Ningún macro cargado",
    exported: "Exportado: {}", done: "Listo",
    ed_open: "✂ Abrir editor", ed_title: "Editor de macro",
    ed_human: "Relato", ed_raw: "Eventos en bruto",
    ed_selected: "Seleccionado: {} … {}", ed_steps: "{} pasos",
    step_wait: "Esperó {}", step_move: "Movió el cursor a {}",
    step_click: "Clic {} en {}", step_dbl: "Doble clic {} en {}",
    step_drag: "Arrastró con {} de {} a {}", step_scroll: "Desplazó {} — {} muescas",
    step_type: "Escribió \u{201c}{}\u{201d}", step_key: "Pulsó {}", step_hold: "Mantuvo {} durante {}",
    dir_up: "arriba", dir_down: "abajo", dir_left: "izquierda", dir_right: "derecha",
    btn_l: "Izquierdo", btn_r: "Derecho", btn_m: "Central",
    insp_title: "Acción seleccionada", insp_none: "Haz clic en una línea para editarla",
    insp_time: "Momento (ms)", insp_key: "Tecla", insp_delta: "Delta", insp_horiz: "Horizontal",
    insp_extended: "Extendida", insp_dup: "Duplicar", insp_del_one: "Eliminar acción",
    st_down: "pulsar", st_up: "soltar",
    bulk_replace: "Reemplazar en la selección", bulk_shift: "Desplazar coordenadas",
};

const ZH: Strings = Strings {
    record: "🔴 录制", stop_rec: "⏹ 停止录制", play: "▶ 播放", pause: "⏸ 暂停",
    resume: "▶ 继续", stop_play: "⏹ 停止",
    rec_time: "⏱ 录制中: {}…", rec_done: "⏱ 已录制: {}", play_inf: "🔄 播放次数: {} (∞)",
    play_lim: "🔄 播放次数: {} / {}", events: "📦 事件: {}", duration: "⏳ 时长: {}",
    status_ready: "就绪", status_rec: "录制中…", status_play: "播放中…",
    status_paused: "已暂停", status_held: "已挂起 — 其他虚拟桌面",
    status_pixel: "已按像素条件停止",
    sec_playback: "▶ 播放", sec_recording: "🎬 录制", sec_limit: "⏱ 时间限制",
    sec_pixel: "🎯 像素条件", sec_appearance: "🎨 外观", sec_hotkeys: "⌨ 快捷键",
    sec_files: "📁 文件", sec_editor: "✂ 编辑器", sec_profiles: "📋 配置",
    loop_cb: "循环播放", play_count: "播放次数:", speed: "速度",
    repeat_delay: "循环间隔 (毫秒)", jitter: "时间抖动 (%)",
    abs_mouse: "绝对鼠标 (修复DPI)", anchor_use: "跟随锚定窗口",
    capture_moves: "记录鼠标移动", sample_rate: "移动采样 (毫秒)",
    anchor_rec: "记住目标窗口", anchor_of: "⚓ 锚点: {}", anchor_none: "无",
    time_limit_cb: "到达时限后停止", time_limit_h: "时", time_limit_m: "分",
    time_limit_s: "秒", action_on_limit: "然后:",
    action_stop: "停止", action_shutdown: "关机", action_reboot: "重启",
    action_sleep: "睡眠", action_hibernate: "休眠", action_logoff: "注销",
    shutdown_delay: "关机倒计时 (秒)",
    pixel_cb: "按屏幕像素停止", pixel_pick: "🎯 3 秒后取色",
    pixel_picking: "请将光标移到目标… {} 秒", pixel_tol: "容差",
    pixel_match: "当匹配时", pixel_differ: "当不匹配时",
    theme: "主题:", language: "语言:", lang_auto: "自动 (系统)",
    transparent_ui: "🌓 透明界面", on_top: "📌 始终置顶",
    tray_cb: "托盘图标", close_tray_cb: "关闭按钮最小化到托盘",
    lang_template: "🌍 导出语言模板",
    hk_record: "录制:", hk_play: "播放 / 停止:", hk_pause: "暂停:",
    hk_stop: "紧急停止:", hk_failed: "⚠ 部分快捷键被其他程序占用",
    hk_bind: "设置", hk_press: "请按一个键…（Esc 取消）", hk_clear: "清除",
    save: "💾 保存", save_as: "💾 另存为…", load: "📂 加载", open_file: "📂 打开…",
    clear: "🗑 清空", recent: "最近:", compress: "压缩保存 (.mrz)",
    data_dir: "📁 数据目录:", save_settings: "💾 保存设置",
    export_exe: "⚙ 导出 .exe", export_ahk: "📜 导出 .ahk",
    ed_from: "从", ed_to: "到", ed_delete: "删除", ed_crop: "仅保留",
    ed_insert: "插入暂停 (毫秒)", ed_scale: "时间缩放 ×", ed_drop_moves: "移除移动",
    ed_zero: "裁剪开头", ed_undo: "⏪ 撤销",
    prof_name: "名称:", prof_save: "保存", prof_load: "加载", prof_delete: "删除",
    saved: "已保存: {}", loaded: "已加载: {}", cleared: "宏已清空",
    settings_saved: "设置已保存", save_err: "保存错误: {}", load_err: "加载错误: {}",
    no_macro: "未加载宏", exported: "已导出: {}", done: "完成",
    ed_open: "✂ 打开编辑器", ed_title: "宏编辑器",
    ed_human: "叙述", ed_raw: "原始事件",
    ed_selected: "已选: {} … {}", ed_steps: "{} 步",
    step_wait: "等待 {}", step_move: "光标移动到 {}",
    step_click: "{} 点击于 {}", step_dbl: "{} 双击于 {}",
    step_drag: "用 {} 从 {} 拖到 {}", step_scroll: "滚动 {} — {} 格",
    step_type: "输入 \u{201c}{}\u{201d}", step_key: "按下 {}", step_hold: "{} 按住了 {}",
    dir_up: "向上", dir_down: "向下", dir_left: "向左", dir_right: "向右",
    btn_l: "左键", btn_r: "右键", btn_m: "中键",
    insp_title: "所选动作", insp_none: "点击一行即可编辑",
    insp_time: "时刻 (毫秒)", insp_key: "按键", insp_delta: "增量", insp_horiz: "水平",
    insp_extended: "扩展键", insp_dup: "复制", insp_del_one: "删除动作",
    st_down: "按下", st_up: "松开",
    bulk_replace: "在选区中替换", bulk_shift: "偏移坐标",
};

const LANG_CODES: [&str; 6] = ["en", "ru", "uk", "pt", "es", "zh"];

/// Built-in tables, with `<data>/lang/<code>.json` applied on top when present.
fn tables() -> &'static [&'static Strings; 6] {
    static ACTIVE: OnceLock<[&'static Strings; 6]> = OnceLock::new();
    ACTIVE.get_or_init(|| {
        let base: [&'static Strings; 6] = [&EN, &RU, &UK, &PT, &ES, &ZH];
        let mut out = base;
        for i in 0..6 {
            let path = paths::lang_dir().join(format!("{}.json", LANG_CODES[i]));
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match serde_json::from_str::<BTreeMap<String, String>>(&text) {
                Ok(map) => {
                    out[i] = Box::leak(Box::new(base[i].with_overrides(&map)));
                    info!("loaded translation overrides from {}", path.display());
                }
                Err(e) => warn!("bad translation file {}: {e}", path.display()),
            }
        }
        out
    })
}

/// Writes a filled-in template users can translate and drop back into `lang/`.
fn export_lang_template(lang_index: usize) -> Result<PathBuf> {
    let idx = lang_index.min(5);
    let map = tables()[idx].to_map();
    let path = paths::lang_dir().join(format!("{}.template.json", LANG_CODES[idx]));
    std::fs::write(&path, serde_json::to_string_pretty(&map)?)?;
    Ok(path)
}

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
    let idx = match lang {
        Lang::En => 0,
        Lang::Ru => 1,
        Lang::Uk => 2,
        Lang::Pt => 3,
        Lang::Es => 4,
        Lang::Zh => 5,
    };
    tables()[idx]
}

// ============================================================================
// Themes
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
            // Stored as ABGR.
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
            widget_hover: rgb(58, 58, 58), widget_active: rgb(75, 75, 75),
            active_fg: rgb(255, 255, 255), border: rgb(70, 70, 70), hover_border: rgb(95, 95, 95),
            text: rgb(230, 230, 230), faint: rgb(130, 130, 130), accent: rgb(70, 130, 255),
            focus_border: rgb(0, 200, 255), widget_round: 4.0, shadow_blur: 4, shadow_offset: 1,
            shadow_alpha: 60, item_spacing_y: 5.0, button_padding: 3.0, animation_time: 0.15,
            backdrop: 1,
        },
        Theme::Oled => Palette {
            dark: true, bg: rgb(0, 0, 0), panel: rgb(0, 0, 0), widget: rgb(20, 20, 20),
            widget_hover: rgb(35, 35, 35), widget_active: rgb(50, 50, 50),
            active_fg: rgb(255, 255, 255), border: rgb(40, 40, 40), hover_border: rgb(80, 80, 80),
            text: rgb(240, 240, 240), faint: rgb(120, 120, 120), accent: rgb(0, 122, 204),
            focus_border: rgb(0, 255, 255), widget_round: 2.0, shadow_blur: 0, shadow_offset: 0,
            shadow_alpha: 0, item_spacing_y: 5.0, button_padding: 3.0, animation_time: 0.1,
            backdrop: 1,
        },
        Theme::Material3 => {
            let accent = get_system_accent_color().unwrap_or_else(|| rgb(208, 188, 255));
            Palette {
                dark: true, bg: rgb(18, 17, 24), panel: rgb(24, 23, 31), widget: rgb(32, 31, 42),
                widget_hover: rgb(40, 39, 52), widget_active: accent,
                active_fg: rgb(255, 255, 255), border: rgb(73, 69, 82), hover_border: accent,
                text: rgb(230, 224, 233), faint: rgb(147, 143, 153), accent,
                focus_border: rgb(255, 255, 0), widget_round: 20.0, shadow_blur: 0,
                shadow_offset: 0, shadow_alpha: 0, item_spacing_y: 7.0, button_padding: 6.0,
                animation_time: 0.4, backdrop: 1,
            }
        }
        Theme::Catppuccin => Palette {
            dark: true, bg: rgb(17, 17, 27), panel: rgb(30, 30, 46), widget: rgb(49, 50, 68),
            widget_hover: rgb(69, 71, 90), widget_active: rgb(203, 166, 247),
            active_fg: rgb(17, 17, 27), border: rgb(88, 91, 112), hover_border: rgb(203, 166, 247),
            text: rgb(205, 214, 244), faint: rgb(166, 172, 200), accent: rgb(203, 166, 247),
            focus_border: rgb(250, 178, 102), widget_round: 10.0, shadow_blur: 6, shadow_offset: 2,
            shadow_alpha: 90, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.25,
            backdrop: 1,
        },
        Theme::Nord => Palette {
            dark: true, bg: rgb(46, 52, 64), panel: rgb(46, 52, 64), widget: rgb(59, 66, 82),
            widget_hover: rgb(67, 76, 94), widget_active: rgb(136, 192, 208),
            active_fg: rgb(46, 52, 64), border: rgb(76, 86, 106), hover_border: rgb(136, 192, 208),
            text: rgb(216, 222, 233), faint: rgb(148, 155, 168), accent: rgb(136, 192, 208),
            focus_border: rgb(143, 188, 187), widget_round: 6.0, shadow_blur: 5, shadow_offset: 1,
            shadow_alpha: 80, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.2,
            backdrop: 1,
        },
        Theme::Dracula => Palette {
            dark: true, bg: rgb(40, 42, 54), panel: rgb(40, 42, 54), widget: rgb(68, 71, 90),
            widget_hover: rgb(80, 83, 105), widget_active: rgb(255, 121, 198),
            active_fg: rgb(40, 42, 54), border: rgb(98, 114, 164), hover_border: rgb(255, 121, 198),
            text: rgb(248, 248, 242), faint: rgb(135, 140, 160), accent: rgb(255, 121, 198),
            focus_border: rgb(189, 147, 249), widget_round: 8.0, shadow_blur: 6, shadow_offset: 2,
            shadow_alpha: 90, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.25,
            backdrop: 1,
        },
        Theme::Glass => Palette {
            dark: true, bg: rgb(24, 28, 40), panel: rgba(40, 46, 64, 110),
            widget: rgba(255, 255, 255, 45), widget_hover: rgba(255, 255, 255, 75),
            widget_active: rgba(120, 180, 255, 200), active_fg: rgb(255, 255, 255),
            border: rgba(255, 255, 255, 110), hover_border: rgba(255, 255, 255, 170),
            text: rgb(240, 245, 255), faint: rgb(190, 200, 220), accent: rgb(120, 180, 255),
            focus_border: rgb(255, 255, 255), widget_round: 14.0, shadow_blur: 12,
            shadow_offset: 3, shadow_alpha: 100, item_spacing_y: 5.0, button_padding: 4.0,
            animation_time: 0.3, backdrop: 3,
        },
        Theme::Neumorphism => Palette {
            dark: false, bg: rgb(224, 229, 236), panel: rgb(224, 229, 236),
            widget: rgb(224, 229, 236), widget_hover: rgb(231, 236, 243),
            widget_active: rgb(93, 120, 255), active_fg: rgb(255, 255, 255),
            border: rgb(224, 229, 236), hover_border: rgb(224, 229, 236), text: rgb(60, 70, 90),
            faint: rgb(120, 130, 150), accent: rgb(93, 120, 255), focus_border: rgb(255, 100, 100),
            widget_round: 12.0, shadow_blur: 10, shadow_offset: 5, shadow_alpha: 110,
            item_spacing_y: 6.0, button_padding: 5.0, animation_time: 0.25, backdrop: 1,
        },
        Theme::Fluent => {
            let accent = get_system_accent_color().unwrap_or_else(|| rgb(76, 156, 255));
            Palette {
                dark: true, bg: rgb(32, 32, 32), panel: rgba(43, 43, 43, 150),
                widget: rgba(255, 255, 255, 22), widget_hover: rgba(255, 255, 255, 38),
                widget_active: accent, active_fg: rgb(255, 255, 255),
                border: rgba(255, 255, 255, 40), hover_border: accent, text: rgb(240, 240, 240),
                faint: rgb(165, 165, 165), accent, focus_border: accent, widget_round: 7.0,
                shadow_blur: 0, shadow_offset: 0, shadow_alpha: 0, item_spacing_y: 6.0,
                button_padding: 5.0, animation_time: 0.2, backdrop: 2,
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

/// Applies a theme and returns the fill the central panel should use.
///
/// The translucency of the window and the background of popups have to come from
/// two different places. egui paints combo-box lists, menus and tooltips with
/// `panel_fill`, and those float *above* the app's own content - a see-through list
/// there means reading two layers of text at once. So `panel_fill` stays opaque for
/// the popups, and the window's translucency is applied straight to the central
/// panel's frame by the caller.
#[must_use]
fn apply_theme(ctx: &egui::Context, theme: Theme, transparent_ui: bool) -> egui::Color32 {
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
    let panel_fill = if translucent {
        if p.backdrop > 1 { p.panel } else { rgba(30, 30, 30, 140) }
    } else {
        p.panel
    };

    if translucent {
        // Everything egui might use for a floating surface is forced opaque, and gets
        // a border plus a shadow so it reads as a separate layer.
        visuals.panel_fill = p.bg;
        visuals.window_fill = p.bg;
        visuals.extreme_bg_color = p.bg;
        visuals.window_stroke = egui::Stroke::new(1.0, p.hover_border);
        visuals.window_shadow = make_shadow(&p);
        visuals.popup_shadow = egui::Shadow {
            offset: [0, 4],
            blur: 14,
            spread: 0,
            color: egui::Color32::from_black_alpha(170),
        };
    }

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = visuals;
    style.animation_time = p.animation_time;
    style.spacing.item_spacing = egui::vec2(8.0, p.item_spacing_y);
    style.spacing.button_padding = egui::vec2(p.button_padding, p.button_padding);

    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);

    // Pushed unconditionally (1 = none) so leaving a backdrop theme clears the effect.
    #[cfg(windows)]
    platform::apply_system_backdrop(platform::app_hwnd(), p.backdrop);

    panel_fill
}

// ============================================================================
// Macro editing helpers
// ============================================================================

fn describe_event(ev: &MacroEvent) -> String {
    match ev.kind {
        InputEventKind::Key { vk, down, .. } => {
            format!("{} {}", if down { "key↓" } else { "key↑" }, vk_name(vk as u32))
        }
        InputEventKind::MouseMove { x, y, .. } => format!("move {x},{y}"),
        InputEventKind::MouseButton { button, down, x, y } => {
            format!("{:?}{} {x},{y}", button, if down { "↓" } else { "↑" })
        }
        InputEventKind::MouseWheel { delta, horizontal, .. } => {
            format!("wheel{} {delta}", if horizontal { " h" } else { "" })
        }
    }
}

/// One line of the human-readable summary, mapped back onto its raw events.
#[derive(Clone, Debug)]
pub struct Step {
    /// Index of the first raw event this line covers.
    pub first: usize,
    /// Index of the last one, inclusive.
    pub last: usize,
    pub t_us: u64,
    pub text: String,
}

fn format_dur(us: u64) -> String {
    if us < 1_000 {
        format!("{us} \u{b5}s")
    } else if us < 1_000_000 {
        format!("{} ms", us / 1_000)
    } else if us < 60_000_000 {
        format!("{:.1} s", us as f64 / 1_000_000.0)
    } else {
        format_us(us)
    }
}

fn point(x: i32, y: i32) -> String {
    format!("({x}, {y})")
}

fn button_name(b: MouseButton, s: &Strings) -> String {
    match b {
        MouseButton::Left => s.btn_l.to_string(),
        MouseButton::Right => s.btn_r.to_string(),
        MouseButton::Middle => s.btn_m.to_string(),
        MouseButton::X1 => "X1".into(),
        MouseButton::X2 => "X2".into(),
    }
}

/// The character a key produces, for the "Typed ..." lines.
///
/// Only letters, digits and space: anything else is reported as a key press, which
/// keeps the summary honest instead of guessing at layouts and modifiers.
fn typed_char(vk: u16) -> Option<char> {
    match vk {
        0x41..=0x5A => Some((b'A' + (vk - 0x41) as u8) as char),
        0x30..=0x39 => Some((b'0' + (vk - 0x30) as u8) as char),
        0x20 => Some(' '),
        _ => None,
    }
}

fn find_key_up(events: &[MacroEvent], from: usize, vk: u16) -> Option<usize> {
    events.iter().enumerate().skip(from).find_map(|(j, e)| match e.kind {
        InputEventKind::Key { vk: v, down: false, .. } if v == vk => Some(j),
        _ => None,
    })
}

/// Turns raw events into a readable story: moves, clicks, drags, typing and pauses.
///
/// Every line remembers the event range it came from, so selecting a line in the
/// editor selects exactly those events for deleting, cropping or re-timing.
fn summarize(events: &[MacroEvent], s: &Strings) -> Vec<Step> {
    /// Movement below this is treated as hand tremor rather than a drag.
    const DRAG_PX: i32 = 8;
    /// Two presses closer than this at the same spot read as one double click.
    const DOUBLE_US: u64 = 400_000;
    /// Idle time worth mentioning.
    const IDLE_US: u64 = 300_000;

    let mut out: Vec<Step> = Vec::new();
    let mut i = 0usize;
    let mut prev_end_t = 0u64;

    while i < events.len() {
        let ev = events[i];
        let gap = ev.t_us.saturating_sub(prev_end_t);
        if gap >= IDLE_US && !out.is_empty() {
            out.push(Step {
                first: i,
                last: i,
                t_us: prev_end_t,
                text: s.step_wait.replace("{}", &format_dur(gap)),
            });
        }

        match ev.kind {
            InputEventKind::MouseMove { .. } => {
                let mut last = i;
                while last + 1 < events.len()
                    && matches!(events[last + 1].kind, InputEventKind::MouseMove { .. })
                {
                    last += 1;
                }
                if let InputEventKind::MouseMove { x, y, .. } = events[last].kind {
                    out.push(Step {
                        first: i,
                        last,
                        t_us: ev.t_us,
                        text: s.step_move.replace("{}", &point(x, y)),
                    });
                }
                prev_end_t = events[last].t_us;
                i = last + 1;
            }

            InputEventKind::MouseButton { button, down: true, x, y } => {
                // Walk to the matching release, noting whether the cursor travelled.
                let mut end = i;
                let (mut ex, mut ey) = (x, y);
                let mut moved = false;
                let mut j = i + 1;
                while j < events.len() {
                    match events[j].kind {
                        InputEventKind::MouseButton { button: b2, down: false, x: ux, y: uy }
                            if b2 == button =>
                        {
                            ex = ux;
                            ey = uy;
                            end = j;
                            break;
                        }
                        InputEventKind::MouseMove { x: mx, y: my, .. } => {
                            if (mx - x).abs() > DRAG_PX || (my - y).abs() > DRAG_PX {
                                moved = true;
                            }
                            ex = mx;
                            ey = my;
                        }
                        _ => {}
                    }
                    j += 1;
                }

                let name = button_name(button, s);
                let text = if moved {
                    s.step_drag
                        .replacen("{}", &name, 1)
                        .replacen("{}", &point(x, y), 1)
                        .replacen("{}", &point(ex, ey), 1)
                } else {
                    // A second press of the same button, near the same spot, soon after.
                    let mut second_end = None;
                    if let Some(next) = events.get(end + 1) {
                        if let InputEventKind::MouseButton {
                            button: b2,
                            down: true,
                            x: nx,
                            y: ny,
                        } = next.kind
                        {
                            if b2 == button
                                && next.t_us.saturating_sub(events[end].t_us) < DOUBLE_US
                                && (nx - x).abs() <= DRAG_PX
                                && (ny - y).abs() <= DRAG_PX
                            {
                                let mut k = end + 2;
                                while k < events.len() {
                                    if let InputEventKind::MouseButton {
                                        button: b3,
                                        down: false,
                                        ..
                                    } = events[k].kind
                                    {
                                        if b3 == button {
                                            second_end = Some(k);
                                            break;
                                        }
                                    }
                                    k += 1;
                                }
                            }
                        }
                    }
                    if let Some(k) = second_end {
                        end = k;
                        s.step_dbl.replacen("{}", &name, 1).replacen("{}", &point(x, y), 1)
                    } else {
                        s.step_click.replacen("{}", &name, 1).replacen("{}", &point(x, y), 1)
                    }
                };

                out.push(Step { first: i, last: end, t_us: ev.t_us, text });
                prev_end_t = events[end].t_us;
                i = end + 1;
            }

            InputEventKind::MouseWheel { delta, horizontal, .. } => {
                let sign = delta >= 0;
                let mut last = i;
                let mut notches = (delta.abs() / 120).max(1);
                while let Some(next) = events.get(last + 1) {
                    match next.kind {
                        InputEventKind::MouseWheel { delta: d2, horizontal: h2, .. }
                            if h2 == horizontal && (d2 >= 0) == sign =>
                        {
                            notches += (d2.abs() / 120).max(1);
                            last += 1;
                        }
                        _ => break,
                    }
                }
                let dir = match (horizontal, sign) {
                    (false, true) => s.dir_up,
                    (false, false) => s.dir_down,
                    (true, true) => s.dir_right,
                    (true, false) => s.dir_left,
                };
                out.push(Step {
                    first: i,
                    last,
                    t_us: ev.t_us,
                    text: s
                        .step_scroll
                        .replacen("{}", dir, 1)
                        .replacen("{}", &notches.to_string(), 1),
                });
                prev_end_t = events[last].t_us;
                i = last + 1;
            }

            InputEventKind::Key { vk, down: true, .. } => {
                // Greedily collect a run of printable keys into one "Typed ..." line.
                let mut chars = String::new();
                let mut last = i;
                let mut j = i;
                while j < events.len() {
                    let InputEventKind::Key { vk: v, down: true, .. } = events[j].kind else {
                        break;
                    };
                    let (Some(c), Some(up)) = (typed_char(v), find_key_up(events, j, v)) else {
                        break;
                    };
                    chars.push(c);
                    last = up;
                    j = up + 1;
                }

                let text = if chars.chars().count() >= 2 {
                    s.step_type.replace("{}", &chars)
                } else {
                    let up = find_key_up(events, i, vk).unwrap_or(i);
                    last = up;
                    let held = events[up].t_us.saturating_sub(ev.t_us);
                    if held > 500_000 {
                        s.step_hold
                            .replacen("{}", &vk_name(vk as u32), 1)
                            .replacen("{}", &format_dur(held), 1)
                    } else {
                        s.step_key.replace("{}", &vk_name(vk as u32))
                    }
                };

                out.push(Step { first: i, last, t_us: ev.t_us, text });
                prev_end_t = events[last].t_us;
                i = last + 1;
            }

            // Stray releases with no matching press: show them as-is rather than hide them.
            _ => {
                out.push(Step {
                    first: i,
                    last: i,
                    t_us: ev.t_us,
                    text: describe_event(&ev),
                });
                prev_end_t = ev.t_us;
                i += 1;
            }
        }
    }
    out
}

/// Removes `[from, to]` and pulls the tail back so no silent gap is left behind.
fn editor_delete_range(data: &mut MacroData, from: usize, to: usize) {
    if data.events.is_empty() {
        return;
    }
    let from = from.min(data.events.len() - 1);
    let to = to.min(data.events.len() - 1).max(from);
    let start_t = data.events[from].t_us;
    let end_t = data.events[to].t_us;
    let removed = end_t.saturating_sub(start_t);
    data.events.drain(from..=to);
    for ev in data.events.iter_mut().skip(from) {
        ev.t_us = ev.t_us.saturating_sub(removed);
    }
    data.duration_us = data.duration_us.saturating_sub(removed).max(data.last_t());
}

fn editor_crop(data: &mut MacroData, from: usize, to: usize) {
    if data.events.is_empty() {
        return;
    }
    let from = from.min(data.events.len() - 1);
    let to = to.min(data.events.len() - 1).max(from);
    let slice: Vec<MacroEvent> = data.events[from..=to].to_vec();
    let base = slice.first().map(|e| e.t_us).unwrap_or(0);
    data.events = slice
        .into_iter()
        .map(|mut e| {
            e.t_us = e.t_us.saturating_sub(base);
            e
        })
        .collect();
    data.duration_us = data.last_t();
}

fn editor_insert_delay(data: &mut MacroData, at: usize, ms: u64) {
    let us = ms * 1_000;
    for ev in data.events.iter_mut().skip(at) {
        ev.t_us = ev.t_us.saturating_add(us);
    }
    data.duration_us = data.duration_us.saturating_add(us).max(data.last_t());
}

fn editor_scale(data: &mut MacroData, factor: f64) {
    let f = factor.clamp(0.05, 20.0);
    for ev in data.events.iter_mut() {
        ev.t_us = (ev.t_us as f64 * f) as u64;
    }
    data.duration_us = ((data.duration_us as f64) * f) as u64;
}

fn editor_drop_moves(data: &mut MacroData) {
    data.events.retain(|e| !matches!(e.kind, InputEventKind::MouseMove { .. }));
}

/// Replaces one event outright.
///
/// Changing a key zeroes its scancode: playback prefers the scancode when it is
/// non-zero, so keeping the old one would silently replay the old key.
fn editor_set_event(data: &mut MacroData, index: usize, kind: InputEventKind) {
    if let Some(ev) = data.events.get_mut(index) {
        ev.kind = kind;
    }
}

/// Moves one event in time, without letting it jump past its neighbours.
fn editor_set_time(data: &mut MacroData, index: usize, t_us: u64) {
    let lo = if index == 0 { 0 } else { data.events[index - 1].t_us };
    let hi = data.events.get(index + 1).map(|e| e.t_us).unwrap_or(u64::MAX);
    if let Some(ev) = data.events.get_mut(index) {
        ev.t_us = t_us.clamp(lo, hi);
    }
    data.duration_us = data.duration_us.max(data.last_t());
}

fn editor_delete_one(data: &mut MacroData, index: usize) {
    if index < data.events.len() {
        editor_delete_range(data, index, index);
    }
}

/// Copies an event and drops the copy 10 ms later.
fn editor_duplicate(data: &mut MacroData, index: usize) {
    let Some(ev) = data.events.get(index).copied() else {
        return;
    };
    let mut copy = ev;
    copy.t_us = ev.t_us.saturating_add(10_000);
    for e in data.events.iter_mut().skip(index + 1) {
        e.t_us = e.t_us.saturating_add(10_000);
    }
    data.events.insert(index + 1, copy);
    data.duration_us = data.duration_us.saturating_add(10_000).max(data.last_t());
}

/// Swaps one mouse button for another across a range. Returns how many changed.
fn editor_replace_button(
    data: &mut MacroData,
    from: usize,
    to: usize,
    old: MouseButton,
    new: MouseButton,
) -> usize {
    if data.events.is_empty() || old == new {
        return 0;
    }
    let from = from.min(data.events.len() - 1);
    let to = to.min(data.events.len() - 1).max(from);
    let mut n = 0;
    for ev in &mut data.events[from..=to] {
        if let InputEventKind::MouseButton { button, .. } = &mut ev.kind {
            if *button == old {
                *button = new;
                n += 1;
            }
        }
    }
    n
}

/// Offsets every screen coordinate in a range - for when the target window moved.
fn editor_shift_coords(data: &mut MacroData, from: usize, to: usize, dx: i32, dy: i32) {
    if data.events.is_empty() || (dx == 0 && dy == 0) {
        return;
    }
    let from = from.min(data.events.len() - 1);
    let to = to.min(data.events.len() - 1).max(from);
    for ev in &mut data.events[from..=to] {
        match &mut ev.kind {
            InputEventKind::MouseMove { x, y, .. }
            | InputEventKind::MouseButton { x, y, .. }
            | InputEventKind::MouseWheel { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
            InputEventKind::Key { .. } => {}
        }
    }
}

/// Keys offered when retyping a keyboard event. Anything else goes in by code.
const EDIT_KEYS: [(&str, u16); 30] = [
    ("Space", 0x20), ("Enter", 0x0D), ("Tab", 0x09), ("Esc", 0x1B),
    ("Backspace", 0x08), ("Delete", 0x2E), ("Shift", 0x10), ("Ctrl", 0x11),
    ("Alt", 0x12), ("Left", 0x25), ("Up", 0x26), ("Right", 0x27), ("Down", 0x28),
    ("A", 0x41), ("B", 0x42), ("C", 0x43), ("D", 0x44), ("E", 0x45), ("Q", 0x51),
    ("R", 0x52), ("S", 0x53), ("W", 0x57), ("1", 0x31), ("2", 0x32), ("3", 0x33),
    ("F1", 0x70), ("F2", 0x71), ("F5", 0x74), ("F8", 0x77), ("F9", 0x78),
];

/// Shifts everything so the first event happens at t = 0.
fn editor_trim_lead(data: &mut MacroData) {
    let Some(first) = data.events.first().map(|e| e.t_us) else {
        return;
    };
    if first == 0 {
        return;
    }
    for ev in data.events.iter_mut() {
        ev.t_us = ev.t_us.saturating_sub(first);
    }
    data.duration_us = data.duration_us.saturating_sub(first).max(data.last_t());
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
    // editor
    ed_from: usize,
    ed_to: usize,
    ed_delay_ms: u64,
    ed_scale: f64,
    ed_undo: Option<MacroData>,
    /// The editor lives in its own OS window.
    editor_open: bool,
    /// Story view vs the raw event list.
    ed_human: bool,
    /// Cached summary plus the inputs it was built from, so a long macro is not
    /// re-narrated on every frame.
    ed_steps: Vec<Step>,
    ed_steps_key: (usize, u64, usize),
    /// The single event shown in the inspector.
    ed_cursor: usize,
    /// Which event the current undo snapshot belongs to, so dragging a value does
    /// not overwrite the snapshot on every frame.
    ed_undo_key: Option<usize>,
    ed_pick_deadline: Option<Instant>,
    bulk_from_btn: usize,
    bulk_to_btn: usize,
    bulk_dx: i32,
    bulk_dy: i32,
    // profiles
    profiles: Vec<String>,
    profile_name: String,
    /// Fill for the central panel - this is what makes the window translucent.
    panel_fill: egui::Color32,
    // pixel picking
    pick_deadline: Option<Instant>,
    /// When the current "press a key" session started.
    capture_started: Option<Instant>,
}

impl MacroApp {
    fn new(cc: &eframe::CreationContext<'_>, state: Arc<AppState>, config: AppConfig) -> Self {
        setup_fonts(&cc.egui_ctx);
        let panel_fill =
            apply_theme(&cc.egui_ctx, theme_at(config.default_theme), config.transparent_ui);
        Self {
            panel_fill,
            state,
            config,
            system_lang: detect_system_lang(),
            status_msg: String::new(),
            theme_dirty: true,
            ed_from: 0,
            ed_to: 0,
            ed_delay_ms: 500,
            ed_scale: 1.0,
            ed_undo: None,
            editor_open: false,
            ed_human: true,
            ed_steps: Vec::new(),
            ed_steps_key: (usize::MAX, 0, 0),
            ed_cursor: 0,
            ed_undo_key: None,
            ed_pick_deadline: None,
            bulk_from_btn: 0,
            bulk_to_btn: 1,
            bulk_dx: 0,
            bulk_dy: 0,
            profiles: list_profiles(),
            profile_name: String::new(),
            pick_deadline: None,
            capture_started: None,
        }
    }

    fn strs(&self) -> &'static Strings {
        get_strings(self.config.default_lang, self.system_lang)
    }

    fn busy(&self) -> bool {
        self.state.recording.load(Ordering::Relaxed) || self.state.playing.load(Ordering::Relaxed)
    }

    fn snapshot_for_undo(&mut self) {
        self.ed_undo = Some(self.state.macro_data.lock().clone());
    }

    /// Edit path for the inspector: snapshots once per event, so undo returns to
    /// the state before you started fiddling with *this* action rather than to the
    /// previous animation frame.
    fn edit_event<F: FnOnce(&mut MacroData)>(&mut self, index: usize, f: F) {
        if self.busy() {
            return;
        }
        if self.ed_undo_key != Some(index) {
            self.ed_undo = Some(self.state.macro_data.lock().clone());
            self.ed_undo_key = Some(index);
        }
        let mut data = self.state.macro_data.lock();
        f(&mut data);
        let dur = data.duration_us;
        drop(data);
        self.state.recorded_time_us.store(dur, Ordering::Relaxed);
    }

    fn edit<F: FnOnce(&mut MacroData)>(&mut self, f: F) {
        if self.busy() {
            return;
        }
        self.snapshot_for_undo();
        self.ed_undo_key = None;
        let mut data = self.state.macro_data.lock();
        f(&mut data);
        let dur = data.duration_us;
        drop(data);
        self.state.recorded_time_us.store(dur, Ordering::Relaxed);
    }

    fn do_save(&mut self, path: PathBuf) {
        let s = self.strs();
        let data = self.state.macro_data.lock().clone();
        if data.is_empty() {
            self.status_msg = s.no_macro.to_string();
            return;
        }
        match save_macro(&path, &data) {
            Ok(()) => {
                self.config.push_recent(&path);
                *self.state.current_path.lock() = Some(path.clone());
                self.status_msg = s.saved.replace("{}", &file_label(&path));
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
                self.ed_undo = None;
                self.status_msg = s.loaded.replace("{}", &file_label(&path));
            }
            Err(e) => self.status_msg = s.load_err.replace("{}", &e.to_string()),
        }
    }

    /// Rebuilds the story only when the macro or the language actually changed.
    fn refresh_steps(&mut self) {
        let s = self.strs();
        let (count, dur, events) = {
            let d = self.state.macro_data.lock();
            (d.events.len(), d.duration_us, d.events.clone())
        };
        let key = (count, dur, self.config.default_lang);
        if key != self.ed_steps_key {
            self.ed_steps = summarize(&events, s);
            self.ed_steps_key = key;
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
}

fn file_label(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
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

/// One row of the hotkey editor. Returns true when the binding changed.
///
/// Two ways to set a key, because one of them always works: click the button and
/// press anything, or pick from the list (which also covers keys the window never
/// receives, such as Pause and the NumPad).
fn hotkey_row(
    ui: &mut egui::Ui,
    s: &Strings,
    label: &str,
    salt: &str,
    slot: u32,
    hk: &mut Hotkey,
) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(label);

        // Left cell is the action, right cell is the current value: neither can end
        // up blank, and the two no longer say the same thing twice.
        let capturing = CAPTURE_SLOT.load(Ordering::Relaxed) == slot;
        let text = if capturing { s.hk_press } else { s.hk_bind };
        let button = egui::Button::new(text).min_size(egui::vec2(110.0, 0.0));
        if ui.add(button).clicked() {
            if capturing {
                end_capture();
            } else {
                begin_capture(slot);
            }
        }

        egui::ComboBox::from_id_salt(salt)
            .selected_text(vk_name(hk.vk))
            .width(112.0)
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
        if ui.small_button(s.hk_clear).clicked() && hk.vk != 0 {
            *hk = Hotkey::plain(0);
            changed = true;
        }
    });
    changed
}

impl MacroApp {
    /// The editor, drawn into whatever `Ui` it is given.
    ///
    /// Kept separate from the window plumbing on purpose: if the child viewport is
    /// ever unavailable, this same function can be dropped straight back into the
    /// main window without touching any of the logic.
    fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let s = self.strs();
        let busy = self.busy();
        self.refresh_steps();

        let (count, dur) = {
            let d = self.state.macro_data.lock();
            (d.events.len(), d.duration_us)
        };
        if count == 0 {
            ui.label(s.no_macro);
            return;
        }
        let last_index = count - 1;
        self.ed_cursor = self.ed_cursor.min(last_index);
        self.ed_from = self.ed_from.min(last_index);
        self.ed_to = self.ed_to.min(last_index).max(self.ed_from);

        ui.horizontal_wrapped(|ui| {
            ui.label(s.events.replace("{}", &count.to_string()));
            ui.label(s.duration.replace("{}", &format_us(dur)));
            ui.separator();
            ui.selectable_value(&mut self.ed_human, true, s.ed_human);
            ui.selectable_value(&mut self.ed_human, false, s.ed_raw);
        });
        ui.separator();

        // The list gets whatever is left after the inspector, which is pinned to the
        // bottom - letting the scroll area take the whole window used to push every
        // control off-screen.
        let list_h = (ui.available_height() - 330.0).max(120.0);

        if self.ed_human {
            let steps = self.ed_steps.clone();
            let mut pick = None;
            egui::ScrollArea::vertical().id_salt("story").max_height(list_h).auto_shrink(
                [false, false],
            ).show(ui, |ui| {
                for st in &steps {
                    let selected = st.first >= self.ed_from && st.last <= self.ed_to;
                    let line = format!("{}   {}", format_us(st.t_us), st.text);
                    if ui.selectable_label(selected, line).clicked() {
                        pick = Some((st.first, st.last));
                    }
                }
            });
            if let Some((a, b)) = pick {
                self.ed_from = a;
                self.ed_to = b;
                self.ed_cursor = a;
            }
        } else {
            let from = self.ed_from;
            let rows: Vec<(usize, u64, String)> = {
                let d = self.state.macro_data.lock();
                let end = (from + 400).min(count);
                d.events
                    .get(from..end)
                    .unwrap_or(&[])
                    .iter()
                    .enumerate()
                    .map(|(i, ev)| (from + i, ev.t_us, describe_event(ev)))
                    .collect()
            };
            let mut pick = None;
            egui::ScrollArea::vertical().id_salt("raw").max_height(list_h).auto_shrink(
                [false, false],
            ).show(ui, |ui| {
                for (i, t, text) in &rows {
                    let selected = *i == self.ed_cursor;
                    let line = format!("{i:>6}  {}  {text}", format_us(*t));
                    if ui
                        .selectable_label(selected, egui::RichText::new(line).monospace().small())
                        .clicked()
                    {
                        pick = Some(*i);
                    }
                }
            });
            if let Some(i) = pick {
                self.ed_cursor = i;
                self.ed_from = i;
                self.ed_to = i;
            }
        }

        // ---- inspector: one action at a time ---------------------------------
        ui.separator();
        let idx = self.ed_cursor;
        let current = self.state.macro_data.lock().events.get(idx).copied();
        if let Some(ev) = current {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(s.insp_title).strong());
                ui.label(egui::RichText::new(format!("#{idx}")).weak());
                if ui.small_button("\u{25c0}").clicked() && idx > 0 {
                    self.ed_cursor = idx - 1;
                }
                if ui.small_button("\u{25b6}").clicked() && idx < last_index {
                    self.ed_cursor = idx + 1;
                }
                ui.label(egui::RichText::new(describe_event(&ev)).weak());
            });

            let mut kind = ev.kind;
            let mut kind_changed = false;

            ui.horizontal_wrapped(|ui| {
                ui.label(s.insp_time);
                let mut t_ms = ev.t_us as f64 / 1000.0;
                if ui
                    .add_enabled(
                        !busy,
                        egui::DragValue::new(&mut t_ms).speed(1.0).range(0.0..=1.0e9),
                    )
                    .changed()
                {
                    let t_us = (t_ms * 1000.0).max(0.0) as u64;
                    self.edit_event(idx, |d| editor_set_time(d, idx, t_us));
                }
            });

            match &mut kind {
                InputEventKind::Key { vk, scan, down, extended } => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.insp_key);
                        egui::ComboBox::from_id_salt("insp_key")
                            .selected_text(vk_name(*vk as u32))
                            .width(120.0)
                            .show_ui(ui, |ui| {
                                for (name, code) in EDIT_KEYS {
                                    if ui.selectable_label(*vk == code, name).clicked()
                                        && *vk != code
                                    {
                                        *vk = code;
                                        // A stale scancode would replay the old key.
                                        *scan = 0;
                                        kind_changed = true;
                                    }
                                }
                            });
                        let mut raw = *vk as u32;
                        if ui.add(egui::DragValue::new(&mut raw).range(1..=254)).changed() {
                            *vk = raw as u16;
                            *scan = 0;
                            kind_changed = true;
                        }
                        kind_changed |= ui.selectable_value(down, true, s.st_down).clicked();
                        kind_changed |= ui.selectable_value(down, false, s.st_up).clicked();
                        kind_changed |= ui.checkbox(extended, s.insp_extended).changed();
                    });
                }
                InputEventKind::MouseButton { button, down, x, y } => {
                    ui.horizontal_wrapped(|ui| {
                        let names = [s.btn_l, s.btn_r, s.btn_m, "X1", "X2"];
                        let all = [
                            MouseButton::Left,
                            MouseButton::Right,
                            MouseButton::Middle,
                            MouseButton::X1,
                            MouseButton::X2,
                        ];
                        egui::ComboBox::from_id_salt("insp_btn")
                            .selected_text(
                                names[all.iter().position(|b| b == button).unwrap_or(0)],
                            )
                            .width(110.0)
                            .show_ui(ui, |ui| {
                                for (b, n) in all.iter().zip(names) {
                                    if ui.selectable_label(button == b, n).clicked()
                                        && button != b
                                    {
                                        *button = *b;
                                        kind_changed = true;
                                    }
                                }
                            });
                        kind_changed |= ui.selectable_value(down, true, s.st_down).clicked();
                        kind_changed |= ui.selectable_value(down, false, s.st_up).clicked();
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label("X");
                        kind_changed |= ui
                            .add(egui::DragValue::new(x).range(-32000..=32000))
                            .changed();
                        ui.label("Y");
                        kind_changed |= ui
                            .add(egui::DragValue::new(y).range(-32000..=32000))
                            .changed();
                        match self.ed_pick_deadline {
                            Some(d) => {
                                let left = d
                                    .saturating_duration_since(Instant::now())
                                    .as_secs()
                                    + 1;
                                ui.label(s.pixel_picking.replace("{}", &left.to_string()));
                            }
                            None => {
                                if ui.button(s.pixel_pick).clicked() {
                                    self.ed_pick_deadline =
                                        Some(Instant::now() + Duration::from_secs(3));
                                }
                            }
                        }
                    });
                }
                InputEventKind::MouseMove { x, y, dx, dy } => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("X");
                        kind_changed |= ui
                            .add(egui::DragValue::new(x).range(-32000..=32000))
                            .changed();
                        ui.label("Y");
                        kind_changed |= ui
                            .add(egui::DragValue::new(y).range(-32000..=32000))
                            .changed();
                        ui.label("dX");
                        kind_changed |= ui.add(egui::DragValue::new(dx)).changed();
                        ui.label("dY");
                        kind_changed |= ui.add(egui::DragValue::new(dy)).changed();
                    });
                    ui.horizontal_wrapped(|ui| match self.ed_pick_deadline {
                        Some(d) => {
                            let left =
                                d.saturating_duration_since(Instant::now()).as_secs() + 1;
                            ui.label(s.pixel_picking.replace("{}", &left.to_string()));
                        }
                        None => {
                            if ui.button(s.pixel_pick).clicked() {
                                self.ed_pick_deadline =
                                    Some(Instant::now() + Duration::from_secs(3));
                            }
                        }
                    });
                }
                InputEventKind::MouseWheel { delta, horizontal, x, y } => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.insp_delta);
                        kind_changed |= ui
                            .add(egui::DragValue::new(delta).range(-2400..=2400).speed(10.0))
                            .changed();
                        kind_changed |= ui.checkbox(horizontal, s.insp_horiz).changed();
                        ui.label("X");
                        kind_changed |= ui
                            .add(egui::DragValue::new(x).range(-32000..=32000))
                            .changed();
                        ui.label("Y");
                        kind_changed |= ui
                            .add(egui::DragValue::new(y).range(-32000..=32000))
                            .changed();
                    });
                }
            }

            if kind_changed && !busy {
                self.edit_event(idx, |d| editor_set_event(d, idx, kind));
            }

            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!busy, egui::Button::new(s.insp_dup)).clicked() {
                    self.edit(|d| editor_duplicate(d, idx));
                }
                if ui.add_enabled(!busy, egui::Button::new(s.insp_del_one)).clicked() {
                    self.edit(|d| editor_delete_one(d, idx));
                    self.ed_cursor = idx.saturating_sub(1);
                }
                if ui
                    .add_enabled(self.ed_undo.is_some() && !busy, egui::Button::new(s.ed_undo))
                    .clicked()
                {
                    if let Some(prev) = self.ed_undo.take() {
                        let d = prev.duration_us;
                        *self.state.macro_data.lock() = prev;
                        self.state.recorded_time_us.store(d, Ordering::Relaxed);
                        self.ed_undo_key = None;
                    }
                }
            });
        } else {
            ui.label(s.insp_none);
        }

        // ---- range operations -------------------------------------------------
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(s.ed_from);
            ui.add(egui::DragValue::new(&mut self.ed_from).range(0..=last_index));
            ui.label(s.ed_to);
            ui.add(egui::DragValue::new(&mut self.ed_to).range(0..=last_index));
            if self.ed_to < self.ed_from {
                self.ed_to = self.ed_from;
            }
            let (from, to) = (self.ed_from, self.ed_to);
            if ui.add_enabled(!busy, egui::Button::new(s.ed_delete)).clicked() {
                self.edit(|d| editor_delete_range(d, from, to));
            }
            if ui.add_enabled(!busy, egui::Button::new(s.ed_crop)).clicked() {
                self.edit(|d| editor_crop(d, from, to));
            }
            if ui.add_enabled(!busy, egui::Button::new(s.ed_drop_moves)).clicked() {
                self.edit(editor_drop_moves);
            }
            if ui.add_enabled(!busy, egui::Button::new(s.ed_zero)).clicked() {
                self.edit(editor_trim_lead);
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.label(s.ed_insert);
            ui.add(egui::DragValue::new(&mut self.ed_delay_ms).range(0..=600_000).speed(10.0));
            if ui.add_enabled(!busy, egui::Button::new("\u{ff0b}")).clicked() {
                let (at, ms) = (self.ed_from, self.ed_delay_ms);
                self.edit(|d| editor_insert_delay(d, at, ms));
            }
            ui.label(s.ed_scale);
            ui.add(egui::DragValue::new(&mut self.ed_scale).range(0.05..=20.0).speed(0.05));
            if ui.add_enabled(!busy, egui::Button::new("\u{2714}")).clicked() {
                let f = self.ed_scale;
                self.edit(|d| editor_scale(d, f));
            }
        });

        // Bulk edits: the two things people actually want across a whole recording.
        ui.horizontal_wrapped(|ui| {
            let names = [s.btn_l, s.btn_r, s.btn_m, "X1", "X2"];
            ui.label(s.bulk_replace);
            egui::ComboBox::from_id_salt("bulk_a")
                .selected_text(names[self.bulk_from_btn.min(4)])
                .width(90.0)
                .show_ui(ui, |ui| {
                    for (i, n) in names.iter().enumerate() {
                        ui.selectable_value(&mut self.bulk_from_btn, i, *n);
                    }
                });
            ui.label("\u{2192}");
            egui::ComboBox::from_id_salt("bulk_b")
                .selected_text(names[self.bulk_to_btn.min(4)])
                .width(90.0)
                .show_ui(ui, |ui| {
                    for (i, n) in names.iter().enumerate() {
                        ui.selectable_value(&mut self.bulk_to_btn, i, *n);
                    }
                });
            if ui.add_enabled(!busy, egui::Button::new("\u{2714}")).clicked() {
                let (from, to) = (self.ed_from, self.ed_to);
                let a = MouseButton::from_index(self.bulk_from_btn);
                let b = MouseButton::from_index(self.bulk_to_btn);
                self.edit(|d| {
                    editor_replace_button(d, from, to, a, b);
                });
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.label(s.bulk_shift);
            ui.label("dX");
            ui.add(egui::DragValue::new(&mut self.bulk_dx).range(-32000..=32000));
            ui.label("dY");
            ui.add(egui::DragValue::new(&mut self.bulk_dy).range(-32000..=32000));
            if ui.add_enabled(!busy, egui::Button::new("\u{2714}")).clicked() {
                let (from, to, dx, dy) =
                    (self.ed_from, self.ed_to, self.bulk_dx, self.bulk_dy);
                self.edit(|d| editor_shift_coords(d, from, to, dx, dy));
            }
        });
    }

    /// Hosts the editor in its own OS window while `editor_open` is set.
    fn editor_viewport(&mut self, ctx: &egui::Context) {
        if !self.editor_open {
            return;
        }
        let title = self.strs().ed_title;
        let mut close = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("macro_editor"),
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([640.0, 560.0])
                .with_min_inner_size([420.0, 320.0]),
            |ctx, _class| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.editor_ui(ui);
                });
                if ctx.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
            },
        );
        if close {
            self.editor_open = false;
        }
    }
}

impl eframe::App for MacroApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let s = self.strs();
        let recording = self.state.recording.load(Ordering::Relaxed);
        let playing = self.state.playing.load(Ordering::Relaxed);
        let paused = self.state.paused.load(Ordering::Relaxed);
        let busy = recording || playing;

        // The window only exists after the first frame, so the backdrop lands here.
        if self.theme_dirty {
            self.panel_fill = apply_theme(
                ui.ctx(),
                theme_at(self.config.default_theme),
                self.config.transparent_ui,
            );
            self.theme_dirty = false;
        }

        // ---- close button -----------------------------------------------------
        // Minimize to tray instead of quitting - unless the tray itself asked us to
        // quit, and never when the icon failed to appear, which would leave the app
        // running with no way to reach it.
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            let to_tray = self.config.tray_enabled
                && self.config.close_to_tray
                && tray::is_active()
                && !ALLOW_CLOSE.load(Ordering::Relaxed);
            if to_tray {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
                set_window_visible(false);
            }
        }

        // ---- hotkey binding ---------------------------------------------------
        let slot = CAPTURE_SLOT.load(Ordering::Relaxed);
        if slot != 0 {
            // Keep the window awake and give up after a while so a forgotten binding
            // session can never leave the global hotkeys switched off.
            ui.ctx().request_repaint();
            match self.capture_started {
                Some(t) if t.elapsed() > Duration::from_secs(15) => {
                    self.capture_started = None;
                    end_capture();
                }
                None => self.capture_started = Some(Instant::now()),
                _ => {}
            }
            // Path 1: normal window input (works whenever this window has focus).
            if let Some(hk) = capture_from_window(ui.ctx()) {
                *CAPTURED_KEY.lock() = Some(hk);
            }
        } else {
            self.capture_started = None;
        }

        // Path 2: the low-level hook, which also sees keys while another app is focused.
        let captured = CAPTURED_KEY.lock().take();
        if let Some(hk) = captured {
            if slot != 0 && hk.vk != 0 {
                match slot {
                    1 => self.config.hotkey_record = hk,
                    2 => self.config.hotkey_play = hk,
                    3 => self.config.hotkey_stop = hk,
                    4 => self.config.hotkey_pause = hk,
                    _ => {}
                }
                self.status_msg = format!("{} {}", s.hk_bind, hk.label());
            }
            self.capture_started = None;
            end_capture();
            publish_hotkeys(&self.config);
        }

        // ---- deferred pick of a coordinate for the selected action ------------
        if let Some(deadline) = self.ed_pick_deadline {
            ui.ctx().request_repaint();
            if Instant::now() >= deadline {
                self.ed_pick_deadline = None;
                let (px, py) = platform::cursor_pos();
                let idx = self.ed_cursor;
                let current = self.state.macro_data.lock().events.get(idx).copied();
                if let Some(ev) = current {
                    let mut kind = ev.kind;
                    match &mut kind {
                        InputEventKind::MouseMove { x, y, .. }
                        | InputEventKind::MouseButton { x, y, .. }
                        | InputEventKind::MouseWheel { x, y, .. } => {
                            *x = px;
                            *y = py;
                        }
                        InputEventKind::Key { .. } => {}
                    }
                    self.edit_event(idx, |d| editor_set_event(d, idx, kind));
                }
                self.status_msg = format!("{px}, {py}");
            }
        }

        // ---- deferred pixel pick ---------------------------------------------
        if let Some(deadline) = self.pick_deadline {
            if Instant::now() >= deadline {
                self.pick_deadline = None;
                let (x, y) = platform::cursor_pos();
                self.config.pixel_x = x;
                self.config.pixel_y = y;
                if let Some((r, g, b)) = platform::screen_pixel(x, y) {
                    self.config.pixel_r = r;
                    self.config.pixel_g = g;
                    self.config.pixel_b = b;
                }
                self.status_msg = s.done.to_string();
            }
        }

        if self.state.pixel_triggered.swap(false, Ordering::Relaxed) {
            self.status_msg = s.status_pixel.to_string();
        }

        // Drawn before the main panel so it is a sibling window, not a nested one.
        let ctx = ui.ctx().clone();
        self.editor_viewport(&ctx);

        let panel = egui::Frame::central_panel(ui.style()).fill(self.panel_fill);
        egui::CentralPanel::default().frame(panel).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(APP_TITLE);
                    ui.label(egui::RichText::new(format!("v{APP_VERSION}")).weak());
                });
                ui.separator();

                // ---- transport ------------------------------------------------
                ui.horizontal(|ui| {
                    let label = if recording { s.stop_rec } else { s.record };
                    if ui.add_enabled(!playing, egui::Button::new(label)).clicked() {
                        toggle_recording(&self.state);
                    }
                    ui.label(egui::RichText::new(self.config.hotkey_record.label()).weak());
                });
                ui.horizontal(|ui| {
                    let label = if playing { s.stop_play } else { s.play };
                    if ui.add_enabled(!recording, egui::Button::new(label)).clicked() {
                        toggle_playback(&self.state);
                    }
                    let label = if paused { s.resume } else { s.pause };
                    if ui.add_enabled(playing, egui::Button::new(label)).clicked() {
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

                // ---- playback --------------------------------------------------
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
                    ui.checkbox(&mut self.config.use_window_anchor, s.anchor_use);
                    let anchor = self
                        .state
                        .macro_data
                        .lock()
                        .anchor
                        .as_ref()
                        .map(|a| a.title.clone());
                    let text = anchor.unwrap_or_else(|| s.anchor_none.to_string());
                    ui.label(egui::RichText::new(s.anchor_of.replace("{}", &text)).weak().small());
                });

                // ---- recording -------------------------------------------------
                egui::CollapsingHeader::new(s.sec_recording).show(ui, |ui| {
                    ui.checkbox(&mut self.config.capture_mouse_moves, s.capture_moves);
                    ui.horizontal(|ui| {
                        ui.label(s.sample_rate);
                        ui.add(
                            egui::DragValue::new(&mut self.config.mouse_sample_ms).range(1..=100),
                        );
                    });
                    ui.checkbox(&mut self.config.record_window_anchor, s.anchor_rec);
                });

                // ---- time limit -------------------------------------------------
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
                    }
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
                            ui.selectable_value(&mut self.config.action_on_completion, i, *name);
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
                });

                // ---- pixel condition ---------------------------------------------
                egui::CollapsingHeader::new(s.sec_pixel).show(ui, |ui| {
                    ui.checkbox(&mut self.config.pixel_enabled, s.pixel_cb);
                    ui.horizontal(|ui| {
                        ui.label("X");
                        ui.add(
                            egui::DragValue::new(&mut self.config.pixel_x).range(-32000..=32000),
                        );
                        ui.label("Y");
                        ui.add(
                            egui::DragValue::new(&mut self.config.pixel_y).range(-32000..=32000),
                        );
                    });
                    ui.horizontal(|ui| {
                        let mut col = [
                            self.config.pixel_r,
                            self.config.pixel_g,
                            self.config.pixel_b,
                        ];
                        if ui.color_edit_button_srgb(&mut col).changed() {
                            self.config.pixel_r = col[0];
                            self.config.pixel_g = col[1];
                            self.config.pixel_b = col[2];
                        }
                        ui.label(s.pixel_tol);
                        ui.add(
                            egui::DragValue::new(&mut self.config.pixel_tolerance).range(0..=255),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.config.pixel_mode, 0, s.pixel_match);
                        ui.selectable_value(&mut self.config.pixel_mode, 1, s.pixel_differ);
                    });
                    match self.pick_deadline {
                        Some(d) => {
                            let left = d.saturating_duration_since(Instant::now()).as_secs() + 1;
                            ui.label(s.pixel_picking.replace("{}", &left.to_string()));
                        }
                        None => {
                            if ui.button(s.pixel_pick).clicked() {
                                self.pick_deadline =
                                    Some(Instant::now() + Duration::from_secs(3));
                            }
                        }
                    }
                });

                // ---- editor --------------------------------------------------------
                egui::CollapsingHeader::new(s.sec_editor).show(ui, |ui| {
                    let count = self.state.macro_data.lock().events.len();
                    if count == 0 {
                        ui.label(s.no_macro);
                    } else {
                        self.refresh_steps();
                        ui.horizontal_wrapped(|ui| {
                            if ui.button(s.ed_open).clicked() {
                                self.editor_open = true;
                            }
                            ui.label(
                                egui::RichText::new(
                                    s.ed_steps.replace("{}", &self.ed_steps.len().to_string()),
                                )
                                .weak(),
                            );
                        });
                    }
                });

                // ---- hotkeys --------------------------------------------------------
                egui::CollapsingHeader::new(s.sec_hotkeys).show(ui, |ui| {
                    let mut changed = false;
                    changed |=
                        hotkey_row(ui, s, s.hk_record, "hk1", 1, &mut self.config.hotkey_record);
                    changed |=
                        hotkey_row(ui, s, s.hk_play, "hk2", 2, &mut self.config.hotkey_play);
                    changed |=
                        hotkey_row(ui, s, s.hk_pause, "hk4", 4, &mut self.config.hotkey_pause);
                    changed |=
                        hotkey_row(ui, s, s.hk_stop, "hk3", 3, &mut self.config.hotkey_stop);
                    if changed {
                        publish_hotkeys(&self.config);
                        request_hotkey_refresh();
                    }
                    if HK_FAILED.load(Ordering::Relaxed) != 0 {
                        ui.colored_label(egui::Color32::from_rgb(255, 170, 60), s.hk_failed);
                    }
                });

                // ---- appearance ------------------------------------------------------
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
                    ui.checkbox(&mut self.config.tray_enabled, s.tray_cb);
                    if self.config.tray_enabled {
                        ui.checkbox(&mut self.config.close_to_tray, s.close_tray_cb);
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
                    if ui.button(s.lang_template).clicked() {
                        let idx = self.config.default_lang.saturating_sub(1);
                        match export_lang_template(idx) {
                            Ok(p) => self.status_msg = s.exported.replace("{}", &file_label(&p)),
                            Err(e) => {
                                self.status_msg = s.save_err.replace("{}", &e.to_string());
                            }
                        }
                    }
                });

                // ---- profiles ---------------------------------------------------------
                egui::CollapsingHeader::new(s.sec_profiles).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(s.prof_name);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.profile_name)
                                .desired_width(120.0),
                        );
                        if ui.button(s.prof_save).clicked() && !self.profile_name.trim().is_empty()
                        {
                            let path = profile_path(&self.profile_name);
                            match save_config_to(&path, &self.config) {
                                Ok(()) => {
                                    self.profiles = list_profiles();
                                    self.status_msg =
                                        s.saved.replace("{}", &file_label(&path));
                                }
                                Err(e) => {
                                    self.status_msg = s.save_err.replace("{}", &e.to_string())
                                }
                            }
                        }
                    });
                    let names = self.profiles.clone();
                    ui.horizontal_wrapped(|ui| {
                        for name in names {
                            if ui.small_button(&name).clicked() {
                                let path = profile_path(&name);
                                let mut cfg = load_config_from(&path);
                                cfg.recent_files = self.config.recent_files.clone();
                                self.config = cfg;
                                self.profile_name = name.clone();
                                self.theme_dirty = true;
                                publish_hotkeys(&self.config);
                                request_hotkey_refresh();
                                self.status_msg = s.loaded.replace("{}", &name);
                            }
                        }
                    });
                    if ui.button(s.prof_delete).clicked() && !self.profile_name.trim().is_empty() {
                        let _ = std::fs::remove_file(profile_path(&self.profile_name));
                        self.profiles = list_profiles();
                        self.status_msg = s.done.to_string();
                    }
                });

                // ---- files ------------------------------------------------------------
                egui::CollapsingHeader::new(s.sec_files).default_open(true).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button(s.save).clicked() {
                            let path = self.default_save_path();
                            self.do_save(path);
                        }
                        if ui.button(s.save_as).clicked() {
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
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("Macro", &["json", "mrz", "gz"])
                                .set_directory(paths::data_dir())
                                .pick_file()
                            {
                                self.do_load(path);
                            }
                        }
                        if ui.add_enabled(!busy, egui::Button::new(s.clear)).clicked() {
                            *self.state.macro_data.lock() = MacroData::default();
                            self.state.recorded_time_us.store(0, Ordering::Relaxed);
                            *self.state.current_path.lock() = None;
                            self.ed_undo = None;
                            self.status_msg = s.cleared.to_string();
                        }
                    });

                    ui.horizontal_wrapped(|ui| {
                        if ui.button(s.export_exe).clicked() {
                            let data = self.state.macro_data.lock().clone();
                            if data.is_empty() {
                                self.status_msg = s.no_macro.to_string();
                            } else if let Some(path) = rfd::FileDialog::new()
                                .add_filter("Executable", &["exe"])
                                .set_directory(paths::data_dir())
                                .set_file_name("macro-player.exe")
                                .save_file()
                            {
                                let payload = Payload {
                                    loops: if self.config.loop_play {
                                        0
                                    } else {
                                        self.config.play_count_limit
                                    },
                                    speed: self.config.speed,
                                    absolute_mouse: self.config.absolute_mouse,
                                    repeat_delay_ms: self.config.repeat_delay_ms,
                                    macro_data: data,
                                };
                                match export_self_running_exe(&path, &payload) {
                                    Ok(()) => {
                                        self.status_msg =
                                            s.exported.replace("{}", &file_label(&path))
                                    }
                                    Err(e) => {
                                        self.status_msg =
                                            s.save_err.replace("{}", &e.to_string())
                                    }
                                }
                            }
                        }
                        if ui.button(s.export_ahk).clicked() {
                            let data = self.state.macro_data.lock().clone();
                            if data.is_empty() {
                                self.status_msg = s.no_macro.to_string();
                            } else if let Some(path) = rfd::FileDialog::new()
                                .add_filter("AutoHotkey", &["ahk"])
                                .set_directory(paths::data_dir())
                                .set_file_name("macro.ahk")
                                .save_file()
                            {
                                let loops = if self.config.loop_play {
                                    0
                                } else {
                                    self.config.play_count_limit
                                };
                                match export_ahk(&path, &data, loops) {
                                    Ok(()) => {
                                        self.status_msg =
                                            s.exported.replace("{}", &file_label(&path))
                                    }
                                    Err(e) => {
                                        self.status_msg =
                                            s.save_err.replace("{}", &e.to_string())
                                    }
                                }
                            }
                        }
                    });

                    ui.checkbox(&mut self.config.compress_on_save, s.compress);

                    if !self.config.recent_files.is_empty() {
                        let recent = self.config.recent_files.clone();
                        ui.horizontal_wrapped(|ui| {
                            ui.label(s.recent);
                            for path in recent {
                                let name = file_label(Path::new(&path));
                                let label = if name.is_empty() { path.clone() } else { name };
                                if ui.small_button(label).on_hover_text(&path).clicked() {
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

                // ---- footer -------------------------------------------------------------
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
                        "{} [{} · {} · {}]",
                        s.status_ready,
                        self.config.hotkey_record.label(),
                        self.config.hotkey_play.label(),
                        self.config.hotkey_stop.label()
                    )
                };
                ui.label(format!("ℹ {status}"));
            });
        });

        // Idempotent every frame: the engine can never drift from the UI.
        self.config.sanitize();
        apply_config_to_state(&self.config, &self.state);
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if CAPTURE_SLOT.load(Ordering::Relaxed) != 0 {
            end_capture();
        }
        self.state.stop_play.store(true, Ordering::Relaxed);
        self.state.paused.store(false, Ordering::Relaxed);
        stop_recording(&self.state);

        // Let the playback thread release anything it was holding.
        let deadline = Instant::now() + Duration::from_millis(400);
        while self.state.playing.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

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

// ============================================================================
// Command line & headless playback
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

/// Plays a macro without any window. Shared by `--no-gui` and exported executables.
fn run_headless(data: MacroData, loops: u64, speed: f64, absolute: bool, delay_ms: u64) -> Result<()> {
    let (tx, rx) = unbounded();
    let state = AppState::new(tx);
    state.loop_play.store(loops == 0, Ordering::Relaxed);
    state.play_count_limit.store(loops.max(1), Ordering::Relaxed);
    state.absolute_mouse.store(absolute, Ordering::Relaxed);
    state.repeat_delay_ms.store(delay_ms, Ordering::Relaxed);
    *state.speed.lock() = speed.clamp(0.05, 10.0);

    std::thread::spawn(move || while rx.recv().is_ok() {});

    #[cfg(windows)]
    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("hooks".into())
            .spawn(move || input_hook_thread(st, HookMode::HotkeysOnly, false))?;
    }

    println!("Playing {} events…", data.events.len());
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

/// Taskbar / title-bar icon, with a fallback if the embedded blob is the wrong size.
fn load_window_icon() -> egui::IconData {
    if ICON_RGBA.len() == (ICON_SIZE * ICON_SIZE * 4) as usize {
        egui::IconData { rgba: ICON_RGBA.to_vec(), width: ICON_SIZE, height: ICON_SIZE }
    } else {
        warn!("embedded icon has an unexpected size - using the OS default");
        egui::IconData::default()
    }
}

fn main() -> Result<()> {
    init_epoch();
    let _log_guard = init_logging();
    platform::set_dpi_awareness();

    // An exported self-running executable carries its macro appended to the image.
    if let Some(payload) = read_self_payload() {
        platform::attach_parent_console();
        info!("running as an exported macro player");
        let mut data = payload.macro_data;
        if data.normalize().is_err() {
            return Ok(());
        }
        return run_headless(
            data,
            payload.loops,
            payload.speed,
            payload.absolute_mouse,
            payload.repeat_delay_ms,
        );
    }

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

    let mut config = load_config();
    publish_hotkeys(&config);
    info!("data directory: {}", paths::data_dir().display());

    if args.no_gui {
        platform::attach_parent_console();
        let path = args.play.clone().context("--no-gui requires --play <FILE>")?;
        let data = load_macro(&path)?;
        return run_headless(
            data,
            args.loops.unwrap_or(if config.loop_play { 0 } else { config.play_count_limit }),
            args.speed.unwrap_or(config.speed),
            config.absolute_mouse,
            config.repeat_delay_ms,
        );
    }

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
        let tray_on = config.tray_enabled;
        std::thread::Builder::new()
            .name("hooks".into())
            .spawn(move || input_hook_thread(st, HookMode::Full, tray_on))?;
    }

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
        .with_inner_size([460.0, 720.0])
        .with_min_inner_size([380.0, 420.0])
        .with_icon(load_window_icon())
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
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t_us: u64) -> MacroEvent {
        MacroEvent { t_us, kind: InputEventKind::MouseMove { x: 1, y: 2, dx: 0, dy: 0 } }
    }

    fn click(t_us: u64) -> MacroEvent {
        MacroEvent {
            t_us,
            kind: InputEventKind::MouseButton {
                button: MouseButton::Left,
                down: true,
                x: 5,
                y: 5,
            },
        }
    }

    #[test]
    fn roundtrip_v2() {
        let data = MacroData::new(vec![ev(0), ev(1000)], 5000);
        let back: MacroData = serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        assert_eq!(back.events.len(), 2);
        assert_eq!(back.duration_us, 5000);
        assert_eq!(back.version, 2);
    }

    #[test]
    fn accepts_legacy_v1_array() {
        let json = serde_json::to_string(&vec![ev(0), ev(42)]).unwrap();
        let data = parse_macro(&json).unwrap();
        assert_eq!(data.events.len(), 2);
        assert_eq!(data.duration_us, 42);
    }

    #[test]
    fn normalize_sorts_and_fixes_duration() {
        let mut data = MacroData::new(vec![ev(500), ev(100)], 0);
        data.normalize().unwrap();
        assert_eq!(data.events[0].t_us, 100);
        assert_eq!(data.duration_us, 500);
    }

    #[test]
    fn cycle_length_keeps_trailing_pause() {
        let data = MacroData::new(vec![ev(0), ev(3_000_000)], 5_000_000);
        assert_eq!(data.cycle_len_us(), 5_000_000);
    }

    fn key(t_us: u64, vk: u16, down: bool) -> MacroEvent {
        MacroEvent { t_us, kind: InputEventKind::Key { vk, scan: 0, down, extended: false } }
    }

    fn btn(t_us: u64, down: bool, x: i32, y: i32) -> MacroEvent {
        MacroEvent {
            t_us,
            kind: InputEventKind::MouseButton { button: MouseButton::Left, down, x, y },
        }
    }

    #[test]
    fn set_event_swaps_the_button() {
        let mut d = MacroData::new(vec![btn(0, true, 1, 2), btn(1000, false, 1, 2)], 1000);
        editor_set_event(
            &mut d,
            0,
            InputEventKind::MouseButton { button: MouseButton::Right, down: true, x: 1, y: 2 },
        );
        match d.events[0].kind {
            InputEventKind::MouseButton { button, .. } => {
                assert_eq!(button, MouseButton::Right)
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn replace_button_covers_only_the_selection() {
        let mut d = MacroData::new(
            vec![btn(0, true, 0, 0), btn(10, false, 0, 0), btn(20, true, 0, 0)],
            20,
        );
        let n = editor_replace_button(&mut d, 0, 1, MouseButton::Left, MouseButton::Right);
        assert_eq!(n, 2);
        match d.events[2].kind {
            InputEventKind::MouseButton { button, .. } => {
                assert_eq!(button, MouseButton::Left, "outside the range must not change")
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn shift_coords_moves_clicks_and_moves() {
        let mut d = MacroData::new(vec![ev(0), btn(10, true, 100, 100)], 10);
        editor_shift_coords(&mut d, 0, 1, 5, -7);
        match d.events[1].kind {
            InputEventKind::MouseButton { x, y, .. } => assert_eq!((x, y), (105, 93)),
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn set_time_cannot_jump_past_neighbours() {
        let mut d = MacroData::new(vec![ev(0), ev(1000), ev(2000)], 2000);
        editor_set_time(&mut d, 1, 9_999);
        assert_eq!(d.events[1].t_us, 2000);
        editor_set_time(&mut d, 1, 0);
        assert_eq!(d.events[1].t_us, 0);
    }

    #[test]
    fn duplicate_inserts_a_copy_and_shifts_the_tail() {
        let mut d = MacroData::new(vec![ev(0), ev(50_000)], 50_000);
        editor_duplicate(&mut d, 0);
        assert_eq!(d.events.len(), 3);
        assert_eq!(d.events[1].t_us, 10_000);
        assert_eq!(d.events[2].t_us, 60_000);
    }

    #[test]
    fn story_reports_a_click_not_two_events() {
        let events = vec![btn(0, true, 10, 20), btn(50_000, false, 10, 20)];
        let steps = summarize(&events, &EN);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].text.contains("(10, 20)"));
        assert_eq!((steps[0].first, steps[0].last), (0, 1));
    }

    #[test]
    fn story_detects_a_double_click() {
        let events = vec![
            btn(0, true, 5, 5),
            btn(40_000, false, 5, 5),
            btn(120_000, true, 5, 5),
            btn(160_000, false, 5, 5),
        ];
        let steps = summarize(&events, &EN);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].text.contains("double"));
        assert_eq!(steps[0].last, 3);
    }

    #[test]
    fn story_detects_a_drag() {
        let events = vec![
            btn(0, true, 0, 0),
            ev(10_000),
            MacroEvent {
                t_us: 20_000,
                kind: InputEventKind::MouseMove { x: 300, y: 400, dx: 0, dy: 0 },
            },
            btn(30_000, false, 300, 400),
        ];
        let steps = summarize(&events, &EN);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].text.contains("Dragged"), "{}", steps[0].text);
    }

    #[test]
    fn story_merges_typing_and_reports_a_pause() {
        let events = vec![
            key(0, 0x48, true),        // H
            key(10_000, 0x48, false),
            key(20_000, 0x49, true),   // I
            key(30_000, 0x49, false),
            key(2_000_000, 0x1B, true), // Esc, after a long gap
            key(2_010_000, 0x1B, false),
        ];
        let steps = summarize(&events, &EN);
        assert!(steps[0].text.contains("HI"), "{}", steps[0].text);
        assert!(steps[1].text.starts_with("Waited"), "{}", steps[1].text);
        assert!(steps[2].text.contains("Esc"), "{}", steps[2].text);
    }

    #[test]
    fn story_collapses_a_run_of_moves() {
        let events = vec![
            MacroEvent { t_us: 0, kind: InputEventKind::MouseMove { x: 1, y: 1, dx: 0, dy: 0 } },
            MacroEvent { t_us: 5_000, kind: InputEventKind::MouseMove { x: 50, y: 60, dx: 0, dy: 0 } },
            MacroEvent { t_us: 10_000, kind: InputEventKind::MouseMove { x: 99, y: 88, dx: 0, dy: 0 } },
        ];
        let steps = summarize(&events, &EN);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].text.contains("(99, 88)"));
        assert_eq!(steps[0].last, 2);
    }

    #[test]
    fn duration_formatting_switches_units() {
        assert_eq!(format_dur(500), "500 \u{b5}s");
        assert_eq!(format_dur(2_000), "2 ms");
        assert_eq!(format_dur(1_500_000), "1.5 s");
    }

    #[test]
    fn editor_delete_pulls_the_tail_back() {
        let mut d = MacroData::new(vec![ev(0), ev(1000), ev(2000), ev(3000)], 3000);
        editor_delete_range(&mut d, 1, 2);
        assert_eq!(d.events.len(), 2);
        assert_eq!(d.events[1].t_us, 2000); // 3000 - (2000 - 1000)
    }

    #[test]
    fn editor_crop_rebases_to_zero() {
        let mut d = MacroData::new(vec![ev(0), ev(1000), ev(2500)], 2500);
        editor_crop(&mut d, 1, 2);
        assert_eq!(d.events.len(), 2);
        assert_eq!(d.events[0].t_us, 0);
        assert_eq!(d.events[1].t_us, 1500);
        assert_eq!(d.duration_us, 1500);
    }

    #[test]
    fn editor_insert_delay_shifts_the_tail() {
        let mut d = MacroData::new(vec![ev(0), ev(1000)], 1000);
        editor_insert_delay(&mut d, 1, 500);
        assert_eq!(d.events[0].t_us, 0);
        assert_eq!(d.events[1].t_us, 501_000);
    }

    #[test]
    fn editor_scale_and_trim() {
        let mut d = MacroData::new(vec![ev(1000), ev(2000)], 2000);
        editor_scale(&mut d, 2.0);
        assert_eq!(d.events[1].t_us, 4000);
        editor_trim_lead(&mut d);
        assert_eq!(d.events[0].t_us, 0);
        assert_eq!(d.events[1].t_us, 2000);
    }

    #[test]
    fn editor_drop_moves_keeps_clicks() {
        let mut d = MacroData::new(vec![ev(0), click(10), ev(20)], 20);
        editor_drop_moves(&mut d);
        assert_eq!(d.events.len(), 1);
    }

    #[test]
    fn payload_roundtrip_through_a_fake_image() {
        let payload = Payload {
            loops: 3,
            speed: 1.5,
            absolute_mouse: true,
            repeat_delay_ms: 250,
            macro_data: MacroData::new(vec![ev(0), ev(1000)], 1000),
        };
        let mut image = b"MZ fake executable body".to_vec();
        let blob = gzip(&serde_json::to_vec(&payload).unwrap()).unwrap();
        image.extend_from_slice(&blob);
        image.extend_from_slice(&(blob.len() as u64).to_le_bytes());
        image.extend_from_slice(PAYLOAD_MAGIC);

        let start = payload_offset(&image).expect("payload should be found");
        let json = gunzip(&image[start..image.len() - 16]).unwrap();
        let back: Payload = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.loops, 3);
        assert_eq!(back.macro_data.events.len(), 2);
    }

    #[test]
    fn plain_image_has_no_payload() {
        assert!(payload_offset(b"MZ just an executable").is_none());
        assert!(payload_offset(b"tiny").is_none());
    }

    #[test]
    fn ahk_export_emits_a_loop_and_sleeps() {
        let dir = std::env::temp_dir().join("mr_ahk_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.ahk");
        let d = MacroData::new(vec![click(0), click(1_500_000)], 2_000_000);
        export_ahk(&path, &d, 4).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("Loop 4 {"));
        assert!(text.contains("Sleep 1500"));
        assert!(text.contains("Click"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn config_sanitize_clamps() {
        let mut cfg = AppConfig {
            speed: f64::NAN,
            play_count_limit: 100_000,
            jitter_pct: 900,
            mouse_sample_ms: 0,
            default_theme: 99,
            pixel_tolerance: 900,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.speed, 1.0);
        assert_eq!(cfg.play_count_limit, 9999);
        assert_eq!(cfg.jitter_pct, 50);
        assert_eq!(cfg.mouse_sample_ms, 1);
        assert_eq!(cfg.default_theme, THEME_NAMES.len() - 1);
        assert_eq!(cfg.pixel_tolerance, 255);
    }

    #[test]
    fn time_limit_math() {
        let cfg =
            AppConfig { time_limit_h: 1, time_limit_m: 2, time_limit_s: 3, ..Default::default() };
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
        assert_eq!(platform::normalize_abs(0, 0, 0, 0, 1920, 1080), (0, 0));
        assert_eq!(platform::normalize_abs(1919, 1079, 0, 0, 1920, 1080), (65535, 65535));
    }

    #[test]
    fn language_overrides_apply() {
        let mut map = BTreeMap::new();
        map.insert("record".to_string(), "REC!".to_string());
        map.insert("play".to_string(), String::new()); // empty values are ignored
        let s = EN.with_overrides(&map);
        assert_eq!(s.record, "REC!");
        assert_eq!(s.play, EN.play);
        assert!(s.to_map().contains_key("stop_play"));
    }

    #[test]
    fn vk_names_cover_the_common_keys() {
        assert_eq!(vk_name(0x77), "F8");
        assert_eq!(vk_name(0x13), "Pause");
        assert_eq!(vk_name(0x41), "A");
        assert_eq!(vk_name(0x30), "0");
        assert!(vk_name(0xFE).starts_with("VK "));
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

    #[test]
    fn profile_names_are_sanitized() {
        let p = profile_path("farm/../evil");
        assert!(p.file_name().unwrap().to_string_lossy().starts_with("farm_"));
    }
}
