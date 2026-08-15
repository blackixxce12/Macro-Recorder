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
    pub use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
        CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetPixel,
        HGDIOBJ, HRGN, ReleaseDC, SRCCOPY, SelectObject,
    };
    pub use windows::Win32::System::DataExchange::*;
    pub use windows::Win32::System::Memory::*;
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
    // BOOL moved out of Win32::Foundation into windows-core in the 0.62 family;
    // the EnumWindows callback has to return exactly that type.
    pub use windows::core::{BOOL, PCSTR, PCWSTR, w};
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
const HK_ID_FASTER: i32 = 5;
const HK_ID_SLOWER: i32 = 6;
const HK_ID_SKIP: i32 = 7;

/// Every hotkey id, in the order the slots are numbered in the UI.
const HK_IDS: [i32; 7] = [
    HK_ID_RECORD,
    HK_ID_PLAY,
    HK_ID_STOP,
    HK_ID_PAUSE,
    HK_ID_FASTER,
    HK_ID_SLOWER,
    HK_ID_SKIP,
];

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
    /// Uniform value in `-span..=span`.
    fn signed(&mut self, span: i64) -> i64 {
        if span <= 0 { 0 } else { (self.next_u64() % (span as u64 * 2 + 1)) as i64 - span }
    }
    /// Uniform value in `-1.0..=1.0`.
    fn unit(&mut self) -> f32 {
        self.below(2001) as f32 / 1000.0 - 1.0
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
static HK_VK: [AtomicU32; 7] = [
    AtomicU32::new(0x75), // F6 record
    AtomicU32::new(0x76), // F7 play
    AtomicU32::new(0x78), // F9 emergency stop
    AtomicU32::new(0x77), // F8 pause
    AtomicU32::new(0),    // faster
    AtomicU32::new(0),    // slower
    AtomicU32::new(0),    // skip step
];

/// Bit mask of hotkeys that failed to register.
static HK_FAILED: AtomicU32 = AtomicU32::new(0);
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

static PENDING_HOTKEYS: Mutex<[Hotkey; 7]> = Mutex::new([
    Hotkey::plain(0x75),
    Hotkey::plain(0x76),
    Hotkey::plain(0x78),
    Hotkey::plain(0x77),
    Hotkey::plain(0),
    Hotkey::plain(0),
    Hotkey::plain(0),
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
    let hk = [
        cfg.hotkey_record,
        cfg.hotkey_play,
        cfg.hotkey_stop,
        cfg.hotkey_pause,
        cfg.hotkey_faster,
        cfg.hotkey_slower,
        cfg.hotkey_skip,
    ];
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
    /// Also stretch coordinates when the anchored window changed size.
    pub anchor_scale: bool,
    /// Glide along a curved path instead of teleporting the cursor.
    pub human_mouse: bool,
    /// 0-100: how far the arc bows away from the straight line.
    pub human_curve: u64,
    /// Random spread applied to every target point, in pixels.
    pub mouse_jitter_px: i32,

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
    pub hotkey_faster: Hotkey,
    pub hotkey_slower: Hotkey,
    pub hotkey_skip: Hotkey,

    // schedule
    pub schedule_enabled: bool,
    pub schedule_h: u32,
    pub schedule_m: u32,
    /// Bit 0 = Monday … bit 6 = Sunday.
    pub schedule_days: u8,

    // target window
    pub target_title: String,
    pub target_pause_unfocused: bool,

    // files
    pub recent_files: Vec<String>,
    pub compress_on_save: bool,

    // image search
    pub img_threshold: f64,
    pub img_multiscale: bool,
    pub img_region_enabled: bool,
    pub img_rx: i32,
    pub img_ry: i32,
    pub img_rw: i32,
    pub img_rh: i32,
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
            anchor_scale: true,
            human_mouse: false,
            human_curve: 35,
            mouse_jitter_px: 0,

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
            hotkey_faster: Hotkey::plain(0),
            hotkey_slower: Hotkey::plain(0),
            hotkey_skip: Hotkey::plain(0),

            schedule_enabled: false,
            schedule_h: 9,
            schedule_m: 0,
            schedule_days: 0b0111_1111,

            target_title: String::new(),
            target_pause_unfocused: false,

            recent_files: Vec::new(),
            compress_on_save: false,

            img_threshold: 0.85,
            img_multiscale: false,
            img_region_enabled: false,
            img_rx: 0,
            img_ry: 0,
            img_rw: 800,
            img_rh: 600,
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
        self.human_curve = self.human_curve.min(100);
        self.mouse_jitter_px = self.mouse_jitter_px.clamp(0, 60);
        self.schedule_h = self.schedule_h.min(23);
        self.schedule_m = self.schedule_m.min(59);
        self.target_title.truncate(120);
        self.mouse_sample_ms = self.mouse_sample_ms.clamp(1, 100);
        self.time_limit_h = self.time_limit_h.min(240);
        self.time_limit_m = self.time_limit_m.min(59);
        self.time_limit_s = self.time_limit_s.min(59);
        self.action_on_completion = self.action_on_completion.min(EndAction::COUNT - 1);
        self.shutdown_delay_s = self.shutdown_delay_s.min(600);
        self.pixel_tolerance = self.pixel_tolerance.min(255);
        self.pixel_mode = self.pixel_mode.min(1);
        self.recent_files.truncate(8);
        self.img_threshold = if self.img_threshold.is_finite() {
            self.img_threshold.clamp(0.3, 1.0)
        } else {
            0.85
        };
        self.img_rw = self.img_rw.clamp(8, 32000);
        self.img_rh = self.img_rh.clamp(8, 32000);
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

// ============================================================================
// Script model
// ============================================================================

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    fn test(self, a: f64, b: f64) -> bool {
        match self {
            Cmp::Eq => (a - b).abs() < f64::EPSILON,
            Cmp::Ne => (a - b).abs() >= f64::EPSILON,
            Cmp::Lt => a < b,
            Cmp::Le => a <= b,
            Cmp::Gt => a > b,
            Cmp::Ge => a >= b,
        }
    }
    fn symbol(self) -> &'static str {
        match self {
            Cmp::Eq => "==",
            Cmp::Ne => "!=",
            Cmp::Lt => "<",
            Cmp::Le => "<=",
            Cmp::Gt => ">",
            Cmp::Ge => ">=",
        }
    }
    const ALL: [Cmp; 6] = [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge];
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum VarOp {
    Set,
    Add,
    Sub,
    Mul,
}

impl VarOp {
    fn apply(self, cur: f64, v: f64) -> f64 {
        match self {
            VarOp::Set => v,
            VarOp::Add => cur + v,
            VarOp::Sub => cur - v,
            VarOp::Mul => cur * v,
        }
    }
    fn symbol(self) -> &'static str {
        match self {
            VarOp::Set => "=",
            VarOp::Add => "+=",
            VarOp::Sub => "-=",
            VarOp::Mul => "*=",
        }
    }
    const ALL: [VarOp; 4] = [VarOp::Set, VarOp::Add, VarOp::Sub, VarOp::Mul];
}

/// Something the script can ask about the screen or about itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Condition {
    Always,
    Var { name: String, cmp: Cmp, value: f64 },
    /// A template from the `templates/` folder is on screen.
    Image { template: String, threshold: f64 },
    Pixel { x: i32, y: i32, r: u8, g: u8, b: u8, tol: u32 },
    Window { title: String },
    /// Text recognised inside a screen rectangle contains `needle`.
    Text { x: i32, y: i32, w: i32, h: i32, needle: String },
}

impl Condition {
    fn kind_index(&self) -> usize {
        match self {
            Condition::Always => 0,
            Condition::Var { .. } => 1,
            Condition::Image { .. } => 2,
            Condition::Pixel { .. } => 3,
            Condition::Window { .. } => 4,
            Condition::Text { .. } => 5,
        }
    }
    fn from_index(i: usize) -> Self {
        match i {
            1 => Condition::Var { name: "count".into(), cmp: Cmp::Lt, value: 10.0 },
            2 => Condition::Image { template: String::new(), threshold: 0.85 },
            3 => Condition::Pixel { x: 0, y: 0, r: 255, g: 0, b: 0, tol: 20 },
            4 => Condition::Window { title: String::new() },
            5 => Condition::Text { x: 0, y: 0, w: 400, h: 120, needle: String::new() },
            _ => Condition::Always,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StepKind {
    /// Replays a slice of the recorded events, honouring their original timing.
    PlayEvents { from: usize, to: usize },
    Wait { ms: u64 },
    /// Blocks until a condition becomes true (or false, when `appear` is off).
    WaitFor { cond: Condition, appear: bool, timeout_ms: u64 },
    ClickImage { template: String, threshold: f64, button: MouseButton },
    Click { x: i32, y: i32, button: MouseButton },
    Key { vk: u16, down: bool },
    SetVar { name: String, op: VarOp, value: f64 },
    If { cond: Condition },
    Else,
    EndIf,
    While { cond: Condition },
    EndWhile,
    Break,
    Run { path: String, args: String },
    Exit,
    Log { text: String },
    /// Recognises a screen rectangle and stores the first number it finds.
    ReadNumber { x: i32, y: i32, w: i32, h: i32, var: String },
}

impl StepKind {
    /// Order used by the "Add" menu and the kind picker.
    const COUNT: usize = 17;

    fn index(&self) -> usize {
        match self {
            StepKind::PlayEvents { .. } => 0,
            StepKind::Wait { .. } => 1,
            StepKind::WaitFor { .. } => 2,
            StepKind::ClickImage { .. } => 3,
            StepKind::Click { .. } => 4,
            StepKind::Key { .. } => 5,
            StepKind::SetVar { .. } => 6,
            StepKind::If { .. } => 7,
            StepKind::Else => 8,
            StepKind::EndIf => 9,
            StepKind::While { .. } => 10,
            StepKind::EndWhile => 11,
            StepKind::Break => 12,
            StepKind::Run { .. } => 13,
            StepKind::Exit => 14,
            StepKind::Log { .. } => 15,
            StepKind::ReadNumber { .. } => 16,
        }
    }

    fn from_index(i: usize) -> Self {
        match i {
            1 => StepKind::Wait { ms: 1000 },
            2 => StepKind::WaitFor {
                cond: Condition::Image { template: String::new(), threshold: 0.85 },
                appear: true,
                timeout_ms: 10_000,
            },
            3 => StepKind::ClickImage {
                template: String::new(),
                threshold: 0.85,
                button: MouseButton::Left,
            },
            4 => StepKind::Click { x: 0, y: 0, button: MouseButton::Left },
            5 => StepKind::Key { vk: 0x20, down: true },
            6 => StepKind::SetVar { name: "count".into(), op: VarOp::Add, value: 1.0 },
            7 => StepKind::If { cond: Condition::Always },
            8 => StepKind::Else,
            9 => StepKind::EndIf,
            10 => StepKind::While { cond: Condition::Always },
            11 => StepKind::EndWhile,
            12 => StepKind::Break,
            13 => StepKind::Run { path: String::new(), args: String::new() },
            14 => StepKind::Exit,
            15 => StepKind::Log { text: String::new() },
            16 => StepKind::ReadNumber {
                x: 0,
                y: 0,
                w: 300,
                h: 80,
                var: "amount".into(),
            },
            _ => StepKind::PlayEvents { from: 0, to: 0 },
        }
    }

    fn name(&self, s: &Strings) -> &'static str {
        match self {
            StepKind::PlayEvents { .. } => s.k_play,
            StepKind::Wait { .. } => s.k_wait,
            StepKind::WaitFor { .. } => s.k_waitfor,
            StepKind::ClickImage { .. } => s.k_clickimg,
            StepKind::Click { .. } => s.k_click,
            StepKind::Key { .. } => s.k_key,
            StepKind::SetVar { .. } => s.k_setvar,
            StepKind::If { .. } => s.k_if,
            StepKind::Else => s.k_else,
            StepKind::EndIf => s.k_endif,
            StepKind::While { .. } => s.k_while,
            StepKind::EndWhile => s.k_endwhile,
            StepKind::Break => s.k_break,
            StepKind::Run { .. } => s.k_run,
            StepKind::Exit => s.k_exit,
            StepKind::Log { .. } => s.k_log,
            StepKind::ReadNumber { .. } => s.k_readnum,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScriptStep {
    pub kind: StepKind,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl ScriptStep {
    fn new(kind: StepKind) -> Self {
        Self { kind, enabled: true }
    }
}

fn describe_condition(c: &Condition, s: &Strings) -> String {
    match c {
        Condition::Always => s.c_always.to_string(),
        Condition::Var { name, cmp, value } => format!("{name} {} {value}", cmp.symbol()),
        Condition::Image { template, threshold } => {
            format!("{}: {template} ≥ {threshold:.2}", s.c_image)
        }
        Condition::Pixel { x, y, r, g, b, tol } => {
            format!("{}: ({x},{y}) = {r},{g},{b} ±{tol}", s.c_pixel)
        }
        Condition::Window { title } => format!("{}: {title}", s.c_window),
        Condition::Text { x, y, w, h, needle } => {
            format!("{}: \"{needle}\" @ ({x},{y} {w}x{h})", s.c_text)
        }
    }
}

fn describe_step(step: &ScriptStep, s: &Strings, total_events: usize) -> String {
    let name = step.kind.name(s);
    match &step.kind {
        StepKind::PlayEvents { from, to } => {
            let covered = to.saturating_sub(*from) + 1;
            format!("{name} {from}…{to}  ({covered}/{total_events})")
        }
        StepKind::Wait { ms } => format!("{name} {ms} ms"),
        StepKind::WaitFor { cond, appear, timeout_ms } => format!(
            "{name} {} {} ({timeout_ms} ms)",
            describe_condition(cond, s),
            if *appear { s.f_appear } else { s.f_gone }
        ),
        StepKind::ClickImage { template, threshold, .. } => {
            format!("{name}: {template} ≥ {threshold:.2}")
        }
        StepKind::Click { x, y, button } => format!("{name} ({x}, {y}) {button:?}"),
        StepKind::Key { vk, down } => {
            format!("{name} {} {}", vk_name(*vk as u32), if *down { s.st_down } else { s.st_up })
        }
        StepKind::SetVar { name: v, op, value } => {
            format!("{name} {v} {} {value}", op.symbol())
        }
        StepKind::If { cond } | StepKind::While { cond } => {
            format!("{name} {}", describe_condition(cond, s))
        }
        StepKind::Run { path, args } => format!("{name} {path} {args}"),
        StepKind::Log { text } => format!("{name}: {text}"),
        StepKind::ReadNumber { x, y, w, h, var } => {
            format!("{name} ({x},{y} {w}x{h}) → {var}")
        }
        _ => name.to_string(),
    }
}

/// Where each block-opening step jumps to.
#[derive(Clone, Debug, Default)]
pub struct Blocks {
    /// For `If`: the matching `Else`, if any.
    pub else_of: Vec<Option<usize>>,
    /// For `If`/`While`: the matching `EndIf`/`EndWhile`.
    pub end_of: Vec<Option<usize>>,
    /// For `Else`/`EndWhile`: the step that opened the block.
    pub start_of: Vec<Option<usize>>,
}

/// Matches block openers with their closers.
///
/// Resolved once before the script runs, so the interpreter itself never has to
/// scan for a matching `EndIf` - and an unbalanced script is reported up front
/// instead of behaving strangely halfway through.
fn resolve_blocks(steps: &[ScriptStep]) -> std::result::Result<Blocks, String> {
    let n = steps.len();
    let mut b = Blocks {
        else_of: vec![None; n],
        end_of: vec![None; n],
        start_of: vec![None; n],
    };
    // (index, is_while)
    let mut stack: Vec<(usize, bool)> = Vec::new();

    for (i, st) in steps.iter().enumerate() {
        match st.kind {
            StepKind::If { .. } => stack.push((i, false)),
            StepKind::While { .. } => stack.push((i, true)),
            StepKind::Else => match stack.last() {
                Some(&(open, false)) => {
                    if b.else_of[open].is_some() {
                        return Err(format!("{} #{i}", "Else"));
                    }
                    b.else_of[open] = Some(i);
                    b.start_of[i] = Some(open);
                }
                _ => return Err(format!("Else #{i}")),
            },
            StepKind::EndIf => match stack.pop() {
                Some((open, false)) => {
                    b.end_of[open] = Some(i);
                    b.start_of[i] = Some(open);
                    if let Some(e) = b.else_of[open] {
                        b.end_of[e] = Some(i);
                    }
                }
                Some((open, true)) => return Err(format!("While #{open}")),
                None => return Err(format!("End if #{i}")),
            },
            StepKind::EndWhile => match stack.pop() {
                Some((open, true)) => {
                    b.end_of[open] = Some(i);
                    b.start_of[i] = Some(open);
                }
                Some((open, false)) => return Err(format!("If #{open}")),
                None => return Err(format!("End while #{i}")),
            },
            _ => {}
        }
    }
    if let Some(&(open, _)) = stack.last() {
        return Err(format!("#{open}"));
    }
    Ok(b)
}

/// Index of the first step that can never run.
///
/// Anything after an `Exit` at the outermost level is dead: the program is gone
/// before it gets there. Easy to create by accident, invisible without a marker.
fn first_unreachable(steps: &[ScriptStep]) -> Option<usize> {
    let depths = script_depths(steps);
    steps.iter().enumerate().find_map(|(i, st)| {
        if matches!(st.kind, StepKind::Exit) && st.enabled && depths[i] == 0 && i + 1 < steps.len()
        {
            Some(i + 1)
        } else {
            None
        }
    })
}

/// Indentation level of every step, for the editor list.
fn script_depths(steps: &[ScriptStep]) -> Vec<usize> {
    let mut out = Vec::with_capacity(steps.len());
    let mut depth = 0usize;
    for st in steps {
        match st.kind {
            StepKind::EndIf | StepKind::EndWhile => depth = depth.saturating_sub(1),
            StepKind::Else => {}
            _ => {}
        }
        let shown = match st.kind {
            StepKind::Else => depth.saturating_sub(1),
            _ => depth,
        };
        out.push(shown);
        if matches!(st.kind, StepKind::If { .. } | StepKind::While { .. }) {
            depth += 1;
        }
    }
    out
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
    /// Optional program. Empty means "just replay the events", which is what every
    /// macro recorded before version 3 does.
    #[serde(default)]
    pub script: Vec<ScriptStep>,
    /// Starting values for the script's variables.
    #[serde(default)]
    pub vars: std::collections::BTreeMap<String, f64>,
}

fn format_version() -> u32 {
    3
}

impl MacroData {
    fn new(events: Vec<MacroEvent>, duration_us: u64) -> Self {
        Self {
            version: 3,
            duration_us,
            anchor: None,
            events,
            script: Vec::new(),
            vars: Default::default(),
        }
    }

    fn has_script(&self) -> bool {
        self.script.iter().any(|s| s.enabled)
    }
    fn is_empty(&self) -> bool {
        self.events.is_empty() && self.script.is_empty()
    }
    fn last_t(&self) -> u64 {
        self.events.last().map(|e| e.t_us).unwrap_or(0)
    }
    fn cycle_len_us(&self) -> u64 {
        self.duration_us.max(self.last_t()).max(1)
    }
    /// Sorts non-monotonic timestamps and rejects obviously broken files.
    fn normalize(&mut self) -> Result<()> {
        if self.events.is_empty() && self.script.is_empty() {
            anyhow::bail!("macro contains no events and no script");
        }
        if let Err(e) = resolve_blocks(&self.script) {
            anyhow::bail!("script has unbalanced blocks near {e}");
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
// Image search
// ============================================================================

/// Normalised cross-correlation template matching.
///
/// Written here rather than pulled from a crate on purpose: `template-matching`
/// drags in the whole wgpu stack for a 6 MB app, and `imageproc::match_template`
/// has no mask, no scale search and no coarse pass. What this needs is narrow and
/// specific - search a screen buffer we already own, ignore transparent pixels,
/// tolerate a slightly different size - and that is about 150 lines.
pub mod vision {
    /// A rectangle of screen pixels, RGBA, plus where it came from.
    #[derive(Clone)]
    pub struct Frame {
        pub x: i32,
        pub y: i32,
        pub w: u32,
        pub h: u32,
        pub rgba: Vec<u8>,
    }

    /// A picture to look for, kept both as pixels and as a prepared grey plane.
    #[derive(Clone)]
    pub struct Template {
        pub w: u32,
        pub h: u32,
        pub rgba: Vec<u8>,
        pub name: String,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Hit {
        /// Centre of the match, in screen coordinates.
        pub x: i32,
        pub y: i32,
        pub score: f32,
        pub scale: f32,
    }

    fn luma(rgba: &[u8], i: usize) -> f32 {
        0.299 * rgba[i] as f32 + 0.587 * rgba[i + 1] as f32 + 0.114 * rgba[i + 2] as f32
    }

    /// Grey plane plus a mask: fully transparent pixels take no part in the score,
    /// which is what lets a non-rectangular icon be matched.
    fn plane(rgba: &[u8], w: u32, h: u32) -> (Vec<f32>, Vec<bool>) {
        let n = (w * h) as usize;
        let mut g = vec![0.0; n];
        let mut m = vec![true; n];
        for i in 0..n {
            g[i] = luma(rgba, i * 4);
            m[i] = rgba[i * 4 + 3] >= 16;
        }
        (g, m)
    }

    /// Nearest-neighbour box shrink. Good enough: the coarse pass only has to get
    /// close, and the refine pass runs at full resolution.
    fn shrink(src: &[f32], w: u32, h: u32, step: u32) -> (Vec<f32>, u32, u32) {
        if step <= 1 {
            return (src.to_vec(), w, h);
        }
        let (nw, nh) = ((w / step).max(1), (h / step).max(1));
        let mut out = vec![0.0; (nw * nh) as usize];
        for y in 0..nh {
            for x in 0..nw {
                let mut acc = 0.0;
                let mut cnt = 0.0;
                for dy in 0..step {
                    for dx in 0..step {
                        let sx = x * step + dx;
                        let sy = y * step + dy;
                        if sx < w && sy < h {
                            acc += src[(sy * w + sx) as usize];
                            cnt += 1.0;
                        }
                    }
                }
                out[(y * nw + x) as usize] = if cnt > 0.0 { acc / cnt } else { 0.0 };
            }
        }
        (out, nw, nh)
    }

    fn shrink_mask(src: &[bool], w: u32, h: u32, step: u32) -> (Vec<bool>, u32, u32) {
        if step <= 1 {
            return (src.to_vec(), w, h);
        }
        let (nw, nh) = ((w / step).max(1), (h / step).max(1));
        let mut out = vec![false; (nw * nh) as usize];
        for y in 0..nh {
            for x in 0..nw {
                // A coarse pixel counts only if its whole block counted.
                let mut all = true;
                for dy in 0..step {
                    for dx in 0..step {
                        let sx = x * step + dx;
                        let sy = y * step + dy;
                        if sx < w && sy < h && !src[(sy * w + sx) as usize] {
                            all = false;
                        }
                    }
                }
                out[(y * nw + x) as usize] = all;
            }
        }
        (out, nw, nh)
    }

    /// Resamples a template to a different size, nearest neighbour.
    fn resize_rgba(rgba: &[u8], w: u32, h: u32, nw: u32, nh: u32) -> Vec<u8> {
        let mut out = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            let sy = (y * h / nh).min(h - 1);
            for x in 0..nw {
                let sx = (x * w / nw).min(w - 1);
                let d = ((y * nw + x) * 4) as usize;
                let sidx = ((sy * w + sx) * 4) as usize;
                out[d..d + 4].copy_from_slice(&rgba[sidx..sidx + 4]);
            }
        }
        out
    }

    /// Correlation of one template placed at (ox, oy) in the haystack.
    ///
    /// Returns a value in -1.0 ..= 1.0; 1.0 means identical up to brightness and
    /// contrast, which is why a screenshot taken under a different theme still
    /// matches.
    fn score_at(
        hay: &[f32],
        hw: u32,
        _hh: u32,
        tpl: &[f32],
        mask: &[bool],
        tw: u32,
        th: u32,
        ox: u32,
        oy: u32,
    ) -> f32 {
        let mut n = 0.0f32;
        let mut sum_i = 0.0f32;
        let mut sum_t = 0.0f32;
        for y in 0..th {
            let hrow = ((oy + y) * hw) as usize;
            let trow = (y * tw) as usize;
            for x in 0..tw {
                if mask[trow + x as usize] {
                    sum_i += hay[hrow + (ox + x) as usize];
                    sum_t += tpl[trow + x as usize];
                    n += 1.0;
                }
            }
        }
        if n < 4.0 {
            return -1.0;
        }
        let (mi, mt) = (sum_i / n, sum_t / n);
        let mut num = 0.0f32;
        let mut di = 0.0f32;
        let mut dt = 0.0f32;
        for y in 0..th {
            let hrow = ((oy + y) * hw) as usize;
            let trow = (y * tw) as usize;
            for x in 0..tw {
                if mask[trow + x as usize] {
                    let a = hay[hrow + (ox + x) as usize] - mi;
                    let b = tpl[trow + x as usize] - mt;
                    num += a * b;
                    di += a * a;
                    dt += b * b;
                }
            }
        }
        let den = (di * dt).sqrt();
        if den <= f32::EPSILON { -1.0 } else { num / den }
    }

    /// Best position of `tpl` inside `hay` at one fixed scale.
    fn find_at_scale(hay: &Frame, tpl_rgba: &[u8], tw: u32, th: u32) -> Option<(u32, u32, f32)> {
        if tw == 0 || th == 0 || tw > hay.w || th > hay.h {
            return None;
        }
        let (hg, _) = plane(&hay.rgba, hay.w, hay.h);
        let (tg, tm) = plane(tpl_rgba, tw, th);

        // Coarse pass on a shrunken copy: a full-resolution sweep of a 4K screen is
        // billions of operations, and the answer is always in the same place anyway.
        let step = (th.min(tw) / 12).clamp(1, 8);
        let (chay, chw, chh) = shrink(&hg, hay.w, hay.h, step);
        let (ctpl, ctw, cth) = shrink(&tg, tw, th, step);
        let (cmask, _, _) = shrink_mask(&tm, tw, th, step);

        let mut best = (0u32, 0u32, -1.0f32);
        if ctw > 0 && cth > 0 && ctw <= chw && cth <= chh {
            for oy in 0..=(chh - cth) {
                for ox in 0..=(chw - ctw) {
                    let sc = score_at(&chay, chw, chh, &ctpl, &cmask, ctw, cth, ox, oy);
                    if sc > best.2 {
                        best = (ox, oy, sc);
                    }
                }
            }
        }
        if best.2 < -0.5 {
            return None;
        }

        // Refine around the coarse winner at full resolution.
        let cx = best.0 * step;
        let cy = best.1 * step;
        let pad = step * 2 + 2;
        let x0 = cx.saturating_sub(pad);
        let y0 = cy.saturating_sub(pad);
        let x1 = (cx + pad).min(hay.w - tw);
        let y1 = (cy + pad).min(hay.h - th);

        let mut fine = (x0, y0, -1.0f32);
        for oy in y0..=y1 {
            for ox in x0..=x1 {
                let sc = score_at(&hg, hay.w, hay.h, &tg, &tm, tw, th, ox, oy);
                if sc > fine.2 {
                    fine = (ox, oy, sc);
                }
            }
        }
        Some(fine)
    }

    /// Looks for `tpl` in `hay`, optionally trying nearby sizes.
    ///
    /// Returns the best hit found even when it is below `threshold`, so the UI can
    /// tell "nothing like it on screen" apart from "almost, try a lower threshold".
    pub fn find(hay: &Frame, tpl: &Template, multiscale: bool) -> Option<Hit> {
        let scales: &[f32] =
            if multiscale { &[1.0, 0.9, 1.1, 0.8, 1.25] } else { &[1.0] };
        let mut best: Option<Hit> = None;
        for &sc in scales {
            let tw = ((tpl.w as f32 * sc).round() as u32).max(2);
            let th = ((tpl.h as f32 * sc).round() as u32).max(2);
            let rgba = if (sc - 1.0).abs() < f32::EPSILON {
                tpl.rgba.clone()
            } else {
                resize_rgba(&tpl.rgba, tpl.w, tpl.h, tw, th)
            };
            if let Some((ox, oy, score)) = find_at_scale(hay, &rgba, tw, th) {
                let hit = Hit {
                    x: hay.x + ox as i32 + tw as i32 / 2,
                    y: hay.y + oy as i32 + th as i32 / 2,
                    score,
                    scale: sc,
                };
                if best.map(|b| hit.score > b.score).unwrap_or(true) {
                    best = Some(hit);
                }
            }
        }
        best
    }
}

// ============================================================================
// OCR
// ============================================================================

/// Text recognition.
///
/// The backend sits behind a cargo feature so a second one (ONNX models via
/// `oar-ocr`) can be added later without touching anything above this line, and so
/// a build that cannot use WinRT still compiles with `--no-default-features`.
pub mod ocr {
    /// One recognised line, in screen coordinates.
    #[derive(Clone, Debug, PartialEq)]
    pub struct TextBox {
        pub text: String,
        pub x: i32,
        pub y: i32,
        pub w: i32,
        pub h: i32,
    }

    /// Loose text comparison for screen text.
    ///
    /// OCR output is never exactly what a human reads: case wanders, whitespace
    /// doubles up, and stray punctuation appears at the edges. Comparing the
    /// squashed, lower-cased forms is far more useful than an exact match.
    pub fn text_matches(haystack: &str, needle: &str) -> bool {
        fn squash(s: &str) -> String {
            s.to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric() || c.is_whitespace())
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        }
        let n = squash(needle);
        !n.is_empty() && squash(haystack).contains(&n)
    }

    /// First number in a piece of recognised text.
    ///
    /// Handles the thousands separators OCR picks up from game UIs ("1,250" and
    /// "1 250") and ignores a trailing period so "Gems: 500." reads as 500.
    pub fn first_number(text: &str) -> Option<f64> {
        let mut cur = String::new();
        let mut best: Option<String> = None;
        for ch in text.chars().chain(std::iter::once(' ')) {
            if ch.is_ascii_digit() {
                cur.push(ch);
            } else if (ch == ',' || ch == ' ' || ch == '\u{a0}' || ch == '.') && !cur.is_empty() {
                // Could be a separator inside a number: keep going, decide later.
                cur.push('\u{1}');
            } else if !cur.is_empty() {
                best = Some(std::mem::take(&mut cur));
                break;
            }
        }
        if best.is_none() && !cur.is_empty() {
            best = Some(cur);
        }
        let raw = best?;
        let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
        digits.parse::<f64>().ok()
    }

    /// Reads a clock like `02:34` or `1:02:03` as a number of seconds.
    pub fn parse_clock(text: &str) -> Option<f64> {
        let cleaned: String = text
            .chars()
            .map(|c| if c.is_ascii_digit() || c == ':' { c } else { ' ' })
            .collect();
        for token in cleaned.split_whitespace() {
            let parts: Vec<&str> = token.split(':').filter(|p| !p.is_empty()).collect();
            if parts.len() < 2 || parts.len() > 3 {
                continue;
            }
            let nums: Option<Vec<f64>> =
                parts.iter().map(|p| p.parse::<f64>().ok()).collect();
            let nums = nums?;
            return Some(match nums.len() {
                2 => nums[0] * 60.0 + nums[1],
                _ => nums[0] * 3600.0 + nums[1] * 60.0 + nums[2],
            });
        }
        None
    }

    /// Joins every recognised line into one string.
    pub fn joined(boxes: &[TextBox]) -> String {
        boxes.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    /// Enlarges a frame and converts it to the BGRA order SoftwareBitmap expects.
    ///
    /// Nearest neighbour on purpose: rendered glyphs are hard-edged, and smoothing
    /// them on the way up makes recognition worse, not better.
    #[cfg(all(windows, feature = "winocr"))]
    fn upscale_to_bgra(rgba: &[u8], w: u32, h: u32, k: u32) -> (Vec<u8>, u32, u32) {
        let (nw, nh) = (w * k, h * k);
        let mut out = vec![0u8; (nw as usize) * (nh as usize) * 4];
        for y in 0..nh {
            let sy = y / k;
            for x in 0..nw {
                let sx = x / k;
                let s = ((sy * w + sx) * 4) as usize;
                let d = ((y * nw + x) * 4) as usize;
                out[d] = rgba[s + 2];
                out[d + 1] = rgba[s + 1];
                out[d + 2] = rgba[s];
                out[d + 3] = 255;
            }
        }
        (out, nw, nh)
    }

    #[cfg(all(windows, feature = "winocr"))]
    pub fn recognize(frame: &crate::vision::Frame) -> anyhow::Result<Vec<TextBox>> {
        use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
        use windows::Media::Ocr::OcrEngine;
        use windows::Security::Cryptography::CryptographicBuffer;

        if frame.w == 0 || frame.h == 0 {
            return Ok(Vec::new());
        }
        // Windows OCR returns nothing at all for images under 40x40, and small
        // interface text reads much better enlarged. Scale so the short side clears
        // that floor with room to spare, while keeping the long side inside the
        // engine's own limit of roughly 4096 pixels.
        let short = frame.w.min(frame.h).max(1);
        let long = frame.w.max(frame.h).max(1);
        let want = (64 + short - 1) / short;
        let cap = (4000 / long).max(1);
        let scale = want.clamp(1, 8).min(cap);
        if short * scale < 40 {
            // Fully qualified: this module deliberately imports nothing from the
            // crate root, and an unqualified `warn!` is a rustc attribute here.
            tracing::warn!(
                "region {}x{} is too small for Windows OCR even scaled {scale}x",
                frame.w, frame.h
            );
        }

        let (bgra, bw, bh) = upscale_to_bgra(&frame.rgba, frame.w, frame.h, scale);
        let buffer = CryptographicBuffer::CreateFromByteArray(&bgra)?;
        let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
            &buffer,
            BitmapPixelFormat::Bgra8,
            bw as i32,
            bh as i32,
        )?;

        let engine = OcrEngine::TryCreateFromUserProfileLanguages()?;
        // windows-future 0.3 renamed the blocking wait from get() to join();
        // this runs on the playback thread, so blocking here is intended.
        let result = engine.RecognizeAsync(&bitmap)?.join()?;

        let mut out = Vec::new();
        for line in result.Lines()? {
            let text = line.Text()?.to_string();
            if text.trim().is_empty() {
                continue;
            }
            // A line has no rectangle of its own: take the union of its words.
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for word in line.Words()? {
                let r = word.BoundingRect()?;
                x0 = x0.min(r.X);
                y0 = y0.min(r.Y);
                x1 = x1.max(r.X + r.Width);
                y1 = y1.max(r.Y + r.Height);
            }
            if x0 > x1 {
                x0 = 0.0;
                y0 = 0.0;
                x1 = 0.0;
                y1 = 0.0;
            }
            let k = scale as f32;
            out.push(TextBox {
                text,
                x: frame.x + (x0 / k) as i32,
                y: frame.y + (y0 / k) as i32,
                w: ((x1 - x0) / k) as i32,
                h: ((y1 - y0) / k) as i32,
            });
        }
        Ok(out)
    }

    #[cfg(not(all(windows, feature = "winocr")))]
    pub fn recognize(_frame: &crate::vision::Frame) -> anyhow::Result<Vec<TextBox>> {
        Err(anyhow::anyhow!("this build has no OCR backend"))
    }

    /// Recognises a rectangle of the screen.
    pub fn read_region(x: i32, y: i32, w: i32, h: i32) -> anyhow::Result<Vec<TextBox>> {
        let frame = crate::platform::capture(x, y, w, h)
            .ok_or_else(|| anyhow::anyhow!("could not capture the screen"))?;
        recognize(&frame)
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

    /// Grabs a rectangle of the screen as RGBA.
    ///
    /// GDI hands back bottom-up BGRA; a negative height in the header asks for
    /// top-down rows, and the channel swap happens on the way out.
    pub fn capture(x: i32, y: i32, w: i32, h: i32) -> Option<crate::vision::Frame> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, HGDIOBJ(bmp.0));

            let ok = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_ok();

            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
            let rows = if ok {
                GetDIBits(
                    mem,
                    bmp,
                    0,
                    h as u32,
                    Some(buf.as_mut_ptr() as *mut c_void),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            } else {
                0
            };

            SelectObject(mem, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);

            if rows == 0 {
                return None;
            }
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2); // BGRA -> RGBA
                px[3] = 255; // GDI leaves alpha at zero
            }
            Some(crate::vision::Frame { x, y, w: w as u32, h: h as u32, rgba: buf })
        }
    }

    pub fn virtual_screen_rect() -> (i32, i32, i32, i32) {
        unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            )
        }
    }

    /// Reads a bitmap off the clipboard, which is what Win+Shift+S leaves behind.
    ///
    /// Handles the 24/32-bit DIBs that the Snipping Tool and browsers produce,
    /// including the V4/V5 headers where the colour masks live inside the header.
    pub fn clipboard_image() -> Option<(u32, u32, Vec<u8>)> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }
            let result = (|| {
                const CF_DIB: u32 = 8;
                let handle = GetClipboardData(CF_DIB).ok()?;
                let hglobal = HGLOBAL(handle.0);
                let ptr = GlobalLock(hglobal) as *const u8;
                if ptr.is_null() {
                    return None;
                }
                let size = GlobalSize(hglobal);
                let bytes = std::slice::from_raw_parts(ptr, size);
                let out = parse_dib(bytes);
                let _ = GlobalUnlock(hglobal);
                out
            })();
            let _ = CloseClipboard();
            result
        }
    }

    fn rd_u32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }
    fn rd_i32(b: &[u8], at: usize) -> i32 {
        rd_u32(b, at) as i32
    }

    /// Turns a packed DIB into top-down RGBA.
    fn parse_dib(b: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        if b.len() < 40 {
            return None;
        }
        let hdr = rd_u32(b, 0) as usize;
        let w = rd_i32(b, 4);
        let raw_h = rd_i32(b, 8);
        let bpp = u16::from_le_bytes([b[14], b[15]]) as u32;
        let compression = rd_u32(b, 16);
        if w <= 0 || raw_h == 0 || !(bpp == 24 || bpp == 32) {
            return None;
        }
        let bottom_up = raw_h > 0;
        let h = raw_h.unsigned_abs();
        let w_u = w as u32;

        // BI_BITFIELDS adds three masks after a plain 40-byte header; V4/V5 headers
        // are bigger and already contain them.
        let mut offset = hdr;
        if compression == 3 && hdr == 40 {
            offset += 12;
        }
        let stride = (((w_u * bpp + 31) / 32) * 4) as usize;
        if b.len() < offset + stride * h as usize {
            return None;
        }

        let mut out = vec![0u8; (w_u * h * 4) as usize];
        let bytes_pp = (bpp / 8) as usize;
        for row in 0..h {
            let src_row = if bottom_up { h - 1 - row } else { row } as usize;
            let src = offset + src_row * stride;
            for x in 0..w_u as usize {
                let s = src + x * bytes_pp;
                let d = ((row as usize) * w_u as usize + x) * 4;
                out[d] = b[s + 2];
                out[d + 1] = b[s + 1];
                out[d + 2] = b[s];
                out[d + 3] = if bpp == 32 { 255 } else { 255 };
            }
        }
        Some((w_u, h, out))
    }

    /// Local wall-clock time: year, month, day, weekday (0 = Monday), hour, minute.
    ///
    /// Straight from Windows rather than a date crate: no dependency, and it is
    /// already the user's timezone and DST, which is what a schedule means.
    pub fn local_time() -> (u16, u16, u16, u8, u16, u16) {
        unsafe {
            let t = windows::Win32::System::SystemInformation::GetLocalTime();
            let monday_based = ((t.wDayOfWeek + 6) % 7) as u8;
            (t.wYear, t.wMonth, t.wDay, monday_based, t.wHour, t.wMinute)
        }
    }

    /// Title of whatever window currently has focus.
    pub fn foreground_title() -> Option<String> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return None;
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return None;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            if n <= 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..n as usize]))
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

    thread_local! {
        /// Needle and result for the EnumWindows callback below.
        static FIND_STATE: std::cell::RefCell<(String, Option<HWND>)> =
            const { std::cell::RefCell::new((String::new(), None)) };
    }

    unsafe extern "system" fn enum_find_proc(hwnd: HWND, _lp: LPARAM) -> BOOL {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return true.into();
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return true.into();
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            if n <= 0 {
                return true.into();
            }
            let title = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();
            let mut stop = false;
            FIND_STATE.with(|c| {
                let mut c = c.borrow_mut();
                if c.1.is_none() && !c.0.is_empty() && title.contains(c.0.as_str()) {
                    c.1 = Some(hwnd);
                    stop = true;
                }
            });
            (!stop).into()
        }
    }

    /// Finds a top-level window by title and returns its rectangle.
    ///
    /// Exact match first, then a case-insensitive substring search: window titles
    /// pick up suffixes all the time ("Roblox" becomes "Roblox - Level 7"), and an
    /// exact-only lookup made anchoring fail exactly when it was needed most.
    pub fn find_window_rect(title: &str) -> Option<(i32, i32, i32, i32)> {
        unsafe {
            let mut hwnd = HWND::default();
            let w = wide(title);
            if let Ok(h) = FindWindowW(None, PCWSTR(w.as_ptr())) {
                hwnd = h;
            }
            if hwnd.0.is_null() {
                let needle: String =
                    title.to_lowercase().chars().take(24).collect::<String>().trim().to_string();
                FIND_STATE.with(|c| *c.borrow_mut() = (needle, None));
                let _ = EnumWindows(Some(enum_find_proc), LPARAM(0));
                hwnd = FIND_STATE.with(|c| c.borrow().1.unwrap_or_default());
            }
            if hwnd.0.is_null() {
                return None;
            }
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() {
                return None;
            }
            Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
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
    pub fn local_time() -> (u16, u16, u16, u8, u16, u16) {
        (1970, 1, 1, 0, 0, 0)
    }
    pub fn foreground_title() -> Option<String> {
        None
    }
    pub fn capture(_: i32, _: i32, _: i32, _: i32) -> Option<crate::vision::Frame> {
        None
    }
    pub fn virtual_screen_rect() -> (i32, i32, i32, i32) {
        (0, 0, 1, 1)
    }
    pub fn clipboard_image() -> Option<(u32, u32, Vec<u8>)> {
        None
    }
    pub fn foreground_anchor() -> Option<WindowAnchor> {
        None
    }
    pub fn find_window_rect(_: &str) -> Option<(i32, i32, i32, i32)> {
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
    /// Raised by the hotkey: abandon whatever step is running and move on.
    pub skip_step: AtomicBool,
    /// Set while playback is parked waiting for the target window.
    pub waiting_window: AtomicBool,
    pub target_pause_unfocused: AtomicBool,
    pub target_title: Mutex<String>,
    // schedule
    pub schedule_enabled: AtomicBool,
    pub schedule_hm: AtomicU32,
    pub schedule_days: AtomicU32,
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
    pub anchor_scale: AtomicBool,
    pub human_mouse: AtomicBool,
    pub human_curve: AtomicU64,
    pub mouse_jitter_px: AtomicI32,
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
            skip_step: AtomicBool::new(false),
            waiting_window: AtomicBool::new(false),
            target_pause_unfocused: AtomicBool::new(false),
            target_title: Mutex::new(String::new()),
            schedule_enabled: AtomicBool::new(false),
            schedule_hm: AtomicU32::new(9 * 60),
            schedule_days: AtomicU32::new(0b0111_1111),
            pixel_triggered: AtomicBool::new(false),

            loop_play: AtomicBool::new(true),
            play_count: AtomicU64::new(0),
            play_count_limit: AtomicU64::new(1),
            absolute_mouse: AtomicBool::new(true),
            repeat_delay_ms: AtomicU64::new(0),
            jitter_pct: AtomicU64::new(0),
            use_window_anchor: AtomicBool::new(false),
            anchor_scale: AtomicBool::new(true),
            human_mouse: AtomicBool::new(false),
            human_curve: AtomicU64::new(35),
            mouse_jitter_px: AtomicI32::new(0),
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
    state.anchor_scale.store(cfg.anchor_scale, Ordering::Relaxed);
    state.human_mouse.store(cfg.human_mouse, Ordering::Relaxed);
    state.human_curve.store(cfg.human_curve, Ordering::Relaxed);
    state.mouse_jitter_px.store(cfg.mouse_jitter_px, Ordering::Relaxed);
    // While playing, the speed hotkeys own this value; the slider takes over again
    // once the run has finished.
    if !state.playing.load(Ordering::Relaxed) {
        *state.speed.lock() = cfg.speed;
    }
    state.target_pause_unfocused.store(cfg.target_pause_unfocused, Ordering::Relaxed);
    *state.target_title.lock() = cfg.target_title.clone();
    state.schedule_enabled.store(cfg.schedule_enabled, Ordering::Relaxed);
    state.schedule_hm.store(cfg.schedule_h * 60 + cfg.schedule_m, Ordering::Relaxed);
    state.schedule_days.store(cfg.schedule_days as u32, Ordering::Relaxed);

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

/// Multiplies the live playback speed, clamped to the engine's range.
fn nudge_speed(state: &AppState, factor: f64) {
    let mut sp = state.speed.lock();
    *sp = (*sp * factor).clamp(0.05, 10.0);
    info!("speed is now {:.2}x", *sp);
}

/// True when playback may proceed: either no target window is configured, or the
/// one in front matches it.
fn target_window_ready(state: &AppState) -> bool {
    if !state.target_pause_unfocused.load(Ordering::Relaxed) {
        return true;
    }
    let needle = state.target_title.lock().to_lowercase();
    if needle.trim().is_empty() {
        return true;
    }
    platform::foreground_title()
        .map(|t| t.to_lowercase().contains(needle.trim()))
        .unwrap_or(false)
}

/// Fires the macro at the configured time on the configured days.
///
/// Runs in its own thread rather than off the UI tick: a window minimised to the
/// tray stops painting, and a schedule that only works while you are looking at it
/// would be worse than none.
fn scheduler_thread(state: Arc<AppState>) {
    let mut last_fired: Option<(u16, u16, u16, u32)> = None;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        if !state.schedule_enabled.load(Ordering::Relaxed) {
            continue;
        }
        let (y, mo, d, dow, h, mi) = platform::local_time();
        let days = state.schedule_days.load(Ordering::Relaxed);
        if days & (1 << dow as u32) == 0 {
            continue;
        }
        let now_hm = h as u32 * 60 + mi as u32;
        if now_hm != state.schedule_hm.load(Ordering::Relaxed) {
            continue;
        }
        // The minute is checked several times; the date keeps it to one launch.
        let key = (y, mo, d, now_hm);
        if last_fired == Some(key) {
            continue;
        }
        last_fired = Some(key);
        if state.playing.load(Ordering::Relaxed) || state.recording.load(Ordering::Relaxed) {
            info!("schedule skipped: already busy");
            continue;
        }
        info!("schedule fired at {h:02}:{mi:02}");
        start_playback(&state);
    }
}

fn current_rec_time_us(state: &AppState) -> u64 {
    now_us().saturating_sub(state.rec_start_us.load(Ordering::Relaxed))
}

/// Result of the last image search, shared with the worker thread.
static SEARCHING: AtomicBool = AtomicBool::new(false);
static LAST_HIT: Mutex<Option<vision::Hit>> = Mutex::new(None);

/// Runs one search off the UI thread: a full-screen sweep takes long enough that
/// doing it inline would visibly freeze the window.
fn spawn_search(
    tpl: Arc<vision::Template>,
    region: Option<(i32, i32, i32, i32)>,
    multiscale: bool,
) {
    if SEARCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    let started = std::thread::Builder::new()
        .name("image-search".into())
        .spawn(move || {
            let (rx, ry, rw, rh) = region.unwrap_or_else(platform::virtual_screen_rect);
            let hit = platform::capture(rx, ry, rw, rh).and_then(|frame| {
                info!("searching {}x{} for '{}'", frame.w, frame.h, tpl.name);
                vision::find(&frame, &tpl, multiscale)
            });
            *LAST_HIT.lock() = hit;
            SEARCHING.store(false, Ordering::SeqCst);
        });
    if let Err(e) = started {
        warn!("could not start the search thread: {e}");
        SEARCHING.store(false, Ordering::SeqCst);
    }
}

/// Inserts a click at `(x, y)` into the macro, right after `at`.
fn editor_insert_click(data: &mut MacroData, at: usize, x: i32, y: i32) {
    let t = data.events.get(at).map(|e| e.t_us).unwrap_or(0);
    let gap = 30_000u64;
    let batch = [
        MacroEvent { t_us: t + 1, kind: InputEventKind::MouseMove { x, y, dx: 0, dy: 0 } },
        MacroEvent {
            t_us: t + 2,
            kind: InputEventKind::MouseButton { button: MouseButton::Left, down: true, x, y },
        },
        MacroEvent {
            t_us: t + gap,
            kind: InputEventKind::MouseButton { button: MouseButton::Left, down: false, x, y },
        },
    ];
    for e in data.events.iter_mut().skip(at + 1) {
        e.t_us = e.t_us.saturating_add(gap);
    }
    let pos = (at + 1).min(data.events.len());
    for (i, e) in batch.into_iter().enumerate() {
        data.events.insert(pos + i, e);
    }
    data.duration_us = data.duration_us.saturating_add(gap).max(data.last_t());
}

// ============================================================================
// Playback engine
// ============================================================================

/// Maps recorded screen coordinates onto the current position of the anchored window.
///
/// Translation alone was not enough: a window that was *resized* since the recording
/// moves every control proportionally, so the mapping scales as well.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoordMap {
    /// Where the window used to be.
    pub rx: i32,
    pub ry: i32,
    /// Where it is now.
    pub ox: i32,
    pub oy: i32,
    pub sx: f32,
    pub sy: f32,
}

impl CoordMap {
    const IDENTITY: Self = Self { rx: 0, ry: 0, ox: 0, oy: 0, sx: 1.0, sy: 1.0 };

    fn map(&self, x: i32, y: i32) -> (i32, i32) {
        let nx = self.ox as f32 + (x - self.rx) as f32 * self.sx;
        let ny = self.oy as f32 + (y - self.ry) as f32 * self.sy;
        (nx.round() as i32, ny.round() as i32)
    }

    /// Scales a relative movement, so drags keep their proportions too.
    fn map_delta(&self, dx: i32, dy: i32) -> (i32, i32) {
        ((dx as f32 * self.sx).round() as i32, (dy as f32 * self.sy).round() as i32)
    }

    fn build(anchor: &WindowAnchor, allow_scale: bool) -> Option<Self> {
        let (x, y, w, h) = platform::find_window_rect(&anchor.title)?;
        let (sx, sy) = if allow_scale && anchor.w > 0 && anchor.h > 0 {
            (
                (w as f32 / anchor.w as f32).clamp(0.2, 5.0),
                (h as f32 / anchor.h as f32).clamp(0.2, 5.0),
            )
        } else {
            (1.0, 1.0)
        };
        Some(Self { rx: anchor.x, ry: anchor.y, ox: x, oy: y, sx, sy })
    }
}

/// Interior points of a cubic Bezier from `from` to `to`.
///
/// Both control points sit on a random side of the straight line, which is what
/// stops every replayed movement from tracing the exact same arc.
fn bezier_path(
    from: (i32, i32),
    to: (i32, i32),
    curve: f32,
    rng: &mut Rng,
    steps: usize,
) -> Vec<(i32, i32)> {
    let (x0, y0) = (from.0 as f32, from.1 as f32);
    let (x3, y3) = (to.0 as f32, to.1 as f32);
    let (dx, dy) = (x3 - x0, y3 - y0);
    let dist = (dx * dx + dy * dy).sqrt().max(1.0);
    // Unit vector perpendicular to the path: the arc bows along this.
    let (px, py) = (-dy / dist, dx / dist);
    let amp = dist * curve.clamp(0.0, 1.0) * 0.35;
    let o1 = rng.unit() * amp;
    let o2 = rng.unit() * amp;
    let c1 = (x0 + dx * 0.30 + px * o1, y0 + dy * 0.30 + py * o1);
    let c2 = (x0 + dx * 0.70 + px * o2, y0 + dy * 0.70 + py * o2);

    (1..steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let u = 1.0 - t;
            let x = u * u * u * x0 + 3.0 * u * u * t * c1.0 + 3.0 * u * t * t * c2.0 + t * t * t * x3;
            let y = u * u * u * y0 + 3.0 * u * u * t * c1.1 + 3.0 * u * t * t * c2.1 + t * t * t * y3;
            (x.round() as i32, y.round() as i32)
        })
        .collect()
}

/// Moves the cursor, optionally the way a hand would.
struct MoveEngine {
    last: Option<(i32, i32)>,
    rng: Rng,
    human: bool,
    curve: f32,
    jitter: i32,
}

impl MoveEngine {
    fn new(state: &AppState) -> Self {
        Self {
            last: None,
            rng: Rng::new(),
            human: state.human_mouse.load(Ordering::Relaxed),
            curve: state.human_curve.load(Ordering::Relaxed) as f32 / 100.0,
            jitter: state.mouse_jitter_px.load(Ordering::Relaxed),
        }
    }

    /// A do-nothing engine for the paths that only release stuck buttons.
    fn inert() -> Self {
        Self { last: None, rng: Rng::new(), human: false, curve: 0.0, jitter: 0 }
    }

    fn goto(&mut self, x: i32, y: i32) {
        let (mut tx, mut ty) = (x, y);
        if self.jitter > 0 {
            tx += self.rng.signed(self.jitter as i64) as i32;
            ty += self.rng.signed(self.jitter as i64) as i32;
        }
        if self.human {
            if let Some(from) = self.last {
                let d = (((tx - from.0).pow(2) + (ty - from.1).pow(2)) as f64).sqrt();
                if d > 24.0 {
                    // Roughly one step per 8 px, capped so a long haul cannot eat
                    // more than ~60 ms of the schedule.
                    let steps = ((d / 8.0) as usize).clamp(6, 48);
                    for p in bezier_path(from, (tx, ty), self.curve, &mut self.rng, steps) {
                        unsafe { platform::send_absolute_mouse_move(p.0, p.1) };
                        spin_sleep::sleep(Duration::from_micros(1_200));
                    }
                }
            }
        }
        unsafe { platform::send_absolute_mouse_move(tx, ty) };
        self.last = Some((tx, ty));
    }
}

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
        // Releases never move the cursor, so an inert engine and the identity map.
        let mut mv = MoveEngine::inert();
        while let Some((vk, scan, extended)) = self.keys.pop() {
            unsafe {
                send_input_event(
                    &InputEventKind::Key { vk, scan, down: false, extended },
                    state,
                    &mut PressedInputs::default(),
                    CoordMap::IDENTITY,
                    &mut mv,
                );
            }
        }
        while let Some(button) = self.buttons.pop() {
            unsafe {
                send_input_event(
                    &InputEventKind::MouseButton { button, down: false, x: 0, y: 0 },
                    state,
                    &mut PressedInputs::default(),
                    CoordMap::IDENTITY,
                    &mut mv,
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

/// Everything one script run needs to carry around.
struct ScriptCtx<'a> {
    state: &'a Arc<AppState>,
    data: &'a MacroData,
    generation: u64,
    map: CoordMap,
    vars: std::collections::HashMap<String, f64>,
    templates: std::collections::HashMap<String, Option<Arc<vision::Template>>>,
    /// What OCR last read, for diagnosis.
    last_text: String,
}

/// Why a script run ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScriptEnd {
    Finished,
    Stopped,
    QuitApp,
}

/// Loads a template from `<data>/templates/<name>.png`, once per run.
fn load_template_file(name: &str) -> Option<Arc<vision::Template>> {
    let mut path = paths::sub_dir("templates").join(name);
    if path.extension().is_none() {
        path.set_extension("png");
    }
    match image::open(&path) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            Some(Arc::new(vision::Template {
                w,
                h,
                rgba: rgba.into_raw(),
                name: name.to_string(),
            }))
        }
        Err(e) => {
            warn!("template '{}' could not be loaded: {e}", path.display());
            None
        }
    }
}

impl ScriptCtx<'_> {
    fn stopping(&self) -> bool {
        self.state.stop_play.load(Ordering::Relaxed)
            || self.state.play_generation.load(Ordering::Relaxed) != self.generation
    }

    /// Blocks while paused, held on another desktop, or the target window is away.
    /// Returns false if playback should end.
    fn hold_if_needed(&self) -> bool {
        loop {
            if self.stopping() {
                return false;
            }
            let window_ok = target_window_ready(self.state);
            self.state.waiting_window.store(!window_ok, Ordering::Relaxed);
            if !self.state.paused.load(Ordering::Relaxed) && window_ok {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The speed as it stands right now, so the hotkeys take effect mid-step.
    fn live_speed(&self) -> f64 {
        (*self.state.speed.lock()).clamp(0.05, 10.0)
    }

    /// Sleeps in slices so Stop stays responsive, and honours Pause.
    fn nap(&self, ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            if self.stopping() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        !self.stopping()
    }

    fn template(&mut self, name: &str) -> Option<Arc<vision::Template>> {
        if !self.templates.contains_key(name) {
            let t = load_template_file(name);
            self.templates.insert(name.to_string(), t);
        }
        self.templates.get(name).cloned().flatten()
    }

    /// Searches the screen and records the result in `match_x` / `match_y` / `match_score`.
    fn find_image(&mut self, name: &str, threshold: f64) -> bool {
        let Some(tpl) = self.template(name) else {
            return false;
        };
        let (rx, ry, rw, rh) = platform::virtual_screen_rect();
        let Some(frame) = platform::capture(rx, ry, rw, rh) else {
            return false;
        };
        match vision::find(&frame, &tpl, false) {
            Some(hit) => {
                self.vars.insert("match_x".into(), hit.x as f64);
                self.vars.insert("match_y".into(), hit.y as f64);
                self.vars.insert("match_score".into(), hit.score as f64);
                hit.score as f64 >= threshold
            }
            None => {
                self.vars.insert("match_score".into(), 0.0);
                false
            }
        }
    }

    fn eval(&mut self, cond: &Condition) -> bool {
        match cond {
            Condition::Always => true,
            Condition::Var { name, cmp, value } => {
                cmp.test(self.vars.get(name).copied().unwrap_or(0.0), *value)
            }
            Condition::Image { template, threshold } => self.find_image(template, *threshold),
            Condition::Pixel { x, y, r, g, b, tol } => {
                match platform::screen_pixel(*x, *y) {
                    Some((pr, pg, pb)) => {
                        let t = *tol as i32;
                        (pr as i32 - *r as i32).abs() <= t
                            && (pg as i32 - *g as i32).abs() <= t
                            && (pb as i32 - *b as i32).abs() <= t
                    }
                    None => false,
                }
            }
            Condition::Window { title } => platform::find_window_rect(title).is_some(),
            Condition::Text { x, y, w, h, needle } => {
                match ocr::read_region(*x, *y, *w, *h) {
                    Ok(boxes) => {
                        let all = ocr::joined(&boxes);
                        self.last_text = all.clone();
                        ocr::text_matches(&all, needle)
                    }
                    Err(e) => {
                        warn!("ocr failed: {e}");
                        false
                    }
                }
            }
        }
    }
}

/// Replays events `from..=to` with their recorded timing.
fn play_event_range(
    ctx: &ScriptCtx<'_>,
    from: usize,
    to: usize,
    pressed: &mut PressedInputs,
    mover: &mut MoveEngine,
) -> bool {
    let events = &ctx.data.events;
    if events.is_empty() {
        return true;
    }
    let from = from.min(events.len() - 1);
    let to = to.min(events.len() - 1).max(from);
    info!("playing events {from}..={to} of {}", events.len());
    let start = Instant::now();
    // Scaled schedule accumulated gap by gap: comparing the running total against
    // real elapsed time keeps it drift-free, while letting the speed change mid-run.
    let mut due: u64 = 0;
    let mut prev_t = events[from].t_us;

    for ev in &events[from..=to] {
        if !ctx.hold_if_needed() {
            return false;
        }
        if ctx.state.skip_step.load(Ordering::Relaxed) {
            return true; // the skip hotkey abandons the rest of this range
        }
        let gap = ev.t_us.saturating_sub(prev_t);
        prev_t = ev.t_us;
        due = due.saturating_add((gap as f64 / ctx.live_speed()) as u64);
        loop {
            let now = start.elapsed().as_micros() as u64;
            if now >= due || ctx.stopping() {
                break;
            }
            let left = due - now;
            if left > SPIN_THRESHOLD_US {
                std::thread::sleep(Duration::from_micros(
                    left.saturating_sub(1_000).min(SLEEP_CHUNK_US).max(1),
                ));
            } else {
                spin_sleep::sleep(Duration::from_micros(left));
                break;
            }
        }
        #[cfg(windows)]
        unsafe {
            send_input_event(&ev.kind, ctx.state, pressed, ctx.map, mover);
        }
    }
    true
}

/// Starts a program, or opens a document, folder, shortcut or URL.
///
/// Only real executables go through `CreateProcess`. Everything else is handed to
/// the shell, because `CreateProcess` cannot open a `.lnk` shortcut, a URL or a
/// document - it would just fail, silently, which is the worst possible outcome
/// for a step that runs while nobody is watching.
fn run_program(path: &str, args: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    let lower = path.to_lowercase();
    let is_executable = lower.ends_with(".exe")
        || lower.ends_with(".bat")
        || lower.ends_with(".cmd")
        || lower.ends_with(".com");

    let mut cmd = if is_executable {
        let mut c = std::process::Command::new(path);
        if !args.trim().is_empty() {
            c.args(args.split_whitespace());
        }
        c
    } else {
        // `start` takes a window title first; the empty string keeps a quoted path
        // from being mistaken for one.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", path]);
        if !args.trim().is_empty() {
            c.args(args.split_whitespace());
        }
        c
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: otherwise `cmd /C start` flashes a console.
        cmd.creation_flags(0x0800_0000);
    }
    match cmd.spawn() {
        Ok(_) => info!("launched {path}"),
        Err(e) => warn!("could not launch {path}: {e}"),
    }
}

/// Walks the script once.
///
/// Control flow is a flat list with pre-resolved jumps rather than a tree: it maps
/// one-to-one onto the editor list, and a malformed script is rejected before a
/// single action is sent.
fn run_script(
    ctx: &mut ScriptCtx<'_>,
    pressed: &mut PressedInputs,
    mover: &mut MoveEngine,
) -> ScriptEnd {
    let steps = &ctx.data.script;
    let blocks = match resolve_blocks(steps) {
        Ok(b) => b,
        Err(e) => {
            warn!("script rejected: unbalanced blocks near {e}");
            return ScriptEnd::Finished;
        }
    };

    let mut pc = 0usize;
    let mut fuel: u64 = 0;
    const FUEL_LIMIT: u64 = 50_000_000;

    while pc < steps.len() {
        if ctx.stopping() {
            return ScriptEnd::Stopped;
        }
        if !ctx.hold_if_needed() {
            return ScriptEnd::Stopped;
        }
        fuel += 1;
        if fuel > FUEL_LIMIT {
            warn!("script exceeded its step budget - stopping");
            return ScriptEnd::Finished;
        }

        let step = &steps[pc];
        if !step.enabled {
            pc += 1;
            continue;
        }
        if ctx.state.skip_step.swap(false, Ordering::Relaxed) {
            info!("skipping step #{pc}");
            pc += 1;
            continue;
        }

        match &step.kind {
            StepKind::PlayEvents { from, to } => {
                if !play_event_range(ctx, *from, *to, pressed, mover) {
                    return ScriptEnd::Stopped;
                }
                pc += 1;
            }
            StepKind::Wait { ms } => {
                if !ctx.nap(*ms) {
                    return ScriptEnd::Stopped;
                }
                pc += 1;
            }
            StepKind::WaitFor { cond, appear, timeout_ms } => {
                let deadline = Instant::now() + Duration::from_millis(*timeout_ms);
                loop {
                    if ctx.stopping() {
                        return ScriptEnd::Stopped;
                    }
                    if ctx.state.skip_step.swap(false, Ordering::Relaxed) {
                        info!("wait skipped");
                        break;
                    }
                    let c = cond.clone();
                    if ctx.eval(&c) == *appear {
                        break;
                    }
                    if Instant::now() >= deadline {
                        info!("wait timed out");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(120));
                }
                pc += 1;
            }
            StepKind::ClickImage { template, threshold, button } => {
                let name = template.clone();
                if ctx.find_image(&name, *threshold) {
                    let x = ctx.vars.get("match_x").copied().unwrap_or(0.0) as i32;
                    let y = ctx.vars.get("match_y").copied().unwrap_or(0.0) as i32;
                    mover.goto(x, y);
                    #[cfg(windows)]
                    unsafe {
                        send_input_event(
                            &InputEventKind::MouseButton { button: *button, down: true, x, y },
                            ctx.state,
                            pressed,
                            CoordMap::IDENTITY,
                            mover,
                        );
                        spin_sleep::sleep(Duration::from_millis(30));
                        send_input_event(
                            &InputEventKind::MouseButton { button: *button, down: false, x, y },
                            ctx.state,
                            pressed,
                            CoordMap::IDENTITY,
                            mover,
                        );
                    }
                }
                pc += 1;
            }
            StepKind::Click { x, y, button } => {
                let (mx, my) = ctx.map.map(*x, *y);
                mover.goto(mx, my);
                #[cfg(windows)]
                unsafe {
                    send_input_event(
                        &InputEventKind::MouseButton {
                            button: *button,
                            down: true,
                            x: mx,
                            y: my,
                        },
                        ctx.state,
                        pressed,
                        CoordMap::IDENTITY,
                        mover,
                    );
                    spin_sleep::sleep(Duration::from_millis(30));
                    send_input_event(
                        &InputEventKind::MouseButton {
                            button: *button,
                            down: false,
                            x: mx,
                            y: my,
                        },
                        ctx.state,
                        pressed,
                        CoordMap::IDENTITY,
                        mover,
                    );
                }
                pc += 1;
            }
            StepKind::Key { vk, down } => {
                #[cfg(windows)]
                unsafe {
                    send_input_event(
                        &InputEventKind::Key { vk: *vk, scan: 0, down: *down, extended: false },
                        ctx.state,
                        pressed,
                        CoordMap::IDENTITY,
                        mover,
                    );
                }
                pc += 1;
            }
            StepKind::SetVar { name, op, value } => {
                let cur = ctx.vars.get(name).copied().unwrap_or(0.0);
                ctx.vars.insert(name.clone(), op.apply(cur, *value));
                pc += 1;
            }
            StepKind::If { cond } => {
                let c = cond.clone();
                if ctx.eval(&c) {
                    pc += 1;
                } else {
                    pc = blocks.else_of[pc]
                        .map(|e| e + 1)
                        .or_else(|| blocks.end_of[pc].map(|e| e + 1))
                        .unwrap_or(steps.len());
                }
            }
            // Reached only by falling out of the "then" branch.
            StepKind::Else => {
                pc = blocks.end_of[pc].map(|e| e + 1).unwrap_or(steps.len());
            }
            StepKind::EndIf => pc += 1,
            StepKind::While { cond } => {
                let c = cond.clone();
                let enter = ctx.eval(&c);
                if enter {
                    pc += 1;
                } else {
                    info!("while at #{pc}: condition false, leaving the loop");
                    pc = blocks.end_of[pc].map(|e| e + 1).unwrap_or(steps.len());
                }
            }
            StepKind::EndWhile => {
                pc = blocks.start_of[pc].unwrap_or(steps.len());
            }
            StepKind::Break => {
                // Jump past the innermost enclosing EndWhile.
                let mut depth = 0usize;
                let mut j = pc + 1;
                let mut target = steps.len();
                while j < steps.len() {
                    match steps[j].kind {
                        StepKind::While { .. } => depth += 1,
                        StepKind::EndWhile => {
                            if depth == 0 {
                                target = j + 1;
                                break;
                            }
                            depth -= 1;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                pc = target;
            }
            StepKind::Run { path, args } => {
                run_program(path, args);
                pc += 1;
            }
            StepKind::Exit => return ScriptEnd::QuitApp,
            StepKind::Log { text } => {
                info!("script: {text}");
                pc += 1;
            }
            StepKind::ReadNumber { x, y, w, h, var } => {
                match ocr::read_region(*x, *y, *w, *h) {
                    Ok(boxes) => {
                        let all = ocr::joined(&boxes);
                        // A clock reads as seconds, anything else as a plain number,
                        // which covers both "02:34" and "Gems: 1,250".
                        let value = ocr::parse_clock(&all)
                            .or_else(|| ocr::first_number(&all))
                            .unwrap_or(0.0);
                        info!("ocr read '{}' -> {var} = {value}", all.replace('\n', " / "));
                        ctx.last_text = all;
                        ctx.vars.insert(var.clone(), value);
                    }
                    Err(e) => warn!("ocr failed: {e}"),
                }
                pc += 1;
            }
        }
    }
    ScriptEnd::Finished
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

    // Re-anchor absolute coordinates if the target window moved *or resized*.
    let allow_scale = state.anchor_scale.load(Ordering::Relaxed);
    let map = match (state.use_window_anchor.load(Ordering::Relaxed), data.anchor.as_ref()) {
        (true, Some(anchor)) => match CoordMap::build(anchor, allow_scale) {
            Some(m) => {
                info!(
                    "anchored to '{}': origin {},{} -> {},{}  scale {:.3}x{:.3}",
                    anchor.title, m.rx, m.ry, m.ox, m.oy, m.sx, m.sy
                );
                m
            }
            None => {
                warn!("anchor window '{}' not found - playing unshifted", anchor.title);
                CoordMap::IDENTITY
            }
        },
        _ => CoordMap::IDENTITY,
    };
    let mut mover = MoveEngine::new(&state);
    let _ = &mover;

    let loop_play = state.loop_play.load(Ordering::Relaxed);
    let max_count = if loop_play {
        u64::MAX
    } else {
        state.play_count_limit.load(Ordering::Relaxed).max(1)
    };

    // ---- scripted playback ------------------------------------------------
    // A script replaces the flat replay entirely: each cycle walks the program,
    // which may itself replay slices of the recording.
    if data.has_script() {
        let mut pressed = PressedInputs::default();
        let mut ctx = ScriptCtx {
            state: &state,
            data: &data,
            generation,
            map,
            vars: data.vars.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            templates: Default::default(),
            last_text: String::new(),
        };
        let mut count: u64 = 0;
        let started = Instant::now();
        let mut quit_after = false;

        loop {
            if state.stop_play.load(Ordering::Relaxed)
                || state.play_generation.load(Ordering::Relaxed) != generation
            {
                break;
            }
            let limit = state.time_limit_us.load(Ordering::Relaxed);
            if state.time_limit_enabled.load(Ordering::Relaxed)
                && limit > 0
                && started.elapsed().as_micros() as u64 >= limit
            {
                break;
            }
            match run_script(&mut ctx, &mut pressed, &mut mover) {
                ScriptEnd::Stopped => break,
                ScriptEnd::QuitApp => {
                    quit_after = true;
                    break;
                }
                ScriptEnd::Finished => {}
            }
            count += 1;
            state.play_count.store(count, Ordering::Relaxed);
            if !loop_play && count >= max_count {
                break;
            }
            if repeat_delay_us > 0 && !ctx.nap(repeat_delay_us / 1000) {
                break;
            }
        }

        pressed.release_all(&state);
        platform::end_high_res_timer();
        if state.play_generation.load(Ordering::Relaxed) == generation {
            state.paused.store(false, Ordering::Relaxed);
            state.playing.store(false, Ordering::Relaxed);
        }
        info!("script finished after {count} cycle(s)");
        if quit_after {
            quit_application();
        }
        return;
    }

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
        let window_ok = target_window_ready(&state);
        state.waiting_window.store(!window_ok, Ordering::Relaxed);
        if state.paused.load(Ordering::Relaxed) || !on_desktop || !window_ok {
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

        if state.skip_step.swap(false, Ordering::Relaxed) {
            // Without steps to skip, the useful meaning is "get on with the next
            // repetition".
            index = data.events.len();
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
            send_input_event(&ev.kind, &state, &mut pressed, map, &mut mover);
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
    map: CoordMap,
    mv: &mut MoveEngine,
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
                    let (nx, ny) = map.map(*x, *y);
                    mv.goto(nx, ny);
                } else {
                    // Relative deltas are scaled too, so a drag keeps its shape when
                    // the anchored window is a different size than it was.
                    let (sdx, sdy) = map.map_delta(*dx, *dy);
                    let input = INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: sdx,
                                dy: sdy,
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
                    let (nx, ny) = map.map(*x, *y);
                    mv.goto(nx, ny);
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
    state.skip_step.store(false, Ordering::Relaxed);
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
    let ids = HK_IDS;
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
                    HK_ID_FASTER => nudge_speed(&state, 1.25),
                    HK_ID_SLOWER => nudge_speed(&state, 0.8),
                    HK_ID_SKIP => state.skip_step.store(true, Ordering::Relaxed),
                    _ => {}
                },
                WM_APP_REHOTKEY => register_hotkeys(),
                WM_APP_HK_OFF => {
                    for id in HK_IDS {
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
        for id in HK_IDS {
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
    mouse_rel, human_mouse, human_curve, mouse_jitter, anchor_scale, tip_human,
    sec_vision, img_paste, img_load, img_save, img_none, img_find, img_searching,
    img_threshold, img_multiscale, img_region, img_found, img_not_found,
    img_insert_click, tip_vision,
    sec_script, scr_none, scr_add, scr_from_sel, scr_enabled, scr_invalid, scr_view,
    scr_unreachable,
    k_play, k_wait, k_waitfor, k_clickimg, k_click, k_key, k_setvar,
    k_if, k_else, k_endif, k_while, k_endwhile, k_break, k_run, k_exit, k_log,
    c_always, c_var, c_image, c_pixel, c_window,
    f_timeout, f_var, f_value, f_template, f_appear, f_gone, f_path, f_args, f_text,
    sec_schedule, sch_enabled, sch_time, sch_days, sch_next,
    day_mon, day_tue, day_wed, day_thu, day_fri, day_sat, day_sun,
    hk_faster, hk_slower, hk_skip,
    sec_target, tgt_title, tgt_focus, status_waiting, status_speed, tip_speed,
    sec_ocr, ocr_read, ocr_empty, ocr_off, c_text, k_readnum, f_needle, f_region, tip_ocr,
    ocr_corner1, ocr_corner2, f_from_panel,
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
    abs_mouse: "Absolute mouse", anchor_use: "Follow the anchored window",
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
    mouse_rel: "Relative mouse", human_mouse: "Human-like movement",
    human_curve: "Curvature", mouse_jitter: "Aim spread (px)",
    anchor_scale: "Scale with the window size",
    tip_human: "Glides along a curved path instead of teleporting, with a random arc every time.",
    sec_vision: "🔎 Image search", img_paste: "📋 Paste", img_load: "📂 Load PNG…",
    img_save: "💾 Save PNG…", img_none: "no template",
    img_find: "🔍 Find on screen", img_searching: "searching…",
    img_threshold: "Confidence", img_multiscale: "Try other scales",
    img_region: "Search area only",
    img_found: "Found at {} — {}", img_not_found: "Not found (best {})",
    img_insert_click: "Insert click at match",
    tip_vision: "Snip with Win+Shift+S, then paste. Confidence 1.00 is an exact match; 0.85 tolerates antialiasing and mild colour shifts.",
    sec_script: "🧠 Script", scr_none: "No script — the macro just replays the recording",
    scr_add: "Add", scr_from_sel: "Step from selection", scr_enabled: "on",
    scr_invalid: "⚠ Unbalanced blocks: {}", scr_view: "Script",
    k_play: "Play events", k_wait: "Wait", k_waitfor: "Wait for", k_clickimg: "Click image",
    k_click: "Click at", k_key: "Key", k_setvar: "Set",
    k_if: "If", k_else: "Else", k_endif: "End if", k_while: "While", k_endwhile: "End while",
    k_break: "Break", k_run: "Run", k_exit: "Quit the app", k_log: "Note",
    c_always: "always", c_var: "variable", c_image: "image", c_pixel: "pixel", c_window: "window",
    f_timeout: "Timeout (ms)", f_var: "Variable", f_value: "Value", f_template: "Template",
    f_appear: "appears", f_gone: "disappears", f_path: "Path or URL", f_args: "Arguments",
    f_text: "Text",
    scr_unreachable: "⚠ Steps below can never run — they are past \"Quit the app\"",
    sec_schedule: "📅 Schedule", sch_enabled: "Start at a set time",
    sch_time: "Time", sch_days: "Days", sch_next: "Next run: {}",
    day_mon: "Mon", day_tue: "Tue", day_wed: "Wed", day_thu: "Thu", day_fri: "Fri",
    day_sat: "Sat", day_sun: "Sun",
    hk_faster: "Faster:", hk_slower: "Slower:", hk_skip: "Skip step:",
    sec_target: "🪟 Target window", tgt_title: "Title contains",
    tgt_focus: "Pause while it is not in front",
    status_waiting: "Waiting for the window…", status_speed: "speed {}×",
    tip_speed: "These work while the macro is running.",
    sec_ocr: "🔤 Text on screen", ocr_read: "🔤 Read now", ocr_empty: "nothing recognised",
    ocr_off: "This build has no OCR backend", c_text: "text", k_readnum: "Read number",
    f_needle: "Contains", f_region: "Region",
    tip_ocr: "Uses the Windows text recognition already on your PC. Add the language in Windows settings if your game is not in English.",
    ocr_corner1: "Point at the TOP-LEFT corner… {} s",
    ocr_corner2: "Now the BOTTOM-RIGHT corner… {} s",
    f_from_panel: "⤵ from the panel",
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
    abs_mouse: "Абсолютная мышь", anchor_use: "Следовать за окном привязки",
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
    mouse_rel: "Относительная мышь", human_mouse: "Человеческое движение",
    human_curve: "Кривизна", mouse_jitter: "Разброс прицела (px)",
    anchor_scale: "Масштабировать вместе с окном",
    tip_human: "Курсор едет по дуге, а не телепортируется, и дуга каждый раз новая.",
    sec_vision: "🔎 Поиск по картинке", img_paste: "📋 Вставить", img_load: "📂 Открыть PNG…",
    img_save: "💾 Сохранить PNG…", img_none: "шаблон не задан",
    img_find: "🔍 Найти на экране", img_searching: "ищу…",
    img_threshold: "Порог совпадения", img_multiscale: "Пробовать другие масштабы",
    img_region: "Искать только в области",
    img_found: "Найдено в {} — {}", img_not_found: "Не найдено (лучшее {})",
    img_insert_click: "Вставить клик по найденному",
    tip_vision: "Вырежьте область через Win+Shift+S и вставьте. Порог 1.00 — точное совпадение; 0.85 прощает сглаживание и небольшой сдвиг цвета.",
    sec_script: "🧠 Скрипт", scr_none: "Скрипта нет — макрос просто повторяет запись",
    scr_add: "Добавить", scr_from_sel: "Шаг из выделения", scr_enabled: "вкл",
    scr_invalid: "⚠ Незакрытые блоки: {}", scr_view: "Скрипт",
    k_play: "Воспроизвести события", k_wait: "Пауза", k_waitfor: "Ждать",
    k_clickimg: "Клик по картинке", k_click: "Клик в", k_key: "Клавиша", k_setvar: "Присвоить",
    k_if: "Если", k_else: "Иначе", k_endif: "Конец если", k_while: "Пока",
    k_endwhile: "Конец пока", k_break: "Прервать", k_run: "Запустить",
    k_exit: "Закрыть программу", k_log: "Заметка",
    c_always: "всегда", c_var: "переменная", c_image: "картинка", c_pixel: "пиксель",
    c_window: "окно",
    f_timeout: "Таймаут (мс)", f_var: "Переменная", f_value: "Значение", f_template: "Шаблон",
    f_appear: "появится", f_gone: "исчезнет", f_path: "Путь или ссылка", f_args: "Аргументы",
    f_text: "Текст",
    scr_unreachable: "⚠ Шаги ниже никогда не выполнятся — они после «Закрыть программу»",
    sec_schedule: "📅 Расписание", sch_enabled: "Запускать в заданное время",
    sch_time: "Время", sch_days: "Дни", sch_next: "Следующий запуск: {}",
    day_mon: "Пн", day_tue: "Вт", day_wed: "Ср", day_thu: "Чт", day_fri: "Пт",
    day_sat: "Сб", day_sun: "Вс",
    hk_faster: "Быстрее:", hk_slower: "Медленнее:", hk_skip: "Пропустить шаг:",
    sec_target: "🪟 Целевое окно", tgt_title: "Заголовок содержит",
    tgt_focus: "Пауза, пока оно не впереди",
    status_waiting: "Жду окно…", status_speed: "скорость {}×",
    tip_speed: "Работают во время воспроизведения.",
    sec_ocr: "🔤 Текст на экране", ocr_read: "🔤 Прочитать", ocr_empty: "ничего не распознано",
    ocr_off: "В этой сборке нет движка OCR", c_text: "текст", k_readnum: "Прочитать число",
    f_needle: "Содержит", f_region: "Область",
    tip_ocr: "Использует распознавание текста, уже встроенное в Windows. Если игра не на английском, добавьте язык в параметрах Windows.",
    ocr_corner1: "Наведите на ЛЕВЫЙ ВЕРХНИЙ угол… {} с",
    ocr_corner2: "Теперь на ПРАВЫЙ НИЖНИЙ угол… {} с",
    f_from_panel: "⤵ из панели",
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
    abs_mouse: "Абсолютна миша", anchor_use: "Слідувати за вікном прив'язки",
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
    mouse_rel: "Відносна миша", human_mouse: "Людський рух",
    human_curve: "Кривизна", mouse_jitter: "Розкид прицілу (px)",
    anchor_scale: "Масштабувати разом з вікном",
    tip_human: "Курсор їде по дузі, а не телепортується, і дуга щоразу нова.",
    sec_vision: "🔎 Пошук за картинкою", img_paste: "📋 Вставити", img_load: "📂 Відкрити PNG…",
    img_save: "💾 Зберегти PNG…", img_none: "шаблон не задано",
    img_find: "🔍 Знайти на екрані", img_searching: "шукаю…",
    img_threshold: "Поріг збігу", img_multiscale: "Пробувати інші масштаби",
    img_region: "Шукати лише в області",
    img_found: "Знайдено в {} — {}", img_not_found: "Не знайдено (найкраще {})",
    img_insert_click: "Вставити клік по знайденому",
    tip_vision: "Виріжте область через Win+Shift+S і вставте. Поріг 1.00 — точний збіг; 0.85 прощає згладжування та невеликий зсув кольору.",
    sec_script: "🧠 Скрипт", scr_none: "Скрипта немає — макрос просто повторює запис",
    scr_add: "Додати", scr_from_sel: "Крок із виділення", scr_enabled: "увімк",
    scr_invalid: "⚠ Незакриті блоки: {}", scr_view: "Скрипт",
    k_play: "Відтворити події", k_wait: "Пауза", k_waitfor: "Чекати",
    k_clickimg: "Клік по картинці", k_click: "Клік у", k_key: "Клавіша", k_setvar: "Присвоїти",
    k_if: "Якщо", k_else: "Інакше", k_endif: "Кінець якщо", k_while: "Поки",
    k_endwhile: "Кінець поки", k_break: "Перервати", k_run: "Запустити",
    k_exit: "Закрити програму", k_log: "Нотатка",
    c_always: "завжди", c_var: "змінна", c_image: "картинка", c_pixel: "піксель",
    c_window: "вікно",
    f_timeout: "Таймаут (мс)", f_var: "Змінна", f_value: "Значення", f_template: "Шаблон",
    f_appear: "з'явиться", f_gone: "зникне", f_path: "Шлях або посилання",
    f_args: "Аргументи", f_text: "Текст",
    scr_unreachable: "⚠ Кроки нижче ніколи не виконаються — вони після «Закрити програму»",
    sec_schedule: "📅 Розклад", sch_enabled: "Запускати в заданий час",
    sch_time: "Час", sch_days: "Дні", sch_next: "Наступний запуск: {}",
    day_mon: "Пн", day_tue: "Вт", day_wed: "Ср", day_thu: "Чт", day_fri: "Пт",
    day_sat: "Сб", day_sun: "Нд",
    hk_faster: "Швидше:", hk_slower: "Повільніше:", hk_skip: "Пропустити крок:",
    sec_target: "🪟 Цільове вікно", tgt_title: "Заголовок містить",
    tgt_focus: "Пауза, поки воно не попереду",
    status_waiting: "Чекаю вікно…", status_speed: "швидкість {}×",
    tip_speed: "Працюють під час відтворення.",
    sec_ocr: "🔤 Текст на екрані", ocr_read: "🔤 Прочитати", ocr_empty: "нічого не розпізнано",
    ocr_off: "У цій збірці немає рушія OCR", c_text: "текст", k_readnum: "Прочитати число",
    f_needle: "Містить", f_region: "Область",
    tip_ocr: "Використовує розпізнавання тексту, вбудоване у Windows. Якщо гра не англійською, додайте мову в параметрах Windows.",
    ocr_corner1: "Наведіть на ЛІВИЙ ВЕРХНІЙ кут… {} с",
    ocr_corner2: "Тепер на ПРАВИЙ НИЖНІЙ кут… {} с",
    f_from_panel: "⤵ з панелі",
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
    abs_mouse: "Mouse absoluto", anchor_use: "Seguir a janela ancorada",
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
    mouse_rel: "Mouse relativo", human_mouse: "Movimento humano",
    human_curve: "Curvatura", mouse_jitter: "Dispersão da mira (px)",
    anchor_scale: "Escalar com o tamanho da janela",
    tip_human: "Desliza por uma curva em vez de teleportar, com um arco aleatório a cada vez.",
    sec_vision: "🔎 Busca por imagem", img_paste: "📋 Colar", img_load: "📂 Abrir PNG…",
    img_save: "💾 Salvar PNG…", img_none: "sem modelo",
    img_find: "🔍 Procurar na tela", img_searching: "procurando…",
    img_threshold: "Confiança", img_multiscale: "Tentar outras escalas",
    img_region: "Só nesta área",
    img_found: "Encontrado em {} — {}", img_not_found: "Não encontrado (melhor {})",
    img_insert_click: "Inserir clique no resultado",
    tip_vision: "Recorte com Win+Shift+S e cole. Confiança 1.00 é exata; 0.85 tolera suavização e pequenas mudanças de cor.",
    sec_script: "🧠 Script", scr_none: "Sem script — o macro apenas repete a gravação",
    scr_add: "Adicionar", scr_from_sel: "Passo da seleção", scr_enabled: "ativo",
    scr_invalid: "⚠ Blocos não fechados: {}", scr_view: "Script",
    k_play: "Reproduzir eventos", k_wait: "Esperar", k_waitfor: "Aguardar",
    k_clickimg: "Clicar na imagem", k_click: "Clicar em", k_key: "Tecla", k_setvar: "Definir",
    k_if: "Se", k_else: "Senão", k_endif: "Fim se", k_while: "Enquanto",
    k_endwhile: "Fim enquanto", k_break: "Interromper", k_run: "Executar",
    k_exit: "Fechar o app", k_log: "Nota",
    c_always: "sempre", c_var: "variável", c_image: "imagem", c_pixel: "pixel",
    c_window: "janela",
    f_timeout: "Tempo limite (ms)", f_var: "Variável", f_value: "Valor", f_template: "Modelo",
    f_appear: "aparecer", f_gone: "sumir", f_path: "Caminho ou URL", f_args: "Argumentos",
    f_text: "Texto",
    scr_unreachable: "⚠ Os passos abaixo nunca serão executados — estão após \"Fechar o app\"",
    sec_schedule: "📅 Agenda", sch_enabled: "Iniciar num horário",
    sch_time: "Hora", sch_days: "Dias", sch_next: "Próxima execução: {}",
    day_mon: "Seg", day_tue: "Ter", day_wed: "Qua", day_thu: "Qui", day_fri: "Sex",
    day_sat: "Sáb", day_sun: "Dom",
    hk_faster: "Mais rápido:", hk_slower: "Mais lento:", hk_skip: "Pular passo:",
    sec_target: "🪟 Janela alvo", tgt_title: "Título contém",
    tgt_focus: "Pausar enquanto não estiver à frente",
    status_waiting: "Aguardando a janela…", status_speed: "velocidade {}×",
    tip_speed: "Funcionam enquanto o macro roda.",
    sec_ocr: "🔤 Texto na tela", ocr_read: "🔤 Ler agora", ocr_empty: "nada reconhecido",
    ocr_off: "Esta build não tem backend de OCR", c_text: "texto", k_readnum: "Ler número",
    f_needle: "Contém", f_region: "Região",
    tip_ocr: "Usa o reconhecimento de texto já presente no Windows. Adicione o idioma nas configurações se o jogo não for em inglês.",
    ocr_corner1: "Aponte para o canto SUPERIOR ESQUERDO… {} s",
    ocr_corner2: "Agora o canto INFERIOR DIREITO… {} s",
    f_from_panel: "⤵ do painel",
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
    abs_mouse: "Ratón absoluto", anchor_use: "Seguir la ventana anclada",
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
    mouse_rel: "Ratón relativo", human_mouse: "Movimiento humano",
    human_curve: "Curvatura", mouse_jitter: "Dispersión de puntería (px)",
    anchor_scale: "Escalar con el tamaño de la ventana",
    tip_human: "Se desliza por una curva en vez de teletransportarse, con un arco aleatorio cada vez.",
    sec_vision: "🔎 Búsqueda por imagen", img_paste: "📋 Pegar", img_load: "📂 Abrir PNG…",
    img_save: "💾 Guardar PNG…", img_none: "sin plantilla",
    img_find: "🔍 Buscar en pantalla", img_searching: "buscando…",
    img_threshold: "Confianza", img_multiscale: "Probar otras escalas",
    img_region: "Sólo en esta zona",
    img_found: "Encontrado en {} — {}", img_not_found: "No encontrado (mejor {})",
    img_insert_click: "Insertar clic en el resultado",
    tip_vision: "Recorta con Win+Shift+S y pega. Confianza 1.00 es exacta; 0.85 tolera suavizado y ligeros cambios de color.",
    sec_script: "🧠 Script", scr_none: "Sin script — el macro sólo repite la grabación",
    scr_add: "Añadir", scr_from_sel: "Paso desde la selección", scr_enabled: "activo",
    scr_invalid: "⚠ Bloques sin cerrar: {}", scr_view: "Script",
    k_play: "Reproducir eventos", k_wait: "Esperar", k_waitfor: "Aguardar",
    k_clickimg: "Clic en la imagen", k_click: "Clic en", k_key: "Tecla", k_setvar: "Asignar",
    k_if: "Si", k_else: "Si no", k_endif: "Fin si", k_while: "Mientras",
    k_endwhile: "Fin mientras", k_break: "Romper", k_run: "Ejecutar",
    k_exit: "Cerrar la app", k_log: "Nota",
    c_always: "siempre", c_var: "variable", c_image: "imagen", c_pixel: "píxel",
    c_window: "ventana",
    f_timeout: "Tiempo límite (ms)", f_var: "Variable", f_value: "Valor",
    f_template: "Plantilla", f_appear: "aparezca", f_gone: "desaparezca",
    f_path: "Ruta o URL", f_args: "Argumentos", f_text: "Texto",
    scr_unreachable: "⚠ Los pasos de abajo nunca se ejecutarán — están tras \"Cerrar la app\"",
    sec_schedule: "📅 Programación", sch_enabled: "Iniciar a una hora",
    sch_time: "Hora", sch_days: "Días", sch_next: "Próxima ejecución: {}",
    day_mon: "Lun", day_tue: "Mar", day_wed: "Mié", day_thu: "Jue", day_fri: "Vie",
    day_sat: "Sáb", day_sun: "Dom",
    hk_faster: "Más rápido:", hk_slower: "Más lento:", hk_skip: "Saltar paso:",
    sec_target: "🪟 Ventana objetivo", tgt_title: "El título contiene",
    tgt_focus: "Pausar mientras no esté delante",
    status_waiting: "Esperando la ventana…", status_speed: "velocidad {}×",
    tip_speed: "Funcionan mientras el macro se ejecuta.",
    sec_ocr: "🔤 Texto en pantalla", ocr_read: "🔤 Leer ahora", ocr_empty: "nada reconocido",
    ocr_off: "Esta build no tiene motor de OCR", c_text: "texto", k_readnum: "Leer número",
    f_needle: "Contiene", f_region: "Región",
    tip_ocr: "Usa el reconocimiento de texto que ya trae Windows. Añade el idioma en la configuración si el juego no está en inglés.",
    ocr_corner1: "Apunta a la esquina SUPERIOR IZQUIERDA… {} s",
    ocr_corner2: "Ahora la esquina INFERIOR DERECHA… {} s",
    f_from_panel: "⤵ del panel",
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
    abs_mouse: "绝对鼠标", anchor_use: "跟随锚定窗口",
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
    mouse_rel: "相对鼠标", human_mouse: "拟人移动",
    human_curve: "弯曲度", mouse_jitter: "瞄准抖动 (px)",
    anchor_scale: "随窗口大小缩放",
    tip_human: "沿曲线滑动而不是瞬移，每次弧线都不同。",
    sec_vision: "🔎 图像搜索", img_paste: "📋 粘贴", img_load: "📂 打开 PNG…",
    img_save: "💾 保存 PNG…", img_none: "未设置模板",
    img_find: "🔍 在屏幕上查找", img_searching: "查找中…",
    img_threshold: "匹配阈值", img_multiscale: "尝试其他缩放",
    img_region: "仅在区域内查找",
    img_found: "找到于 {} — {}", img_not_found: "未找到 (最佳 {})",
    img_insert_click: "在匹配处插入点击",
    tip_vision: "用 Win+Shift+S 截图后粘贴。阈值 1.00 为精确匹配；0.85 可容忍抗锯齿和轻微色差。",
    sec_script: "🧠 脚本", scr_none: "没有脚本 — 宏只会重放录制",
    scr_add: "添加", scr_from_sel: "从选区创建步骤", scr_enabled: "启用",
    scr_invalid: "⚠ 未闭合的块: {}", scr_view: "脚本",
    k_play: "回放事件", k_wait: "等待", k_waitfor: "等待条件",
    k_clickimg: "点击图像", k_click: "点击于", k_key: "按键", k_setvar: "赋值",
    k_if: "如果", k_else: "否则", k_endif: "结束如果", k_while: "当",
    k_endwhile: "结束当", k_break: "跳出", k_run: "运行",
    k_exit: "关闭程序", k_log: "备注",
    c_always: "总是", c_var: "变量", c_image: "图像", c_pixel: "像素", c_window: "窗口",
    f_timeout: "超时 (毫秒)", f_var: "变量", f_value: "值", f_template: "模板",
    f_appear: "出现", f_gone: "消失", f_path: "路径或链接", f_args: "参数", f_text: "文本",
    scr_unreachable: "⚠ 下面的步骤永远不会执行 — 它们在「关闭程序」之后",
    sec_schedule: "📅 计划", sch_enabled: "按设定时间启动",
    sch_time: "时间", sch_days: "星期", sch_next: "下次运行: {}",
    day_mon: "一", day_tue: "二", day_wed: "三", day_thu: "四", day_fri: "五",
    day_sat: "六", day_sun: "日",
    hk_faster: "加速:", hk_slower: "减速:", hk_skip: "跳过步骤:",
    sec_target: "🪟 目标窗口", tgt_title: "标题包含",
    tgt_focus: "不在前台时暂停",
    status_waiting: "等待窗口…", status_speed: "速度 {}×",
    tip_speed: "在宏运行时生效。",
    sec_ocr: "🔤 屏幕文字", ocr_read: "🔤 立即识别", ocr_empty: "未识别到内容",
    ocr_off: "此版本不含 OCR 引擎", c_text: "文本", k_readnum: "读取数字",
    f_needle: "包含", f_region: "区域",
    tip_ocr: "使用 Windows 自带的文字识别。若游戏不是英文，请在 Windows 设置中添加对应语言。",
    ocr_corner1: "请指向左上角… {} 秒",
    ocr_corner2: "现在指向右下角… {} 秒",
    f_from_panel: "⤵ 取自面板",
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
    /// The picture we are looking for, shared with the search thread.
    template: Option<Arc<vision::Template>>,
    ocr_text: String,
    ocr_rect: (i32, i32, i32, i32),
    /// Two-corner region picker: deadline, and whether we are on the second corner.
    ocr_pick: Option<(Instant, bool)>,
    ocr_corner: (i32, i32),
    /// 0 = story, 1 = raw events, 2 = script.
    ed_view: usize,
    scr_sel: usize,
    scr_add_kind: usize,
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
            ed_steps: Vec::new(),
            ed_steps_key: (usize::MAX, 0, 0),
            ed_cursor: 0,
            ed_undo_key: None,
            ed_pick_deadline: None,
            bulk_from_btn: 0,
            bulk_to_btn: 1,
            bulk_dx: 0,
            bulk_dy: 0,
            template: None,
            ocr_text: String::new(),
            ocr_rect: (0, 0, 600, 200),
            ocr_pick: None,
            ocr_corner: (0, 0),
            ed_view: 0,
            scr_sel: 0,
            scr_add_kind: 0,
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

    fn set_template(&mut self, w: u32, h: u32, rgba: Vec<u8>, name: String) {
        self.template = Some(Arc::new(vision::Template { w, h, rgba, name }));
        *LAST_HIT.lock() = None;
    }

    fn load_template_png(&mut self, path: &Path) {
        let s = self.strs();
        match image::open(path) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());
                self.set_template(w, h, rgba.into_raw(), file_label(path));
                self.status_msg = s.loaded.replace("{}", &file_label(path));
            }
            Err(e) => self.status_msg = s.load_err.replace("{}", &e.to_string()),
        }
    }

    fn search_region(&self) -> Option<(i32, i32, i32, i32)> {
        if self.config.img_region_enabled {
            Some((
                self.config.img_rx,
                self.config.img_ry,
                self.config.img_rw,
                self.config.img_rh,
            ))
        } else {
            None
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

/// Mouse button picker shared by the script steps.
fn button_picker(ui: &mut egui::Ui, s: &Strings, salt: &str, button: &mut MouseButton) -> bool {
    let names = [s.btn_l, s.btn_r, s.btn_m, "X1", "X2"];
    let all = [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::X1,
        MouseButton::X2,
    ];
    let mut changed = false;
    egui::ComboBox::from_id_salt(salt)
        .selected_text(names[all.iter().position(|b| b == button).unwrap_or(0)])
        .width(100.0)
        .show_ui(ui, |ui| {
            for (b, n) in all.iter().zip(names) {
                if ui.selectable_label(button == b, n).clicked() && button != b {
                    *button = *b;
                    changed = true;
                }
            }
        });
    changed
}

/// Editor for one condition, used by If, While and Wait for.
fn condition_ui(
    ui: &mut egui::Ui,
    s: &Strings,
    salt: &str,
    cond: &mut Condition,
    from_panel: Option<(i32, i32, i32, i32)>,
) -> bool {
    let names = [s.c_always, s.c_var, s.c_image, s.c_pixel, s.c_window, s.c_text];
    let mut changed = false;

    ui.horizontal_wrapped(|ui| {
        egui::ComboBox::from_id_salt(format!("{salt}_kind"))
            .selected_text(names[cond.kind_index()])
            .width(120.0)
            .show_ui(ui, |ui| {
                for (i, n) in names.iter().enumerate() {
                    if ui.selectable_label(cond.kind_index() == i, *n).clicked()
                        && cond.kind_index() != i
                    {
                        *cond = Condition::from_index(i);
                        changed = true;
                    }
                }
            });
    });

    match cond {
        Condition::Always => {}
        Condition::Var { name, cmp, value } => {
            ui.horizontal_wrapped(|ui| {
                changed |=
                    ui.add(egui::TextEdit::singleline(name).desired_width(90.0)).changed();
                egui::ComboBox::from_id_salt(format!("{salt}_cmp"))
                    .selected_text(cmp.symbol())
                    .width(60.0)
                    .show_ui(ui, |ui| {
                        for c in Cmp::ALL {
                            if ui.selectable_label(*cmp == c, c.symbol()).clicked() {
                                *cmp = c;
                                changed = true;
                            }
                        }
                    });
                changed |= ui.add(egui::DragValue::new(value).speed(0.5)).changed();
            });
        }
        Condition::Image { template, threshold } => {
            ui.horizontal_wrapped(|ui| {
                ui.label(s.f_template);
                changed |= ui
                    .add(egui::TextEdit::singleline(template).desired_width(150.0))
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(threshold).range(0.3..=1.0).speed(0.01))
                    .changed();
            });
        }
        Condition::Pixel { x, y, r, g, b, tol } => {
            ui.horizontal_wrapped(|ui| {
                ui.label("X");
                changed |= ui.add(egui::DragValue::new(x).range(-32000..=32000)).changed();
                ui.label("Y");
                changed |= ui.add(egui::DragValue::new(y).range(-32000..=32000)).changed();
                let mut col = [*r, *g, *b];
                if ui.color_edit_button_srgb(&mut col).changed() {
                    *r = col[0];
                    *g = col[1];
                    *b = col[2];
                    changed = true;
                }
                ui.label(s.pixel_tol);
                changed |= ui.add(egui::DragValue::new(tol).range(0..=255)).changed();
            });
        }
        Condition::Window { title } => {
            ui.horizontal_wrapped(|ui| {
                changed |= ui
                    .add(egui::TextEdit::singleline(title).desired_width(200.0))
                    .changed();
            });
        }
        Condition::Text { x, y, w, h, needle } => {
            ui.horizontal_wrapped(|ui| {
                ui.label(s.f_needle);
                changed |= ui
                    .add(egui::TextEdit::singleline(needle).desired_width(180.0))
                    .changed();
            });
            changed |= region_ui(ui, s, x, y, w, h, from_panel);
        }
    }
    changed
}

/// X / Y / W / H row shared by every step that works on a screen rectangle.
fn region_ui(
    ui: &mut egui::Ui,
    s: &Strings,
    x: &mut i32,
    y: &mut i32,
    w: &mut i32,
    h: &mut i32,
    // The rectangle last picked in the "Text on screen" panel, if any.
    from_panel: Option<(i32, i32, i32, i32)>,
) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(s.f_region);
        ui.label("X");
        changed |= ui.add(egui::DragValue::new(x).range(-32000..=32000)).changed();
        ui.label("Y");
        changed |= ui.add(egui::DragValue::new(y).range(-32000..=32000)).changed();
        ui.label("W");
        changed |= ui.add(egui::DragValue::new(w).range(8..=32000)).changed();
        ui.label("H");
        changed |= ui.add(egui::DragValue::new(h).range(8..=32000)).changed();
        // Retyping four numbers from the test panel is exactly the kind of chore
        // that produces off-by-a-hundred bugs.
        if let Some(r) = from_panel {
            if ui.small_button(s.f_from_panel).clicked() {
                *x = r.0;
                *y = r.1;
                *w = r.2;
                *h = r.3;
                changed = true;
            }
        }
    });
    changed
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
    /// Script view: the program list plus an inspector for the selected step.
    fn script_ui(&mut self, ui: &mut egui::Ui, list_h: f32) {
        let s = self.strs();
        let busy = self.busy();
        let (steps, total_events) = {
            let d = self.state.macro_data.lock();
            (d.script.clone(), d.events.len())
        };

        if let Err(e) = resolve_blocks(&steps) {
            ui.colored_label(
                egui::Color32::from_rgb(255, 170, 60),
                s.scr_invalid.replace("{}", &e),
            );
        }

        let dead_from = first_unreachable(&steps);
        if dead_from.is_some() {
            ui.colored_label(egui::Color32::from_rgb(255, 170, 60), s.scr_unreachable);
        }

        if steps.is_empty() {
            ui.label(s.scr_none);
        } else {
            let depths = script_depths(&steps);
            let mut pick = None;
            egui::ScrollArea::vertical()
                .id_salt("script")
                .max_height(list_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (i, st) in steps.iter().enumerate() {
                        let pad = "    ".repeat(depths[i]);
                        let line = format!("{i:>3} {pad}{}", describe_step(st, s, total_events));
                        let mut rt = egui::RichText::new(line).monospace();
                        if !st.enabled {
                            rt = rt.weak().strikethrough();
                        } else if dead_from.map(|d| i >= d).unwrap_or(false) {
                            rt = rt.color(egui::Color32::from_rgb(255, 170, 60));
                        }
                        if ui.selectable_label(i == self.scr_sel, rt).clicked() {
                            pick = Some(i);
                        }
                    }
                });
            if let Some(i) = pick {
                self.scr_sel = i;
            }
        }

        // ---- add / move / delete -------------------------------------------
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            let names: Vec<&str> = (0..StepKind::COUNT)
                .map(|i| StepKind::from_index(i).name(s))
                .collect();
            egui::ComboBox::from_id_salt("scr_kind")
                .selected_text(names[self.scr_add_kind.min(StepKind::COUNT - 1)])
                .width(170.0)
                .show_ui(ui, |ui| {
                    for (i, n) in names.iter().enumerate() {
                        ui.selectable_value(&mut self.scr_add_kind, i, *n);
                    }
                });
            if ui.add_enabled(!busy, egui::Button::new(s.scr_add)).clicked() {
                let mut kind = StepKind::from_index(self.scr_add_kind);
                // A fresh "play events" step covers the whole recording: 0…0 would
                // be a single event, which is never what anyone means.
                if let StepKind::PlayEvents { from, to } = &mut kind {
                    if *from == 0 && *to == 0 {
                        *to = self.state.macro_data.lock().events.len().saturating_sub(1);
                    }
                }
                let at = self.scr_sel;
                self.edit(|d| {
                    let pos = (at + 1).min(d.script.len());
                    d.script.insert(pos, ScriptStep::new(kind));
                });
                self.scr_sel = (self.scr_sel + 1).min(steps.len());
            }
            // Turning a recorded range into a step is how most scripts start.
            if ui.add_enabled(!busy, egui::Button::new(s.scr_from_sel)).clicked() {
                let (from, to) = (self.ed_from, self.ed_to);
                let at = self.scr_sel;
                self.edit(|d| {
                    let pos = (at + 1).min(d.script.len());
                    d.script.insert(pos, ScriptStep::new(StepKind::PlayEvents { from, to }));
                });
            }
        });

        if steps.is_empty() {
            return;
        }
        let sel = self.scr_sel.min(steps.len() - 1);
        self.scr_sel = sel;

        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("\u{25b2}")).clicked() && sel > 0 {
                self.edit(|d| d.script.swap(sel, sel - 1));
                self.scr_sel = sel - 1;
            }
            if ui.add_enabled(!busy, egui::Button::new("\u{25bc}")).clicked()
                && sel + 1 < steps.len()
            {
                self.edit(|d| d.script.swap(sel, sel + 1));
                self.scr_sel = sel + 1;
            }
            if ui.add_enabled(!busy, egui::Button::new(s.insp_del_one)).clicked() {
                self.edit(|d| {
                    if sel < d.script.len() {
                        d.script.remove(sel);
                    }
                });
                self.scr_sel = sel.saturating_sub(1);
            }
            let mut on = steps[sel].enabled;
            if ui.checkbox(&mut on, s.scr_enabled).changed() {
                self.edit(|d| {
                    if let Some(st) = d.script.get_mut(sel) {
                        st.enabled = on;
                    }
                });
            }
        });

        // ---- inspector for the selected step --------------------------------
        ui.separator();
        let mut kind = steps[sel].kind.clone();
        let mut changed = false;

        // Changing the kind starts from that kind's defaults.
        ui.horizontal_wrapped(|ui| {
            let names: Vec<&str> = (0..StepKind::COUNT)
                .map(|i| StepKind::from_index(i).name(s))
                .collect();
            egui::ComboBox::from_id_salt("scr_selkind")
                .selected_text(names[kind.index()])
                .width(170.0)
                .show_ui(ui, |ui| {
                    for (i, n) in names.iter().enumerate() {
                        if ui.selectable_label(kind.index() == i, *n).clicked()
                            && kind.index() != i
                        {
                            kind = StepKind::from_index(i);
                            changed = true;
                        }
                    }
                });
        });

        match &mut kind {
            StepKind::PlayEvents { from, to } => {
                let last = self.state.macro_data.lock().events.len().saturating_sub(1);
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.ed_from);
                    changed |= ui.add(egui::DragValue::new(from).range(0..=last)).changed();
                    ui.label(s.ed_to);
                    changed |= ui.add(egui::DragValue::new(to).range(0..=last)).changed();
                });
            }
            StepKind::Wait { ms } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.ed_insert);
                    changed |=
                        ui.add(egui::DragValue::new(ms).range(0..=3_600_000).speed(10.0)).changed();
                });
            }
            StepKind::WaitFor { cond, appear, timeout_ms } => {
                changed |= condition_ui(ui, s, "wf", cond, Some(self.ocr_rect));
                ui.horizontal_wrapped(|ui| {
                    changed |= ui.selectable_value(appear, true, s.f_appear).clicked();
                    changed |= ui.selectable_value(appear, false, s.f_gone).clicked();
                    ui.label(s.f_timeout);
                    changed |= ui
                        .add(egui::DragValue::new(timeout_ms).range(0..=3_600_000).speed(50.0))
                        .changed();
                });
            }
            StepKind::ClickImage { template, threshold, button } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_template);
                    changed |= ui
                        .add(egui::TextEdit::singleline(template).desired_width(140.0))
                        .changed();
                    changed |= ui
                        .add(egui::DragValue::new(threshold).range(0.3..=1.0).speed(0.01))
                        .changed();
                    changed |= button_picker(ui, s, "ci", button);
                });
            }
            StepKind::Click { x, y, button } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label("X");
                    changed |=
                        ui.add(egui::DragValue::new(x).range(-32000..=32000)).changed();
                    ui.label("Y");
                    changed |=
                        ui.add(egui::DragValue::new(y).range(-32000..=32000)).changed();
                    changed |= button_picker(ui, s, "cl", button);
                });
            }
            StepKind::Key { vk, down } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.insp_key);
                    egui::ComboBox::from_id_salt("scr_key")
                        .selected_text(vk_name(*vk as u32))
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for (name, code) in EDIT_KEYS {
                                if ui.selectable_label(*vk == code, name).clicked()
                                    && *vk != code
                                {
                                    *vk = code;
                                    changed = true;
                                }
                            }
                        });
                    changed |= ui.selectable_value(down, true, s.st_down).clicked();
                    changed |= ui.selectable_value(down, false, s.st_up).clicked();
                });
            }
            StepKind::SetVar { name, op, value } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_var);
                    changed |=
                        ui.add(egui::TextEdit::singleline(name).desired_width(90.0)).changed();
                    egui::ComboBox::from_id_salt("scr_op")
                        .selected_text(op.symbol())
                        .width(60.0)
                        .show_ui(ui, |ui| {
                            for o in VarOp::ALL {
                                if ui.selectable_label(*op == o, o.symbol()).clicked() {
                                    *op = o;
                                    changed = true;
                                }
                            }
                        });
                    ui.label(s.f_value);
                    changed |= ui.add(egui::DragValue::new(value).speed(0.5)).changed();
                });
            }
            StepKind::If { cond } | StepKind::While { cond } => {
                changed |= condition_ui(ui, s, "cnd", cond, Some(self.ocr_rect));
            }
            StepKind::Run { path, args } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_path);
                    changed |= ui
                        .add(egui::TextEdit::singleline(path).desired_width(220.0))
                        .changed();
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_args);
                    changed |= ui
                        .add(egui::TextEdit::singleline(args).desired_width(220.0))
                        .changed();
                });
            }
            StepKind::ReadNumber { x, y, w, h, var } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_var);
                    changed |= ui
                        .add(egui::TextEdit::singleline(var).desired_width(100.0))
                        .changed();
                });
                changed |= region_ui(ui, s, x, y, w, h, Some(self.ocr_rect));
            }
            StepKind::Log { text } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_text);
                    changed |= ui
                        .add(egui::TextEdit::singleline(text).desired_width(240.0))
                        .changed();
                });
            }
            _ => {}
        }

        if changed && !busy {
            self.edit_event(sel, |d| {
                if let Some(st) = d.script.get_mut(sel) {
                    st.kind = kind.clone();
                }
            });
        }
    }

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
            ui.selectable_value(&mut self.ed_view, 0, s.ed_human);
            ui.selectable_value(&mut self.ed_view, 1, s.ed_raw);
            ui.selectable_value(&mut self.ed_view, 2, s.scr_view);
        });
        ui.separator();

        // The list gets whatever is left after the inspector, which is pinned to the
        // bottom - letting the scroll area take the whole window used to push every
        // control off-screen.
        let list_h = (ui.available_height() - 330.0).max(120.0);

        if self.ed_view == 2 {
            self.script_ui(ui, list_h);
            return;
        }

        if self.ed_view == 0 {
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

            // A found match becomes a real click in the macro, right after the
            // selected action - the whole point of searching from the editor.
            let hit = *LAST_HIT.lock();
            if let Some(h) = hit {
                if h.score as f64 >= self.config.img_threshold {
                    ui.horizontal_wrapped(|ui| {
                        let label = format!("{}  ({}, {})", s.img_insert_click, h.x, h.y);
                        if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                            self.edit(|d| editor_insert_click(d, idx, h.x, h.y));
                        }
                    });
                }
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
                    5 => self.config.hotkey_faster = hk,
                    6 => self.config.hotkey_slower = hk,
                    7 => self.config.hotkey_skip = hk,
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

        // ---- deferred OCR region pick ----------------------------------------
        // Two points, because a rectangle cannot be described by one. The first
        // press captures the top-left corner, the second the bottom-right.
        if let Some((deadline, second)) = self.ocr_pick {
            ui.ctx().request_repaint();
            if Instant::now() >= deadline {
                let (px, py) = platform::cursor_pos();
                if second {
                    let (ax, ay) = self.ocr_corner;
                    let x = ax.min(px);
                    let y = ay.min(py);
                    let w = (px - ax).abs().max(8);
                    let h = (py - ay).abs().max(8);
                    self.ocr_rect = (x, y, w, h);
                    self.ocr_pick = None;
                    // Read straight away: seeing the text is the proof it worked.
                    match ocr::read_region(x, y, w, h) {
                        Ok(boxes) if boxes.is_empty() => {
                            self.ocr_text = s.ocr_empty.to_string()
                        }
                        Ok(boxes) => self.ocr_text = ocr::joined(&boxes),
                        Err(e) => self.ocr_text = format!("{} — {e}", s.ocr_off),
                    }
                } else {
                    self.ocr_corner = (px, py);
                    self.ocr_pick = Some((Instant::now() + Duration::from_secs(3), true));
                }
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
                    // One setting, two switches: picking either turns the other off.
                    ui.horizontal_wrapped(|ui| {
                        if ui.radio(self.config.absolute_mouse, s.abs_mouse).clicked() {
                            self.config.absolute_mouse = true;
                        }
                        if ui.radio(!self.config.absolute_mouse, s.mouse_rel).clicked() {
                            self.config.absolute_mouse = false;
                        }
                    });
                    ui.checkbox(&mut self.config.human_mouse, s.human_mouse)
                        .on_hover_text(s.tip_human);
                    if self.config.human_mouse {
                        ui.add(
                            egui::Slider::new(&mut self.config.human_curve, 0..=100)
                                .text(s.human_curve),
                        );
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.mouse_jitter);
                        ui.add(
                            egui::DragValue::new(&mut self.config.mouse_jitter_px).range(0..=60),
                        );
                    });
                    ui.checkbox(&mut self.config.use_window_anchor, s.anchor_use);
                    if self.config.use_window_anchor {
                        ui.checkbox(&mut self.config.anchor_scale, s.anchor_scale);
                    }
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

                // ---- scheduler ------------------------------------------------------
                egui::CollapsingHeader::new(s.sec_schedule).show(ui, |ui| {
                    ui.checkbox(&mut self.config.schedule_enabled, s.sch_enabled);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.sch_time);
                        ui.add(egui::DragValue::new(&mut self.config.schedule_h).range(0..=23));
                        ui.label(":");
                        ui.add(egui::DragValue::new(&mut self.config.schedule_m).range(0..=59));
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.sch_days);
                        let names =
                            [s.day_mon, s.day_tue, s.day_wed, s.day_thu, s.day_fri, s.day_sat,
                             s.day_sun];
                        for (i, n) in names.iter().enumerate() {
                            let bit = 1u8 << i;
                            let on = self.config.schedule_days & bit != 0;
                            if ui.selectable_label(on, *n).clicked() {
                                if on {
                                    self.config.schedule_days &= !bit;
                                } else {
                                    self.config.schedule_days |= bit;
                                }
                            }
                        }
                    });
                    if self.config.schedule_enabled {
                        ui.label(
                            egui::RichText::new(s.sch_next.replace(
                                "{}",
                                &format!(
                                    "{:02}:{:02}",
                                    self.config.schedule_h, self.config.schedule_m
                                ),
                            ))
                            .weak(),
                        );
                    }
                });

                // ---- target window ---------------------------------------------------
                egui::CollapsingHeader::new(s.sec_target).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.tgt_title);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.target_title)
                                .desired_width(200.0),
                        );
                    });
                    ui.checkbox(&mut self.config.target_pause_unfocused, s.tgt_focus);
                });

                // ---- text on screen --------------------------------------------------
                egui::CollapsingHeader::new(s.sec_ocr).show(ui, |ui| {
                    ui.label(egui::RichText::new(s.tip_ocr).weak().small());
                    let (mut rx, mut ry, mut rw, mut rh) = self.ocr_rect;
                    region_ui(ui, s, &mut rx, &mut ry, &mut rw, &mut rh, None);
                    self.ocr_rect = (rx, ry, rw, rh);

                    ui.horizontal_wrapped(|ui| {
                        if ui.button(s.ocr_read).clicked() {
                            match ocr::read_region(rx, ry, rw, rh) {
                                Ok(boxes) if boxes.is_empty() => {
                                    self.ocr_text = s.ocr_empty.to_string()
                                }
                                Ok(boxes) => self.ocr_text = ocr::joined(&boxes),
                                Err(e) => self.ocr_text = format!("{} — {e}", s.ocr_off),
                            }
                        }
                        match self.ocr_pick {
                            Some((d, second)) => {
                                let left =
                                    d.saturating_duration_since(Instant::now()).as_secs() + 1;
                                let msg = if second { s.ocr_corner2 } else { s.ocr_corner1 };
                                ui.label(msg.replace("{}", &left.to_string()));
                            }
                            None => {
                                if ui.button(s.pixel_pick).clicked() {
                                    self.ocr_pick =
                                        Some((Instant::now() + Duration::from_secs(3), false));
                                }
                            }
                        }
                    });

                    if !self.ocr_text.is_empty() {
                        egui::ScrollArea::vertical().max_height(120.0).id_salt("ocr_out").show(
                            ui,
                            |ui| {
                                ui.label(
                                    egui::RichText::new(&self.ocr_text).monospace().small(),
                                );
                            },
                        );
                    }
                });

                // ---- image search ---------------------------------------------------
                egui::CollapsingHeader::new(s.sec_vision).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button(s.img_paste).on_hover_text(s.tip_vision).clicked() {
                            match platform::clipboard_image() {
                                Some((w, h, rgba)) => {
                                    self.set_template(w, h, rgba, "clipboard".into());
                                    self.status_msg = format!("{w}x{h}");
                                }
                                None => self.status_msg = s.img_none.to_string(),
                            }
                        }
                        if ui.button(s.img_load).clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("PNG", &["png"])
                                .set_directory(paths::sub_dir("templates"))
                                .pick_file()
                            {
                                self.load_template_png(&path);
                            }
                        }
                        let has = self.template.is_some();
                        if ui.add_enabled(has, egui::Button::new(s.img_save)).clicked() {
                            if let (Some(tpl), Some(path)) = (
                                self.template.clone(),
                                rfd::FileDialog::new()
                                    .add_filter("PNG", &["png"])
                                    .set_directory(paths::sub_dir("templates"))
                                    .set_file_name("template.png")
                                    .save_file(),
                            ) {
                                let saved = image::RgbaImage::from_raw(
                                    tpl.w,
                                    tpl.h,
                                    tpl.rgba.clone(),
                                )
                                .ok_or_else(|| anyhow::anyhow!("bad template buffer"))
                                .and_then(|img| Ok(img.save(&path)?));
                                match saved {
                                    Ok(()) => {
                                        self.status_msg =
                                            s.saved.replace("{}", &file_label(&path))
                                    }
                                    Err(e) => {
                                        self.status_msg =
                                            s.save_err.replace("{}", &e.to_string())
                                    }
                                }
                            }
                        }
                    });

                    match self.template.as_ref() {
                        Some(t) => ui.label(
                            egui::RichText::new(format!("{}  {}x{}", t.name, t.w, t.h)).weak(),
                        ),
                        None => ui.label(egui::RichText::new(s.img_none).weak()),
                    };

                    ui.add(
                        egui::Slider::new(&mut self.config.img_threshold, 0.3..=1.0)
                            .text(s.img_threshold),
                    );
                    ui.checkbox(&mut self.config.img_multiscale, s.img_multiscale);
                    ui.checkbox(&mut self.config.img_region_enabled, s.img_region);
                    if self.config.img_region_enabled {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("X");
                            ui.add(
                                egui::DragValue::new(&mut self.config.img_rx)
                                    .range(-32000..=32000),
                            );
                            ui.label("Y");
                            ui.add(
                                egui::DragValue::new(&mut self.config.img_ry)
                                    .range(-32000..=32000),
                            );
                            ui.label("W");
                            ui.add(
                                egui::DragValue::new(&mut self.config.img_rw).range(8..=32000),
                            );
                            ui.label("H");
                            ui.add(
                                egui::DragValue::new(&mut self.config.img_rh).range(8..=32000),
                            );
                        });
                    }

                    let busy_search = SEARCHING.load(Ordering::Relaxed);
                    ui.horizontal_wrapped(|ui| {
                        let can = self.template.is_some() && !busy_search;
                        if ui.add_enabled(can, egui::Button::new(s.img_find)).clicked() {
                            if let Some(tpl) = self.template.clone() {
                                spawn_search(
                                    tpl,
                                    self.search_region(),
                                    self.config.img_multiscale,
                                );
                            }
                        }
                        if busy_search {
                            ui.spinner();
                            ui.label(s.img_searching);
                        }
                    });

                    if !busy_search {
                        let hit = *LAST_HIT.lock();
                        if let Some(h) = hit {
                            let text = if h.score as f64 >= self.config.img_threshold {
                                s.img_found
                                    .replacen("{}", &format!("({}, {})", h.x, h.y), 1)
                                    .replacen("{}", &format!("{:.3}", h.score), 1)
                            } else {
                                s.img_not_found.replace("{}", &format!("{:.3}", h.score))
                            };
                            ui.label(text);
                        }
                    }

                    // Repaint while a search runs so the spinner actually spins.
                    if busy_search {
                        ui.ctx().request_repaint_after(Duration::from_millis(80));
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
                    ui.separator();
                    // These three act while the macro is running.
                    ui.label(egui::RichText::new(s.tip_speed).weak().small());
                    changed |=
                        hotkey_row(ui, s, s.hk_faster, "hk5", 5, &mut self.config.hotkey_faster);
                    changed |=
                        hotkey_row(ui, s, s.hk_slower, "hk6", 6, &mut self.config.hotkey_slower);
                    changed |=
                        hotkey_row(ui, s, s.hk_skip, "hk7", 7, &mut self.config.hotkey_skip);
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

                if playing {
                    let sp = *self.state.speed.lock();
                    ui.label(
                        egui::RichText::new(s.status_speed.replace("{}", &format!("{sp:.2}")))
                            .weak(),
                    );
                }

                let status = if !self.status_msg.is_empty() {
                    self.status_msg.clone()
                } else if recording {
                    s.status_rec.to_string()
                } else if playing && !target_window_ready(&self.state) {
                    s.status_waiting.to_string()
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

    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("scheduler".into())
            .spawn(move || scheduler_thread(st))?;
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

    /// Builds a haystack with `tpl` pasted in at (px, py).
    fn haystack(w: u32, h: u32, tpl: &vision::Template, px: u32, py: u32) -> vision::Frame {
        let mut rgba = vec![30u8; (w * h * 4) as usize];
        for i in (3..rgba.len()).step_by(4) {
            rgba[i] = 255;
        }
        for y in 0..tpl.h {
            for x in 0..tpl.w {
                let d = (((py + y) * w + px + x) * 4) as usize;
                let sidx = ((y * tpl.w + x) * 4) as usize;
                rgba[d..d + 4].copy_from_slice(&tpl.rgba[sidx..sidx + 4]);
            }
        }
        vision::Frame { x: 0, y: 0, w, h, rgba }
    }

    fn checker_template(w: u32, h: u32) -> vision::Template {
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let v = if (x / 4 + y / 4) % 2 == 0 { 240 } else { 15 };
                rgba[i] = v;
                rgba[i + 1] = v / 2;
                rgba[i + 2] = 255 - v;
                rgba[i + 3] = 255;
            }
        }
        vision::Template { w, h, rgba, name: "checker".into() }
    }

    #[test]
    fn ocr_text_matching_is_forgiving() {
        assert!(ocr::text_matches("You  WIN!", "you win"));
        assert!(ocr::text_matches("Claim reward", "CLAIM"));
        assert!(!ocr::text_matches("Locked", "claim"));
        // An empty needle must never match everything.
        assert!(!ocr::text_matches("anything", ""));
    }

    #[test]
    fn ocr_reads_numbers_from_game_text() {
        assert_eq!(ocr::first_number("Gems: 500"), Some(500.0));
        assert_eq!(ocr::first_number("1,250 coins"), Some(1250.0));
        assert_eq!(ocr::first_number("1 250 coins"), Some(1250.0));
        assert_eq!(ocr::first_number("Energy 42/100"), Some(42.0));
        assert_eq!(ocr::first_number("no digits here"), None);
    }

    #[test]
    fn ocr_reads_a_countdown_as_seconds() {
        assert_eq!(ocr::parse_clock("02:34"), Some(154.0));
        assert_eq!(ocr::parse_clock("Next in 1:02:03"), Some(3723.0));
        assert_eq!(ocr::parse_clock("no clock"), None);
    }

    #[test]
    fn ocr_joins_lines() {
        let boxes = vec![
            ocr::TextBox { text: "one".into(), x: 0, y: 0, w: 1, h: 1 },
            ocr::TextBox { text: "two".into(), x: 0, y: 2, w: 1, h: 1 },
        ];
        assert_eq!(ocr::joined(&boxes), "one\ntwo");
    }

    #[test]
    fn finds_a_template_at_the_exact_spot() {
        let tpl = checker_template(32, 24);
        let hay = haystack(320, 240, &tpl, 100, 60);
        let hit = vision::find(&hay, &tpl, false).expect("should find it");
        assert!(hit.score > 0.95, "score was {}", hit.score);
        // The hit reports the centre of the match.
        assert_eq!((hit.x, hit.y), (100 + 16, 60 + 12));
    }

    #[test]
    fn reports_the_frame_origin_in_screen_coordinates() {
        let tpl = checker_template(24, 24);
        let mut hay = haystack(200, 200, &tpl, 40, 30);
        hay.x = 1920; // second monitor
        hay.y = -100;
        let hit = vision::find(&hay, &tpl, false).unwrap();
        assert_eq!((hit.x, hit.y), (1920 + 40 + 12, -100 + 30 + 12));
    }

    #[test]
    fn a_missing_template_scores_low() {
        let tpl = checker_template(32, 32);
        let flat = vision::Frame {
            x: 0,
            y: 0,
            w: 200,
            h: 200,
            rgba: vec![90u8; 200 * 200 * 4],
        };
        let hit = vision::find(&flat, &tpl, false);
        // A featureless screen cannot correlate with a patterned template.
        assert!(hit.map(|h| h.score < 0.5).unwrap_or(true));
    }

    #[test]
    fn template_larger_than_the_screen_is_not_a_match() {
        let tpl = checker_template(64, 64);
        let small = vision::Frame { x: 0, y: 0, w: 32, h: 32, rgba: vec![0u8; 32 * 32 * 4] };
        assert!(vision::find(&small, &tpl, false).is_none());
    }

    #[test]
    fn insert_click_adds_three_events_and_shifts_the_tail() {
        let mut d = MacroData::new(vec![ev(0), ev(100_000)], 100_000);
        editor_insert_click(&mut d, 0, 700, 400);
        assert_eq!(d.events.len(), 5);
        match d.events[2].kind {
            InputEventKind::MouseButton { down: true, x, y, .. } => {
                assert_eq!((x, y), (700, 400))
            }
            _ => panic!("expected a press"),
        }
        // The event that used to follow is pushed back by the click duration.
        assert_eq!(d.events[4].t_us, 130_000);
    }

    fn step(k: StepKind) -> ScriptStep {
        ScriptStep::new(k)
    }

    #[test]
    fn blocks_match_if_else_endif() {
        let steps = vec![
            step(StepKind::If { cond: Condition::Always }),
            step(StepKind::Wait { ms: 1 }),
            step(StepKind::Else),
            step(StepKind::Wait { ms: 2 }),
            step(StepKind::EndIf),
        ];
        let b = resolve_blocks(&steps).unwrap();
        assert_eq!(b.else_of[0], Some(2));
        assert_eq!(b.end_of[0], Some(4));
        assert_eq!(b.end_of[2], Some(4), "Else must know where the block ends");
        assert_eq!(b.start_of[4], Some(0));
    }

    #[test]
    fn blocks_match_nested_while() {
        let steps = vec![
            step(StepKind::While { cond: Condition::Always }),
            step(StepKind::If { cond: Condition::Always }),
            step(StepKind::EndIf),
            step(StepKind::EndWhile),
        ];
        let b = resolve_blocks(&steps).unwrap();
        assert_eq!(b.end_of[0], Some(3));
        assert_eq!(b.end_of[1], Some(2));
        assert_eq!(b.start_of[3], Some(0));
    }

    #[test]
    fn unbalanced_scripts_are_rejected() {
        assert!(resolve_blocks(&[step(StepKind::If { cond: Condition::Always })]).is_err());
        assert!(resolve_blocks(&[step(StepKind::EndIf)]).is_err());
        assert!(resolve_blocks(&[step(StepKind::Else)]).is_err());
        // A While closed by EndIf is a mistake, not a nesting trick.
        assert!(
            resolve_blocks(&[
                step(StepKind::While { cond: Condition::Always }),
                step(StepKind::EndIf),
            ])
            .is_err()
        );
    }

    #[test]
    fn depths_indent_blocks_and_outdent_else() {
        let steps = vec![
            step(StepKind::If { cond: Condition::Always }),
            step(StepKind::Wait { ms: 1 }),
            step(StepKind::Else),
            step(StepKind::Wait { ms: 1 }),
            step(StepKind::EndIf),
        ];
        assert_eq!(script_depths(&steps), vec![0, 1, 0, 1, 0]);
    }

    #[test]
    fn comparisons_and_var_ops() {
        assert!(Cmp::Lt.test(1.0, 2.0));
        assert!(Cmp::Ge.test(2.0, 2.0));
        assert!(!Cmp::Eq.test(1.0, 2.0));
        assert_eq!(VarOp::Add.apply(3.0, 4.0), 7.0);
        assert_eq!(VarOp::Mul.apply(3.0, 4.0), 12.0);
        assert_eq!(VarOp::Set.apply(3.0, 4.0), 4.0);
    }

    #[test]
    fn step_kind_index_roundtrip() {
        for i in 0..StepKind::COUNT {
            assert_eq!(StepKind::from_index(i).index(), i, "kind {i} does not round-trip");
        }
    }

    #[test]
    fn a_scripted_macro_survives_a_save_and_load() {
        let mut d = MacroData::new(vec![ev(0), ev(1000)], 1000);
        d.script = vec![
            step(StepKind::While { cond: Condition::Var {
                name: "n".into(),
                cmp: Cmp::Lt,
                value: 3.0,
            } }),
            step(StepKind::PlayEvents { from: 0, to: 1 }),
            step(StepKind::SetVar { name: "n".into(), op: VarOp::Add, value: 1.0 }),
            step(StepKind::EndWhile),
        ];
        d.vars.insert("n".into(), 0.0);
        let text = serde_json::to_string(&d).unwrap();
        let back = parse_macro(&text).unwrap();
        assert_eq!(back.script.len(), 4);
        assert!(back.has_script());
        assert_eq!(back.vars.get("n"), Some(&0.0));
    }

    #[test]
    fn a_macro_without_a_script_still_loads() {
        let d = MacroData::new(vec![ev(0), ev(10)], 10);
        let back = parse_macro(&serde_json::to_string(&d).unwrap()).unwrap();
        assert!(!back.has_script());
    }

    #[test]
    fn a_disabled_step_does_not_count_as_a_script() {
        let mut d = MacroData::new(vec![ev(0)], 0);
        d.script = vec![ScriptStep { kind: StepKind::Exit, enabled: false }];
        assert!(!d.has_script());
    }

    #[test]
    fn coord_map_identity_changes_nothing() {
        assert_eq!(CoordMap::IDENTITY.map(640, 480), (640, 480));
        assert_eq!(CoordMap::IDENTITY.map_delta(-3, 9), (-3, 9));
    }

    #[test]
    fn coord_map_follows_a_moved_window() {
        let m = CoordMap { rx: 100, ry: 100, ox: 400, oy: 250, sx: 1.0, sy: 1.0 };
        assert_eq!(m.map(150, 130), (450, 280));
    }

    #[test]
    fn coord_map_follows_a_resized_window() {
        // Recorded in a 800x600 window at (100,100); now 1600x600 at (0,0).
        let anchor = WindowAnchor { title: "t".into(), x: 100, y: 100, w: 800, h: 600 };
        let m = CoordMap {
            rx: anchor.x,
            ry: anchor.y,
            ox: 0,
            oy: 0,
            sx: 1600.0 / 800.0,
            sy: 1.0,
        };
        // A click halfway across the old window stays halfway across the new one.
        assert_eq!(m.map(500, 400), (800, 300));
        assert_eq!(m.map_delta(10, 10), (20, 10));
    }

    #[test]
    fn bezier_path_starts_and_ends_near_the_line() {
        let mut rng = Rng::new();
        let pts = bezier_path((0, 0), (100, 0), 0.0, &mut rng, 10);
        assert_eq!(pts.len(), 9);
        // With zero curvature the arc collapses onto the straight line.
        assert!(pts.iter().all(|p| p.1.abs() <= 1), "{pts:?}");
        assert!(pts[0].0 < pts[8].0, "points must advance towards the target");
    }

    #[test]
    fn bezier_bows_away_when_curved() {
        let mut rng = Rng::new();
        let pts = bezier_path((0, 0), (400, 0), 1.0, &mut rng, 16);
        assert!(pts.iter().any(|p| p.1.abs() > 5), "a curved path must leave the line");
    }

    #[test]
    fn rng_signed_and_unit_stay_in_range() {
        let mut rng = Rng::new();
        for _ in 0..2000 {
            assert!((-5..=5).contains(&rng.signed(5)));
            let u = rng.unit();
            assert!((-1.0..=1.0).contains(&u), "{u}");
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
