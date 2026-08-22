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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, CreateCompatibleBitmap,
        CreateCompatibleDC, CreateDIBSection, CreatePen, CreateSolidBrush, DIB_RGB_COLORS,
        DeleteDC, DeleteObject, EndPaint, FillRect, GetDC, GetPixel,
        GetStockObject, HBITMAP, HDC, HGDIOBJ, HRGN, InvalidateRect, NULL_BRUSH,
        PAINTSTRUCT, PS_SOLID, Rectangle, ReleaseDC, SRCCOPY, SelectObject, SetBkMode,
        SetTextColor, TRANSPARENT, TextOutW, UpdateWindow,
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
    pub use windows::Win32::System::Diagnostics::ToolHelp::*;
    pub use windows::core::{BOOL, PCSTR, PCWSTR, PWSTR, w};
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
    pub fn templates_dir() -> PathBuf {
        sub_dir("templates")
    }
    pub fn expansions_path() -> PathBuf {
        data_dir().join("expansions.json")
    }

    /// Creates the folders the documentation tells people to look for.
    ///
    /// `sub_dir` makes a folder the moment something touches it, which meant
    /// `templates/` only appeared once a PNG had already been saved - no use to
    /// somebody who wanted to drop one in beforehand.
    pub fn ensure_dirs() {
        let _ = log_dir();
        let _ = profiles_dir();
        let _ = lang_dir();
        let _ = templates_dir();
    }
}

/// Editor for what a step does when it does not find what it was looking for.
///
/// One row, everywhere it applies, so the answer to "what happens if this is not
/// there" is in the same place on every step that can ask it.
fn miss_ui(ui: &mut egui::Ui, s: &Strings, salt: &str, miss: &mut OnMiss) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(s.m_onmiss).on_hover_text(s.tip_onmiss);
        let cur = miss.index();
        egui::ComboBox::from_id_salt(format!("{salt}_miss"))
            .selected_text(miss.name(s))
            .width(150.0)
            .show_ui(ui, |ui| {
                for i in 0..OnMiss::COUNT {
                    let opt = OnMiss::from_index(i);
                    if ui.selectable_label(cur == i, opt.name(s)).clicked() && cur != i {
                        *miss = opt;
                        changed = true;
                    }
                }
            });
        if let OnMiss::Retry { times, delay_ms } = miss {
            changed |= ui.add(egui::DragValue::new(times).range(1..=100)).changed();
            ui.label(s.m_times);
            changed |= ui
                .add(egui::DragValue::new(delay_ms).range(0..=600_000).speed(50.0))
                .changed();
            ui.label(s.m_delay);
        }
    });
    changed
}

/// Editor for a search area. The same four choices wherever an image is looked for.
fn area_ui(ui: &mut egui::Ui, s: &Strings, salt: &str, area: &mut SearchArea) -> bool {
    let mut changed = false;
    let names = [s.a_full, s.a_window, s.a_rect, s.a_near, s.a_anchor];
    let mut idx = match area {
        SearchArea::FullScreen => 0,
        SearchArea::ActiveWindow => 1,
        SearchArea::Rect { .. } => 2,
        SearchArea::NearLast { .. } => 3,
        SearchArea::NearAnchor { .. } => 4,
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(s.f_area);
        egui::ComboBox::from_id_salt(format!("{salt}_area"))
            .selected_text(names[idx])
            .width(150.0)
            .show_ui(ui, |ui| {
                for (i, n) in names.iter().enumerate() {
                    if ui.selectable_label(idx == i, *n).clicked() {
                        idx = i;
                    }
                }
            });
        // Switching kind keeps the numbers the old kind had, so flicking between them
        // while setting one up does not throw the work away.
        let want = match idx {
            1 => SearchArea::ActiveWindow,
            2 => match area {
                SearchArea::Rect { .. } => area.clone(),
                _ => SearchArea::Rect { x: 0, y: 0, w: 600, h: 400 },
            },
            3 => match area {
                SearchArea::NearLast { .. } => area.clone(),
                _ => SearchArea::NearLast { margin: 100 },
            },
            4 => match area {
                SearchArea::NearAnchor { .. } => area.clone(),
                _ => SearchArea::NearAnchor {
                    anchor: String::new(),
                    dx: -150,
                    dy: 0,
                    w: 300,
                    h: 120,
                },
            },
            _ => SearchArea::FullScreen,
        };
        if want != *area {
            *area = want;
            changed = true;
        }
        match area {
            SearchArea::Rect { x, y, w, h } => {
                for (label, v) in [("X", x), ("Y", y)] {
                    ui.label(label);
                    changed |=
                        ui.add(egui::DragValue::new(v).range(-32000..=32000)).changed();
                }
                for (label, v) in [("W", w), ("H", h)] {
                    ui.label(label);
                    changed |= ui.add(egui::DragValue::new(v).range(1..=32000)).changed();
                }
            }
            SearchArea::NearLast { margin } => {
                ui.label(s.f_margin);
                changed |= ui.add(egui::DragValue::new(margin).range(8..=2000)).changed();
            }
            _ => {}
        }
    });
    // The anchor gets its own row: a name, a picker and four numbers do not fit
    // beside the kind chooser on any sensible window width.
    if let SearchArea::NearAnchor { anchor, dx, dy, w, h } = area {
        ui.horizontal_wrapped(|ui| {
            ui.label(s.f_anchor);
            changed |= ui
                .add(egui::TextEdit::singleline(anchor).desired_width(120.0))
                .changed();
            changed |= template_picker(ui, &format!("{salt}_anchor"), anchor);
            for (label, v) in [("dX", dx), ("dY", dy)] {
                ui.label(label);
                changed |= ui.add(egui::DragValue::new(v).range(-32000..=32000)).changed();
            }
            for (label, v) in [("W", w), ("H", h)] {
                ui.label(label);
                changed |= ui.add(egui::DragValue::new(v).range(1..=32000)).changed();
            }
        });
    }
    changed
}

/// One line describing what an element query is looking for.
fn describe_query(q: &uia::Query) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !q.control.is_empty() {
        parts.push(q.control.clone());
    }
    if !q.name.is_empty() {
        parts.push(format!("\"{}\"", q.name));
    }
    if !q.automation_id.is_empty() {
        parts.push(format!("#{}", q.automation_id));
    }
    if parts.is_empty() {
        parts.push("?".into());
    }
    parts.join(" ")
}

/// Editor for an element query.
fn query_ui(ui: &mut egui::Ui, s: &Strings, salt: &str, q: &mut uia::Query) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(s.f_name);
        changed |= ui
            .add(egui::TextEdit::singleline(&mut q.name).desired_width(150.0))
            .on_hover_text(s.tip_uia)
            .changed();
        let names: Vec<&str> =
            uia::CONTROLS.iter().map(|(n, _)| if n.is_empty() { s.f_any } else { *n }).collect();
        let mut idx = uia::CONTROLS
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(&q.control))
            .unwrap_or(0);
        ui.label(s.f_control);
        egui::ComboBox::from_id_salt(format!("{salt}_ctl"))
            .selected_text(names[idx])
            .width(96.0)
            .show_ui(ui, |ui| {
                for (i, n) in names.iter().enumerate() {
                    if ui.selectable_label(idx == i, *n).clicked() {
                        idx = i;
                    }
                }
            });
        let want = uia::CONTROLS[idx].0.to_string();
        if want != q.control {
            q.control = want;
            changed = true;
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(s.f_autoid);
        changed |= ui
            .add(egui::TextEdit::singleline(&mut q.automation_id).desired_width(130.0))
            .changed();
        changed |= ui.checkbox(&mut q.in_front, s.f_in_front).changed();
    });
    changed
}

/// Picker for where a piece of text is read from.
fn source_picker(
    ui: &mut egui::Ui,
    s: &Strings,
    salt: &str,
    src: &mut TextSource,
) -> bool {
    let names = [s.t_clipboard, s.t_wintitle, s.t_process, s.t_file];
    let mut idx = src.index();
    let mut changed = false;
    egui::ComboBox::from_id_salt(format!("{salt}_src"))
        .selected_text(names[idx])
        .width(140.0)
        .show_ui(ui, |ui| {
            for (i, n) in names.iter().enumerate() {
                if ui.selectable_label(idx == i, *n).clicked() {
                    idx = i;
                }
            }
        });
    if idx != src.index() {
        *src = TextSource::from_index(idx);
        changed = true;
    }
    if let TextSource::File(path) = src {
        changed |= ui
            .add(egui::TextEdit::singleline(path).desired_width(160.0))
            .changed();
    }
    changed
}

/// Picker for where a piece of text is sent.
fn sink_picker(ui: &mut egui::Ui, s: &Strings, salt: &str, sink: &mut TextSink) -> bool {
    let names = [s.t_clipboard, s.t_file];
    let mut idx = sink.index();
    let mut changed = false;
    egui::ComboBox::from_id_salt(format!("{salt}_sink"))
        .selected_text(names[idx])
        .width(120.0)
        .show_ui(ui, |ui| {
            for (i, n) in names.iter().enumerate() {
                if ui.selectable_label(idx == i, *n).clicked() {
                    idx = i;
                }
            }
        });
    if idx != sink.index() {
        *sink = TextSink::from_index(idx);
        changed = true;
    }
    if let TextSink::File { path, append } = sink {
        changed |= ui
            .add(egui::TextEdit::singleline(path).desired_width(150.0))
            .changed();
        changed |= ui.checkbox(append, s.f_append).changed();
    }
    changed
}

/// Editor for a script value: a number, or a piece of text.
///
/// The kind is a visible choice rather than something guessed from what was typed.
/// Guessing would make `007` and `7` the same value on some days and not others.
fn value_ui(ui: &mut egui::Ui, s: &Strings, salt: &str, v: &mut Value) -> bool {
    let mut changed = false;
    let names = [s.v_number, s.v_text];
    let mut idx = usize::from(v.is_text());
    egui::ComboBox::from_id_salt(format!("{salt}_kind"))
        .selected_text(names[idx])
        .width(78.0)
        .show_ui(ui, |ui| {
            for (i, n) in names.iter().enumerate() {
                if ui.selectable_label(idx == i, *n).clicked() {
                    idx = i;
                }
            }
        });
    if (idx == 1) != v.is_text() {
        // Carried across rather than reset: flicking between the two while setting
        // one up should not throw away what was typed.
        *v = if idx == 1 { Value::Str(v.as_text()) } else { Value::Num(v.as_num()) };
        changed = true;
    }
    match v {
        Value::Num(n) => changed |= ui.add(egui::DragValue::new(n).speed(0.5)).changed(),
        Value::Str(t) => {
            changed |= ui
                .add(egui::TextEdit::singleline(t).desired_width(150.0))
                .on_hover_text(s.tip_value_text)
                .changed()
        }
    }
    changed
}

/// Picker for what is done to a region's pixels before it is read.
fn prep_picker(ui: &mut egui::Ui, s: &Strings, salt: &str, prep: &mut ocr::Prep) -> bool {
    let names = [s.p_none, s.p_ui, s.p_small, s.p_game, s.p_digits, s.p_auto];
    let mut idx = prep.index().min(names.len() - 1);
    ui.label(s.f_prep);
    egui::ComboBox::from_id_salt(format!("{salt}_prep"))
        .selected_text(names[idx])
        .width(118.0)
        .show_ui(ui, |ui| {
            for (i, n) in names.iter().enumerate() {
                if ui.selectable_label(idx == i, *n).clicked() {
                    idx = i;
                }
            }
        });
    let want = ocr::Prep::from_index(idx);
    if want != *prep {
        *prep = want;
        return true;
    }
    false
}

/// Picker for the expected format, with a box for the pattern when one is wanted.
fn expect_picker(ui: &mut egui::Ui, s: &Strings, salt: &str, e: &mut ocr::Expect) -> bool {
    let names = [s.x_any, s.x_int, s.x_dec, s.x_time, s.x_pattern];
    let mut idx = e.index().min(names.len() - 1);
    let mut changed = false;
    ui.label(s.f_expect);
    egui::ComboBox::from_id_salt(format!("{salt}_expect"))
        .selected_text(names[idx])
        .width(104.0)
        .show_ui(ui, |ui| {
            for (i, n) in names.iter().enumerate() {
                if ui.selectable_label(idx == i, *n).clicked() {
                    idx = i;
                }
            }
        });
    if idx != e.index() {
        *e = ocr::Expect::from_index(idx);
        changed = true;
    }
    if let ocr::Expect::Pattern(p) = e {
        changed |= ui
            .add(egui::TextEdit::singleline(p).desired_width(90.0))
            .on_hover_text(s.tip_pattern)
            .changed();
    }
    changed
}

fn action_index(a: &expander::Action) -> usize {
    match a {
        expander::Action::Text => 0,
        expander::Action::PlayMacro => 1,
        expander::Action::StopAll => 2,
        expander::Action::RunProgram => 3,
    }
}

fn action_from_index(i: usize) -> expander::Action {
    match i {
        1 => expander::Action::PlayMacro,
        2 => expander::Action::StopAll,
        3 => expander::Action::RunProgram,
        _ => expander::Action::Text,
    }
}

fn trigger_index(t: &expander::Trigger) -> usize {
    match t {
        expander::Trigger::Inherit => 0,
        expander::Trigger::Delimiter => 1,
        expander::Trigger::Prefix(_) => 2,
        expander::Trigger::Instant => 3,
    }
}

fn trigger_from_index(i: usize, prefix: &str) -> expander::Trigger {
    match i {
        1 => expander::Trigger::Delimiter,
        2 => expander::Trigger::Prefix(if prefix.is_empty() {
            ";;".to_string()
        } else {
            prefix.to_string()
        }),
        3 => expander::Trigger::Instant,
        _ => expander::Trigger::Inherit,
    }
}

/// PNG files sitting in the templates folder.
///
/// Read on demand rather than cached: the list only matters while a picker is open,
/// and somebody who has just saved a new template expects to see it there at once.
fn template_names() -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(paths::templates_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")) {
                return None;
            }
            Some(p.file_stem()?.to_string_lossy().into_owned())
        })
        .collect();
    out.sort_unstable();
    out
}

/// A dropdown of saved templates beside the name field, so a script juggling
/// several pictures needs none of their names typed from memory.
fn template_picker(ui: &mut egui::Ui, salt: &str, current: &mut String) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(salt).selected_text("\u{25be}").width(34.0).show_ui(
        ui,
        |ui| {
            let names = template_names();
            if names.is_empty() {
                ui.label("templates/");
            }
            for name in names {
                if ui.selectable_label(*current == name, name.as_str()).clicked() {
                    *current = name;
                    changed = true;
                }
            }
        },
    );
    changed
}

// ============================================================================
// Text expander
// ============================================================================

/// Turns a typed abbreviation into a longer piece of text.
///
/// The engine watches the keyboard hook, keeps a short rolling buffer of the
/// characters that came out of it, and when the tail of that buffer matches an entry
/// it deletes what was typed and writes the replacement instead.
///
/// Three things about it are worth knowing before reading the code.
///
/// **The buffer is a privacy surface.** It holds what you have just typed, in any
/// application, which is a short step from what a keylogger holds. So it never
/// reaches the log at any level, never reaches the disk, is capped at 64 characters,
/// and is emptied whenever the foreground window changes.
///
/// **The hook must not do the work.** Detecting a match happens in the hook callback,
/// which Windows will silently unhook if it dawdles. Everything after that - the
/// backspaces, the replacement, the clipboard - is handed to a worker thread.
///
/// **Some input cannot be handled and is refused rather than guessed at.** An IME
/// commits characters that never correspond to the keystrokes the hook saw, and a
/// dead key turns two keystrokes into one character. In both cases the count of
/// backspaces needed is unknowable, so the buffer is emptied and nothing fires.
pub mod expander {
    use serde::{Deserialize, Serialize};

    /// When an entry fires.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub enum Trigger {
        /// Follow the global setting. What almost every entry should say.
        Inherit,
        /// After the abbreviation and then a delimiter: `addr` then a space.
        Delimiter,
        /// As soon as the abbreviation is typed, but only behind a marker: `;;sig`.
        Prefix(String),
        /// The moment the abbreviation appears, anywhere. Short ones will misfire.
        Instant,
    }

    /// How the replacement gets into the application.
    #[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
    pub enum Insert {
        /// One synthetic keystroke per character. Works everywhere, slow for long text.
        Type,
        /// Through the clipboard and Ctrl+V. Instant, but not every window pastes
        /// that way and the clipboard has to be borrowed and given back.
        Paste,
    }

    /// What an entry does when it fires.
    ///
    /// A text expander that can also start and stop the thing this application is for
    /// stops being a bolted-on extra: `;farm` starts the macro, `;stop` ends it, and
    /// the abbreviation becomes a command line that works in any window.
    #[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
    pub enum Action {
        /// Replace the abbreviation with the text. What every entry did until now.
        #[default]
        Text,
        /// Play the macro. If the text names a file, load that one first.
        PlayMacro,
        /// Stop whatever is running - recording, playback, everything.
        StopAll,
        /// Hand the text to the shell, the way the `Run` script step does.
        RunProgram,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Entry {
        #[serde(default = "yes")]
        pub enabled: bool,
        pub abbr: String,
        pub text: String,
        #[serde(default = "inherit")]
        pub trigger: Trigger,
        #[serde(default = "typing")]
        pub insert: Insert,
        #[serde(default)]
        pub action: Action,
    }

    fn yes() -> bool {
        true
    }
    fn inherit() -> Trigger {
        Trigger::Inherit
    }
    fn typing() -> Insert {
        Insert::Type
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Book {
        /// The master switch. Off until asked for: this one types into other people's
        /// windows.
        #[serde(default)]
        pub enabled: bool,
        /// What `Trigger::Inherit` resolves to.
        #[serde(default = "delimiter")]
        pub default_trigger: Trigger,
        /// Characters that count as the end of a word.
        #[serde(default = "default_delims")]
        pub delimiters: String,
        /// Window titles, matched case-insensitively by substring, where the expander
        /// stays quiet. A password manager and a terminal belong here.
        #[serde(default)]
        pub excluded_windows: Vec<String>,
        #[serde(default)]
        pub entries: Vec<Entry>,
    }

    fn delimiter() -> Trigger {
        Trigger::Delimiter
    }
    fn default_delims() -> String {
        " \t\n.,;:!?)]}\"'".to_string()
    }

    impl Default for Book {
        fn default() -> Self {
            Self {
                enabled: false,
                default_trigger: Trigger::Delimiter,
                delimiters: default_delims(),
                excluded_windows: Vec::new(),
                entries: vec![
                    Entry {
                        enabled: true,
                        abbr: "addr".into(),
                        text: "221B Baker Street\nLondon".into(),
                        trigger: Trigger::Inherit,
                        insert: Insert::Type,
                        action: Action::Text,
                    },
                    Entry {
                        enabled: true,
                        abbr: "today".into(),
                        text: "{date}".into(),
                        trigger: Trigger::Inherit,
                        insert: Insert::Type,
                        action: Action::Text,
                    },
                    Entry {
                        enabled: true,
                        abbr: ";sig".into(),
                        text: "Kind regards,\n{cursor}".into(),
                        trigger: Trigger::Instant,
                        insert: Insert::Type,
                        action: Action::Text,
                    },
                    Entry {
                        enabled: true,
                        abbr: ";stop".into(),
                        text: String::new(),
                        trigger: Trigger::Instant,
                        insert: Insert::Type,
                        action: Action::StopAll,
                    },
                ],
            }
        }
    }

    /// One piece of a rendered replacement.
    ///
    /// A replacement is a list rather than a string because `{key:Tab}` and `{cursor}`
    /// cannot be expressed as characters: a keystroke has to be sent as a keystroke,
    /// and the cursor is moved after everything else has landed.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Segment {
        Text(String),
        Key(u16),
        Cursor,
    }

    /// What the hook decided, for the worker to carry out.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Fire {
        /// Characters to delete, including the delimiter when there was one.
        pub backspaces: usize,
        pub segments: Vec<Segment>,
        pub insert: Insert,
        pub action: Action,
        /// The entry's raw text, for the actions that read it as a path.
        pub payload: String,
    }

    pub fn is_delimiter(book: &Book, c: char) -> bool {
        book.delimiters.contains(c)
    }

    fn resolve(book: &Book, t: &Trigger) -> Trigger {
        match t {
            Trigger::Inherit => match &book.default_trigger {
                Trigger::Inherit => Trigger::Delimiter,
                other => other.clone(),
            },
            other => other.clone(),
        }
    }

    /// Does `buf` end with `needle`, and is the character before it a word boundary?
    fn ends_on_boundary(book: &Book, buf: &[char], needle: &str) -> bool {
        let n: Vec<char> = needle.chars().collect();
        if n.is_empty() || buf.len() < n.len() {
            return false;
        }
        if buf[buf.len() - n.len()..] != n[..] {
            return false;
        }
        // Without this, `addr` fires inside `readdr`.
        match buf.len().checked_sub(n.len() + 1).map(|i| buf[i]) {
            Some(prev) => is_delimiter(book, prev),
            None => true,
        }
    }

    /// Decides whether the character just typed completed an abbreviation.
    ///
    /// `allow_text` is false while a macro is replaying: an entry that types would
    /// fight with the macro, but one that only starts or stops something is exactly
    /// what somebody reaching for `;stop` wants.
    pub fn match_at(
        book: &Book,
        buf: &[char],
        typed: char,
        allow_text: bool,
    ) -> Option<Fire> {
        if !book.enabled || buf.is_empty() {
            return None;
        }
        let mut best: Option<(usize, &Entry)> = None;
        for e in book
            .entries
            .iter()
            .filter(|e| e.enabled && !e.abbr.is_empty())
            .filter(|e| allow_text || e.action != Action::Text)
        {
            let hit = match resolve(book, &e.trigger) {
                Trigger::Instant => ends_on_boundary(book, buf, &e.abbr)
                    .then(|| e.abbr.chars().count()),
                Trigger::Prefix(p) => {
                    let whole = format!("{p}{}", e.abbr);
                    ends_on_boundary(book, buf, &whole).then(|| whole.chars().count())
                }
                _ => {
                    // The delimiter itself is the last character in the buffer, so the
                    // abbreviation has to be checked against everything before it.
                    if !is_delimiter(book, typed) {
                        None
                    } else {
                        let head = &buf[..buf.len() - 1];
                        ends_on_boundary(book, head, &e.abbr)
                            .then(|| e.abbr.chars().count() + 1)
                    }
                }
            };
            if let Some(back) = hit {
                // Longest wins, so `;sign` beats `;sig`.
                if best.map_or(true, |(b, _)| back > b) {
                    best = Some((back, e));
                }
            }
        }
        let (backspaces, entry) = best?;
        // A command reads its text as a path, so expanding placeholders in it and
        // splitting it into keystrokes would be nonsense.
        if entry.action != Action::Text {
            return Some(Fire {
                backspaces,
                segments: Vec::new(),
                insert: entry.insert,
                action: entry.action,
                payload: entry.text.clone(),
            });
        }
        let mut segments = render(&entry.text);
        // In delimiter mode the delimiter was eaten with the abbreviation, so it has
        // to come back or the next word runs into the replacement.
        if matches!(resolve(book, &entry.trigger), Trigger::Delimiter) {
            match segments.last_mut() {
                Some(Segment::Text(t)) => t.push(typed),
                _ => segments.push(Segment::Text(typed.to_string())),
            }
        }
        Some(Fire {
            backspaces,
            segments,
            insert: entry.insert,
            action: Action::Text,
            payload: String::new(),
        })
    }

    /// Splits a replacement into segments, expanding the placeholders.
    ///
    /// A backslash escapes the next character, which is how a replacement can contain
    /// a literal `{date}`.
    pub fn render(text: &str) -> Vec<Segment> {
        let mut out: Vec<Segment> = Vec::new();
        let mut lit = String::new();
        let mut it = text.chars().peekable();
        let push_lit = |out: &mut Vec<Segment>, lit: &mut String| {
            if !lit.is_empty() {
                out.push(Segment::Text(std::mem::take(lit)));
            }
        };
        while let Some(c) = it.next() {
            match c {
                '\\' => {
                    if let Some(n) = it.next() {
                        lit.push(n);
                    }
                }
                '{' => {
                    let mut token = String::new();
                    let mut closed = false;
                    for t in it.by_ref() {
                        if t == '}' {
                            closed = true;
                            break;
                        }
                        token.push(t);
                    }
                    if !closed {
                        // An unclosed brace is a typo, not a token. Show it as typed.
                        lit.push('{');
                        lit.push_str(&token);
                        continue;
                    }
                    let (name, arg) = match token.split_once(':') {
                        Some((n, a)) => (n.trim(), a),
                        None => (token.trim(), ""),
                    };
                    match name {
                        "cursor" => {
                            push_lit(&mut out, &mut lit);
                            out.push(Segment::Cursor);
                        }
                        "key" => {
                            if let Some(vk) = key_by_name(arg.trim()) {
                                push_lit(&mut out, &mut lit);
                                out.push(Segment::Key(vk));
                            }
                        }
                        "date" => lit.push_str(&stamp(if arg.is_empty() {
                            "yyyy-MM-dd"
                        } else {
                            arg
                        })),
                        "time" => {
                            lit.push_str(&stamp(if arg.is_empty() { "HH:mm" } else { arg }))
                        }
                        "datetime" => lit.push_str(&stamp(if arg.is_empty() {
                            "yyyy-MM-dd HH:mm"
                        } else {
                            arg
                        })),
                        "clipboard" => lit.push_str(&clipboard_text()),
                        "random" => {
                            let choices: Vec<&str> =
                                arg.split('|').filter(|s| !s.is_empty()).collect();
                            if !choices.is_empty() {
                                let i = (super::now_us() as usize) % choices.len();
                                lit.push_str(choices[i]);
                            }
                        }
                        // Anything unrecognised is left as the user wrote it, which is
                        // friendlier than swallowing a typo silently.
                        _ => {
                            lit.push('{');
                            lit.push_str(&token);
                            lit.push('}');
                        }
                    }
                }
                _ => lit.push(c),
            }
        }
        push_lit(&mut out, &mut lit);
        out
    }

    pub fn key_by_name(name: &str) -> Option<u16> {
        Some(match name.to_ascii_lowercase().as_str() {
            "tab" => 0x09,
            "enter" | "return" => 0x0D,
            "esc" | "escape" => 0x1B,
            "space" => 0x20,
            "backspace" => 0x08,
            "delete" | "del" => 0x2E,
            "home" => 0x24,
            "end" => 0x23,
            "up" => 0x26,
            "down" => 0x28,
            "left" => 0x25,
            "right" => 0x27,
            "pageup" => 0x21,
            "pagedown" => 0x22,
            _ => return None,
        })
    }

    /// Formats the current local time. A hand-rolled subset of the .NET patterns,
    /// which read better to a non-programmer than strftime and cost less than a date
    /// library.
    pub fn stamp(fmt: &str) -> String {
        let (y, mo, d, h, mi, sec) = local_now();
        fmt.replace("yyyy", &format!("{y:04}"))
            .replace("yy", &format!("{:02}", y % 100))
            .replace("MM", &format!("{mo:02}"))
            .replace("dd", &format!("{d:02}"))
            .replace("HH", &format!("{h:02}"))
            .replace("mm", &format!("{mi:02}"))
            .replace("ss", &format!("{sec:02}"))
    }

    #[cfg(windows)]
    fn local_now() -> (u16, u16, u16, u16, u16, u16) {
        unsafe {
            let t = windows::Win32::System::SystemInformation::GetLocalTime();
            (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
        }
    }

    #[cfg(not(windows))]
    fn local_now() -> (u16, u16, u16, u16, u16, u16) {
        (2026, 1, 1, 0, 0, 0)
    }

    #[cfg(windows)]
    fn clipboard_text() -> String {
        super::platform::clipboard_text()
    }

    #[cfg(not(windows))]
    fn clipboard_text() -> String {
        String::new()
    }

    // ---- live state ------------------------------------------------------

    use crossbeam_channel::Sender;
    use parking_lot::Mutex;
    use std::sync::LazyLock;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicIsize, Ordering};

    static BOOK: LazyLock<Mutex<Book>> = LazyLock::new(|| Mutex::new(Book::default()));
    /// The characters typed recently. Capped, never logged, never written to disk.
    static BUF: LazyLock<Mutex<Vec<char>>> = LazyLock::new(|| Mutex::new(Vec::new()));
    static LAST_WINDOW: AtomicIsize = AtomicIsize::new(0);
    static TX: OnceLock<Sender<Fire>> = OnceLock::new();

    const BUF_MAX: usize = 64;

    pub fn snapshot() -> Book {
        BOOK.lock().clone()
    }

    pub fn enabled() -> bool {
        BOOK.lock().enabled
    }

    pub fn set_enabled(on: bool) {
        BOOK.lock().enabled = on;
        reset();
    }

    /// Swaps in a book edited elsewhere. The buffer goes with it: entries that no
    /// longer exist must not fire off half-typed words.
    pub fn replace(book: Book) {
        *BOOK.lock() = book;
        reset();
    }

    pub fn reset() {
        BUF.lock().clear();
    }

    pub fn load() {
        let path = super::paths::expansions_path();
        let book = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| match serde_json::from_str::<Book>(&t) {
                Ok(b) => Some(b),
                Err(e) => {
                    tracing::warn!("expansions.json could not be read: {e}");
                    None
                }
            });
        match book {
            Some(b) => *BOOK.lock() = b,
            None => {
                let d = Book::default();
                let _ = save(&d);
                *BOOK.lock() = d;
            }
        }
    }

    pub fn save(book: &Book) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(book)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(super::paths::expansions_path(), text)
    }

    pub fn save_current() -> std::io::Result<()> {
        let b = BOOK.lock().clone();
        save(&b)
    }

    // ---- the hook side ---------------------------------------------------

    /// Feeds one keystroke in. Called from the keyboard hook, so it does the least
    /// it can get away with and hands anything slow to the worker.
    #[cfg(windows)]
    pub fn on_key(vk: u16, scan: u16, down: bool) {
        use super::win32::*;
        if !down {
            return;
        }
        // Never while the application is doing its own thing: expanding into a
        // recording writes the expansion into the macro, and expanding during
        // playback fights with it.
        // Recording swallows the expander whole: the keystrokes would go into the
        // macro and the expansion with them. Playback only rules out the entries that
        // type, which is what leaves `;stop` able to stop it.
        let mut allow_text = true;
        if let Some(st) = super::GLOBAL_STATE.get() {
            if st.recording.load(Ordering::Relaxed) {
                reset();
                return;
            }
            allow_text = !st.playing.load(Ordering::Relaxed);
        }
        let book = BOOK.lock().clone();
        if !book.enabled || book.entries.is_empty() {
            return;
        }

        unsafe {
            let fg = GetForegroundWindow();
            // A buffer carried from one window into another would match text the
            // user never typed here, so the window change empties it.
            if LAST_WINDOW.swap(fg.0 as isize, Ordering::Relaxed) != fg.0 as isize {
                reset();
            }
            if window_excluded(&book, fg) {
                reset();
                return;
            }

            // A modifier pressed by itself types nothing and moves nothing, and
            // treating it as the end of a word broke the commonest case there is:
            // Alt+Shift switches the keyboard layout, so a Cyrillic word followed by
            // an English one had the buffer cleared in between, and the second word
            // looked like the start of a line. Which is how `предt1` expanded.
            let modifier_itself = matches!(
                vk,
                0x10..=0x12       // Shift, Ctrl, Alt
                    | 0x14        // Caps Lock
                    | 0x5B..=0x5C // Win
                    | 0xA0..=0xA5 // the left and right halves of each
            );
            if modifier_itself {
                return;
            }
            let ctrl = GetAsyncKeyState(VK_CONTROL.0 as i32) < 0;
            let alt = GetAsyncKeyState(VK_MENU.0 as i32) < 0;
            let win = GetAsyncKeyState(VK_LWIN.0 as i32) < 0
                || GetAsyncKeyState(VK_RWIN.0 as i32) < 0;
            // A Win combination never writes into the focused field, so it leaves the
            // word alone too - Win+Space being the other way people change layout.
            if win {
                return;
            }
            // Ctrl and Alt combinations do edit and do move the caret: Ctrl+A,
            // Ctrl+V, Ctrl+Backspace. Those really are the end of a word.
            if ctrl || alt {
                reset();
                return;
            }

            let mut ks = [0u8; 256];
            if GetAsyncKeyState(VK_SHIFT.0 as i32) < 0 {
                ks[VK_SHIFT.0 as usize] = 0x80;
            }
            if GetKeyState(VK_CAPITAL.0 as i32) & 1 != 0 {
                ks[VK_CAPITAL.0 as usize] = 0x01;
            }

            let layout = GetKeyboardLayout(GetWindowThreadProcessId(fg, None));
            let mut out = [0u16; 8];
            let n = ToUnicodeEx(vk as u32, scan as u32, &ks, &mut out, 0, Some(layout));
            if n < 0 {
                // A dead key: the next keystroke will produce one character out of
                // two, and the count of backspaces stops being knowable. Flush the
                // kernel's dead-key state and give up on this word.
                let _ = ToUnicodeEx(vk as u32, scan as u32, &ks, &mut out, 0, Some(layout));
                reset();
                return;
            }
            if n == 0 {
                // Arrows, function keys, Home, End: no character, and the caret may
                // have moved somewhere the buffer knows nothing about.
                reset();
                return;
            }
            let text = String::from_utf16_lossy(&out[..n as usize]);
            for c in text.chars() {
                feed(&book, c, allow_text);
            }
        }
    }

    #[cfg(not(windows))]
    pub fn on_key(_vk: u16, _scan: u16, _down: bool) {}

    /// Adds one character and fires if it completed an abbreviation.
    fn feed(book: &Book, raw: char, allow_text: bool) {
        let c = match raw {
            '\r' => '\n',
            '\u{8}' => {
                BUF.lock().pop();
                return;
            }
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' => {
                reset();
                return;
            }
            c => c,
        };
        let fire = {
            let mut buf = BUF.lock();
            buf.push(c);
            if buf.len() > BUF_MAX {
                let cut = buf.len() - BUF_MAX;
                buf.drain(..cut);
            }
            match match_at(book, &buf, c, allow_text) {
                Some(f) => {
                    buf.clear();
                    Some(f)
                }
                None => None,
            }
        };
        if let Some(f) = fire {
            if let Some(tx) = TX.get() {
                let _ = tx.try_send(f);
            }
        }
    }

    #[cfg(windows)]
    fn window_excluded(book: &Book, hwnd: super::win32::HWND) -> bool {
        use super::win32::*;
        if book.excluded_windows.is_empty() {
            return false;
        }
        unsafe {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(hwnd, &mut buf);
            if n <= 0 {
                return false;
            }
            let title = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();
            book.excluded_windows
                .iter()
                .any(|x| !x.trim().is_empty() && title.contains(&x.trim().to_lowercase()))
        }
    }

    // ---- the worker side -------------------------------------------------

    pub fn start_worker() {
        let (tx, rx) = crossbeam_channel::unbounded::<Fire>();
        if TX.set(tx).is_err() {
            return;
        }
        let _ = std::thread::Builder::new().name("expander".into()).spawn(move || {
            for f in rx {
                // The keystroke that triggered this was let through rather than
                // swallowed, so give the application a moment to draw it before
                // deleting it again.
                std::thread::sleep(std::time::Duration::from_millis(12));
                deliver(&f);
            }
        });
    }

    #[cfg(windows)]
    fn deliver(f: &Fire) {
        for _ in 0..f.backspaces {
            tap(0x08);
        }
        if f.action != Action::Text {
            run_action(f);
            return;
        }
        // Everything after the cursor marker has to be walked back over at the end.
        let mut after_cursor: Option<usize> = None;
        let plain: Option<&str> = match f.segments.as_slice() {
            [Segment::Text(t)] => Some(t.as_str()),
            _ => None,
        };
        if f.insert == Insert::Paste {
            if let Some(t) = plain {
                if paste(t) {
                    return;
                }
            }
        }
        for seg in &f.segments {
            match seg {
                Segment::Text(t) => {
                    for c in t.chars() {
                        match c {
                            // A carriage return is half of a Windows line ending and
                            // has nothing of its own to show.
                            '\r' => continue,
                            // Edit controls ignore U+000A and U+0009 arriving as
                            // unicode scan codes. A line break has to be a real
                            // Return, and a tab a real Tab.
                            '\n' => tap(0x0D),
                            '\t' => tap(0x09),
                            _ => unicode_char(c),
                        }
                        if let Some(n) = after_cursor.as_mut() {
                            *n += 1;
                        }
                    }
                }
                Segment::Key(vk) => tap(*vk),
                Segment::Cursor => after_cursor = Some(0),
            }
        }
        for _ in 0..after_cursor.unwrap_or(0) {
            tap(0x25); // VK_LEFT
        }
    }

    #[cfg(not(windows))]
    fn deliver(_f: &Fire) {}

    /// Carries out a command entry. On the worker thread, never in the hook.
    fn run_action(f: &Fire) {
        let Some(state) = super::GLOBAL_STATE.get() else {
            return;
        };
        match f.action {
            Action::StopAll => super::stop_everything(state),
            Action::RunProgram => super::run_program(f.payload.trim(), ""),
            Action::PlayMacro => {
                let path = f.payload.trim();
                if !path.is_empty() {
                    // A named file makes the abbreviation a launcher rather than a
                    // second Play button.
                    match super::load_macro(std::path::Path::new(path)) {
                        Ok(data) => *state.macro_data.lock() = data,
                        Err(e) => {
                            tracing::warn!("expander could not load {path}: {e}");
                            return;
                        }
                    }
                }
                super::start_playback(state);
            }
            Action::Text => {}
        }
    }

    #[cfg(windows)]
    fn tap(vk: u16) {
        use super::win32::*;
        // Virtual key alone is not enough. This project already learned that once -
        // the replay path sends scan codes because that is what games and a good many
        // controls actually read - and a synthetic Return with no scan code behind it
        // is exactly the kind of keystroke that arrives nowhere.
        let extended = matches!(
            vk,
            0x21..=0x28 // PageUp, PageDown, End, Home, arrows
                | 0x2D    // Insert
                | 0x2E    // Delete
                | 0x5B    // Left Win
                | 0x5C    // Right Win
                | 0xA3    // Right Ctrl
                | 0xA5 // Right Alt
        );
        unsafe {
            let scan = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16;
            for up in [false, true] {
                let mut input = INPUT { r#type: INPUT_KEYBOARD, ..Default::default() };
                input.Anonymous.ki.wVk = VIRTUAL_KEY(vk);
                input.Anonymous.ki.wScan = scan;
                let mut flags = KEYBD_EVENT_FLAGS(0);
                if up {
                    flags |= KEYEVENTF_KEYUP;
                }
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                input.Anonymous.ki.dwFlags = flags;
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
        }
    }

    #[cfg(windows)]
    fn unicode_char(c: char) {
        use super::win32::*;
        let mut units = [0u16; 2];
        for unit in c.encode_utf16(&mut units).iter() {
            unsafe {
                for up in [false, true] {
                    let mut input = INPUT { r#type: INPUT_KEYBOARD, ..Default::default() };
                    input.Anonymous.ki.wScan = *unit;
                    input.Anonymous.ki.dwFlags = if up {
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                    } else {
                        KEYEVENTF_UNICODE
                    };
                    SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                }
            }
        }
    }

    /// Borrows the clipboard, pastes, and gives it back. Returns false when the
    /// clipboard would not cooperate, so the caller can type the text instead.
    #[cfg(windows)]
    fn paste(text: &str) -> bool {
        use super::win32::*;
        let saved = clipboard_text();
        // Windows edit controls want CRLF, and normalising first keeps text that
        // already had it from gaining a second carriage return.
        let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
        unsafe {
            if !set_clipboard_text(&text) {
                return false;
            }
            let ctrl = VIRTUAL_KEY(0x11);
            let v = VIRTUAL_KEY(0x56);
            for (key, up) in [(ctrl, false), (v, false), (v, true), (ctrl, true)] {
                let mut input = INPUT { r#type: INPUT_KEYBOARD, ..Default::default() };
                input.Anonymous.ki.wVk = key;
                input.Anonymous.ki.dwFlags =
                    if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
            // Long enough for the target to have taken the data before it is replaced.
            std::thread::sleep(std::time::Duration::from_millis(120));
            if !saved.is_empty() {
                set_clipboard_text(&saved);
            }
        }
        true
    }

    #[cfg(windows)]
    fn set_clipboard_text(text: &str) -> bool {
        super::platform::set_clipboard_text(text)
    }
}

// ============================================================================
// Self-test instrumentation
// ============================================================================

/// Scaffolding for `--selftest`.
///
/// The scheduler cannot be judged from outside the process: an event that fires
/// 40 ms late looks exactly like one that fires on time. So a self-test run is made
/// *dry* - every call into Windows is suppressed and nothing moves on screen - while
/// the real scheduler, the real frame guard and the real slip logic run untouched,
/// and each dispatch is timestamped against the moment it was due.
///
/// Cost when idle: one relaxed load, on paths that were about to enter a syscall
/// anyway.
mod selftest {
    use parking_lot::Mutex;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};

    static DRY: AtomicBool = AtomicBool::new(false);
    /// Tracing is separate from dryness: the churn test runs dry for ten minutes and
    /// would otherwise fill memory with a trace nobody reads.
    static TRACING: AtomicBool = AtomicBool::new(false);
    /// Net synthetic presses outstanding. Has to come back to zero after a stop.
    static HELD: AtomicI64 = AtomicI64::new(0);
    /// Playback loops currently running, and the most that ever ran at once.
    static LIVE: AtomicI64 = AtomicI64::new(0);
    static PEAK_LIVE: AtomicI64 = AtomicI64::new(0);
    static STALL_AT: AtomicUsize = AtomicUsize::new(usize::MAX);
    static STALL_US: AtomicU64 = AtomicU64::new(0);
    static SLIPS: AtomicU64 = AtomicU64::new(0);
    static SLIPPED_US: AtomicU64 = AtomicU64::new(0);
    /// (scheduled, actual), both on the playback clock.
    static TRACE: LazyLock<Mutex<Vec<(u64, u64)>>> = LazyLock::new(|| Mutex::new(Vec::new()));

    pub fn dry() -> bool {
        DRY.load(Ordering::Relaxed)
    }

    /// Arms a run. `stall_at` is the event index to freeze the thread on, which is
    /// how the anti-burst logic gets something to prove itself against.
    pub fn arm(expected: usize, stall_at: usize, stall_us: u64) {
        let mut t = TRACE.lock();
        t.clear();
        t.reserve(expected);
        drop(t);
        STALL_AT.store(stall_at, Ordering::Relaxed);
        STALL_US.store(stall_us, Ordering::Relaxed);
        SLIPS.store(0, Ordering::Relaxed);
        SLIPPED_US.store(0, Ordering::Relaxed);
        TRACING.store(true, Ordering::Relaxed);
        DRY.store(true, Ordering::Relaxed);
    }

    /// Suppresses the OS calls without collecting a trace.
    pub fn arm_dry() {
        HELD.store(0, Ordering::Relaxed);
        LIVE.store(0, Ordering::Relaxed);
        PEAK_LIVE.store(0, Ordering::Relaxed);
        DRY.store(true, Ordering::Relaxed);
    }

    pub fn held() -> i64 {
        HELD.load(Ordering::Relaxed)
    }
    pub fn live() -> i64 {
        LIVE.load(Ordering::Relaxed)
    }
    pub fn peak_live() -> i64 {
        PEAK_LIVE.load(Ordering::Relaxed)
    }

    /// Counts a press or a release on its way out.
    ///
    /// A stop that leaves this above zero is the failure this application can least
    /// afford: a key still down after the macro has finished.
    pub fn note_input(kind: &crate::InputEventKind) {
        if !dry() {
            return;
        }
        let down = match kind {
            crate::InputEventKind::Key { down, .. }
            | crate::InputEventKind::MouseButton { down, .. } => *down,
            _ => return,
        };
        if down {
            HELD.fetch_add(1, Ordering::Relaxed);
        } else {
            HELD.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Counts a playback loop for as long as it runs, however it leaves.
    pub struct LoopGuard;

    impl Drop for LoopGuard {
        fn drop(&mut self) {
            LIVE.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn enter_playback() -> LoopGuard {
        HELD.store(0, Ordering::Relaxed);
        let n = LIVE.fetch_add(1, Ordering::Relaxed) + 1;
        PEAK_LIVE.fetch_max(n, Ordering::Relaxed);
        LoopGuard
    }

    /// Ends a run and hands back what it collected: trace, slip count, slipped time.
    pub fn disarm() -> (Vec<(u64, u64)>, u64, u64) {
        DRY.store(false, Ordering::Relaxed);
        TRACING.store(false, Ordering::Relaxed);
        STALL_AT.store(usize::MAX, Ordering::Relaxed);
        let trace = std::mem::take(&mut *TRACE.lock());
        (trace, SLIPS.load(Ordering::Relaxed), SLIPPED_US.load(Ordering::Relaxed))
    }

    /// Records one dispatch, then freezes if this is the armed index.
    ///
    /// The freeze happens after the record, so the stall shows up as lateness on the
    /// events that follow - which is exactly where the scheduler has to deal with it.
    pub fn note(index: usize, scheduled_us: u64, actual_us: u64) {
        if !TRACING.load(Ordering::Relaxed) {
            return;
        }
        TRACE.lock().push((scheduled_us, actual_us));
        if index == STALL_AT.load(Ordering::Relaxed) {
            let us = STALL_US.load(Ordering::Relaxed);
            if us > 0 {
                std::thread::sleep(std::time::Duration::from_micros(us));
            }
        }
    }

    pub fn note_slip(late_us: u64) {
        SLIPS.fetch_add(1, Ordering::Relaxed);
        SLIPPED_US.fetch_add(late_us, Ordering::Relaxed);
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
    /// Hold presses long enough for a game running at a low frame rate to see them.
    pub frame_guard: bool,
    /// The slowest frame rate the target is expected to drop to. The fallback for
    /// when nothing has been measured yet.
    pub frame_guard_fps: u64,
    /// Size the guard from the measured window latency instead of the figure above.
    pub frame_guard_auto: bool,
    /// Keep probing the target window and show the numbers.
    pub perf_enabled: bool,

    // recording
    pub capture_mouse_moves: bool,
    /// Desktop Duplication rather than GDI for every screen grab. On by default;
    /// it falls back on its own where it cannot run, so the switch is here for the
    /// machine where it runs badly rather than not at all.
    #[serde(default = "yes")]
    pub fast_capture: bool,
    /// Keep a small square of the screen around every click while recording, so the
    /// recording can be turned into steps that look for the button.
    #[serde(default)]
    pub record_click_shots: bool,
    /// The side of that square, in pixels.
    #[serde(default = "shot_size_default")]
    pub click_shot_size: u32,
    /// What the generated `Click image` steps do when the picture is not there.
    /// Not `Continue`: a step this program wrote, that cannot find the button it
    /// was cut from, has nothing useful to do next.
    #[serde(default = "shot_miss_default")]
    pub click_shot_miss: OnMiss,
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
    /// Draw a see-through window over everything showing what the script just
    /// looked at. Off by default: it is a diagnostic, not a feature to leave on.
    #[serde(default)]
    pub debug_overlay: bool,
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
            // Off by default: most macros drive ordinary desktop software, which
            // reads its input queue as fast as the queue fills.
            frame_guard: false,
            frame_guard_fps: 30,
            frame_guard_auto: true,
            perf_enabled: false,

            capture_mouse_moves: true,
            fast_capture: true,
            record_click_shots: false,
            click_shot_size: 64,
            click_shot_miss: OnMiss::Stop,
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
            debug_overlay: false,
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
        self.frame_guard_fps = self.frame_guard_fps.clamp(5, 240);
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

/// A square of screen kept from around one recorded click.
///
/// The missing half of recording. A recording is a list of coordinates, and
/// coordinates are the brittle part: the window moves, the resolution changes, the
/// list opens one row lower, and every click lands somewhere it should not. The
/// cure has been in the program since 1.2 - a `Click image` step finds the button
/// wherever it is - but it had to be built by hand afterwards, from screenshots
/// taken separately, by somebody who remembered which button each click was for.
///
/// The one moment that information exists for free is the moment of the click. So
/// it is taken then, and the offer to use it is made when the recording stops.
#[derive(Clone)]
pub struct ClickShot {
    /// Where this click landed in `events`.
    pub index: usize,
    pub button: MouseButton,
    /// Screen coordinates of the click itself.
    pub x: i32,
    pub y: i32,
    /// Top-left of the square, so the click's offset inside it is recoverable.
    pub left: i32,
    pub top: i32,
    pub w: u32,
    pub h: u32,
    /// RGBA, opaque - ready to be a PNG without further thought.
    pub rgba: Vec<u8>,
    /// The scale the square was cut at, for the sidecar.
    pub dpi: u32,
}

/// A recording of a hundred clicks holds 100 x 64 x 64 x 4 = 1.6 MB of squares.
/// Ten times that is still small, and past it the offer stops being useful anyway.
const MAX_CLICK_SHOTS: usize = 1000;

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
    /// Text containment, the one comparison that only makes sense once a variable
    /// can hold text. Forgiving in the same way screen-text matching is.
    Has,
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
            // Numbers do not contain one another. Whoever asked for this meant the
            // text, so the text is what answers.
            Cmp::Has => Value::Num(a).as_text().contains(&Value::Num(b).as_text()),
        }
    }

    /// The comparison a script actually asks for, once a variable can hold text.
    ///
    /// Two things that both read as numbers are compared as numbers, whichever kind
    /// they are stored as: a count read off the screen into text and then compared
    /// against 10 has to work. Anything else is compared as text, trimmed and with
    /// case ignored, because screen text is never exactly what a human reads.
    fn test_values(self, a: &Value, b: &Value) -> bool {
        if self == Cmp::Has {
            let (x, y) = (a.as_text(), b.as_text());
            return crate::ocr::text_matches(&x, &y);
        }
        if let (Some(x), Some(y)) = (a.numeric(), b.numeric()) {
            return self.test(x, y);
        }
        let (x, y) = (a.as_text(), b.as_text());
        let (x, y) = (x.trim().to_lowercase(), y.trim().to_lowercase());
        match self {
            Cmp::Eq => x == y,
            Cmp::Ne => x != y,
            Cmp::Lt => x < y,
            Cmp::Le => x <= y,
            Cmp::Gt => x > y,
            Cmp::Ge => x >= y,
            Cmp::Has => x.contains(&y),
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
            Cmp::Has => "has",
        }
    }
    const ALL: [Cmp; 7] =
        [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge, Cmp::Has];
}

/// What a script variable holds.
///
/// Numbers only, until now. That made the three most useful things on a screen -
/// what the text recognition read, what the window is called, what is on the
/// clipboard - impossible to keep hold of. One more kind covers all three. It stops
/// there deliberately: lists, tables and functions would turn the step list into a
/// programming language, and a programming language cannot be edited with a mouse.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Num(f64),
    Str(String),
}

impl Default for Value {
    fn default() -> Self {
        Value::Num(0.0)
    }
}

impl Value {
    /// The number this holds, if it holds one. Text that reads as a number counts:
    /// a variable filled from the screen should not need converting before it can
    /// be compared against a count.
    pub fn numeric(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            Value::Str(s) => {
                let t = s.trim();
                if t.is_empty() { None } else { t.parse::<f64>().ok() }
            }
        }
    }

    /// The number, with anything unreadable counting as zero. What every step that
    /// wants a coordinate or a count uses.
    pub fn as_num(&self) -> f64 {
        self.numeric().unwrap_or(0.0)
    }

    pub fn as_text(&self) -> String {
        match self {
            // Whole numbers print without a tail: a count that shows as `7.0` in a
            // log or a clipboard is noise.
            Value::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{n:.0}")
                } else {
                    format!("{n}")
                }
            }
            Value::Str(s) => s.clone(),
        }
    }

    pub fn is_text(&self) -> bool {
        matches!(self, Value::Str(_))
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_text())
    }
}

/// Replaces `{name}` with what the variable holds.
///
/// `{{` is a literal brace, and a name nobody set is left exactly as written rather
/// than becoming an empty string - a step whose text silently loses a word is much
/// harder to diagnose than one that shows the placeholder it could not fill.
fn expand_vars(text: &str, vars: &std::collections::HashMap<String, Value>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        // Both braces, because a doubled one of either kind is a literal, the same
        // rule Rust's own format strings use.
        let Some(at) = rest.find(['{', '}']) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if rest.starts_with("{{") || rest.starts_with("}}") {
            out.push_str(&rest[..1]);
            rest = &rest[2..];
            continue;
        }
        if rest.starts_with('}') {
            // A closing brace with nothing open is just a brace.
            out.push('}');
            rest = &rest[1..];
            continue;
        }
        match rest.find('}') {
            Some(end) => {
                let name = &rest[1..end];
                match vars.get(name) {
                    Some(v) => out.push_str(&v.as_text()),
                    None => out.push_str(&rest[..=end]),
                }
                rest = &rest[end + 1..];
            }
            // An unclosed brace is text, not an error.
            None => {
                out.push_str(rest);
                return out;
            }
        }
    }
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

    /// The same, once either side can be text.
    ///
    /// Adding to text joins it, which is how a message is built up a piece at a
    /// time. Taking away from it or multiplying it is meaningless, so those read
    /// both sides as numbers and answer with a number.
    fn apply_values(self, cur: &Value, v: &Value) -> Value {
        if self == VarOp::Set {
            return v.clone();
        }
        if self == VarOp::Add && (cur.is_text() || v.is_text()) {
            let joined = match (cur.numeric(), v.numeric()) {
                // Two numbers written as text are still two numbers.
                (Some(a), Some(b)) => return Value::Num(a + b),
                _ => format!("{}{}", cur.as_text(), v.as_text()),
            };
            return Value::Str(joined);
        }
        Value::Num(self.apply(cur.as_num(), v.as_num()))
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

/// Is the picture there, given what we decided last time?
///
/// One threshold makes a wobbling score flip between found and lost several times a
/// second: 79, 81, 79, 82 around a threshold of 80 reads as four state changes and a
/// script that acts on each of them. Two thresholds - one to appear, a lower one to
/// disappear - turn that into one state change, which is what a Schmitt trigger is for
/// and what every noisy sensor since has needed.
fn match_decision(score: f64, appear_at: f64, lose_at: f64, was_present: bool) -> bool {
    // A `lose_at` of zero, or one that is not actually lower, means the caller wants
    // the old all-or-nothing behaviour.
    let lo = if lose_at > 0.0 && lose_at < appear_at { lose_at } else { appear_at };
    if was_present { score >= lo } else { score >= appear_at }
}

/// Folds one observation into a rolling history and answers "N of the last M".
///
/// A single frame is not evidence. A score of 83, then 51, then 74 is noise finding
/// something briefly plausible; 82, 84, 83 is an object. `within` is capped at 32
/// because the history is a bitmask, which is enough for several seconds of looking.
fn stable_enough(history: &mut u32, raw: bool, of: u32, within: u32) -> bool {
    let m = within.clamp(1, 32);
    *history = (*history << 1) | u32::from(raw);
    if of <= 1 && m <= 1 {
        return raw;
    }
    let mask = if m >= 32 { u32::MAX } else { (1u32 << m) - 1 };
    (*history & mask).count_ones() >= of.clamp(1, m)
}

/// Where a piece of text comes from.
///
/// The three that are not a file are the reason variables learnt to hold text at
/// all: what the window is called, what is running, and what was last copied.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub enum TextSource {
    #[default]
    Clipboard,
    /// Title of the window in front.
    WindowTitle,
    /// Executable name of the window in front, such as `RobloxPlayerBeta.exe`.
    ProcessName,
    /// A file, read as text. Capped, because a script should not be able to pull a
    /// gigabyte into a variable by naming the wrong path.
    File(String),
}

impl TextSource {
    pub fn index(&self) -> usize {
        match self {
            TextSource::Clipboard => 0,
            TextSource::WindowTitle => 1,
            TextSource::ProcessName => 2,
            TextSource::File(_) => 3,
        }
    }
    pub fn from_index(i: usize) -> Self {
        match i {
            1 => TextSource::WindowTitle,
            2 => TextSource::ProcessName,
            3 => TextSource::File(String::new()),
            _ => TextSource::Clipboard,
        }
    }
}

/// Where a piece of text goes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub enum TextSink {
    #[default]
    Clipboard,
    File {
        path: String,
        /// Add to the end rather than replace. A log wants this; a hand-off file
        /// does not.
        append: bool,
    },
}

impl TextSink {
    pub fn index(&self) -> usize {
        match self {
            TextSink::Clipboard => 0,
            TextSink::File { .. } => 1,
        }
    }
    pub fn from_index(i: usize) -> Self {
        match i {
            1 => TextSink::File { path: String::new(), append: false },
            _ => TextSink::Clipboard,
        }
    }
}

/// Most a file read into a variable is allowed to be.
const TEXT_FILE_CAP: u64 = 1 << 20;

/// Serde default for a flag that is on unless a file says otherwise.
fn yes_bool() -> bool {
    true
}

/// What the debug overlay draws, written by whatever last looked at the screen.
///
/// A global rather than something threaded through the engine: the looking happens
/// on the playback thread and the drawing on the UI thread, and the only thing the
/// two need to agree about is a few rectangles.
#[derive(Clone, Debug, Default)]
pub struct Sighting {
    /// Where the search was allowed to look.
    pub area: Option<(i32, i32, i32, i32)>,
    /// Where it found something, and how sure it was.
    pub hit: Option<(i32, i32, i32, i32, f32)>,
    /// The rectangle text was last read from.
    pub text: Option<(i32, i32, i32, i32)>,
    /// The element UI Automation last returned.
    pub element: Option<(i32, i32, i32, i32)>,
    /// One line: what was looked for and what came back.
    pub note: String,
    /// Bumped on every write. The overlay redraws when this changes and not
    /// otherwise: a layered window repainted ten times a second for no reason is a
    /// visible flicker and a pointless slice of a core.
    pub seq: u64,
}

static SIGHTING: Mutex<Sighting> = Mutex::new(Sighting {
    area: None,
    hit: None,
    text: None,
    element: None,
    note: String::new(),
    seq: 0,
});

/// What the variables window shows, written by the interpreter before each step.
///
/// The other half of the overlay. The overlay says where the script is looking;
/// this says what it has found out so far, which is the half you need when the
/// script is looking in the right place and still doing the wrong thing.
#[derive(Clone, Debug, Default)]
pub struct ScriptView {
    /// Every variable, sorted, already rendered to text. Sorted here rather than in
    /// the window so the rows do not dance about between frames.
    pub vars: Vec<(String, String)>,
    /// Which step is about to run, and how it reads.
    pub pc: usize,
    pub step: String,
    /// How many `Call` steps deep this is.
    pub depth: u32,
    /// True while the interpreter is parked waiting for "next step".
    pub waiting: bool,
    pub running: bool,
    pub seq: u64,
}

static SCRIPT_VIEW: Mutex<Option<ScriptView>> = Mutex::new(None);

/// Off unless the variables window is open. A run nobody is watching pays one
/// relaxed load per step for this.
static WATCHING_VARS: AtomicBool = AtomicBool::new(false);

pub fn watching_vars() -> bool {
    WATCHING_VARS.load(Ordering::Relaxed)
}

pub fn set_watching_vars(on: bool) {
    WATCHING_VARS.store(on, Ordering::Relaxed);
    if !on {
        *SCRIPT_VIEW.lock() = None;
    }
}

pub fn script_view() -> Option<ScriptView> {
    SCRIPT_VIEW.lock().clone()
}

fn note_script_view(f: impl FnOnce(&mut ScriptView)) {
    if !watching_vars() {
        return;
    }
    let mut slot = SCRIPT_VIEW.lock();
    let v = slot.get_or_insert_with(ScriptView::default);
    f(v);
    v.seq = v.seq.wrapping_add(1);
}

/// Off unless the overlay window is open. Every write below is behind this, so a
/// script that is not being watched pays one relaxed load per look.
static WATCHING: AtomicBool = AtomicBool::new(false);

pub fn watching() -> bool {
    WATCHING.load(Ordering::Relaxed)
}

/// Records what was just looked at, if anybody is watching.
fn note_sighting(f: impl FnOnce(&mut Sighting)) {
    if !watching() {
        return;
    }
    let mut s = SIGHTING.lock();
    f(&mut s);
    s.seq = s.seq.wrapping_add(1);
}

/// Something the script can ask about the screen or about itself.
/// Where on the screen an image step is allowed to look.
///
/// Measured on a 2560x1440 desktop, one full-screen step costs 111 ms - 43 to copy the
/// screen and 68 to sweep it - which caps a polling loop at about nine looks a second.
/// A few hundred pixels square costs closer to twelve. Nothing else in this release
/// buys as much.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub enum SearchArea {
    /// Everything, across every monitor. Correct, and the slowest thing here.
    #[default]
    FullScreen,
    /// Whatever window is in front right now.
    ActiveWindow,
    Rect { x: i32, y: i32, w: i32, h: i32 },
    /// Around where this same template was last seen, which for anything that stays
    /// put is a few hundred pixels rather than four million.
    NearLast { margin: i32 },
    /// Find another picture first, then look in a rectangle placed relative to
    /// where that one landed.
    ///
    /// This is what a threshold cannot do. A row of identical buttons is identical;
    /// which one to press is decided by the heading above it, and an anchor is how
    /// a script says so. Two searches instead of one, and usually faster anyway,
    /// because the second is confined to a few hundred pixels.
    NearAnchor {
        anchor: String,
        /// Where the rectangle sits relative to the centre of the anchor.
        dx: i32,
        dy: i32,
        w: i32,
        h: i32,
    },
}

/// What a step that did not find what it was looking for should do about it.
///
/// Until 1.5.0 there was only the first of these, and it was not a choice anybody
/// had made - a `Click image` whose picture was not on screen did nothing at all
/// and the script walked on to the next step. That is right for a poll inside a
/// `While` and wrong for everything else, and the difference between the two is a
/// macro that stops when the game logs you out and one that clicks at an empty
/// desktop until morning.
///
/// `Continue` stays the default so that every macro written before this release
/// behaves exactly as it did.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum OnMiss {
    /// Walk on to the next step. What every version before 1.5.0 did, always.
    #[default]
    Continue,
    /// End the run. The whole point of the field: a night macro that has lost its
    /// footing should stop, not keep going.
    Stop,
    /// Leave the innermost `While`, exactly as a `Break` step would. For a loop
    /// that is looking for one of several things.
    Break,
    /// Look again, up to `times` more times, `delay_ms` apart - and stop the run if
    /// it is still not there.
    ///
    /// The stop at the end is deliberate and is the reason this is not spelled
    /// "retry, then carry on". A retry that gives up quietly is the trap this whole
    /// enum exists to close; somebody who wants that can set `Continue` and put the
    /// step in a loop, which says so.
    Retry { times: u32, delay_ms: u64 },
}

impl OnMiss {
    const COUNT: usize = 4;

    fn index(&self) -> usize {
        match self {
            OnMiss::Continue => 0,
            OnMiss::Stop => 1,
            OnMiss::Break => 2,
            OnMiss::Retry { .. } => 3,
        }
    }

    fn from_index(i: usize) -> Self {
        match i {
            1 => OnMiss::Stop,
            2 => OnMiss::Break,
            3 => OnMiss::Retry { times: 3, delay_ms: 500 },
            _ => OnMiss::Continue,
        }
    }

    fn name(&self, s: &Strings) -> &'static str {
        match self {
            OnMiss::Continue => s.m_continue,
            OnMiss::Stop => s.m_stop,
            OnMiss::Break => s.m_break,
            OnMiss::Retry { .. } => s.m_retry,
        }
    }

    /// How many extra looks this policy asks for.
    fn retries(&self) -> (u32, u64) {
        match self {
            OnMiss::Retry { times, delay_ms } => (*times, *delay_ms),
            _ => (0, 0),
        }
    }
}

/// A step that looks for something always carries one of these, and it is always
/// `Continue` unless somebody said otherwise.
fn miss_default() -> OnMiss {
    OnMiss::Continue
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Condition {
    Always,
    Var { name: String, cmp: Cmp, value: Value },
    /// A template from the `templates/` folder is on screen.
    Image {
        template: String,
        threshold: f64,
        #[serde(default)]
        area: SearchArea,
        /// Confidence at which a picture already found counts as gone. Zero, or
        /// anything not below `threshold`, keeps the single-threshold behaviour.
        #[serde(default)]
        lose_at: f64,
        /// Require the match in `stable_of` of the last `stable_in` looks. Both at
        /// zero or one means every look decides on its own.
        #[serde(default)]
        stable_of: u32,
        #[serde(default)]
        stable_in: u32,
        /// Match outlines instead of greys. Survives a theme change and a
        /// highlight; costs one extra pass over each plane.
        #[serde(default)]
        edge: bool,
    },
    Pixel { x: i32, y: i32, r: u8, g: u8, b: u8, tol: u32 },
    Window { title: String },
    /// An interface element Windows knows about is there.
    ///
    /// The first rung of the cascade: ask the application what is on screen before
    /// resorting to looking at the pixels. Silent in anything that draws its own
    /// interface, which includes every game engine, so a script that has to work
    /// there falls through to the picture search below.
    Element { query: uia::Query },
    /// A process with this name is running. The name is matched without its path
    /// and without case, so `roblox` finds `RobloxPlayerBeta.exe`.
    Process { name: String },
    /// Text recognised inside a screen rectangle contains `needle`.
    Text {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        needle: String,
        /// What is done to the pixels first. Absent in every file written before
        /// 1.4.0, which is exactly the old behaviour.
        #[serde(default)]
        prep: ocr::Prep,
    },
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
            Condition::Process { .. } => 6,
            Condition::Element { .. } => 7,
        }
    }
    fn from_index(i: usize) -> Self {
        match i {
            1 => Condition::Var {
                name: "count".into(),
                cmp: Cmp::Lt,
                value: Value::Num(10.0),
            },
            2 => Condition::Image {
                template: String::new(),
                threshold: 0.85,
                area: SearchArea::default(),
                lose_at: 0.0,
                stable_of: 0,
                stable_in: 0,
                edge: false,
            },
            3 => Condition::Pixel { x: 0, y: 0, r: 255, g: 0, b: 0, tol: 20 },
            4 => Condition::Window { title: String::new() },
            5 => Condition::Text {
                x: 0,
                y: 0,
                w: 400,
                h: 120,
                needle: String::new(),
                prep: ocr::Prep::default(),
            },
            6 => Condition::Process { name: String::new() },
            7 => Condition::Element { query: uia::Query::default() },
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
    WaitFor {
        cond: Condition,
        appear: bool,
        timeout_ms: u64,
        /// What a timeout means. A wait that gives up used to be indistinguishable
        /// from a wait that succeeded, which is the quietest failure in the program.
        #[serde(default = "miss_default")]
        miss: OnMiss,
    },
    ClickImage {
        template: String,
        threshold: f64,
        button: MouseButton,
        #[serde(default)]
        area: SearchArea,
        #[serde(default)]
        edge: bool,
        #[serde(default = "miss_default")]
        miss: OnMiss,
    },
    /// Looks for a picture and writes what it found into variables, without clicking.
    FindImage {
        template: String,
        threshold: f64,
        #[serde(default)]
        area: SearchArea,
        /// Prefix for the results: `<var>.found`, `.x`, `.y`, `.w`, `.h`, `.score`.
        var: String,
        #[serde(default)]
        edge: bool,
        #[serde(default = "miss_default")]
        miss: OnMiss,
    },
    Click { x: i32, y: i32, button: MouseButton },
    Key { vk: u16, down: bool },
    SetVar { name: String, op: VarOp, value: Value },
    If { cond: Condition },
    Else,
    EndIf,
    While { cond: Condition },
    EndWhile,
    Break,
    Run { path: String, args: String },
    Exit,
    Log { text: String },
    /// Finds an interface element and writes what it found into variables.
    FindElement {
        query: uia::Query,
        /// Prefix for the results: `<var>` itself is the text, and `<var>.found`,
        /// `.x`, `.y`, `.w`, `.h`, `.name` carry the rest.
        var: String,
        /// How long to keep looking. An interface that is still drawing itself is
        /// the normal case just after a click.
        #[serde(default)]
        timeout_ms: u64,
        #[serde(default = "miss_default")]
        miss: OnMiss,
    },
    /// Presses an interface element.
    ClickElement {
        query: uia::Query,
        button: MouseButton,
        /// Ask the application to press it rather than clicking at it. Nothing
        /// moves on screen, the window need not even be in front, and a control
        /// that has shifted since it was found is still the one that is pressed.
        #[serde(default = "yes_bool")]
        invoke: bool,
        #[serde(default)]
        timeout_ms: u64,
        #[serde(default = "miss_default")]
        miss: OnMiss,
    },
    /// Recognises a screen rectangle and stores what it says, as text.
    ReadText {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        var: String,
        #[serde(default)]
        prep: ocr::Prep,
    },
    /// Puts the clipboard, a window title, a process name or a file into a variable.
    GetText { source: TextSource, var: String },
    /// Sends text - with `{name}` filled in from the variables - to the clipboard or
    /// to a file.
    PutText { sink: TextSink, text: String },
    /// Runs another macro file's script here, then carries on.
    ///
    /// The reuse that people ask for when they ask for functions, without becoming
    /// a language. A subroutine is an ordinary macro: it is edited in this same
    /// editor, played on its own to test it, and shared between projects as a file.
    /// There is no parameter list and no return value - the variables are the same
    /// ones, so a caller sets `target` before the call and reads `result` after it,
    /// which is what a list of steps can express without growing a grammar.
    ///
    /// Nesting is capped at `MAX_CALL_DEPTH`, which is what keeps a file that calls
    /// itself from being a stack overflow.
    Call {
        /// A file name, resolved next to the macro that named it and then in the
        /// data folder. `.json` is added when no extension is given.
        path: String,
        /// What a file that will not load means. The same four choices as a search
        /// that found nothing, for the same reason.
        #[serde(default = "miss_default")]
        miss: OnMiss,
    },
    /// Recognises a screen rectangle and stores the number it finds.
    ReadNumber {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        var: String,
        #[serde(default)]
        prep: ocr::Prep,
        /// What the reading is supposed to look like. Wrong-shaped readings are
        /// refused rather than turned into a number nobody asked for, and with
        /// `Auto` this is also what picks the profile.
        #[serde(default)]
        expect: ocr::Expect,
    },
}

impl StepKind {
    /// Order used by the "Add" menu and the kind picker.
    const COUNT: usize = 24;

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
            StepKind::FindImage { .. } => 17,
            StepKind::ReadText { .. } => 18,
            StepKind::GetText { .. } => 19,
            StepKind::PutText { .. } => 20,
            StepKind::FindElement { .. } => 21,
            StepKind::ClickElement { .. } => 22,
            StepKind::Call { .. } => 23,
        }
    }

    fn from_index(i: usize) -> Self {
        match i {
            1 => StepKind::Wait { ms: 1000 },
            2 => StepKind::WaitFor {
                cond: Condition::Image {
                    template: String::new(),
                    threshold: 0.85,
                    area: SearchArea::default(),
                    lose_at: 0.0,
                    stable_of: 0,
                    stable_in: 0,
                    edge: false,
                },
                appear: true,
                timeout_ms: 10_000,
                miss: OnMiss::Continue,
            },
            3 => StepKind::ClickImage {
                template: String::new(),
                threshold: 0.85,
                button: MouseButton::Left,
                area: SearchArea::default(),
                edge: false,
                miss: OnMiss::Continue,
            },
            4 => StepKind::Click { x: 0, y: 0, button: MouseButton::Left },
            5 => StepKind::Key { vk: 0x20, down: true },
            6 => StepKind::SetVar {
                name: "count".into(),
                op: VarOp::Add,
                value: Value::Num(1.0),
            },
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
                prep: ocr::Prep::default(),
                expect: ocr::Expect::default(),
            },
            17 => StepKind::FindImage {
                template: String::new(),
                threshold: 0.85,
                area: SearchArea::default(),
                var: "target".into(),
                edge: false,
                miss: OnMiss::Continue,
            },
            18 => StepKind::ReadText {
                x: 0,
                y: 0,
                w: 400,
                h: 120,
                var: "line".into(),
                prep: ocr::Prep::default(),
            },
            19 => StepKind::GetText {
                source: TextSource::default(),
                var: "text".into(),
            },
            20 => StepKind::PutText {
                sink: TextSink::default(),
                text: "{text}".into(),
            },
            21 => StepKind::FindElement {
                query: uia::Query::default(),
                var: "elem".into(),
                timeout_ms: 0,
                miss: OnMiss::Continue,
            },
            22 => StepKind::ClickElement {
                query: uia::Query::default(),
                button: MouseButton::Left,
                invoke: true,
                timeout_ms: 2000,
                miss: OnMiss::Continue,
            },
            23 => StepKind::Call { path: String::new(), miss: OnMiss::Stop },
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
            StepKind::FindImage { .. } => s.k_findimg,
            StepKind::ReadText { .. } => s.k_readtext,
            StepKind::GetText { .. } => s.k_gettext,
            StepKind::PutText { .. } => s.k_puttext,
            StepKind::FindElement { .. } => s.k_findelem,
            StepKind::ClickElement { .. } => s.k_clickelem,
            StepKind::Call { .. } => s.k_call,
        }
    }

    /// The policy this step carries, if it is one that can miss.
    fn miss(&self) -> OnMiss {
        match self {
            StepKind::WaitFor { miss, .. }
            | StepKind::ClickImage { miss, .. }
            | StepKind::FindImage { miss, .. }
            | StepKind::FindElement { miss, .. }
            | StepKind::ClickElement { miss, .. }
            | StepKind::Call { miss, .. } => *miss,
            _ => OnMiss::Continue,
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
        Condition::Image { template, threshold, .. } => {
            format!("{}: {template} ≥ {threshold:.2}", s.c_image)
        }
        Condition::Pixel { x, y, r, g, b, tol } => {
            format!("{}: ({x},{y}) = {r},{g},{b} ±{tol}", s.c_pixel)
        }
        Condition::Window { title } => format!("{}: {title}", s.c_window),
        Condition::Text { x, y, w, h, needle, .. } => {
            format!("{}: \"{needle}\" @ ({x},{y} {w}x{h})", s.c_text)
        }
        Condition::Process { name } => format!("{}: {name}", s.c_process),
        Condition::Element { query } => format!("{}: {}", s.c_element, describe_query(query)),
    }
}

/// The tail a step's line gets when it will do something other than walk on.
///
/// Only shown when it is not the default: a list where every line ends in "carry
/// on" says nothing, and a list where one line ends in "stop the script" says
/// exactly where the run can end.
fn describe_miss(kind: &StepKind, s: &Strings) -> String {
    match kind.miss() {
        OnMiss::Continue => String::new(),
        OnMiss::Retry { times, delay_ms } => {
            format!("  ⟲ {times}×{delay_ms}ms → {}", s.m_stop)
        }
        other => format!("  ⚠ {}", other.name(s)),
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
        StepKind::WaitFor { cond, appear, timeout_ms, .. } => format!(
            "{name} {} {} ({timeout_ms} ms){}",
            describe_condition(cond, s),
            if *appear { s.f_appear } else { s.f_gone },
            describe_miss(&step.kind, s)
        ),
        StepKind::FindImage { template, threshold, var, .. } => {
            format!("{name} {template} ≥ {threshold:.2} → {var}{}", describe_miss(&step.kind, s))
        }
        StepKind::ClickImage { template, threshold, .. } => {
            format!("{name}: {template} ≥ {threshold:.2}{}", describe_miss(&step.kind, s))
        }
        StepKind::Call { path, .. } => {
            format!("{name} {path}{}", describe_miss(&step.kind, s))
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
        StepKind::ReadNumber { x, y, w, h, var, .. } => {
            format!("{name} ({x},{y} {w}x{h}) → {var}")
        }
        StepKind::ReadText { x, y, w, h, var, .. } => {
            format!("{name} ({x},{y} {w}x{h}) → {var}")
        }
        StepKind::FindElement { query, var, .. } => {
            format!("{name}: {} → {var}{}", describe_query(query), describe_miss(&step.kind, s))
        }
        StepKind::ClickElement { query, invoke, .. } => {
            format!(
                "{name}: {}{}{}",
                describe_query(query),
                if *invoke { " ⚡" } else { "" },
                describe_miss(&step.kind, s)
            )
        }
        StepKind::GetText { source, var } => {
            let from = match source {
                TextSource::Clipboard => s.t_clipboard.to_string(),
                TextSource::WindowTitle => s.t_wintitle.to_string(),
                TextSource::ProcessName => s.t_process.to_string(),
                TextSource::File(p) => p.clone(),
            };
            format!("{name}: {from} → {var}")
        }
        StepKind::PutText { sink, text } => {
            let to = match sink {
                TextSink::Clipboard => s.t_clipboard.to_string(),
                TextSink::File { path, append } => {
                    format!("{path}{}", if *append { " +" } else { "" })
                }
            };
            format!("{name}: \"{text}\" → {to}")
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

/// Macro container, format version 3.
///
/// v1 files were a bare `[MacroEvent, ...]` array; v2 had no `script` or `vars`.
/// Both are still accepted on load, and both come back out as version 3.
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
    pub vars: std::collections::BTreeMap<String, Value>,
}

fn format_version() -> u32 {
    3
}

fn shot_size_default() -> u32 {
    64
}

fn shot_miss_default() -> OnMiss {
    OnMiss::Stop
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

/// The most a compressed macro is allowed to expand to.
///
/// Deflate reaches about 1000:1 on a repetitive stream, so a one-megabyte tail can
/// ask for a gigabyte of memory. With `panic = "abort"` a failed allocation is not
/// an error anybody handles - it is the process gone, mid-macro, with keys held.
/// 512 MB is far beyond any real recording: the event cap is four million events,
/// which is roughly 400 MB of JSON at its most verbose.
const MAX_INFLATED: u64 = 512 * 1024 * 1024;

fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    // `take` rather than a check afterwards: the point is never to allocate the
    // gigabyte, not to notice having done so.
    let read = flate2::read::GzDecoder::new(bytes).take(MAX_INFLATED + 1);
    let mut read = read;
    read.read_to_end(&mut out)?;
    if out.len() as u64 > MAX_INFLATED {
        anyhow::bail!("compressed data expands past {MAX_INFLATED} bytes - refusing it");
    }
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

/// The largest appended payload that will be believed.
///
/// A macro is JSON and then gzip; 64 MB of that is a recording nobody has made.
/// The number exists so that a length read out of a file cannot become the size of
/// an allocation.
const MAX_PAYLOAD: u64 = 64 * 1024 * 1024;

/// Returns the offset where an appended payload starts, if this image has one.
///
/// Everything after the magic is attacker-controlled: this is the only place in the
/// program that takes a length out of bytes it did not write and then acts on it,
/// so the length is checked three ways before it is used. `16 + len` is a
/// `checked_add` because in a release build - where overflow checks are off - a
/// length of `u64::MAX - 15` wraps it to zero, and a `checked_sub` of zero
/// succeeds. That was the hole: the subtraction looked careful and the addition
/// underneath it was not.
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
    let len = u64::from_le_bytes(len);
    if len > MAX_PAYLOAD {
        return None;
    }
    let total = 16u64.checked_add(len)?;
    let start = (bytes.len() as u64).checked_sub(total)?;
    Some(start as usize)
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
    // Three refusals before a single byte is allocated: a length larger than any
    // real payload, a length whose footer arithmetic overflows, and a length that
    // claims more bytes than the file holds. Only then is the read sized by it.
    if len > MAX_PAYLOAD {
        warn!("this executable's footer claims a {len}-byte payload - ignoring it");
        return None;
    }
    let total = 16u64.checked_add(len)?;
    let start = size.checked_sub(total)?;

    let mut blob = vec![0u8; len as usize];
    file.seek(SeekFrom::Start(start)).ok()?;
    file.read_exact(&mut blob).ok()?;

    let json = match gunzip(&blob) {
        Ok(j) => j,
        Err(e) => {
            warn!("this executable's payload would not decompress: {e}");
            return None;
        }
    };
    // `normalize` is what caps the event count and rejects unbalanced blocks. It
    // ran on files opened through the file dialog and never on this path, which
    // meant the one input nobody chose was the one input nobody checked.
    let mut payload = serde_json::from_slice::<Payload>(&json).ok()?;
    if let Err(e) = payload.macro_data.normalize() {
        warn!("this executable's payload is not a usable macro: {e}");
        return None;
    }
    payload.speed = payload.speed.clamp(0.05, 10.0);
    Some(payload)
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
    /// Which way round the three colour bytes of a pixel lie.
    ///
    /// The screen hands back BGRA and nothing downstream cares: the search reads a
    /// brightness, and a brightness is the same number whichever end the red
    /// coefficient is applied at. Carrying the order instead of rewriting fourteen
    /// megabytes to hide it is what took the floor under a capture from 5.5 ms to
    /// one `BitBlt`. Only the two places that genuinely need red first - saving a
    /// PNG, and cutting a template - convert, and both are cold.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub enum Order {
        /// Red, green, blue, alpha. What PNGs and templates hold.
        #[default]
        Rgba,
        /// Blue, green, red, alpha. What GDI hands back.
        Bgra,
    }

    impl Order {
        /// Luma weights for byte 0, 1 and 2 in this order.
        #[inline]
        pub fn weights(self) -> (f32, f32, f32) {
            match self {
                Order::Rgba => (0.299, 0.587, 0.114),
                Order::Bgra => (0.114, 0.587, 0.299),
            }
        }
    }

    /// A rectangle of screen pixels plus where it came from.
    ///
    /// `px` is four bytes a pixel in `order`. The alpha byte is whatever the source
    /// left there - GDI leaves it at zero - and nothing that reads a `Frame` looks
    /// at it: the search throws the haystack's mask away and keeps only the
    /// template's. `to_rgba` is the one way out to a buffer that can be trusted.
    #[derive(Clone)]
    pub struct Frame {
        pub x: i32,
        pub y: i32,
        pub w: u32,
        pub h: u32,
        pub px: Vec<u8>,
        pub order: Order,
    }

    impl Frame {
        /// A frame that already holds red-first pixels.
        pub fn rgba(x: i32, y: i32, w: u32, h: u32, px: Vec<u8>) -> Self {
            Self { x, y, w, h, px, order: Order::Rgba }
        }

        /// True RGBA with an opaque alpha - what a PNG encoder and a template want.
        ///
        /// The only full pass over a captured buffer left in the program, and it
        /// runs when a picture is saved rather than when one is looked for.
        pub fn to_rgba(&self) -> Vec<u8> {
            let mut out = self.px.clone();
            let swap = self.order == Order::Bgra;
            // One pass, not two. The alpha has to be written whatever the order -
            // GDI leaves it at zero, and a template with a zero alpha is a template
            // masked out of its own comparison.
            for p in out.chunks_exact_mut(4) {
                if swap {
                    p.swap(0, 2);
                }
                p[3] = 255;
            }
            out
        }

        /// Cuts the whole frame out as a template.
        pub fn as_template(&self, name: &str) -> Template {
            Template { w: self.w, h: self.h, rgba: self.to_rgba(), name: name.to_string() }
        }
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

    fn luma(px: &[u8], i: usize, w: (f32, f32, f32)) -> f32 {
        w.0 * px[i] as f32 + w.1 * px[i + 1] as f32 + w.2 * px[i + 2] as f32
    }

    /// Grey plane plus a mask: fully transparent pixels take no part in the score,
    /// which is what lets a non-rectangular icon be matched.
    ///
    /// Scaled to 0..=1 rather than 0..=255. The correlation does not care - it is
    /// invariant to both - but the one-pass form below subtracts a sum of squares
    /// from a square of sums, and at 255 that subtraction throws away most of an
    /// f32's precision on a large template.
    fn plane(px: &[u8], w: u32, h: u32, order: Order) -> (Vec<f32>, Vec<bool>) {
        let n = (w * h) as usize;
        let mut g = vec![0.0; n];
        let mut m = vec![true; n];
        let k = order.weights();
        for i in 0..n {
            g[i] = luma(px, i * 4, k) * (1.0 / 255.0);
            m[i] = px[i * 4 + 3] >= 16;
        }
        (g, m)
    }

    /// The grey plane only, for a haystack whose mask is thrown away anyway.
    ///
    /// Half the allocation and none of the alpha reads. A screen grab has no alpha
    /// worth reading - GDI leaves it at zero - which is exactly why the search must
    /// not consult it.
    fn plane_grey(px: &[u8], w: u32, h: u32, order: Order) -> Vec<f32> {
        let n = (w * h) as usize;
        let mut g = vec![0.0; n];
        let k = order.weights();
        for i in 0..n {
            g[i] = luma(px, i * 4, k) * (1.0 / 255.0);
        }
        g
    }

    /// Gradient magnitude, by the Sobel operator.
    ///
    /// What survives a change of theme: a button's outline sits in the same place
    /// whether the panel behind it is light or dark, while its grey level does not.
    /// Correlating the gradients instead of the greys is what makes a template cut
    /// under one theme still match under another, and it is also what makes a
    /// highlighted row match its unhighlighted self.
    ///
    /// The border replicates its neighbour rather than wrapping, so a template cut
    /// tight against an edge still scores sensibly.
    fn sobel(g: &[f32], w: u32, h: u32) -> Vec<f32> {
        let n = (w as usize) * (h as usize);
        let mut out = vec![0.0f32; n];
        if w < 2 || h < 2 || g.len() < n {
            return out;
        }
        let (wu, hu) = (w as usize, h as usize);
        // The interior, where every neighbour exists. Two clamps per neighbour and
        // eight neighbours per pixel is sixteen branches a pixel; over a 3.7
        // megapixel screen that was most of the cost, and none of it was needed
        // anywhere except the one-pixel border.
        for y in 1..hu.saturating_sub(1) {
            let (up, mid, dn) = ((y - 1) * wu, y * wu, (y + 1) * wu);
            for x in 1..wu.saturating_sub(1) {
                let (a, b, c) = (g[up + x - 1], g[up + x], g[up + x + 1]);
                let (d, f) = (g[mid + x - 1], g[mid + x + 1]);
                let (i, j, k) = (g[dn + x - 1], g[dn + x], g[dn + x + 1]);
                let gx = (c + 2.0 * f + k) - (a + 2.0 * d + i);
                let gy = (i + 2.0 * j + k) - (a + 2.0 * b + c);
                out[mid + x] = (gx * gx + gy * gy).sqrt();
            }
        }
        // The border replicates its neighbour rather than wrapping, so a template
        // cut tight against an edge still scores sensibly. Copied from the ring
        // inside it, which is what clamping would have produced anyway.
        if wu >= 3 && hu >= 3 {
            for x in 0..wu {
                let sx = x.clamp(1, wu - 2);
                out[x] = out[wu + sx];
                out[(hu - 1) * wu + x] = out[(hu - 2) * wu + sx];
            }
            for y in 0..hu {
                let sy = y.clamp(1, hu - 2);
                out[y * wu] = out[sy * wu + 1];
                out[y * wu + wu - 1] = out[sy * wu + wu - 2];
            }
        }
        out
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
    pub fn resize_rgba(rgba: &[u8], w: u32, h: u32, nw: u32, nh: u32) -> Vec<u8> {
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

    /// A template ready to be correlated: its mean already taken out, its own
    /// variance already known.
    ///
    /// Both are the same at every position the window can land in, and the old code
    /// worked them out again for each one - a second full pass over the template for
    /// every pixel of the screen. Taking them out of the loop is most of the speed
    /// here; the vector kernel below is the rest.
    struct Prepared {
        /// The template minus its mean, and zero wherever the mask says to ignore.
        /// Because it sums to zero, the window's own mean drops out of the
        /// numerator and the correlation needs one pass instead of two.
        centered: Vec<f32>,
        /// 1.0 where the pixel counts, 0.0 where it does not. Floats rather than
        /// bools so the inner loop multiplies instead of branching.
        mask: Vec<f32>,
        /// True when nothing is masked out, which is every ordinary PNG.
        dense: bool,
        /// Whether to take the vector kernel. Decided here rather than at every
        /// position: the coarse pass asks for a score a quarter of a million times,
        /// and an atomic load and a branch on each of them is not free.
        use_block: bool,
        w: u32,
        h: u32,
        n: f32,
        dt: f32,
    }

    fn prepare_template(tpl: &[f32], mask: &[bool], w: u32, h: u32) -> Option<Prepared> {
        let len = (w as usize).checked_mul(h as usize)?;
        if len == 0 || tpl.len() < len || mask.len() < len {
            return None;
        }
        let n = mask[..len].iter().filter(|m| **m).count();
        if n < 4 {
            return None;
        }
        let sum: f32 = (0..len).filter(|i| mask[*i]).map(|i| tpl[i]).sum();
        let mt = sum / n as f32;
        let mut centered = vec![0.0f32; len];
        let mut mf = vec![0.0f32; len];
        let mut dt = 0.0f32;
        for i in 0..len {
            if mask[i] {
                let c = tpl[i] - mt;
                centered[i] = c;
                mf[i] = 1.0;
                dt += c * c;
            }
        }
        // A template with no contrast at all correlates with everything equally,
        // which is not an answer.
        if dt <= f32::EPSILON {
            return None;
        }
        let dense = n == len;
        #[cfg(target_arch = "x86_64")]
        let use_block = dense && w >= 8 && vectorised();
        #[cfg(not(target_arch = "x86_64"))]
        let use_block = false;
        Some(Prepared {
            centered,
            mask: mf,
            dense,
            use_block,
            w,
            h,
            n: n as f32,
            dt,
        })
    }

    /// Correlation of a prepared template placed at (ox, oy) in the haystack.
    ///
    /// Returns a value in -1.0 ..= 1.0; 1.0 means identical up to brightness and
    /// contrast, which is why a screenshot taken under a different theme still
    /// matches.
    fn score_at(hay: &[f32], hw: u32, p: &Prepared, ox: u32, oy: u32) -> f32 {
        let tw = p.w as usize;
        // Once, up front, instead of once per row: the last row of the window
        // reaches furthest into the haystack, so if it fits, every earlier one does.
        let last = ((oy + p.h - 1) as usize) * (hw as usize) + ox as usize + tw;
        if p.h == 0 || tw == 0 || last > hay.len() || p.centered.len() < tw * p.h as usize
        {
            return -1.0;
        }
        let (sum_i, sum_ii, dot) = if p.use_block {
            #[cfg(target_arch = "x86_64")]
            {
                // SAFETY: the bounds above cover every row it reads, and vectorised
                // has asked the processor for the features it is compiled against.
                unsafe { sums_block_avx2(hay, hw, p, ox, oy) }
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                unreachable!()
            }
        } else {
            let (mut a, mut b, mut d) = (0.0f32, 0.0f32, 0.0f32);
            for y in 0..p.h {
                let hrow = ((oy + y) * hw + ox) as usize;
                let trow = (y * p.w) as usize;
                let hs = &hay[hrow..hrow + tw];
                let cs = &p.centered[trow..trow + tw];
                let (ra, rb, rd) = if p.dense {
                    sums_dense_scalar(hs, cs)
                } else {
                    sums_masked(hs, &p.mask[trow..trow + tw], cs)
                };
                a += ra;
                b += rb;
                d += rd;
            }
            (a, b, d)
        };
        let mi = sum_i / p.n;
        let di = sum_ii - p.n * mi * mi;
        let den = (di * p.dt).sqrt();
        if den <= f32::EPSILON { -1.0 } else { dot / den }
    }

    /// Set by the benchmark to price the vector kernel against the plain one.
    /// Nothing else touches it, and it costs one relaxed load per row.
    static SCALAR_ONLY: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    pub fn set_scalar_only(on: bool) {
        SCALAR_ONLY.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether this build is using the vector kernel right now.
    pub fn vectorised() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            !SCALAR_ONLY.load(std::sync::atomic::Ordering::Relaxed) && has_avx2()
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    }

    /// One row: the sum of the window, the sum of its squares, and its dot product
    /// with the centred template.
    fn sums_dense_scalar(h: &[f32], c: &[f32]) -> (f32, f32, f32) {
        let n = h.len().min(c.len());
        let (mut a, mut b, mut d) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..n {
            let x = h[i];
            a += x;
            b += x * x;
            d += x * c[i];
        }
        (a, b, d)
    }

    /// The same, when some of the template is transparent. The mask is a multiplier
    /// rather than a branch, and the centred template is already zero where it does
    /// not count, so the dot product needs no mask at all.
    fn sums_masked(h: &[f32], m: &[f32], c: &[f32]) -> (f32, f32, f32) {
        let n = h.len().min(m.len()).min(c.len());
        let (mut a, mut b, mut d) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..n {
            let x = h[i] * m[i];
            a += x;
            b += x * h[i];
            d += h[i] * c[i];
        }
        (a, b, d)
    }

    /// Does this processor have AVX2 and FMA? Asked once, then remembered.
    ///
    /// A runtime question rather than a build-time one on purpose: the release ships
    /// a plain x86-64 build as well as an x86-64-v3 one, and the plain build should
    /// still use the vector unit on a machine that has one.
    #[cfg(target_arch = "x86_64")]
    fn has_avx2() -> bool {
        use std::sync::atomic::{AtomicU8, Ordering};
        static CACHE: AtomicU8 = AtomicU8::new(0);
        match CACHE.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let ok = is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma");
                CACHE.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
                ok
            }
        }
    }

    /// The whole window at once, rather than a row at a time.
    ///
    /// The accumulators live across every row and are folded down once at the end.
    /// Per row it was three horizontal sums for every eight floats, and on the
    /// coarse pass - where a 32-pixel template shrinks to an eight-wide one - that
    /// cost more than the multiply-adds it replaced. Measured: row at a time, a
    /// 32x32 template came out slower with the vector path on than off.
    ///
    /// # Safety
    /// Every row from `oy` to `oy + p.h`, at column `ox` for `p.w` floats, must lie
    /// inside `hay`, and `p.centered` must hold `p.w * p.h` of them. `score_at`
    /// checks both before calling.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn sums_block_avx2(
        hay: &[f32],
        hw: u32,
        p: &Prepared,
        ox: u32,
        oy: u32,
    ) -> (f32, f32, f32) {
        use std::arch::x86_64::*;
        let tw = p.w as usize;
        unsafe {
            let (mut va, mut vb, mut vd) =
                (_mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps());
            let (mut a, mut b, mut d) = (0.0f32, 0.0f32, 0.0f32);
            for y in 0..p.h {
                let hp = hay.as_ptr().add(((oy + y) as usize) * (hw as usize) + ox as usize);
                let cp = p.centered.as_ptr().add((y as usize) * tw);
                let mut i = 0usize;
                while i + 8 <= tw {
                    let x = _mm256_loadu_ps(hp.add(i));
                    let c = _mm256_loadu_ps(cp.add(i));
                    va = _mm256_add_ps(va, x);
                    vb = _mm256_fmadd_ps(x, x, vb);
                    vd = _mm256_fmadd_ps(x, c, vd);
                    i += 8;
                }
                while i < tw {
                    let x = *hp.add(i);
                    a += x;
                    b += x * x;
                    d += x * *cp.add(i);
                    i += 1;
                }
            }
            (a + hsum256(va), b + hsum256(vb), d + hsum256(vd))
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn hsum256(v: std::arch::x86_64::__m256) -> f32 {
        use std::arch::x86_64::*;
        // No unsafe block: with the feature enabled these intrinsics are safe, and
        // the caller has already established that the processor has it.
        let s = _mm_add_ps(_mm256_extractf128_ps(v, 1), _mm256_castps256_ps128(v));
        let s = _mm_hadd_ps(s, s);
        let s = _mm_hadd_ps(s, s);
        _mm_cvtss_f32(s)
    }

    /// Best position of `tpl` inside `hay` at one fixed scale.
    /// 0 means "as many as the machine has". Set to 1 to compare a parallel answer
    /// against the answer the single-threaded sweep would have given, which is the
    /// only way to tell a real regression from a template that matches in ten places.
    static MAX_THREADS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    pub fn set_max_threads(n: usize) {
        MAX_THREADS.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    fn find_at_scale(
        hay: &Frame,
        tpl_rgba: &[u8],
        tw: u32,
        th: u32,
        edge: bool,
    ) -> Option<(u32, u32, f32)> {
        if tw == 0 || th == 0 || tw > hay.w || th > hay.h {
            return None;
        }
        let mut hg = plane_grey(&hay.px, hay.w, hay.h, hay.order);
        let (mut tg, tm) = plane(tpl_rgba, tw, th, Order::Rgba);
        if edge {
            hg = sobel(&hg, hay.w, hay.h);
            tg = sobel(&tg, tw, th);
        }

        // Coarse pass on a shrunken copy: a full-resolution sweep of a 4K screen is
        // billions of operations, and the answer is always in the same place anyway.
        // The grid was chosen from the template alone, but the work it implies also
        // depends on the haystack, and a small template on a large screen was
        // pathological: measured on a 2560x1440 desktop, a 32 px template took 465 ms
        // against 64 ms for a 64 px one. It was handed a step of 2, so the coarse pass
        // examined a quarter of every pixel position with a 16x16 kernel - 236 million
        // operations against 25 million for the larger template.
        //
        // So coarsen until the pass fits a budget, and only then: on a small haystack
        // the finer grid is cheap and is kept, which is where its accuracy is worth
        // having.
        const COARSE_BUDGET: u64 = 24_000_000;
        let mut step = (th.min(tw) / 12).clamp(1, 8);
        while step < 8 {
            let positions = (hay.w as u64 / step as u64) * (hay.h as u64 / step as u64);
            let kernel = ((tw / step).max(1) as u64) * ((th / step).max(1) as u64);
            if positions.saturating_mul(kernel) <= COARSE_BUDGET {
                break;
            }
            step += 1;
        }
        let (chay, chw, chh) = shrink(&hg, hay.w, hay.h, step);
        let (ctpl, ctw, cth) = shrink(&tg, tw, th, step);
        let (cmask, _, _) = shrink_mask(&tm, tw, th, step);

        let mut best = (0u32, 0u32, -1.0f32);
        let coarse = prepare_template(&ctpl, &cmask, ctw, cth);
        if let Some(cp) = coarse.as_ref().filter(|_| ctw <= chw && cth <= chh) {
            let rows = chh - cth + 1;
            let cap = MAX_THREADS.load(std::sync::atomic::Ordering::Relaxed);
            let cores = if cap > 0 {
                cap
            } else {
                std::thread::available_parallelism().map_or(1, |n| n.get())
            };
            // Below a few dozen rows apiece the threads cost more than the work, and a
            // small search area is exactly where that happens.
            let threads = ((rows as usize) / 48).clamp(1, cores);
            let scan = |lo: u32, hi: u32| {
                let mut b = (0u32, 0u32, -1.0f32);
                for oy in lo..hi {
                    for ox in 0..=(chw - ctw) {
                        let sc = score_at(&chay, chw, cp, ox, oy);
                        if sc > b.2 {
                            b = (ox, oy, sc);
                        }
                    }
                }
                b
            };
            if threads <= 1 {
                best = scan(0, rows);
            } else {
                let chunk = rows.div_ceil(threads as u32);
                let parts: Vec<(u32, u32, f32)> = std::thread::scope(|sc| {
                    let handles: Vec<_> = (0..threads as u32)
                        .map(|t| {
                            let (lo, hi) = (t * chunk, ((t + 1) * chunk).min(rows));
                            sc.spawn(move || scan(lo, hi))
                        })
                        .collect();
                    handles.into_iter().map(|h| h.join().unwrap_or((0, 0, -1.0))).collect()
                });
                // Merged in row order with a strict `>`, so a tie resolves to the same
                // position the single-threaded sweep would have picked.
                for p in parts {
                    if p.2 > best.2 {
                        best = p;
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

        let fp = prepare_template(&tg, &tm, tw, th)?;
        let mut fine = (x0, y0, -1.0f32);
        for oy in y0..=y1 {
            for ox in x0..=x1 {
                let sc = score_at(&hg, hay.w, &fp, ox, oy);
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
        find_mode(hay, tpl, multiscale, false)
    }

    /// The same, with a choice about what is correlated.
    ///
    /// `edge` correlates the outlines instead of the greys. Slower by one pass over
    /// each plane, and the thing to reach for when a template stops matching after
    /// a theme change or under a highlight.
    pub fn find_mode(
        hay: &Frame,
        tpl: &Template,
        multiscale: bool,
        edge: bool,
    ) -> Option<Hit> {
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
            if let Some((ox, oy, score)) = find_at_scale(hay, &rgba, tw, th, edge) {
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
    use serde::{Deserialize, Serialize};

    /// One recognised line, in screen coordinates.
    #[derive(Clone, Debug, PartialEq)]
    pub struct TextBox {
        pub text: String,
        pub x: i32,
        pub y: i32,
        pub w: i32,
        pub h: i32,
    }

    /// What is done to a region's pixels before the engine sees them.
    ///
    /// Windows OCR was built for documents: dark text, light paper, generous size,
    /// and no knobs at all. Screen text is none of those - a pale HUD number over
    /// moving artwork is the hard case - so everything that can be done has to be
    /// done to the pixels first. This is worth more than a second engine would be,
    /// and it adds nothing to the binary.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub enum Prep {
        /// Straight to the engine, bar the enlargement it needs to answer at all.
        /// What every version up to now did.
        #[default]
        None,
        /// Ordinary interface text: grey, with the contrast pulled out to the full
        /// range. Costs nothing when the text was already clean.
        Ui,
        /// The same, enlarged harder. For text too small to have enough pixels.
        Small,
        /// A HUD over artwork: grey, stretched, then cut to black and white at the
        /// threshold that separates them best, so the picture behind stops counting.
        Game,
        /// Digits on a plate: black and white, enlarged hard, nothing else.
        Digits,
        /// Try them in turn and keep the reading that fits the expected format best.
        /// Costs one recognition per rung it has to climb, so it belongs in a step
        /// that runs occasionally, not in a tight polling loop.
        Auto,
    }

    impl Prep {
        /// Everything `Auto` tries, cheapest first.
        pub const LADDER: [Prep; 5] =
            [Prep::None, Prep::Ui, Prep::Small, Prep::Game, Prep::Digits];

        /// The enlargement this profile wants, on top of what the engine needs.
        pub fn min_scale(self) -> u32 {
            match self {
                Prep::Small | Prep::Digits => 3,
                Prep::Game => 2,
                _ => 1,
            }
        }

        pub fn index(self) -> usize {
            match self {
                Prep::None => 0,
                Prep::Ui => 1,
                Prep::Small => 2,
                Prep::Game => 3,
                Prep::Digits => 4,
                Prep::Auto => 5,
            }
        }

        pub fn from_index(i: usize) -> Self {
            match i {
                1 => Prep::Ui,
                2 => Prep::Small,
                3 => Prep::Game,
                4 => Prep::Digits,
                5 => Prep::Auto,
                _ => Prep::None,
            }
        }
    }

    /// Luma, one byte per pixel. Takes the channel order because a screen grab
    /// arrives blue-first and converting it just to weight it would be a wasted
    /// pass over the whole region.
    fn gray_of(px: &[u8], order: crate::vision::Order) -> Vec<u8> {
        let (r, b) = match order {
            crate::vision::Order::Rgba => (299u32, 114u32),
            crate::vision::Order::Bgra => (114u32, 299u32),
        };
        px.chunks_exact(4)
            .map(|p| {
                ((r * p[0] as u32 + 587 * p[1] as u32 + b * p[2] as u32) / 1000) as u8
            })
            .collect()
    }

    /// The darkest and brightest values worth keeping, ignoring `cut` of the pixels
    /// at each end.
    ///
    /// Plain min and max are useless on a screen: one antialiased pixel at pure black
    /// and one at pure white, and the stretch that follows does nothing at all.
    fn ends(g: &[u8], cut: f32) -> (u8, u8) {
        let mut hist = [0u32; 256];
        for &v in g {
            hist[v as usize] += 1;
        }
        let drop = ((g.len() as f32) * cut) as u32;
        let (mut lo, mut hi) = (0u8, 255u8);
        let mut acc = 0u32;
        for (i, &c) in hist.iter().enumerate() {
            acc += c;
            if acc > drop {
                lo = i as u8;
                break;
            }
        }
        acc = 0;
        for (i, &c) in hist.iter().enumerate().rev() {
            acc += c;
            if acc > drop {
                hi = i as u8;
                break;
            }
        }
        (lo, hi)
    }

    /// Pulls the kept range out to the full 0..=255.
    fn stretch(g: &mut [u8], lo: u8, hi: u8) {
        if hi <= lo {
            return;
        }
        let span = (hi - lo) as f32;
        for v in g.iter_mut() {
            *v = (((*v as f32 - lo as f32) / span) * 255.0).clamp(0.0, 255.0) as u8;
        }
    }

    /// Otsu's threshold: the cut that leaves the two halves as unlike each other as
    /// possible.
    ///
    /// Chosen because it has no parameters. How bright a game's HUD is on this
    /// machine, at this time of day, over this background is not something a script
    /// should have to be told.
    pub fn otsu(g: &[u8]) -> u8 {
        let mut hist = [0u64; 256];
        for &v in g {
            hist[v as usize] += 1;
        }
        let total = g.len() as f64;
        if total == 0.0 {
            return 128;
        }
        let sum: f64 = hist.iter().enumerate().map(|(i, &c)| i as f64 * c as f64).sum();
        let (mut w_b, mut sum_b, mut best, mut best_t) = (0.0f64, 0.0f64, -1.0f64, 128u8);
        for (t, &count) in hist.iter().enumerate() {
            w_b += count as f64;
            if w_b == 0.0 {
                continue;
            }
            let w_f = total - w_b;
            if w_f == 0.0 {
                break;
            }
            sum_b += t as f64 * count as f64;
            let d = sum_b / w_b - (sum - sum_b) / w_f;
            let between = w_b * w_f * d * d;
            if between > best {
                best = between;
                best_t = t as u8;
            }
        }
        best_t
    }

    /// Cuts to black and white, ending up with dark text on a light ground whichever
    /// way round it started.
    ///
    /// The engine was trained on documents. Light text on a dark panel is the common
    /// case on a screen and reads markedly worse than its own inverse, so whichever
    /// side is in the minority is taken to be the text.
    fn binarize(g: &mut [u8], t: u8) {
        let dark = g.iter().filter(|&&v| v <= t).count();
        let invert = dark * 2 > g.len();
        for v in g.iter_mut() {
            let light = *v > t;
            *v = if light != invert { 255 } else { 0 };
        }
    }

    fn gray_to_rgba(g: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; g.len() * 4];
        for (i, &v) in g.iter().enumerate() {
            out[i * 4] = v;
            out[i * 4 + 1] = v;
            out[i * 4 + 2] = v;
            out[i * 4 + 3] = 255;
        }
        out
    }

    /// Applies a profile to a region's pixels.
    ///
    /// Pure and free of anything Windows-shaped, so the whole ladder can be tested
    /// without a screen, an engine or a language pack.
    pub fn prepare(px: &[u8], prep: Prep, order: crate::vision::Order) -> Vec<u8> {
        if prep == Prep::None || px.len() < 4 {
            return px.to_vec();
        }
        let mut g = gray_of(px, order);
        // Digits skips the stretch: it is about to be cut in two anyway, and the
        // stretch only moves where the cut lands.
        if !matches!(prep, Prep::Digits) {
            let (lo, hi) = ends(&g, 0.02);
            stretch(&mut g, lo, hi);
        }
        if matches!(prep, Prep::Game | Prep::Digits) {
            let t = otsu(&g);
            binarize(&mut g, t);
        }
        gray_to_rgba(&g)
    }

    /// What a reading is supposed to look like.
    ///
    /// The useful question is not "how sure is the engine" - that number is on a
    /// scale nobody can interpret, is not comparable between engines, and this one
    /// does not expose it - but "is this the shape of the thing I asked for".
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub enum Expect {
        #[default]
        Any,
        /// Digits, with the separators screens put in them: `1,250` passes.
        Integer,
        /// A number that may carry a decimal point.
        Decimal,
        /// A clock: `2:05` or `1:02:03`.
        Time,
        /// A tiny pattern: `#` one digit, `@` one letter, `?` any one character,
        /// `*` any run including none, everything else itself, case ignored.
        ///
        /// Deliberately not a regular expression. It has to be typeable into a text
        /// box by somebody automating a game, and it must not cost a crate.
        Pattern(String),
    }

    impl Expect {
        pub fn index(&self) -> usize {
            match self {
                Expect::Any => 0,
                Expect::Integer => 1,
                Expect::Decimal => 2,
                Expect::Time => 3,
                Expect::Pattern(_) => 4,
            }
        }

        pub fn from_index(i: usize) -> Self {
            match i {
                1 => Expect::Integer,
                2 => Expect::Decimal,
                3 => Expect::Time,
                4 => Expect::Pattern("##:##".into()),
                _ => Expect::Any,
            }
        }
    }

    /// Does the pattern describe the text?
    pub fn pattern_matches(pat: &str, text: &str) -> bool {
        fn go(p: &[char], t: &[char]) -> bool {
            match p.first() {
                None => t.is_empty(),
                Some('*') => (0..=t.len()).any(|i| go(&p[1..], &t[i..])),
                Some(&c) => match t.first() {
                    None => false,
                    Some(&d) => {
                        let ok = match c {
                            '#' => d.is_ascii_digit(),
                            '@' => d.is_alphabetic(),
                            '?' => true,
                            _ => c.eq_ignore_ascii_case(&d),
                        };
                        ok && go(&p[1..], &t[1..])
                    }
                },
            }
        }
        // A run of stars is one star. Without this, four of them against a long line
        // is exponential, and a pattern typed by hand collects them easily.
        let mut p: Vec<char> = Vec::new();
        for c in pat.chars() {
            if c == '*' && p.last() == Some(&'*') {
                continue;
            }
            p.push(c);
        }
        let t: Vec<char> = text.trim().chars().collect();
        go(&p, &t)
    }

    /// First decimal number in a piece of text.
    ///
    /// `,` is read as a thousands separator and `.` as the point, which is what game
    /// interfaces overwhelmingly use: `1,250.5` is 1250.5.
    pub fn first_decimal(text: &str) -> Option<f64> {
        let mut cur = String::new();
        let mut point = false;
        for ch in text.chars().chain(std::iter::once(' ')) {
            if ch.is_ascii_digit() {
                cur.push(ch);
            } else if ch == ',' && !cur.is_empty() {
                continue;
            } else if ch == '.' && !cur.is_empty() && !point {
                point = true;
                cur.push('.');
            } else if !cur.is_empty() {
                break;
            }
        }
        cur.trim_end_matches('.').parse::<f64>().ok()
    }

    /// Does the reading fit the format that was asked for?
    pub fn accepts(e: &Expect, text: &str) -> bool {
        match e {
            Expect::Any => !text.trim().is_empty(),
            Expect::Integer => first_number(text).is_some(),
            Expect::Decimal => first_decimal(text).is_some(),
            Expect::Time => parse_clock(text).is_some(),
            Expect::Pattern(p) => !p.is_empty() && pattern_matches(p, text),
        }
    }

    /// The number a reading carries, under the format that was asked for.
    pub fn value_of(e: &Expect, text: &str) -> Option<f64> {
        match e {
            Expect::Time => parse_clock(text),
            Expect::Decimal => first_decimal(text),
            Expect::Integer => first_number(text),
            // The old behaviour, and still the right default: a clock reads as
            // seconds, anything else as a plain number.
            _ => parse_clock(text).or_else(|| first_number(text)),
        }
    }

    /// How much a reading looks like what was asked for, from 0 to 1.
    ///
    /// Half of it is whether the format parses at all; half is how much of the text
    /// belongs to the alphabet that format implies. Both halves are needed: a clock
    /// that came back as `O2:3A` fails the first, and a clock lifted out of a
    /// sentence passes the first while failing the second.
    pub fn quality(text: &str, e: &Expect) -> f64 {
        let t = text.trim();
        if t.is_empty() {
            return 0.0;
        }
        let parsed = if accepts(e, t) { 1.0 } else { 0.0 };
        let belongs = |c: char| match e {
            Expect::Integer => c.is_ascii_digit() || matches!(c, ',' | '.'),
            Expect::Decimal => c.is_ascii_digit() || matches!(c, ',' | '.' | '-'),
            Expect::Time => c.is_ascii_digit() || c == ':',
            Expect::Pattern(_) | Expect::Any => {
                c.is_alphanumeric() || c.is_ascii_punctuation()
            }
        };
        let total = t.chars().filter(|c| !c.is_whitespace()).count();
        if total == 0 {
            return 0.0;
        }
        let good = t.chars().filter(|c| !c.is_whitespace() && belongs(*c)).count();
        0.5 * parsed + 0.5 * (good as f64 / total as f64)
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
    /// Enlarges a frame into the BGRA order `SoftwareBitmap` expects.
    ///
    /// A frame that already holds BGRA - everything the screen hands back - is
    /// copied four bytes at a time instead of channel by channel. This used to undo
    /// the swap `capture` had just performed: two full passes over the buffer that
    /// cancelled each other out.
    ///
    /// The alpha byte is always written. GDI leaves it at zero, and a zero alpha in
    /// a premultiplied `Bgra8` bitmap is a fully transparent pixel - the engine
    /// would read an empty image and say so.
    #[cfg(all(windows, feature = "winocr"))]
    fn upscale_to_bgra(
        px: &[u8],
        w: u32,
        h: u32,
        k: u32,
        order: crate::vision::Order,
    ) -> (Vec<u8>, u32, u32) {
        let (nw, nh) = (w * k, h * k);
        let mut out = vec![0u8; (nw as usize) * (nh as usize) * 4];
        let swap = order == crate::vision::Order::Rgba;
        for y in 0..nh {
            let sy = y / k;
            for x in 0..nw {
                let sx = x / k;
                let s = ((sy * w + sx) * 4) as usize;
                let d = ((y * nw + x) * 4) as usize;
                if swap {
                    out[d] = px[s + 2];
                    out[d + 1] = px[s + 1];
                    out[d + 2] = px[s];
                } else {
                    out[d] = px[s];
                    out[d + 1] = px[s + 1];
                    out[d + 2] = px[s + 2];
                }
                out[d + 3] = 255;
            }
        }
        (out, nw, nh)
    }

    /// Recognises a region, preparing its pixels first.
    ///
    /// `Auto` is not resolved here: it means "try several", which needs a format to
    /// judge the answers against, and that lives in `read_region_as`.
    pub fn recognize_with(
        frame: &crate::vision::Frame,
        prep: Prep,
    ) -> anyhow::Result<Vec<TextBox>> {
        if prep == Prep::None {
            return recognize_prepared(frame, 1);
        }
        let px = prepare(&frame.px, prep, frame.order);
        // Grey: every channel holds the same byte, so the order label is a
        // formality. `Rgba` is the honest one - `gray_to_rgba` also sets alpha.
        let prepared = crate::vision::Frame::rgba(frame.x, frame.y, frame.w, frame.h, px);
        recognize_prepared(&prepared, prep.min_scale())
    }

    /// The plain reading, with nothing done to the pixels. What 1.3.5 did.
    pub fn recognize(frame: &crate::vision::Frame) -> anyhow::Result<Vec<TextBox>> {
        recognize_with(frame, Prep::None)
    }

    #[cfg(all(windows, feature = "winocr"))]
    fn recognize_prepared(
        frame: &crate::vision::Frame,
        min_scale: u32,
    ) -> anyhow::Result<Vec<TextBox>> {
        use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
        use windows::Media::Ocr::OcrEngine;
        use windows::Security::Cryptography::CryptographicBuffer;

        if frame.w == 0 || frame.h == 0 {
            return Ok(Vec::new());
        }
        // Windows OCR returns nothing at all for images under 40x40, and small
        // interface text reads much better enlarged. Scale so the short side clears
        // that floor with room to spare, while keeping the long side inside the
        // engine's own limit of roughly 4096 pixels. A profile can ask for more than
        // the floor needs; it can never ask for more than the ceiling allows.
        let short = frame.w.min(frame.h).max(1);
        let long = frame.w.max(frame.h).max(1);
        let want = (64 + short - 1) / short;
        let cap = (4000 / long).max(1);
        let scale = want.max(min_scale).clamp(1, 8).min(cap);
        if short * scale < 40 {
            // Fully qualified: this module deliberately imports nothing from the
            // crate root, and an unqualified `warn!` is a rustc attribute here.
            tracing::warn!(
                "region {}x{} is too small for Windows OCR even scaled {scale}x",
                frame.w, frame.h
            );
        }

        let (bgra, bw, bh) =
            upscale_to_bgra(&frame.px, frame.w, frame.h, scale, frame.order);
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
    fn recognize_prepared(
        _frame: &crate::vision::Frame,
        _min_scale: u32,
    ) -> anyhow::Result<Vec<TextBox>> {
        Err(anyhow::anyhow!("this build has no OCR backend"))
    }

    /// Recognises a rectangle of the screen.
    pub fn read_region(x: i32, y: i32, w: i32, h: i32) -> anyhow::Result<Vec<TextBox>> {
        let frame = crate::platform::capture(x, y, w, h)
            .ok_or_else(|| anyhow::anyhow!("could not capture the screen"))?;
        recognize(&frame)
    }

    /// What one attempt at a region came back with.
    pub struct Reading {
        pub boxes: Vec<TextBox>,
        /// How well the text fits the format asked for, 0 to 1.
        pub quality: f64,
        /// Which profile produced it. Interesting when `Auto` chose.
        pub prep: Prep,
    }

    impl Reading {
        pub fn text(&self) -> String {
            joined(&self.boxes)
        }
    }

    /// Reads a region, climbing the ladder of profiles when asked to.
    ///
    /// The ladder stops at the first perfect fit rather than always running to the
    /// end: on text that was already clean, `Auto` costs exactly what `None` costs.
    pub fn read_region_as(
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        prep: Prep,
        expect: &Expect,
    ) -> anyhow::Result<Reading> {
        let frame = crate::platform::capture(x, y, w, h)
            .ok_or_else(|| anyhow::anyhow!("could not capture the screen"))?;
        if prep != Prep::Auto {
            let boxes = recognize_with(&frame, prep)?;
            let quality = quality(&joined(&boxes), expect);
            return Ok(Reading { boxes, quality, prep });
        }
        let mut best: Option<Reading> = None;
        let mut last_err: Option<anyhow::Error> = None;
        for rung in Prep::LADDER {
            match recognize_with(&frame, rung) {
                Ok(boxes) => {
                    let q = quality(&joined(&boxes), expect);
                    if best.as_ref().is_none_or(|b| q > b.quality) {
                        best = Some(Reading { boxes, quality: q, prep: rung });
                    }
                    if best.as_ref().is_some_and(|b| b.quality >= 0.999) {
                        break;
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
        best.ok_or_else(|| {
            last_err.unwrap_or_else(|| anyhow::anyhow!("no profile produced a reading"))
        })
    }
}

// ============================================================================
// UI Automation
// ============================================================================

/// Asking Windows what is on the screen instead of looking at it.
///
/// Where it works, this beats both of the other two: a button found by its name is
/// found at any resolution, under any theme, in any language the application was
/// not translated into, and without a threshold to tune. Where it does not work it
/// is worth saying plainly - and it does not work often:
///
///   * it only sees what an application chooses to expose;
///   * Unity, DirectX, OpenGL and canvas-drawn interfaces expose nothing at all,
///     which includes the game this program was written for;
///   * across a privilege boundary it is limited or silent.
///
/// So the honest arrangement is a cascade: ask here first, fall back to the picture
/// search, then to text recognition, then to fixed coordinates. This is the rung
/// that costs nothing when it works and has to be given up on quickly when it does
/// not.
pub mod uia {
    use serde::{Deserialize, Serialize};

    /// What to look for.
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
    pub struct Query {
        /// The text a screen reader would read out. Matched without case, exactly
        /// first and then as a substring.
        pub name: String,
        /// The identifier the application gives the control, when it gives one.
        /// Exact, and by far the most reliable thing here when it exists.
        pub automation_id: String,
        /// `Button`, `Edit`, `Text`, `CheckBox`, `List`, `ListItem`, `Tab`,
        /// `TreeItem`, `MenuItem`, `Window`. Empty means any.
        pub control: String,
        /// Search inside the window in front rather than the whole desktop. Much
        /// faster, and almost always what was meant.
        #[serde(default = "yes")]
        pub in_front: bool,
    }

    fn yes() -> bool {
        true
    }

    impl Query {
        /// Does this actually name anything?
        ///
        /// It matters more than it looks. With all three fields empty the conditions
        /// below collapse to "true", and a subtree search for "true" returns the
        /// root - so an unfilled query reported the whole window as found, and
        /// `Press element` fell through to clicking the middle of it. A step that
        /// has just been added from the menu is exactly that query.
        pub fn is_empty(&self) -> bool {
            self.name.trim().is_empty()
                && self.automation_id.trim().is_empty()
                && control_id(&self.control) == 0
        }
    }

    /// What was found.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Found {
        pub name: String,
        /// What the control holds, for the ones that hold something.
        pub value: String,
        /// Centre, in screen coordinates.
        pub x: i32,
        pub y: i32,
        pub w: i32,
        pub h: i32,
    }

    /// The control types worth naming in a picker, and the identifiers Windows uses
    /// for them.
    ///
    /// A short list on purpose: these are the ones an automation script presses,
    /// reads or waits for. The full set has fifty entries and would make the picker
    /// useless.
    pub const CONTROLS: [(&str, i32); 11] = [
        ("", 0),
        ("Button", 50000),
        ("CheckBox", 50002),
        ("ComboBox", 50003),
        ("Edit", 50004),
        ("List", 50008),
        ("ListItem", 50007),
        ("MenuItem", 50011),
        ("Tab", 50018),
        ("Text", 50020),
        ("Window", 50032),
    ];

    pub fn control_id(name: &str) -> i32 {
        CONTROLS
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, id)| *id)
            .unwrap_or(0)
    }

    #[cfg(windows)]
    mod imp {
        use super::{Found, Query, control_id};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
        use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_I4, VariantClear};
        use windows::Win32::UI::Accessibility::*;
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        use windows::core::{BSTR, Interface};

        thread_local! {
            /// One automation object per thread, kept for the life of the thread.
            ///
            /// Creating it is a COM activation and costs milliseconds; a script that
            /// polls for an element would otherwise pay that on every look.
            static AUTOMATION: std::cell::RefCell<Option<IUIAutomation>> =
                const { std::cell::RefCell::new(None) };
        }

        fn automation() -> Option<IUIAutomation> {
            AUTOMATION.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.is_none() {
                    // The caller's thread has already called CoInitializeEx. This is
                    // never reached from the hook thread, where a COM activation
                    // would be a way to have Windows unhook us.
                    *slot = unsafe {
                        CoCreateInstance::<_, IUIAutomation>(
                            &CUIAutomation,
                            None,
                            CLSCTX_INPROC_SERVER,
                        )
                        .ok()
                    };
                }
                slot.clone()
            })
        }

        /// A VARIANT built by hand, released when it goes out of scope.
        ///
        /// windows-rs exposes the raw union here rather than a wrapper, and the union
        /// owns the string it is given, so something has to free it. The conditions
        /// below copy what they need out of it before this drops.
        struct Var(VARIANT);

        impl Var {
            fn int(v: i32) -> Self {
                let mut out = VARIANT::default();
                unsafe {
                    let inner = &mut *out.Anonymous.Anonymous;
                    inner.vt = VT_I4;
                    inner.Anonymous.lVal = v;
                }
                Var(out)
            }

            fn text(s: &str) -> Self {
                let mut out = VARIANT::default();
                unsafe {
                    let inner = &mut *out.Anonymous.Anonymous;
                    inner.vt = VT_BSTR;
                    inner.Anonymous.bstrVal = std::mem::ManuallyDrop::new(BSTR::from(s));
                }
                Var(out)
            }
        }

        impl Drop for Var {
            fn drop(&mut self) {
                unsafe {
                    let _ = VariantClear(&mut self.0);
                }
            }
        }

        /// The element to search from: the window in front, or the desktop.
        fn root(a: &IUIAutomation, in_front: bool) -> Option<IUIAutomationElement> {
            unsafe {
                if in_front {
                    let hwnd: HWND = GetForegroundWindow();
                    if !hwnd.0.is_null() {
                        if let Ok(e) = a.ElementFromHandle(hwnd) {
                            return Some(e);
                        }
                    }
                }
                a.GetRootElement().ok()
            }
        }

        /// Everything about an element the caller could want, asked for in one trip.
        ///
        /// Every property is a call into another process, and there are several.
        /// Asking one at a time is several round trips into an application that may
        /// be busy; a cache request makes it one.
        fn cache(a: &IUIAutomation) -> Option<IUIAutomationCacheRequest> {
            unsafe {
                let c = a.CreateCacheRequest().ok()?;
                let _ = c.AddProperty(UIA_NamePropertyId);
                let _ = c.AddProperty(UIA_BoundingRectanglePropertyId);
                let _ = c.AddProperty(UIA_ControlTypePropertyId);
                let _ = c.AddPattern(UIA_ValuePatternId);
                let _ = c.AddPattern(UIA_InvokePatternId);
                let _ = c.SetAutomationElementMode(AutomationElementMode_Full);
                Some(c)
            }
        }

        fn pattern<T: Interface>(e: &IUIAutomationElement, id: UIA_PATTERN_ID) -> Option<T> {
            unsafe {
                if let Ok(p) = e.GetCachedPattern(id) {
                    if let Ok(t) = p.cast::<T>() {
                        return Some(t);
                    }
                }
                e.GetCurrentPattern(id).ok()?.cast::<T>().ok()
            }
        }

        fn read(e: &IUIAutomationElement) -> Option<Found> {
            unsafe {
                let name = e.CachedName().or_else(|_| e.CurrentName()).unwrap_or_default();
                let value = pattern::<IUIAutomationValuePattern>(e, UIA_ValuePatternId)
                    .and_then(|p| p.CurrentValue().ok())
                    .unwrap_or_default();
                let r = e
                    .CachedBoundingRectangle()
                    .or_else(|_| e.CurrentBoundingRectangle())
                    .ok()?;
                let (w, h) = (r.right - r.left, r.bottom - r.top);
                // A control scrolled out of sight reports an empty rectangle.
                // Reporting its centre as (0, 0) would send a click to the corner of
                // the screen, so it counts as not found.
                if w <= 0 || h <= 0 {
                    return None;
                }
                Some(Found {
                    name: name.to_string(),
                    value: value.to_string(),
                    x: r.left + w / 2,
                    y: r.top + h / 2,
                    w,
                    h,
                })
            }
        }

        /// Conditions for everything that can be matched exactly.
        fn narrow(
            a: &IUIAutomation,
            q: &Query,
            with_name: bool,
        ) -> Option<IUIAutomationCondition> {
            unsafe {
                let mut cond = a.CreateTrueCondition().ok()?;
                let id = control_id(&q.control);
                if id != 0 {
                    let v = Var::int(id);
                    let c = a.CreatePropertyCondition(UIA_ControlTypePropertyId, &v.0).ok()?;
                    cond = a.CreateAndCondition(&cond, &c).ok()?;
                }
                if !q.automation_id.trim().is_empty() {
                    let v = Var::text(q.automation_id.trim());
                    let c =
                        a.CreatePropertyCondition(UIA_AutomationIdPropertyId, &v.0).ok()?;
                    cond = a.CreateAndCondition(&cond, &c).ok()?;
                }
                if with_name && !q.name.trim().is_empty() {
                    let v = Var::text(q.name.trim());
                    let c = a
                        .CreatePropertyConditionEx(
                            UIA_NamePropertyId,
                            &v.0,
                            PropertyConditionFlags_IgnoreCase,
                        )
                        .ok()?;
                    cond = a.CreateAndCondition(&cond, &c).ok()?;
                }
                Some(cond)
            }
        }

        /// One look. The waiting is added by `find`.
        pub fn look(q: &Query) -> Option<(Found, IUIAutomationElement)> {
            if q.is_empty() {
                return None;
            }
            let a = automation()?;
            let root = root(&a, q.in_front)?;
            let cache = cache(&a);
            let want = q.name.trim().to_lowercase();

            unsafe {
                // The exact name first: one call, and the application does the
                // matching. Only when that fails is the wider sweep worth its cost.
                if let Some(cond) = narrow(&a, q, true) {
                    let hit = match &cache {
                        Some(c) => root.FindFirstBuildCache(TreeScope_Subtree, &cond, c).ok(),
                        None => root.FindFirst(TreeScope_Subtree, &cond).ok(),
                    };
                    if let Some(e) = hit {
                        if let Some(f) = read(&e) {
                            return Some((f, e));
                        }
                    }
                }
                if want.is_empty() {
                    return None;
                }
                // Substring: everything the other conditions allow, filtered here.
                // This is the expensive path, and the reason naming a control type is
                // worth it - it turns a whole tree into a handful of elements.
                let cond = narrow(&a, q, false)?;
                let all = match &cache {
                    Some(c) => root.FindAllBuildCache(TreeScope_Subtree, &cond, c).ok()?,
                    None => root.FindAll(TreeScope_Subtree, &cond).ok()?,
                };
                let n = all.Length().unwrap_or(0);
                for i in 0..n {
                    let Ok(e) = all.GetElement(i) else { continue };
                    let Some(f) = read(&e) else { continue };
                    if f.name.to_lowercase().contains(&want) {
                        return Some((f, e));
                    }
                }
                None
            }
        }

        /// Presses the element the way the application itself would.
        ///
        /// Better than a synthetic click when it is offered: no cursor moves, no
        /// window has to be in front, and a control that has moved since it was found
        /// is still the control that gets pressed.
        pub fn invoke(e: &IUIAutomationElement) -> bool {
            match pattern::<IUIAutomationInvokePattern>(e, UIA_InvokePatternId) {
                Some(p) => unsafe { p.Invoke().is_ok() },
                None => false,
            }
        }
    }

    #[cfg(not(windows))]
    mod imp {
        use super::{Found, Query};
        pub fn look(q: &Query) -> Option<(Found, ())> {
            let _ = q.is_empty();
            None
        }
        pub fn invoke(_e: &()) -> bool {
            false
        }
    }

    /// Looks for an element, waiting up to `timeout_ms` for it to turn up.
    ///
    /// The wait is here rather than in the caller because an interface that is still
    /// drawing itself is the normal case after a click, and a script that has to
    /// write its own retry loop around every element is a script nobody writes.
    pub fn find(q: &Query, timeout_ms: u64) -> Option<Found> {
        if q.is_empty() {
            tracing::warn!("an element step names nothing to look for");
            return None;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            if let Some((f, _)) = imp::look(q) {
                return Some(f);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
    }

    /// Finds an element and presses it through the application's own pattern.
    ///
    /// Returns what was found when the press went through, and nothing when either
    /// the element was not there or it has no press to offer - which is the caller's
    /// cue to fall back to a real click on the rectangle.
    pub fn press(q: &Query, timeout_ms: u64) -> Option<Found> {
        if q.is_empty() {
            tracing::warn!("an element step names nothing to press");
            return None;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            if let Some((f, e)) = imp::look(q) {
                return imp::invoke(&e).then_some(f);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
    }
}

// ============================================================================
// Window responsiveness
// ============================================================================

/// How quickly the target window is getting round to its input.
///
/// This is deliberately **not** a frame counter. Reading another process's real
/// present timings means an ETW session against the DXGI providers - what
/// PresentMon does - which needs administrator rights and a schema parser larger
/// than this whole program. `DwmGetCompositionTimingInfo` is no substitute: since
/// Windows 8.1 it reports the compositor, which keeps ticking at the monitor's
/// refresh rate no matter how badly the game underneath it is doing.
///
/// So this measures the quantity that actually matters here instead: how long an
/// empty message takes to travel through the window's own message loop. A normal
/// game loop drains its queue once per frame, so the round-trip lands within about
/// one frame - and that is precisely the delay the frame guard has to cover, since
/// input is handled on the thread that pumps. When a game renders on a separate
/// thread the figure tracks input handling rather than the rendered frame rate,
/// which for this purpose is the more useful of the two.
pub mod perf {
    /// A summary over the last few seconds of probes.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Stats {
        pub samples: u32,
        pub avg_us: u64,
        /// Mean of the worst 1 % - the "1 % low" of the frame-time vocabulary.
        pub p99_us: u64,
        pub p999_us: u64,
        pub worst_us: u64,
        pub stutters: u32,
        pub found: bool,
    }

    /// Mean of the worst `frac` of the samples.
    ///
    /// Deliberately not a percentile. The 99th percentile of a hundred samples is the
    /// 99th value, which stops one short of the single worst one - precisely the
    /// sample a reader asking for the "1 % low" wants to know about. Averaging the
    /// tail rather than taking its edge also keeps one freak sample from deciding the
    /// answer on its own, which matters because the frame guard is sized from this.
    fn worst_mean(sorted: &[u64], frac: f64) -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        // Always at least one sample, so a short window still answers.
        let k = ((sorted.len() as f64 * frac).ceil() as usize).clamp(1, sorted.len());
        let tail = &sorted[sorted.len() - k..];
        tail.iter().sum::<u64>() / k as u64
    }

    pub fn summarize(samples: &[u64]) -> Stats {
        if samples.is_empty() {
            return Stats::default();
        }
        let mut v = samples.to_vec();
        v.sort_unstable();
        let n = v.len() as u64;
        let avg = v.iter().sum::<u64>() / n;
        // Twice the average, but never less than 8 ms above it: on a window that
        // answers in 200 µs, "twice the average" would call ordinary noise a hitch.
        let hitch = avg.saturating_mul(2).max(avg + 8_000);
        Stats {
            samples: v.len() as u32,
            avg_us: avg,
            p99_us: worst_mean(&v, 0.01),
            p999_us: worst_mean(&v, 0.001),
            worst_us: *v.last().unwrap_or(&0),
            stutters: v.iter().filter(|&&x| x > hitch).count() as u32,
            found: true,
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

    /// True when one of the shell's own window switchers owns the foreground.
    ///
    /// `IsWindowOnCurrentVirtualDesktop` answers honestly and unhelpfully here: Task
    /// View is an overlay drawn *on* the current desktop, so the desktop has not
    /// changed and the check below passes while synthetic clicks land in the switcher
    /// - where they create desktops, close them and move windows between them.
    ///
    /// Asked every time rather than cached, unlike the desktop query: this is two
    /// user32 calls and no COM, and a cache would let a couple of hundred milliseconds
    /// of clicks through before it noticed. Only the two switcher classes are listed.
    /// The Start menu and Search share a class with ordinary packaged apps, and a
    /// macro that silently refuses to run against one of those would be a worse bug
    /// than the one being fixed.
    pub fn shell_switcher_in_front() -> bool {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return false;
            }
            let mut buf = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut buf);
            if n <= 0 {
                return false;
            }
            matches!(
                String::from_utf16_lossy(&buf[..n as usize]).as_str(),
                // Windows 11 Task View and Alt+Tab.
                "XamlExplorerHostIslandWindow"
                // Windows 10 Task View.
                | "MultitaskingViewFrame"
            )
        }
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
    pub fn shell_switcher_in_front() -> bool {
        false
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
            if !crate::selftest::dry() {
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
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


    // ---------------------------------------------------------------------
    // Desktop Duplication
    // ---------------------------------------------------------------------
    //
    // `BitBlt` out of the desktop DC costs about six milliseconds before it has
    // copied a single useful pixel, and the same blit between two memory DCs of
    // the same size costs 0.13 ms. Both numbers are printed by `--selftest vision`
    // under "Where a capture goes". The gap is the readback: the composited
    // desktop does not live in system memory, and GDI has to go and fetch it every
    // time. Nothing about the destination bitmap changes that - the table prices a
    // DIB section against the device bitmap it replaced and they come out equal.
    //
    // Desktop Duplication is the interface that does not pay it. The compositor
    // hands over the surface it already has, the copy that reaches the CPU is only
    // the rectangle that was asked for, and - the part that matters most for a
    // script polling a settled screen - a frame that has not changed is not sent
    // at all, so the poll costs one sub-rectangle copy out of a texture that is
    // already there.
    //
    // Every failure here falls back to GDI rather than failing the capture: an
    // older machine, a remote session, a driver that says no, a rectangle that
    // straddles two monitors, or a rotated display all keep working exactly as
    // they did.
    #[cfg(windows)]
    mod dupe {
        use super::super::win32::RECT;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use windows::Win32::Graphics::Direct3D::{
            D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0,
        };
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_SAMPLE_DESC,
        };
        use windows::Win32::Graphics::Dxgi::{
            DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
            IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
        };
        use windows::core::Interface as _;

        /// Turned off by a configuration switch. Process-wide, because it is a
        /// setting rather than a discovery.
        static ENABLED: AtomicBool = AtomicBool::new(true);

        // Turned off for good, on this thread, by three consecutive failures - a
        // machine that cannot do this should stop being asked twenty times a
        // second.
        //
        // Per thread rather than process-wide, and that is not a detail. Each
        // thread duplicates the output for itself, so a thread that cannot get one
        // - because another already holds it, because it started while a
        // full-screen application had the output, because the monitor it asked
        // about has since been unplugged - is saying something about itself and
        // not about the machine. A global count would let one unlucky search
        // thread put every future playback back on the slow path for the rest of
        // the session, and nothing would ever say why.
        thread_local! {
            static STRIKES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        }
        /// Counts what the fast path actually did, for the benchmark and the log.
        pub static HITS: AtomicU64 = AtomicU64::new(0);
        pub static REUSED: AtomicU64 = AtomicU64::new(0);
        pub static MISSES: AtomicU64 = AtomicU64::new(0);

        const MAX_STRIKES: u64 = 3;

        pub fn set_enabled(on: bool) {
            ENABLED.store(on, Ordering::Relaxed);
            if on {
                // Only this thread's count. Another thread that has given up did so
                // for a reason that flipping a setting does not change; it clears
                // its own when a capture next works.
                STRIKES.with(|c| c.set(0));
            }
        }

        pub fn enabled() -> bool {
            ENABLED.load(Ordering::Relaxed) && STRIKES.with(|c| c.get()) < MAX_STRIKES
        }

        pub fn counters() -> (u64, u64, u64) {
            (
                HITS.load(Ordering::Relaxed),
                REUSED.load(Ordering::Relaxed),
                MISSES.load(Ordering::Relaxed),
            )
        }

        pub fn reset_counters() {
            HITS.store(0, Ordering::Relaxed);
            REUSED.store(0, Ordering::Relaxed);
            MISSES.store(0, Ordering::Relaxed);
        }

        fn strike(why: &str) {
            let n = STRIKES.with(|c| {
                let n = c.get() + 1;
                c.set(n);
                n
            });
            if n == MAX_STRIKES {
                tracing::warn!(
                    "desktop duplication gave up on this thread after {n} failures ({why}); \
                     screen captures fall back to GDI"
                );
            }
        }

        /// One duplicated output plus the textures that go with it.
        ///
        /// `latest` is a full-size copy of the last frame the compositor handed
        /// over, kept on the GPU. It exists so that a poll which finds no new frame
        /// still has the current screen to cut a rectangle out of: a settled screen
        /// sends nothing, and without this the second look would have nothing to
        /// look at.
        struct Dup {
            device: ID3D11Device,
            ctx: ID3D11DeviceContext,
            dup: IDXGIOutputDuplication,
            /// The output's rectangle in desktop coordinates.
            bounds: RECT,
            latest: Option<ID3D11Texture2D>,
            /// Staging texture sized to the last rectangle asked for.
            stage: Option<(ID3D11Texture2D, u32, u32)>,
        }

        thread_local! {
            static DUP: std::cell::RefCell<Option<Dup>> = const {
                std::cell::RefCell::new(None)
            };
        }

        pub fn release() {
            DUP.with(|d| {
                *d.borrow_mut() = None;
            });
            // A run that has ended takes its verdict with it. The next one may find
            // the output free, the resolution settled, or the game closed.
            STRIKES.with(|c| c.set(0));
        }

        /// Builds a duplication for the output that wholly contains `want`.
        ///
        /// A rectangle spanning two monitors is refused rather than stitched: one
        /// duplication is one output, and a script that sweeps both screens is
        /// better served by the path that already handles it than by half a frame.
        unsafe fn build(want: RECT) -> Option<Dup> {
            unsafe {
                let mut device: Option<ID3D11Device> = None;
                let mut ctx: Option<ID3D11DeviceContext> = None;
                let levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0];
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    super::super::win32::HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                )
                .ok()?;
                let device = device?;
                let ctx = ctx?;

                let dxgi: IDXGIDevice = device.cast().ok()?;
                let adapter = dxgi.GetAdapter().ok()?;
                let mut i = 0u32;
                while let Ok(output) = adapter.EnumOutputs(i) {
                    i += 1;
                    let desc = output.GetDesc().ok()?;
                    // A rotated output arrives rotated, and un-rotating it here
                    // would cost more than the readback it saves.
                    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY {
                        continue;
                    }
                    let b = desc.DesktopCoordinates;
                    let holds = want.left >= b.left
                        && want.top >= b.top
                        && want.right <= b.right
                        && want.bottom <= b.bottom;
                    if !holds {
                        continue;
                    }
                    let out1: IDXGIOutput1 = output.cast().ok()?;
                    let dup = out1.DuplicateOutput(&device).ok()?;
                    return Some(Dup {
                        device,
                        ctx,
                        dup,
                        bounds: b,
                        latest: None,
                        stage: None,
                    });
                }
                None
            }
        }

        impl Dup {
            /// Pulls the newest frame across, if the compositor has one.
            ///
            /// `Ok(false)` means nothing changed, which is the common answer while a
            /// script waits for a button and is the reason this is fast at all.
            unsafe fn pump(&mut self, patient: bool) -> Result<bool, windows::core::Error> {
                unsafe {
                    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                    let mut res: Option<IDXGIResource> = None;
                    // Nothing yet to fall back on: wait briefly for the first
                    // frame. Afterwards never block - a poll that sleeps to learn
                    // that nothing moved is worse than the blit it replaced.
                    //
                    // The wait is short and it is struck against, because the
                    // compositor sends a frame when something *changes*: a screen
                    // that is genuinely frozen sends nothing, and a patient wait
                    // followed by a GDI fallback would then be slower than GDI on
                    // its own. Three of those and this thread stops asking.
                    let timeout = if patient { 60 } else { 0 };
                    match self.dup.AcquireNextFrame(timeout, &mut info, &mut res) {
                        Ok(()) => {}
                        Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                        Err(e) => return Err(e),
                    }
                    let got = (|| -> Result<bool, windows::core::Error> {
                        let res = res.ok_or_else(|| {
                            windows::core::Error::from(DXGI_ERROR_ACCESS_LOST)
                        })?;
                        // AccumulatedFrames of 0 with a desktop pointer is a
                        // cursor-only update: the pixels behind it did not move.
                        if info.LastPresentTime == 0 && self.latest.is_some() {
                            return Ok(false);
                        }
                        let tex: ID3D11Texture2D = res.cast()?;
                        if self.latest.is_none() {
                            let mut desc = D3D11_TEXTURE2D_DESC::default();
                            tex.GetDesc(&mut desc);
                            desc.Usage = D3D11_USAGE_DEFAULT;
                            desc.BindFlags = 0;
                            desc.CPUAccessFlags = 0;
                            desc.MiscFlags = 0;
                            let mut made: Option<ID3D11Texture2D> = None;
                            self.device.CreateTexture2D(&desc, None, Some(&mut made))?;
                            self.latest = made;
                        }
                        let Some(dst) = self.latest.as_ref() else {
                            return Ok(false);
                        };
                        // Stays on the GPU. The only thing that crosses to the CPU
                        // is the rectangle the caller asked for, below.
                        self.ctx.CopyResource(dst, &tex);
                        Ok(true)
                    })();
                    let _ = self.dup.ReleaseFrame();
                    got
                }
            }

            /// Copies one rectangle of the last frame into system memory, BGRA.
            unsafe fn read(&mut self, x: i32, y: i32, w: u32, h: u32) -> Option<Vec<u8>> {
                unsafe {
                    let src = self.latest.as_ref()?.clone();
                    let fits = self.stage.as_ref().is_some_and(|(_, sw, sh)| *sw == w && *sh == h);
                    if !fits {
                        let desc = D3D11_TEXTURE2D_DESC {
                            Width: w,
                            Height: h,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Usage: D3D11_USAGE_STAGING,
                            BindFlags: 0,
                            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                            MiscFlags: 0,
                        };
                        let mut made: Option<ID3D11Texture2D> = None;
                        self.device.CreateTexture2D(&desc, None, Some(&mut made)).ok()?;
                        self.stage = made.map(|t| (t, w, h));
                    }
                    let (stage, _, _) = self.stage.as_ref()?;
                    // Desktop coordinates into output-local ones.
                    let lx = (x - self.bounds.left).max(0) as u32;
                    let ly = (y - self.bounds.top).max(0) as u32;
                    let region = D3D11_BOX {
                        left: lx,
                        top: ly,
                        front: 0,
                        right: lx + w,
                        bottom: ly + h,
                        back: 1,
                    };
                    self.ctx.CopySubresourceRegion(
                        stage,
                        0,
                        0,
                        0,
                        0,
                        &src,
                        0,
                        Some(&region),
                    );
                    let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                    self.ctx.Map(stage, 0, D3D11_MAP_READ, 0, Some(&mut map)).ok()?;
                    let row = (w as usize) * 4;
                    let n = row * (h as usize);
                    let mut buf: Vec<u8> = Vec::with_capacity(n);
                    let pitch = map.RowPitch as usize;
                    if map.pData.is_null() || pitch < row {
                        self.ctx.Unmap(stage, 0);
                        return None;
                    }
                    // The pitch is the driver's, not ours, and is routinely larger
                    // than the row: copying the block whole would shear the image.
                    for r in 0..h as usize {
                        std::ptr::copy_nonoverlapping(
                            (map.pData as *const u8).add(r * pitch),
                            buf.as_mut_ptr().add(r * row),
                            row,
                        );
                    }
                    buf.set_len(n);
                    self.ctx.Unmap(stage, 0);
                    Some(buf)
                }
            }
        }

        /// The fast path. `None` means "GDI, please" and is never an error the
        /// caller has to handle.
        pub fn capture(x: i32, y: i32, w: i32, h: i32) -> Option<Vec<u8>> {
            if !enabled() {
                return None;
            }
            // Saturating, not wrapping. A `Read text` step takes its rectangle from
            // whatever somebody typed into the box, and an x of `i32::MAX` would
            // otherwise wrap `right` to a negative number - which is a rectangle the
            // containment test below would happily accept.
            let want = RECT {
                left: x,
                top: y,
                right: x.saturating_add(w),
                bottom: y.saturating_add(h),
            };
            DUP.with(|cell| {
                let mut slot = cell.borrow_mut();
                let holds = slot.as_ref().is_some_and(|d| {
                    want.left >= d.bounds.left
                        && want.top >= d.bounds.top
                        && want.right <= d.bounds.right
                        && want.bottom <= d.bounds.bottom
                });
                if !holds {
                    *slot = None;
                    *slot = unsafe { build(want) };
                }
                let Some(d) = slot.as_mut() else {
                    MISSES.fetch_add(1, Ordering::Relaxed);
                    strike("no duplicable output holds the rectangle");
                    return None;
                };
                let patient = d.latest.is_none();
                match unsafe { d.pump(patient) } {
                    Ok(true) => {
                        HITS.fetch_add(1, Ordering::Relaxed);
                        STRIKES.with(|c| c.set(0));
                    }
                    Ok(false) if d.latest.is_some() => {
                        // Nothing changed, so the frame already here is the screen.
                        REUSED.fetch_add(1, Ordering::Relaxed);
                        STRIKES.with(|c| c.set(0));
                    }
                    Ok(false) => {
                        // Never got a first frame; GDI can answer this one. A strike,
                        // so a screen that never changes cannot make every capture
                        // pay the wait before falling back anyway.
                        MISSES.fetch_add(1, Ordering::Relaxed);
                        strike("no first frame arrived");
                        return None;
                    }
                    Err(e) => {
                        // Access lost happens on a resolution change, a session
                        // switch, or a full-screen application taking the output.
                        // Rebuilding is the documented cure, and it is not a
                        // failure worth a strike.
                        tracing::debug!("desktop duplication reset: {e}");
                        *slot = None;
                        MISSES.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                }
                let out = unsafe { d.read(x, y, w as u32, h as u32) };
                if out.is_none() {
                    MISSES.fetch_add(1, Ordering::Relaxed);
                    *slot = None;
                    strike("the staging copy failed");
                }
                out
            })
        }
    }

    #[cfg(windows)]
    pub use dupe::{
        counters as capture_counters, reset_counters as reset_capture_counters,
        set_enabled as set_fast_capture,
    };

    /// A memory DC and a DIB kept alive between captures, one per thread.
    ///
    /// Three costs used to be paid on every single look at the screen: a memory DC
    /// and a bitmap created and destroyed (GDI object churn, against a per-process
    /// quota of 10 000 objects), a fresh zeroed allocation, and a `GetDIBits` that
    /// copied and reformatted the whole frame a second time.
    ///
    /// A DIB section removes the second copy outright - `BitBlt` writes straight
    /// into memory this process can already read - and caching it removes the
    /// churn. The playback thread looks at the same rectangle thousands of times in
    /// a row, so the cache hits essentially always; a size change throws it away
    /// and builds the next one.
    struct DibCache {
        mem: HDC,
        bmp: HBITMAP,
        old: HGDIOBJ,
        bits: *mut u8,
        w: i32,
        h: i32,
    }

    impl DibCache {
        /// `None` if GDI would not give us either object. Nothing is leaked on the
        /// way out: each step undoes itself.
        unsafe fn new(w: i32, h: i32) -> Option<Self> {
            unsafe {
                let mem = CreateCompatibleDC(None);
                if mem.is_invalid() {
                    return None;
                }
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w,
                        biHeight: -h, // top-down, so row 0 is the top one
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits: *mut c_void = std::ptr::null_mut();
                let made = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0);
                let Ok(bmp) = made else {
                    let _ = DeleteDC(mem);
                    return None;
                };
                if bits.is_null() {
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                    let _ = DeleteDC(mem);
                    return None;
                }
                let old = SelectObject(mem, HGDIOBJ(bmp.0));
                Some(Self { mem, bmp, old, bits: bits as *mut u8, w, h })
            }
        }

        #[inline]
        fn bytes(&self) -> usize {
            (self.w as usize) * (self.h as usize) * 4
        }
    }

    impl Drop for DibCache {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.mem, self.old);
                let _ = DeleteObject(HGDIOBJ(self.bmp.0));
                let _ = DeleteDC(self.mem);
            }
        }
    }

    thread_local! {
        static DIB: std::cell::RefCell<Option<DibCache>> = const {
            std::cell::RefCell::new(None)
        };
    }

    /// Drops this thread's cached bitmap. Called when a playback run ends, so a
    /// full-screen grab does not sit on 14 MB of committed memory afterwards.
    pub fn release_capture_cache() {
        DIB.with(|c| {
            *c.borrow_mut() = None;
        });
        #[cfg(windows)]
        dupe::release();
    }

    /// Just the screen DC, taken and given back. The fixed cost of asking GDI for
    /// the desktop, with no pixels moved. Benchmark only.
    pub fn probe_screen_dc() -> bool {
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return false;
            }
            ReleaseDC(None, screen);
            true
        }
    }

    /// A capture that stops before the copy out: DC, `BitBlt` into the cached DIB,
    /// DC back. What the frame costs to *reach*, as opposed to to own. Benchmark
    /// only - the pixels are left in the cache and nobody reads them.
    pub fn probe_blt(x: i32, y: i32, w: i32, h: i32) -> bool {
        if w <= 0 || h <= 0 {
            return false;
        }
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return false;
            }
            let ok = DIB.with(|cell| {
                let mut slot = cell.borrow_mut();
                if !slot.as_ref().is_some_and(|c| c.w == w && c.h == h) {
                    *slot = None;
                    *slot = DibCache::new(w, h);
                }
                let Some(cache) = slot.as_ref() else { return false };
                BitBlt(cache.mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_ok()
            });
            ReleaseDC(None, screen);
            ok
        }
    }

    /// The old arrangement: a device-format bitmap made and thrown away each time,
    /// which is what a DIB section replaced. Benchmark only, kept so the claim that
    /// the destination was never the expensive part can be checked rather than
    /// asserted.
    pub fn probe_blt_ddb(x: i32, y: i32, w: i32, h: i32) -> bool {
        if w <= 0 || h <= 0 {
            return false;
        }
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return false;
            }
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, HGDIOBJ(bmp.0));
            let ok = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_ok();
            SelectObject(mem, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            ok
        }
    }

    /// The same blit with no screen at either end: cached DIB to a scratch DIB.
    /// Prices the copy itself, so the screen readback can be told apart from it.
    pub fn probe_blt_mem(w: i32, h: i32) -> bool {
        if w <= 0 || h <= 0 {
            return false;
        }
        unsafe {
            let src = DIB.with(|cell| {
                let mut slot = cell.borrow_mut();
                if !slot.as_ref().is_some_and(|c| c.w == w && c.h == h) {
                    *slot = None;
                    *slot = DibCache::new(w, h);
                }
                slot.as_ref().map(|c| c.mem)
            });
            let Some(src) = src else { return false };
            let Some(dst) = DibCache::new(w, h) else { return false };
            BitBlt(dst.mem, 0, 0, w, h, Some(src), 0, 0, SRCCOPY).is_ok()
        }
    }

    /// Grabs a rectangle of the screen.
    ///
    /// The pixels come back in GDI's own BGRA order and are not touched on the way
    /// out; the `Frame` says which order they are in and the two consumers that
    /// care read that. See `vision::Order`.
    pub fn capture(x: i32, y: i32, w: i32, h: i32) -> Option<crate::vision::Frame> {
        if w <= 0 || h <= 0 {
            return None;
        }
        // A rectangle big enough to overflow the multiply below is not a rectangle
        // any screen has; refusing it here keeps every later cast honest.
        if (w as i64) * (h as i64) > (1i64 << 28) {
            return None;
        }
        // The compositor's own copy, when this machine will give us one.
        if let Some(px) = dupe::capture(x, y, w, h) {
            return Some(crate::vision::Frame {
                x,
                y,
                w: w as u32,
                h: h as u32,
                px,
                order: crate::vision::Order::Bgra,
            });
        }
        capture_gdi(x, y, w, h)
    }

    /// The fallback, and the only path before 1.5.0. Kept whole rather than folded
    /// into the caller: `--selftest vision` prices one against the other, and a
    /// machine where duplication misbehaves is one configuration switch away from
    /// this being all that runs.
    pub fn capture_gdi(x: i32, y: i32, w: i32, h: i32) -> Option<crate::vision::Frame> {
        if w <= 0 || h <= 0 || (w as i64) * (h as i64) > (1i64 << 28) {
            return None;
        }
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let out = DIB.with(|cell| {
                let mut slot = cell.borrow_mut();
                let fits = slot.as_ref().is_some_and(|c| c.w == w && c.h == h);
                if !fits {
                    // Dropped first, so the old objects are gone before the new ones
                    // are asked for rather than both being held at once.
                    *slot = None;
                    *slot = DibCache::new(w, h);
                }
                let cache = slot.as_ref()?;
                if BitBlt(cache.mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_err() {
                    return None;
                }
                // `Vec::with_capacity` rather than `vec![0; n]`: the zeroing pass is
                // a full write of the frame that the copy immediately overwrites.
                let n = cache.bytes();
                let mut buf: Vec<u8> = Vec::with_capacity(n);
                std::ptr::copy_nonoverlapping(cache.bits, buf.as_mut_ptr(), n);
                buf.set_len(n);
                Some(buf)
            });
            ReleaseDC(None, screen);
            Some(crate::vision::Frame {
                x,
                y,
                w: w as u32,
                h: h as u32,
                px: out?,
                order: crate::vision::Order::Bgra,
            })
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
    /// Dots per inch of the display the front window is on. 96 is 100 %.
    pub fn current_dpi() -> u32 {
        unsafe {
            let hwnd = GetForegroundWindow();
            let dpi = if hwnd.0.is_null() { 0 } else { GetDpiForWindow(hwnd) };
            if dpi == 0 { 96 } else { dpi }
        }
    }

    /// Where the window in front is, for `SearchArea::ActiveWindow`.
    pub fn foreground_rect() -> Option<(i32, i32, i32, i32)> {
        unsafe {
            let hwnd = GetForegroundWindow();
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

    /// Executable name of the window in front, without its path.
    ///
    /// `PROCESS_QUERY_LIMITED_INFORMATION` rather than the full right: it is the
    /// least this needs, and it is the one that works across an elevation boundary
    /// in the direction that matters.
    pub fn foreground_process() -> String {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return String::new();
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return String::new();
            }
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return String::new();
            };
            let mut buf = [0u16; 260];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(h);
            if !ok {
                return String::new();
            }
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
        }
    }

    /// Is a process whose name contains `name` running?
    ///
    /// A substring, without case, so `roblox` finds `RobloxPlayerBeta.exe` - asking
    /// somebody to type the exact executable name is asking them to get it wrong.
    pub fn process_running(name: &str) -> bool {
        let needle = name.trim().to_lowercase();
        if needle.is_empty() {
            return false;
        }
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return false;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = false;
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let exe = String::from_utf16_lossy(&entry.szExeFile[..end]);
                    if exe.to_lowercase().contains(&needle) {
                        found = true;
                        break;
                    }
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
            found
        }
    }

    /// The clipboard as text. Empty when it holds something else, or nothing.
    pub fn clipboard_text() -> String {
        unsafe {
            if OpenClipboard(None).is_err() {
                return String::new();
            }
            let out = (|| {
                const CF_UNICODETEXT: u32 = 13;
                let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
                let hglobal = HGLOBAL(handle.0);
                let ptr = GlobalLock(hglobal) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0usize;
                while *ptr.add(len) != 0 && len < 1_000_000 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(hglobal);
                Some(text)
            })();
            let _ = CloseClipboard();
            out.unwrap_or_default()
        }
    }

    /// Replaces the clipboard with `text`. False when Windows would not hand it over,
    /// which happens whenever another application is holding it open.
    pub fn set_clipboard_text(text: &str) -> bool {
        unsafe {
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            wide.push(0);
            let bytes = wide.len() * 2;
            let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
                return false;
            };
            let ptr = GlobalLock(h) as *mut u16;
            if ptr.is_null() {
                return false;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(h);
            if OpenClipboard(None).is_err() {
                return false;
            }
            let _ = EmptyClipboard();
            const CF_UNICODETEXT: u32 = 13;
            let ok = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0))).is_ok();
            let _ = CloseClipboard();
            ok
        }
    }

    /// The window a title (or the start of one) refers to.
    fn find_window_handle(title: &str) -> Option<HWND> {
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
            if hwnd.0.is_null() { None } else { Some(hwnd) }
        }
    }

    pub fn find_window_rect(title: &str) -> Option<(i32, i32, i32, i32)> {
        unsafe {
            let hwnd = find_window_handle(title)?;
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() {
                return None;
            }
            Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
        }
    }

    thread_local! {
        /// Title, handle and when it was resolved: re-running EnumWindows forty
        /// times a second to measure latency would be its own source of latency.
        static PROBE_TARGET: std::cell::RefCell<(String, Option<HWND>, u64)> =
            const { std::cell::RefCell::new((String::new(), None, 0)) };
    }

    /// Round-trip of an empty message through the target window's message loop,
    /// in microseconds. `None` when the window is gone or refused to answer.
    ///
    /// `WM_NULL` is chosen because it does nothing at all: the window's procedure
    /// returns immediately, so what is timed is the wait in the queue rather than
    /// any work the message caused. `SMTO_ABORTIFHUNG` keeps a wedged game from
    /// parking this thread for the whole timeout.
    pub fn probe_window_us(title: &str, timeout_ms: u32) -> Option<u64> {
        unsafe {
            let hwnd = PROBE_TARGET.with(|c| {
                let mut c = c.borrow_mut();
                let stale = now_us().saturating_sub(c.2) > 2_000_000;
                let dead = match c.1 {
                    Some(h) => !IsWindow(Some(h)).as_bool(),
                    None => true,
                };
                if c.0 != title || stale || dead {
                    *c = (title.to_string(), find_window_handle(title), now_us());
                }
                c.1
            })?;
            let t0 = now_us();
            let mut out: usize = 0;
            let r = SendMessageTimeoutW(
                hwnd,
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                timeout_ms,
                Some(&mut out),
            );
            if r.0 == 0 {
                // Timed out or the window died between the check and the send.
                PROBE_TARGET.with(|c| c.borrow_mut().1 = None);
                return None;
            }
            Some(now_us().saturating_sub(t0))
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

    /// What this process is currently costing: private bytes, open handles, GDI
    /// objects.
    ///
    /// All three are resolved at run time rather than linked. `GetProcessMemoryInfo`
    /// lives in a psapi feature nothing else here needs, and a number that only a
    /// soak test reads is not worth widening the build's dependency surface for. The
    /// same trick is used for `AttachConsole` a few lines up.
    pub fn process_cost() -> (u64, u32, u32) {
        #[repr(C)]
        #[derive(Default)]
        struct MemCountersEx {
            cb: u32,
            page_fault_count: u32,
            peak_working_set: usize,
            working_set: usize,
            quota_peak_paged_pool: usize,
            quota_paged_pool: usize,
            quota_peak_non_paged_pool: usize,
            quota_non_paged_pool: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
            private_usage: usize,
        }

        unsafe {
            let me = windows::Win32::System::Threading::GetCurrentProcess();
            let mut private = 0u64;
            let mut handles = 0u32;
            let mut gdi = 0u32;

            if let Ok(k32) = GetModuleHandleW(w!("kernel32.dll")) {
                if let Some(sym) =
                    GetProcAddress(k32, PCSTR(b"K32GetProcessMemoryInfo\0".as_ptr()))
                {
                    let f: unsafe extern "system" fn(
                        windows::Win32::Foundation::HANDLE,
                        *mut MemCountersEx,
                        u32,
                    ) -> i32 = std::mem::transmute(sym);
                    let mut pmc = MemCountersEx {
                        cb: std::mem::size_of::<MemCountersEx>() as u32,
                        ..Default::default()
                    };
                    if f(me, &mut pmc, pmc.cb) != 0 {
                        private = pmc.private_usage as u64;
                    }
                }
                if let Some(sym) =
                    GetProcAddress(k32, PCSTR(b"GetProcessHandleCount\0".as_ptr()))
                {
                    let f: unsafe extern "system" fn(
                        windows::Win32::Foundation::HANDLE,
                        *mut u32,
                    ) -> i32 = std::mem::transmute(sym);
                    let mut n = 0u32;
                    if f(me, &mut n) != 0 {
                        handles = n;
                    }
                }
            }
            if let Ok(u32dll) = GetModuleHandleW(w!("user32.dll")) {
                if let Some(sym) = GetProcAddress(u32dll, PCSTR(b"GetGuiResources\0".as_ptr()))
                {
                    let f: unsafe extern "system" fn(
                        windows::Win32::Foundation::HANDLE,
                        u32,
                    ) -> u32 = std::mem::transmute(sym);
                    gdi = f(me, 0); // GR_GDIOBJECTS
                }
            }
            (private, handles, gdi)
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
    pub fn release_capture_cache() {}
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
    pub fn foreground_rect() -> Option<(i32, i32, i32, i32)> {
        None
    }
    pub fn current_dpi() -> u32 {
        96
    }
    pub fn foreground_process() -> String {
        String::new()
    }
    pub fn process_running(_: &str) -> bool {
        false
    }
    pub fn clipboard_text() -> String {
        String::new()
    }
    pub fn set_clipboard_text(_: &str) -> bool {
        false
    }
    pub fn probe_window_us(_: &str, _: u32) -> Option<u64> {
        None
    }
    pub fn process_cost() -> (u64, u32, u32) {
        (0, 0, 0)
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
    /// Stop before every step and wait to be told to go on. Debugging by watching
    /// rather than by reading the log afterwards.
    pub step_mode: AtomicBool,
    /// Raised by the "next step" button: run exactly one more step.
    pub step_once: AtomicBool,
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
    pub frame_guard: AtomicBool,
    pub frame_guard_fps: AtomicU64,
    pub frame_guard_auto: AtomicBool,
    /// Microseconds the frame guard added during the current run, for the UI.
    pub fg_added_us: AtomicU64,
    // window responsiveness
    pub perf_enabled: AtomicBool,
    /// The worst 1 % of probe round-trips, in microseconds. 0 = nothing measured.
    pub perf_frame_us: AtomicU64,
    pub perf_stats: Mutex<perf::Stats>,
    pub speed: Mutex<f64>,

    // recording settings
    pub capture_mouse_moves: AtomicBool,
    /// Cut a square out of the screen at each click while recording.
    pub record_click_shots: AtomicBool,
    pub click_shot_size: AtomicU32,
    /// The squares from the recording that just finished, waiting to be offered.
    pub click_shots: Mutex<Vec<ClickShot>>,
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
            step_mode: AtomicBool::new(false),
            step_once: AtomicBool::new(false),
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
            frame_guard: AtomicBool::new(false),
            frame_guard_fps: AtomicU64::new(30),
            frame_guard_auto: AtomicBool::new(true),
            fg_added_us: AtomicU64::new(0),
            perf_enabled: AtomicBool::new(false),
            perf_frame_us: AtomicU64::new(0),
            perf_stats: Mutex::new(perf::Stats::default()),
            speed: Mutex::new(1.0),

            capture_mouse_moves: AtomicBool::new(true),
            record_click_shots: AtomicBool::new(false),
            click_shot_size: AtomicU32::new(64),
            click_shots: Mutex::new(Vec::new()),
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
    platform::set_fast_capture(cfg.fast_capture);
    state.record_click_shots.store(cfg.record_click_shots, Ordering::Relaxed);
    state.click_shot_size.store(cfg.click_shot_size.clamp(16, 512), Ordering::Relaxed);
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
    state.frame_guard.store(cfg.frame_guard, Ordering::Relaxed);
    state.frame_guard_fps.store(cfg.frame_guard_fps, Ordering::Relaxed);
    state.frame_guard_auto.store(cfg.frame_guard_auto, Ordering::Relaxed);
    state.perf_enabled.store(cfg.perf_enabled, Ordering::Relaxed);
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

/// Samples the target window and publishes the summary.
///
/// Its own thread for the same reason the scheduler has one: a window minimised to
/// the tray stops painting, and a measurement that only exists while somebody is
/// looking at it is no use to a run that lasts all night.
fn perf_thread(state: Arc<AppState>) {
    // 400 samples at 25 ms is a ten-second window: long enough for a 0.1 % figure
    // to mean something, short enough to react when the game starts struggling.
    const CAPACITY: usize = 400;
    const INTERVAL_MS: u64 = 25;
    let mut samples: std::collections::VecDeque<u64> =
        std::collections::VecDeque::with_capacity(CAPACITY);

    let clear = |samples: &mut std::collections::VecDeque<u64>, state: &AppState| {
        if !samples.is_empty() {
            samples.clear();
        }
        state.perf_frame_us.store(0, Ordering::Relaxed);
        let mut st = state.perf_stats.lock();
        if st.found || st.samples > 0 {
            *st = perf::Stats::default();
        }
    };

    loop {
        // The panel switch turns it on; so does the guard, which cannot work out a
        // frame time on its own.
        let wanted = state.perf_enabled.load(Ordering::Relaxed)
            || (state.frame_guard.load(Ordering::Relaxed)
                && state.frame_guard_auto.load(Ordering::Relaxed));
        if !wanted {
            clear(&mut samples, &state);
            std::thread::sleep(Duration::from_millis(400));
            continue;
        }

        let title = state.target_title.lock().trim().to_string();
        if title.is_empty() {
            clear(&mut samples, &state);
            std::thread::sleep(Duration::from_millis(400));
            continue;
        }

        match platform::probe_window_us(&title, 250) {
            Some(us) => {
                if samples.len() == CAPACITY {
                    samples.pop_front();
                }
                samples.push_back(us);
                let flat: Vec<u64> = samples.iter().copied().collect();
                let st = perf::summarize(&flat);
                // The guard is sized from the worst 1 %, not the average: a press has
                // to survive the slow frames, not the comfortable ones.
                state.perf_frame_us.store(st.p99_us, Ordering::Relaxed);
                *state.perf_stats.lock() = st;
            }
            None => clear(&mut samples, &state),
        }
        std::thread::sleep(Duration::from_millis(INTERVAL_MS));
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
/// Turns a recording plus the squares cut at its clicks into a script.
///
/// The rule is that nothing is thrown away. Everything between one converted click
/// and the next stays a `Play events` step over exactly that range, so the
/// keystrokes, the scrolling and the recorded timing all survive; only the clicks
/// themselves become `Click image`. A macro that was a list of coordinates comes
/// back as a list of pictures with the typing still in it.
///
/// A click is only converted when it is really a click: press and release close
/// together in time and place, with nothing in between. A drag is press, move,
/// release, and turning that into "find the picture and click it" would silently
/// drop the drag - so a drag stays in its `Play events` range where it works.
///
/// Returns the new script and the shots that were used, in step order.
fn script_from_click_shots(
    data: &MacroData,
    shots: &[ClickShot],
    names: &[String],
    threshold: f64,
    miss: OnMiss,
) -> (Vec<ScriptStep>, usize) {
    let events = &data.events;
    let mut out: Vec<ScriptStep> = Vec::new();
    let mut cursor = 0usize; // first event not yet covered
    let mut made = 0usize;

    for (shot, name) in shots.iter().zip(names.iter()) {
        let down = shot.index;
        if down < cursor || down >= events.len() {
            continue; // an edit moved the ground under it
        }
        let Some(up) = matching_release(events, down, shot.button) else {
            continue;
        };
        // Everything before the press, replayed as it was recorded.
        if down > cursor {
            out.push(ScriptStep::new(StepKind::PlayEvents { from: cursor, to: down - 1 }));
        }
        out.push(ScriptStep::new(StepKind::ClickImage {
            template: name.clone(),
            threshold,
            button: shot.button,
            // Near where it was last seen, widening to the whole screen when it is
            // not there. The first look of the run is a full sweep because nothing
            // has been seen yet, which is the correct place to spend the time.
            area: SearchArea::NearLast { margin: 160 },
            edge: false,
            miss,
        }));
        made += 1;
        cursor = up + 1;
    }
    if cursor < events.len() {
        out.push(ScriptStep::new(StepKind::PlayEvents {
            from: cursor,
            to: events.len() - 1,
        }));
    }
    (out, made)
}

/// The release that closes a press, when the pair is a click rather than a drag.
///
/// `None` for a drag, for a press that was never released, and for a press with
/// another button event inside it.
fn matching_release(events: &[MacroEvent], down: usize, button: MouseButton) -> Option<usize> {
    let (dx, dy, dt) = match events.get(down)?.kind {
        InputEventKind::MouseButton { x, y, .. } => (x, y, events[down].t_us),
        _ => return None,
    };
    for (j, e) in events.iter().enumerate().skip(down + 1) {
        match e.kind {
            InputEventKind::MouseButton { button: b, down: false, x, y } if b == button => {
                // Eight pixels and two seconds. Past either it was a drag or a
                // press-and-hold, and neither is a click that a picture can stand
                // in for.
                let moved = (x - dx).abs().max((y - dy).abs());
                let held = e.t_us.saturating_sub(dt);
                return (moved <= 8 && held <= 2_000_000).then_some(j);
            }
            // Any other button going down or up inside the pair means this was not
            // a plain click.
            InputEventKind::MouseButton { .. } => return None,
            _ => {}
        }
    }
    None
}

/// Writes one shot to `templates/<name>.png` with its scale beside it.
fn save_click_shot(shot: &ClickShot, name: &str) -> Result<()> {
    let dir = paths::templates_dir();
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join(format!("{name}.png"));
    let img = image::RgbaImage::from_raw(shot.w, shot.h, shot.rgba.clone())
        .ok_or_else(|| anyhow::anyhow!("click shot {name} has the wrong number of bytes"))?;
    img.save(&path).with_context(|| format!("writing {}", path.display()))?;
    // Written now because now is the only moment the scale it was cut at is known.
    save_template_meta(&path, &TemplateMeta { dpi: shot.dpi });
    Ok(())
}

/// A file name for one shot: the recording's stamp, the step number, the button.
///
/// Deliberately not the window title or anything else guessable - two recordings
/// of the same screen must not overwrite each other's pictures, and a name that is
/// obviously machine-made invites renaming it to something meaningful.
fn click_shot_name(stamp: &str, n: usize) -> String {
    format!("rec_{stamp}_{n:02}")
}

/// A stamp that sorts and does not collide within a session.
fn recording_stamp() -> String {
    let (y, mo, d, _, h, mi) = platform::local_time();
    format!("{y:04}{mo:02}{d:02}_{h:02}{mi:02}")
}

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
    /// Microseconds spent drawing curved paths, for the scheduler to absorb.
    spent_us: u64,
}

impl MoveEngine {
    fn new(state: &AppState) -> Self {
        Self {
            // Seeded from where the pointer actually is. Starting at `None` meant the
            // first move of every run had no previous point to curve away from, and
            // that first jump - from wherever the user left the cursor - is the one
            // most likely to be watched.
            last: Some(platform::cursor_pos()),
            rng: Rng::new(),
            human: state.human_mouse.load(Ordering::Relaxed),
            curve: state.human_curve.load(Ordering::Relaxed) as f32 / 100.0,
            jitter: state.mouse_jitter_px.load(Ordering::Relaxed),
            spent_us: 0,
        }
    }

    /// A do-nothing engine for the paths that only release stuck buttons.
    fn inert() -> Self {
        Self {
            last: None,
            rng: Rng::new(),
            human: false,
            curve: 0.0,
            jitter: 0,
            spent_us: 0,
        }
    }

    /// Time spent on curved paths since this was last asked, in microseconds.
    fn take_spent(&mut self) -> u64 {
        std::mem::take(&mut self.spent_us)
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
                    let t0 = now_us();
                    for p in bezier_path(from, (tx, ty), self.curve, &mut self.rng, steps) {
                        unsafe { platform::send_absolute_mouse_move(p.0, p.1) };
                        spin_sleep::sleep(Duration::from_micros(1_200));
                    }
                    self.spent_us = self.spent_us.saturating_add(now_us().saturating_sub(t0));
                }
            }
        }
        unsafe { platform::send_absolute_mouse_move(tx, ty) };
        self.last = Some((tx, ty));
    }
}

/// Keeps synthetic input on screen long enough for a slow game to notice it.
///
/// A game rendering at 15 FPS samples the keyboard and mouse roughly once every
/// 67 ms. A recorded click that lasted 8 ms is invisible to it: the button goes
/// down and back up between two polls, and the frame in between never happened.
/// The macro is not late and nothing was dropped by Windows - the game simply
/// never looked.
///
/// So three spacings are enforced, all derived from the slowest frame time the
/// user expects:
///   * every press is held for two frames before its release,
///   * a re-press waits one frame after the release before it,
///   * a click waits one frame after the cursor moved, so hit-testing has caught up.
///
/// Nothing is ever shortened: a macro can only get slower, never faster, and a
/// recording made on a machine that never stutters is left almost untouched.
#[derive(Default)]
struct FrameGuard {
    enabled: bool,
    /// Follow the measured window latency instead of the configured figure.
    auto: bool,
    frame_us: u64,
    hold_us: u64,
    gap_us: u64,
    settle_us: u64,
    /// How far behind schedule counts as a stall rather than ordinary jitter.
    slip_us: u64,
    down_at: Vec<(u64, u64)>,
    up_at: Vec<(u64, u64)>,
    last_move_us: u64,
}

impl FrameGuard {
    fn set_frame(&mut self, frame: u64) {
        self.frame_us = frame;
        // Two frames, not one: a press that lands just after a poll would still be
        // gone before the next one.
        self.hold_us = frame * 2;
        self.gap_us = frame;
        self.settle_us = frame;
        self.slip_us = (frame * 6).max(150_000);
    }

    fn for_fps(enabled: bool, fps: u64) -> Self {
        let mut g = Self { enabled, ..Default::default() };
        g.set_frame(1_000_000 / fps.clamp(5, 240));
        g
    }

    fn new(state: &AppState) -> Self {
        let mut g = Self::for_fps(
            state.frame_guard.load(Ordering::Relaxed),
            state.frame_guard_fps.load(Ordering::Relaxed),
        );
        g.auto = state.frame_guard_auto.load(Ordering::Relaxed);
        g.retune(state);
        g
    }

    /// Re-sizes the spacings from the live measurement, when on automatic.
    ///
    /// Cheap enough to call before every event: one relaxed load, and the rest only
    /// runs when the measurement has actually moved.
    fn retune(&mut self, state: &AppState) {
        if !self.enabled || !self.auto {
            return;
        }
        let measured = state.perf_frame_us.load(Ordering::Relaxed);
        if measured == 0 {
            return; // nothing measured yet - the configured figure stands in
        }
        // 4 ms to 200 ms, i.e. 250 FPS down to 5.
        let frame = measured.clamp(4_000, 200_000);
        // Ignore wobble below 10 %: rewriting the spacings on every sample would
        // make the guard itself a source of jitter.
        if frame.abs_diff(self.frame_us) * 10 > self.frame_us {
            self.set_frame(frame);
        }
    }

    fn key_id(vk: u16) -> u64 {
        (1u64 << 40) | vk as u64
    }

    fn button_id(b: MouseButton) -> u64 {
        let n = match b {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::X1 => 3,
            MouseButton::X2 => 4,
        };
        (2u64 << 40) | n
    }

    fn stamp(list: &mut Vec<(u64, u64)>, id: u64, t: u64) {
        match list.iter_mut().find(|e| e.0 == id) {
            Some(e) => e.1 = t,
            None => list.push((id, t)),
        }
    }

    fn stamp_of(list: &[(u64, u64)], id: u64) -> Option<u64> {
        list.iter().find(|e| e.0 == id).map(|e| e.1)
    }

    /// Microseconds this event still has to wait before it may be sent.
    fn extra_wait(&self, kind: &InputEventKind, now: u64) -> u64 {
        if !self.enabled {
            return 0;
        }
        let ready = match kind {
            InputEventKind::Key { vk, down, .. } => {
                let id = Self::key_id(*vk);
                if *down {
                    Self::stamp_of(&self.up_at, id).map(|t| t + self.gap_us)
                } else {
                    Self::stamp_of(&self.down_at, id).map(|t| t + self.hold_us)
                }
            }
            InputEventKind::MouseButton { button, down, .. } => {
                let id = Self::button_id(*button);
                if *down {
                    let after_release =
                        Self::stamp_of(&self.up_at, id).map(|t| t + self.gap_us).unwrap_or(0);
                    Some(after_release.max(self.last_move_us + self.settle_us))
                } else {
                    Self::stamp_of(&self.down_at, id).map(|t| t + self.hold_us)
                }
            }
            _ => None,
        };
        ready.map(|r| r.saturating_sub(now)).unwrap_or(0)
    }

    /// Records that an event has just gone out.
    fn note_sent(&mut self, kind: &InputEventKind, now: u64) {
        if !self.enabled {
            return;
        }
        match kind {
            InputEventKind::Key { vk, down, .. } => {
                let id = Self::key_id(*vk);
                if *down {
                    Self::stamp(&mut self.down_at, id, now);
                } else {
                    Self::stamp(&mut self.up_at, id, now);
                }
            }
            InputEventKind::MouseButton { button, down, .. } => {
                let id = Self::button_id(*button);
                if *down {
                    Self::stamp(&mut self.down_at, id, now);
                } else {
                    Self::stamp(&mut self.up_at, id, now);
                }
            }
            InputEventKind::MouseMove { .. } => self.last_move_us = now,
            _ => {}
        }
    }

    /// The script steps move the cursor themselves, outside the event stream.
    fn note_move(&mut self, now: u64) {
        if self.enabled {
            self.last_move_us = now;
        }
    }
}

/// Sleeps for `us`, waking often enough that Stop still feels instant.
/// Returns false when playback should abort.
fn guard_sleep(state: &AppState, generation: u64, us: u64) -> bool {
    let deadline = Instant::now() + Duration::from_micros(us);
    loop {
        if state.stop_play.load(Ordering::Relaxed)
            || state.play_generation.load(Ordering::Relaxed) != generation
        {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        let left = (deadline - now).as_micros() as u64;
        if left > SPIN_THRESHOLD_US {
            std::thread::sleep(Duration::from_micros(
                left.saturating_sub(1_000).min(SLEEP_CHUNK_US).max(1),
            ));
        } else {
            spin_sleep::sleep(Duration::from_micros(left));
            return true;
        }
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
    vars: std::collections::HashMap<String, Value>,
    templates: std::collections::HashMap<String, Vec<Arc<vision::Template>>>,
    /// What OCR last read, for diagnosis.
    last_text: String,
    /// Where each template was last seen, so `NearLast` has somewhere to
    /// look. Per run: a position from an hour ago is a guess, not a hint.
    last_hit: std::collections::HashMap<String, (i32, i32)>,
    /// Whether each template counts as present, for the two-threshold decision.
    /// Keyed by template rather than by step: two steps watching the same picture
    /// are watching the same thing on screen, and should agree about it.
    latched: std::collections::HashMap<String, bool>,
    /// The last 32 answers per template, as a bitmask.
    history: std::collections::HashMap<String, u32>,
    /// How many `Call` steps are on the stack above this one.
    depth: u32,
    /// Macros already loaded by a `Call`, so a call inside a loop reads the file
    /// once rather than once per turn of the loop.
    called: std::collections::HashMap<String, Arc<MacroData>>,
}

/// How deep `Call` may nest.
///
/// This is what stands between a macro that calls itself and a stack overflow, and
/// under `panic = "abort"` a stack overflow is the process gone with keys held. Any
/// number would do; eight is past what a list of steps is worth expressing and
/// small enough that the log tells you what happened rather than scrolling past.
const MAX_CALL_DEPTH: u32 = 8;

/// Where a `Break` at `pc` should land: just past the innermost enclosing
/// `EndWhile`, or the end of the script when there is no loop around it.
///
/// Shared by the `Break` step and by the `Break` miss policy, which have to agree:
/// two spellings of the same jump that disagreed about nesting would be a very
/// quiet bug.
fn break_target(steps: &[ScriptStep], pc: usize) -> usize {
    let mut depth = 0usize;
    let mut j = pc + 1;
    while j < steps.len() {
        match steps[j].kind {
            StepKind::While { .. } => depth += 1,
            StepKind::EndWhile => {
                if depth == 0 {
                    return j + 1;
                }
                depth -= 1;
            }
            _ => {}
        }
        j += 1;
    }
    steps.len()
}

/// What the interpreter does next after a step that looked for something.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MissAct {
    /// It was there.
    Found,
    /// It was not, and the step says to walk on anyway.
    Next,
    /// It was not, and the step says to end the run.
    Stop,
    /// It was not, and the step says to leave the loop.
    Break,
    /// Stop or a new generation arrived while we were looking.
    Cancelled,
}

/// Everything a look at the screen needs to know beyond which picture to find.
struct MatchOpts {
    threshold: f64,
    lose_at: f64,
    stable_of: u32,
    stable_in: u32,
    area: SearchArea,
    /// Correlate outlines rather than greys.
    edge: bool,
}

impl MatchOpts {
    /// The plain case: one threshold, no memory.
    fn plain(threshold: f64, area: SearchArea, edge: bool) -> Self {
        Self { threshold, lose_at: 0.0, stable_of: 0, stable_in: 0, area, edge }
    }
}

/// Why a script run ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScriptEnd {
    Finished,
    Stopped,
    QuitApp,
}

/// Loads a template from `<data>/templates/<name>.png`, once per run.
/// What a template was cut out at, so it can be rescaled when the screen differs.
///
/// A picture snipped on a 150 % display is half again the size of the same button on
/// a 100 % one, and no threshold will bridge that. Recording the scale at capture time
/// costs one small file and removes the guessing.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TemplateMeta {
    /// Dots per inch of the display it was captured on. 96 is 100 %.
    pub dpi: u32,
}

fn meta_path(png: &std::path::Path) -> std::path::PathBuf {
    let mut p = png.to_path_buf();
    let ext = p.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    p.set_extension(format!("{ext}.json"));
    p
}

pub fn load_template_meta(png: &std::path::Path) -> Option<TemplateMeta> {
    let text = std::fs::read_to_string(meta_path(png)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_template_meta(png: &std::path::Path, meta: &TemplateMeta) {
    if let Ok(text) = serde_json::to_string_pretty(meta) {
        let _ = std::fs::write(meta_path(png), text);
    }
}

/// Every picture that counts as `name`.
///
/// A file gives one. A folder gives all the PNGs inside it, which is how one button
/// can be a normal state, a hovered state and a dark theme without three separate
/// steps in the script. The cost is linear in the number of variants, so this wants a
/// search area rather than the whole desktop.
fn load_template_set(name: &str) -> Vec<Arc<vision::Template>> {
    load_template_set_at(&paths::templates_dir().join(name), name)
}

fn load_template_set_at(base: &std::path::Path, name: &str) -> Vec<Arc<vision::Template>> {
    let base = base.to_path_buf();
    if base.is_dir() {
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&base)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")))
            .collect();
        // Alphabetical, so which variant wins a tie does not depend on the file system.
        files.sort();
        let out: Vec<_> = files.iter().filter_map(|p| load_one_template(p, name)).collect();
        if out.is_empty() {
            warn!("template folder '{}' holds no PNGs", base.display());
        }
        return out;
    }
    let mut path = base;
    if path.extension().is_none() {
        path.set_extension("png");
    }
    load_one_template(&path, name).into_iter().collect()
}

fn load_one_template(path: &std::path::Path, name: &str) -> Option<Arc<vision::Template>> {
    match image::open(path) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            let mut raw = rgba.into_raw();
            let (mut w, mut h) = (w, h);
            // The screen it was cut from against the screen it will be looked for on.
            // Only when the first of those is actually recorded: a template with no
            // sidecar is one this version never saw saved, and guessing a scale for it
            // would break every picture made before this release.
            let known_dpi = load_template_meta(path).map(|m| m.dpi.max(1));
            let now = platform::current_dpi().max(1);
            let scale = now as f64 / known_dpi.unwrap_or(now) as f64;
            if !(0.98..=1.02).contains(&scale) && (0.2..=5.0).contains(&scale) {
                let (nw, nh) = (
                    ((w as f64 * scale).round() as u32).max(2),
                    ((h as f64 * scale).round() as u32).max(2),
                );
                raw = vision::resize_rgba(&raw, w, h, nw, nh);
                info!(
                    "template '{}' rescaled {w}x{h} -> {nw}x{nh} ({:?} dpi -> {now} dpi)",
                    path.display(),
                    known_dpi
                );
                w = nw;
                h = nh;
            }
            Some(Arc::new(vision::Template { w, h, rgba: raw, name: name.to_string() }))
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

    fn template_set(&mut self, name: &str) -> Vec<Arc<vision::Template>> {
        if !self.templates.contains_key(name) {
            let t = load_template_set(name);
            self.templates.insert(name.to_string(), t);
        }
        self.templates.get(name).cloned().unwrap_or_default()
    }

    /// Searches the screen and records the result in `match_x` / `match_y` / `match_score`.
    /// Turns a search area into the rectangle to capture, clamped to the desktop.
    fn resolve_area(&self, name: &str, area: &SearchArea, tw: u32, th: u32) -> (i32, i32, i32, i32) {
        let full = platform::virtual_screen_rect();
        let want = match area {
            SearchArea::FullScreen => full,
            SearchArea::ActiveWindow => platform::foreground_rect().unwrap_or(full),
            SearchArea::Rect { x, y, w, h } => (*x, *y, *w, *h),
            SearchArea::NearLast { margin } => match self.last_hit.get(name) {
                Some((cx, cy)) => {
                    let m = (*margin).max(8);
                    let w = tw as i32 + m * 2;
                    let h = th as i32 + m * 2;
                    (cx - w / 2, cy - h / 2, w, h)
                }
                // Nothing seen yet, so there is nowhere to look near.
                None => full,
            },
            // Resolved into a rectangle before this is reached; arriving here means
            // the anchor was not found, and the whole screen is the safe answer.
            SearchArea::NearAnchor { .. } => full,
        };
        // A window can be off-screen and a hand-typed rectangle can be nonsense; the
        // capture has to stay inside the desktop either way.
        let x = want.0.clamp(full.0, full.0 + full.2 - 1);
        let y = want.1.clamp(full.1, full.1 + full.3 - 1);
        let w = want.2.clamp(1, full.0 + full.2 - x);
        let h = want.3.clamp(1, full.1 + full.3 - y);
        (x, y, w, h)
    }

    /// Turns an anchored area into a plain rectangle by finding the anchor first.
    ///
    /// The anchor is looked for near where it was last seen before the whole screen
    /// is swept, so a settled interface costs one cheap look. Its own coordinates
    /// are scaffolding: `match_x` and friends belong to the target, and are put back
    /// exactly as they were.
    fn anchor_area(&mut self, area: &SearchArea, threshold: f64, edge: bool) -> Option<SearchArea> {
        let SearchArea::NearAnchor { anchor, dx, dy, w, h } = area else {
            return Some(area.clone());
        };
        if anchor.trim().is_empty() {
            warn!("an anchored search has no anchor named");
            return None;
        }
        let name = anchor.clone();
        let saved: Vec<(String, Option<Value>)> = ["match_x", "match_y", "match_score"]
            .iter()
            .map(|k| (k.to_string(), self.vars.get(*k).cloned()))
            .collect();
        let opts = MatchOpts::plain(threshold, SearchArea::NearLast { margin: 80 }, edge);
        let found = self.find_image_into(&name, &opts, None);
        for (k, v) in saved {
            match v {
                Some(val) => self.vars.insert(k, val),
                None => self.vars.remove(&k),
            };
        }
        if !found {
            return None;
        }
        let (ax, ay) = *self.last_hit.get(&name)?;
        Some(SearchArea::Rect { x: ax + dx, y: ay + dy, w: *w, h: *h })
    }

    /// Looks for a template and records where it landed.
    ///
    /// `prefix` names the variables written: the old `match_` names are still produced
    /// so existing macros keep working, and a `Find image` step asks for its own.
    fn find_image_into(
        &mut self,
        name: &str,
        opts: &MatchOpts,
        prefix: Option<&str>,
    ) -> bool {
        let set = self.template_set(name);
        let Some(first) = set.first().cloned() else {
            if let Some(p) = prefix {
                self.vars.insert(format!("{p}.found"), Value::Num(0.0));
            }
            return false;
        };
        // An anchored area is resolved before anything is captured: without its
        // anchor there is nowhere to look, and falling back to the whole screen
        // would defeat the point of having said where to look.
        let Some(area) = self.anchor_area(&opts.area, opts.threshold, opts.edge) else {
            self.vars.insert("match_score".into(), Value::Num(0.0));
            if let Some(p) = prefix {
                self.vars.insert(format!("{p}.found"), Value::Num(0.0));
                self.vars.insert(format!("{p}.score"), Value::Num(0.0));
            }
            return false;
        };
        // One capture, every variant scored against it: the screen is the expensive
        // part, and taking it once per variant would make a folder of five cost five
        // times as much for no reason.
        let edge = opts.edge;
        let look = |ctx: &Self, area: &SearchArea| {
            let (rx, ry, rw, rh) = ctx.resolve_area(name, area, first.w, first.h);
            let frame = platform::capture(rx, ry, rw, rh)?;
            set.iter()
                .filter_map(|t| vision::find_mode(&frame, t, false, edge).map(|h| (h, t.w, t.h)))
                .max_by(|a, b| a.0.score.total_cmp(&b.0.score))
        };
        let mut hit = look(self, &area);
        // The cascade: a guess about where something was is worth trying first and
        // worth abandoning quickly. A pinned rectangle is not a guess, so it is not
        // widened - the user meant that rectangle.
        if matches!(area, SearchArea::NearLast { .. })
            && hit.as_ref().is_none_or(|(h, _, _)| (h.score as f64) < opts.threshold)
        {
            hit = look(self, &SearchArea::FullScreen);
        }

        let score = hit.as_ref().map_or(0.0, |(h, _, _)| h.score as f64);
        note_sighting(|s| {
            let (rx, ry, rw, rh) = self.resolve_area(name, &area, first.w, first.h);
            s.area = Some((rx, ry, rw, rh));
            s.hit = hit.as_ref().map(|(h, w, ht)| {
                (h.x - *w as i32 / 2, h.y - *ht as i32 / 2, *w as i32, *ht as i32, h.score)
            });
            s.note = format!("{name}  {score:.3} / {:.2}", opts.threshold);
        });
        if let Some((h, _, _)) = &hit {
            self.vars.insert("match_x".into(), Value::Num(h.x as f64));
            self.vars.insert("match_y".into(), Value::Num(h.y as f64));
        }
        self.vars.insert("match_score".into(), Value::Num(score));

        // Two thresholds and a history, but only for the callers that asked: a click
        // step that shares a template must not leave state behind for a wait step.
        let stateful = opts.lose_at > 0.0 || opts.stable_in > 1;
        let ok = if stateful {
            let was = self.latched.get(name).copied().unwrap_or(false);
            let raw = match_decision(score, opts.threshold, opts.lose_at, was);
            let hist = self.history.entry(name.to_string()).or_insert(0);
            let settled = stable_enough(hist, raw, opts.stable_of, opts.stable_in);
            self.latched.insert(name.to_string(), settled);
            settled
        } else {
            score >= opts.threshold
        };
        if ok {
            if let Some((h, _, _)) = &hit {
                self.last_hit.insert(name.to_string(), (h.x, h.y));
            }
        }
        if let Some(p) = prefix {
            self.vars
                .insert(format!("{p}.found"), Value::Num(if ok { 1.0 } else { 0.0 }));
            let (sc, hx, hy, hw, hh) = match &hit {
                Some((h, w, ht)) => (h.score as f64, h.x as f64, h.y as f64, *w, *ht),
                None => (0.0, 0.0, 0.0, first.w, first.h),
            };
            self.vars.insert(format!("{p}.score"), Value::Num(sc));
            self.vars.insert(format!("{p}.x"), Value::Num(hx));
            self.vars.insert(format!("{p}.y"), Value::Num(hy));
            // The variant that won, not the first one: a hovered button can be a
            // different size from its resting state.
            self.vars.insert(format!("{p}.w"), Value::Num(hw as f64));
            self.vars.insert(format!("{p}.h"), Value::Num(hh as f64));
        }
        ok
    }

    fn find_image(&mut self, name: &str, opts: &MatchOpts) -> bool {
        self.find_image_into(name, opts, None)
    }

    /// Publishes the state of the run and, in step mode, waits to be let through.
    ///
    /// Returns false if the run should end. Called once per step, and when nobody
    /// is watching it is a relaxed load and a return.
    fn step_gate(&self, pc: usize, step: &ScriptStep, total_events: usize) -> bool {
        let stepping = self.state.step_mode.load(Ordering::Relaxed);
        if !watching_vars() && !stepping {
            return true;
        }
        let s = get_strings(0, Lang::En);
        let mut rows: Vec<(String, String)> = self
            .vars
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string()))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let text = describe_step(step, s, total_events);
        let depth = self.depth;
        note_script_view(|v| {
            v.vars = rows;
            v.pc = pc;
            v.step = text;
            v.depth = depth;
            v.running = true;
            v.waiting = stepping;
        });
        if !stepping {
            return true;
        }
        // Parked. Stop, a new generation, and turning step mode off all release it,
        // so there is no way to leave a run stuck here with no button to press.
        loop {
            if self.stopping() {
                return false;
            }
            if self.state.step_once.swap(false, Ordering::Relaxed) {
                break;
            }
            if !self.state.step_mode.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        note_script_view(|v| v.waiting = false);
        true
    }

    /// Runs a look, retries it as the policy asks, and says what to do if it never
    /// came back true.
    ///
    /// One place for all six steps that can fail to find something, so that "stop
    /// the script" means the same thing in each of them and a retry counts the same
    /// way. `what` is only for the log, and the log is the point: a run that ended
    /// because a picture was missing should say which picture.
    fn attempt(
        &mut self,
        miss: OnMiss,
        what: &str,
        mut look: impl FnMut(&mut Self) -> bool,
    ) -> MissAct {
        if look(self) {
            return MissAct::Found;
        }
        let (times, delay_ms) = miss.retries();
        for n in 1..=times {
            if self.stopping() {
                return MissAct::Cancelled;
            }
            info!("{what}: not found, trying again ({n} of {times})");
            if delay_ms > 0 && !self.nap(delay_ms) {
                return MissAct::Cancelled;
            }
            if look(self) {
                return MissAct::Found;
            }
        }
        match miss {
            OnMiss::Continue => MissAct::Next,
            OnMiss::Stop => {
                warn!("{what}: not found - stopping the script, as the step asks");
                MissAct::Stop
            }
            OnMiss::Break => {
                info!("{what}: not found - leaving the loop, as the step asks");
                MissAct::Break
            }
            OnMiss::Retry { times, .. } => {
                warn!(
                    "{what}: still not found after {times} more tries - stopping the script"
                );
                MissAct::Stop
            }
        }
    }

    /// Loads a macro named by a `Call` step, once per run.
    ///
    /// Looked for next to the macro that named it first, then in the data folder.
    /// A bare name gets `.json`, because that is what the save dialog offers and
    /// typing the extension is a thing to get wrong rather than a thing to decide.
    fn call_target(&mut self, path: &str) -> Option<Arc<MacroData>> {
        let key = path.to_string();
        if let Some(hit) = self.called.get(&key) {
            return Some(hit.clone());
        }
        let raw = expand_vars(path, &self.vars);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            warn!("a Call step names no file");
            return None;
        }
        let mut candidate = std::path::PathBuf::from(trimmed);
        if candidate.extension().is_none() {
            candidate.set_extension("json");
        }
        let mut tries: Vec<std::path::PathBuf> = Vec::new();
        if candidate.is_absolute() {
            tries.push(candidate.clone());
        } else {
            if let Some(dir) =
                self.state.current_path.lock().as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf()))
            {
                tries.push(dir.join(&candidate));
            }
            tries.push(paths::data_dir().join(&candidate));
            tries.push(paths::sub_dir("macros").join(&candidate));
        }
        for t in &tries {
            match load_macro(t) {
                Ok(data) => {
                    info!("call: loaded '{}'", t.display());
                    let arc = Arc::new(data);
                    self.called.insert(key, arc.clone());
                    return Some(arc);
                }
                Err(e) => tracing::debug!("call: '{}' did not load: {e}", t.display()),
            }
        }
        warn!(
            "call: '{trimmed}' was not found - looked in {}",
            tries
                .iter()
                .map(|t| t.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        None
    }

    fn eval(&mut self, cond: &Condition) -> bool {
        match cond {
            Condition::Always => true,
            Condition::Var { name, cmp, value } => {
                let cur = self.vars.get(name).cloned().unwrap_or_default();
                cmp.test_values(&cur, value)
            }
            Condition::Image {
                template,
                threshold,
                area,
                lose_at,
                stable_of,
                stable_in,
                edge,
            } => {
                let opts = MatchOpts {
                    threshold: *threshold,
                    lose_at: *lose_at,
                    stable_of: *stable_of,
                    stable_in: *stable_in,
                    area: area.clone(),
                    edge: *edge,
                };
                self.find_image(template, &opts)
            }
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
            Condition::Process { name } => platform::process_running(name),
            // One look, not a wait: a `Wait for` step is already a loop, and an
            // `If` should answer about now rather than about the next two seconds.
            Condition::Element { query } => uia::find(query, 0).is_some(),
            Condition::Text { x, y, w, h, needle, prep } => {
                note_sighting(|s| s.text = Some((*x, *y, *w, *h)));
                // The needle is the format here: a reading that does not contain it
                // is the wrong reading, so `Auto` has something to judge by.
                let expect = ocr::Expect::Pattern(format!("*{needle}*"));
                match ocr::read_region_as(*x, *y, *w, *h, *prep, &expect) {
                    Ok(r) => {
                        let all = r.text();
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
    guard: &mut FrameGuard,
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
        // A stall - the game pegging the CPU, a shader hitch, a page fault storm -
        // leaves the schedule behind. Racing through the backlog would dump several
        // events into one frame, which is precisely what a struggling game cannot
        // absorb, so the schedule slips instead of catching up.
        let now_late = start.elapsed().as_micros() as u64;
        if guard.slip_us > 0 && now_late > due.saturating_add(guard.slip_us) {
            let late = now_late - due;
            info!("script replay is {} ms late - slipping the schedule", late / 1000);
            due = due.saturating_add(late);
        }

        // Whatever the guard adds is added to the schedule too, so the events after
        // this one keep their spacing rather than being fast-forwarded.
        guard.retune(ctx.state);
        let extra = guard.extra_wait(&ev.kind, now_us());
        if extra > 0 {
            ctx.state.fg_added_us.fetch_add(extra, Ordering::Relaxed);
            if !guard_sleep(ctx.state, ctx.generation, extra) {
                return false;
            }
            due = due.saturating_add(extra);
        }

        #[cfg(windows)]
        unsafe {
            send_input_event(&ev.kind, ctx.state, pressed, ctx.map, mover);
        }
        guard.note_sent(&ev.kind, now_us());
        due = due.saturating_add(mover.take_spent());
    }
    true
}

/// Sends one event, first waiting out whatever the frame guard still requires.
/// Returns false when playback should stop.
fn send_guarded(
    ctx: &ScriptCtx<'_>,
    kind: &InputEventKind,
    pressed: &mut PressedInputs,
    mover: &mut MoveEngine,
    guard: &mut FrameGuard,
) -> bool {
    guard.retune(ctx.state);
    let extra = guard.extra_wait(kind, now_us());
    if extra > 0 {
        ctx.state.fg_added_us.fetch_add(extra, Ordering::Relaxed);
        if !guard_sleep(ctx.state, ctx.generation, extra) {
            return false;
        }
    }
    #[cfg(windows)]
    unsafe {
        send_input_event(kind, ctx.state, pressed, CoordMap::IDENTITY, mover);
    }
    #[cfg(not(windows))]
    {
        let _ = &pressed;
    }
    // Script clicks are not on a schedule, so the path time is simply discarded
    // rather than left to shift the next `Play events`.
    let _ = mover.take_spent();
    guard.note_sent(kind, now_us());
    true
}

/// Presses and releases one mouse button wherever the cursor already is.
///
/// The button events carry no coordinates on purpose: the caller has just moved
/// the cursor, and re-sending the position would move a second time and re-roll
/// the aim spread, landing the click a few pixels from where it was aimed.
fn click_guarded(
    ctx: &ScriptCtx<'_>,
    button: MouseButton,
    pressed: &mut PressedInputs,
    mover: &mut MoveEngine,
    guard: &mut FrameGuard,
) -> bool {
    let down = InputEventKind::MouseButton { button, down: true, x: 0, y: 0 };
    let up = InputEventKind::MouseButton { button, down: false, x: 0, y: 0 };
    send_guarded(ctx, &down, pressed, mover, guard)
        && send_guarded(ctx, &up, pressed, mover, guard)
}

/// Starts a program, or opens a document, folder, shortcut or URL.
///
/// Only real executables go through `CreateProcess`. Everything else is handed to
/// the shell, because `CreateProcess` cannot open a `.lnk` shortcut, a URL or a
/// document - it would just fail, silently, which is the worst possible outcome
/// for a step that runs while nobody is watching.
/// A file as text, capped so that naming the wrong path cannot pull a gigabyte
/// into a variable.
fn read_text_file(path: &str) -> String {
    use std::io::Read as _;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            warn!("could not read {path}: {e}");
            return String::new();
        }
    };
    let mut buf = Vec::new();
    match file.take(TEXT_FILE_CAP).read_to_end(&mut buf) {
        Ok(n) => {
            if n as u64 == TEXT_FILE_CAP {
                warn!("{path} was longer than {TEXT_FILE_CAP} bytes and was cut short");
            }
            // Lossy on purpose: a log written by something else is not always UTF-8,
            // and refusing to read it at all would be worse than a few question marks.
            String::from_utf8_lossy(&buf).into_owned()
        }
        Err(e) => {
            warn!("could not read {path}: {e}");
            String::new()
        }
    }
}

/// Writes text to a file, replacing it or adding to the end.
fn write_text_file(path: &str, text: &str, append: bool) {
    use std::io::Write as _;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        // Truncating and appending at once is a contradiction, and Windows returns
        // an error for it rather than picking one.
        .truncate(!append)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()));
    if let Err(e) = result {
        warn!("could not write {path}: {e}");
    }
}

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
    guard: &mut FrameGuard,
) -> ScriptEnd {
    // Only for the log lines a miss policy writes; the interpreter itself has no
    // opinions about language.
    let s = get_strings(0, Lang::En);
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
        if !ctx.step_gate(pc, step, ctx.data.events.len()) {
            return ScriptEnd::Stopped;
        }
        if ctx.state.skip_step.swap(false, Ordering::Relaxed) {
            info!("skipping step #{pc}");
            pc += 1;
            continue;
        }

        match &step.kind {
            StepKind::PlayEvents { from, to } => {
                if !play_event_range(ctx, *from, *to, pressed, mover, guard) {
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
            StepKind::WaitFor { cond, appear, timeout_ms, miss } => {
                // A wait that gave up and a wait that succeeded used to be the same
                // thing to everything downstream. Now the wait answers, and the
                // policy decides what the answer means.
                let (c, want, limit, m) = (cond.clone(), *appear, *timeout_ms, *miss);
                let act = ctx.attempt(m, &format!("wait for {}", describe_condition(&c, s)), |ctx| {
                    let deadline = Instant::now() + Duration::from_millis(limit);
                    loop {
                        if ctx.stopping() {
                            return false;
                        }
                        // Skipping counts as success: the hotkey means "get on with
                        // it", and firing a stop policy because the user asked to move
                        // on would be the opposite of what they asked for.
                        if ctx.state.skip_step.swap(false, Ordering::Relaxed) {
                            info!("wait skipped");
                            return true;
                        }
                        let c = c.clone();
                        if ctx.eval(&c) == want {
                            return true;
                        }
                        if Instant::now() >= deadline {
                            info!("wait timed out");
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(120));
                    }
                });
                match act {
                    MissAct::Found | MissAct::Next => pc += 1,
                    MissAct::Stop => return ScriptEnd::Finished,
                    MissAct::Break => pc = break_target(steps, pc),
                    MissAct::Cancelled => return ScriptEnd::Stopped,
                }
            }
            StepKind::FindImage { template, threshold, area, var, edge, miss } => {
                let (name, v) = (template.clone(), var.clone());
                let opts = MatchOpts::plain(*threshold, area.clone(), *edge);
                let m = *miss;
                let act = ctx.attempt(m, &format!("find image '{name}'"), |ctx| {
                    ctx.find_image_into(&name, &opts, Some(&v))
                });
                match act {
                    MissAct::Found | MissAct::Next => pc += 1,
                    MissAct::Stop => return ScriptEnd::Finished,
                    MissAct::Break => pc = break_target(steps, pc),
                    MissAct::Cancelled => return ScriptEnd::Stopped,
                }
            }
            StepKind::ClickImage { template, threshold, button, area, edge, miss } => {
                let name = template.clone();
                let opts = MatchOpts::plain(*threshold, area.clone(), *edge);
                let (b, m) = (*button, *miss);
                let act = ctx.attempt(m, &format!("click image '{name}'"), |ctx| {
                    ctx.find_image(&name, &opts)
                });
                match act {
                    MissAct::Found => {
                        let x = ctx.vars.get("match_x").map_or(0.0, |v| v.as_num()) as i32;
                        let y = ctx.vars.get("match_y").map_or(0.0, |v| v.as_num()) as i32;
                        mover.goto(x, y);
                        guard.note_move(now_us());
                        if !click_guarded(ctx, b, pressed, mover, guard) {
                            return ScriptEnd::Stopped;
                        }
                        pc += 1;
                    }
                    MissAct::Next => pc += 1,
                    MissAct::Stop => return ScriptEnd::Finished,
                    MissAct::Break => pc = break_target(steps, pc),
                    MissAct::Cancelled => return ScriptEnd::Stopped,
                }
            }
            StepKind::Click { x, y, button } => {
                let (mx, my) = ctx.map.map(*x, *y);
                mover.goto(mx, my);
                guard.note_move(now_us());
                if !click_guarded(ctx, *button, pressed, mover, guard) {
                    return ScriptEnd::Stopped;
                }
                pc += 1;
            }
            StepKind::Key { vk, down } => {
                let kind =
                    InputEventKind::Key { vk: *vk, scan: 0, down: *down, extended: false };
                if !send_guarded(ctx, &kind, pressed, mover, guard) {
                    return ScriptEnd::Stopped;
                }
                pc += 1;
            }
            StepKind::SetVar { name, op, value } => {
                let cur = ctx.vars.get(name).cloned().unwrap_or_default();
                // Text on the right may name other variables, which is what makes
                // building a message out of pieces possible at all.
                let rhs = match value {
                    Value::Str(t) => Value::Str(expand_vars(t, &ctx.vars)),
                    n => n.clone(),
                };
                ctx.vars.insert(name.clone(), op.apply_values(&cur, &rhs));
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
            StepKind::Break => pc = break_target(steps, pc),
            StepKind::Run { path, args } => {
                run_program(
                    &expand_vars(path, &ctx.vars),
                    &expand_vars(args, &ctx.vars),
                );
                pc += 1;
            }
            StepKind::Call { path, miss } => {
                let (want, m) = (path.clone(), *miss);
                if ctx.depth >= MAX_CALL_DEPTH {
                    warn!(
                        "call '{want}' refused: already {} deep, and {MAX_CALL_DEPTH} is \
                         the limit - a macro that calls itself would take the process \
                         down with it",
                        ctx.depth
                    );
                    match m {
                        OnMiss::Break => {
                            pc = break_target(steps, pc);
                            continue;
                        }
                        OnMiss::Continue => {
                            pc += 1;
                            continue;
                        }
                        _ => return ScriptEnd::Finished,
                    }
                }
                let mut loaded = None;
                let act = ctx.attempt(m, &format!("call '{want}'"), |ctx| {
                    loaded = ctx.call_target(&want);
                    loaded.is_some()
                });
                match act {
                    MissAct::Found => {}
                    MissAct::Next => {
                        pc += 1;
                        continue;
                    }
                    MissAct::Stop => return ScriptEnd::Finished,
                    MissAct::Break => {
                        pc = break_target(steps, pc);
                        continue;
                    }
                    MissAct::Cancelled => return ScriptEnd::Stopped,
                }
                let Some(child) = loaded else {
                    pc += 1;
                    continue;
                };
                // The variables, the template cache and the sighting history move
                // into the callee and come back out. Sharing them is what makes a
                // subroutine useful without inventing parameters: the caller sets
                // `target` before the call and reads `result` after it.
                let end = {
                    let mut inner = ScriptCtx {
                        state: ctx.state,
                        data: &child,
                        generation: ctx.generation,
                        map: ctx.map,
                        vars: std::mem::take(&mut ctx.vars),
                        templates: std::mem::take(&mut ctx.templates),
                        last_text: std::mem::take(&mut ctx.last_text),
                        last_hit: std::mem::take(&mut ctx.last_hit),
                        latched: std::mem::take(&mut ctx.latched),
                        history: std::mem::take(&mut ctx.history),
                        depth: ctx.depth + 1,
                        called: std::mem::take(&mut ctx.called),
                    };
                    let end = run_script(&mut inner, pressed, mover, guard);
                    ctx.vars = std::mem::take(&mut inner.vars);
                    ctx.templates = std::mem::take(&mut inner.templates);
                    ctx.last_text = std::mem::take(&mut inner.last_text);
                    ctx.last_hit = std::mem::take(&mut inner.last_hit);
                    ctx.latched = std::mem::take(&mut inner.latched);
                    ctx.history = std::mem::take(&mut inner.history);
                    ctx.called = std::mem::take(&mut inner.called);
                    end
                };
                match end {
                    // A callee that ran out of steps has returned. A `Break` inside
                    // it belonged to its own loops and does not reach out here.
                    ScriptEnd::Finished => pc += 1,
                    ScriptEnd::Stopped => return ScriptEnd::Stopped,
                    ScriptEnd::QuitApp => return ScriptEnd::QuitApp,
                }
            }
            StepKind::Exit => return ScriptEnd::QuitApp,
            StepKind::Log { text } => {
                info!("script: {}", expand_vars(text, &ctx.vars));
                pc += 1;
            }
            StepKind::FindElement { query, var, timeout_ms, miss } => {
                let (q, t, m) = (query.clone(), *timeout_ms, *miss);
                let mut found = None;
                let act = ctx.attempt(m, &format!("find element \"{}\"", q.name), |_| {
                    found = uia::find(&q, t);
                    found.is_some()
                });
                match act {
                    MissAct::Found | MissAct::Next => {}
                    MissAct::Stop => return ScriptEnd::Finished,
                    MissAct::Break => {
                        pc = break_target(steps, pc);
                        continue;
                    }
                    MissAct::Cancelled => return ScriptEnd::Stopped,
                }
                let query = &q;
                note_sighting(|s| {
                    s.element = found.as_ref().map(|f| {
                        (f.x - f.w / 2, f.y - f.h / 2, f.w, f.h)
                    });
                    s.note = match &found {
                        Some(f) => format!("element \"{}\"", f.name),
                        None => format!("no element matched \"{}\"", query.name),
                    };
                });
                let ok = found.is_some();
                ctx.vars.insert(format!("{var}.found"), Value::Num(f64::from(ok)));
                match &found {
                    Some(f) => {
                        info!("element '{}' at {},{} ({}x{})", f.name, f.x, f.y, f.w, f.h);
                        // The value if it has one, the name if it does not: a text
                        // box holds its contents, a button holds its label.
                        let text =
                            if f.value.is_empty() { f.name.clone() } else { f.value.clone() };
                        ctx.vars.insert(var.clone(), Value::Str(text));
                        ctx.vars.insert(format!("{var}.name"), Value::Str(f.name.clone()));
                        ctx.vars.insert(format!("{var}.x"), Value::Num(f.x as f64));
                        ctx.vars.insert(format!("{var}.y"), Value::Num(f.y as f64));
                        ctx.vars.insert(format!("{var}.w"), Value::Num(f.w as f64));
                        ctx.vars.insert(format!("{var}.h"), Value::Num(f.h as f64));
                    }
                    None => {
                        ctx.vars.insert(var.clone(), Value::Str(String::new()));
                    }
                }
                pc += 1;
            }
            StepKind::ClickElement { query, button, invoke, timeout_ms, miss } => {
                // Asking the application to press it comes first when it was asked
                // for; a control with nothing to invoke falls through to a real
                // click on the rectangle, which is what the fallback is for.
                let (q, t, inv, b, m) = (query.clone(), *timeout_ms, *invoke, *button, *miss);
                let mut pressed_it = false;
                let mut target = None;
                let act = ctx.attempt(m, &format!("press element \"{}\"", q.name), |_| {
                    if inv && uia::press(&q, t).is_some() {
                        pressed_it = true;
                        return true;
                    }
                    target = uia::find(&q, t);
                    target.is_some()
                });
                match act {
                    MissAct::Found => {
                        if !pressed_it {
                            if let Some(f) = target {
                                mover.goto(f.x, f.y);
                                guard.note_move(now_us());
                                if !click_guarded(ctx, b, pressed, mover, guard) {
                                    return ScriptEnd::Stopped;
                                }
                            }
                        }
                        pc += 1;
                    }
                    MissAct::Next => pc += 1,
                    MissAct::Stop => return ScriptEnd::Finished,
                    MissAct::Break => pc = break_target(steps, pc),
                    MissAct::Cancelled => return ScriptEnd::Stopped,
                }
            }
            StepKind::ReadText { x, y, w, h, var, prep } => {
                note_sighting(|s| s.text = Some((*x, *y, *w, *h)));
                match ocr::read_region_as(*x, *y, *w, *h, *prep, &ocr::Expect::Any) {
                    Ok(r) => {
                        let all = r.text();
                        info!(
                            "ocr read '{}' [{:?} q{:.2}] -> {var}",
                            all.replace('\n', " / "),
                            r.prep,
                            r.quality
                        );
                        ctx.vars.insert(format!("{var}.quality"), Value::Num(r.quality));
                        ctx.vars.insert(var.clone(), Value::Str(all.clone()));
                        ctx.last_text = all;
                    }
                    Err(e) => warn!("ocr failed: {e}"),
                }
                pc += 1;
            }
            StepKind::GetText { source, var } => {
                let text = match source {
                    TextSource::Clipboard => platform::clipboard_text(),
                    TextSource::WindowTitle => {
                        platform::foreground_title().unwrap_or_default()
                    }
                    TextSource::ProcessName => platform::foreground_process(),
                    TextSource::File(path) => {
                        read_text_file(&expand_vars(path, &ctx.vars))
                    }
                };
                info!("{var} = \"{}\"", text.replace('\n', " / "));
                ctx.vars.insert(var.clone(), Value::Str(text));
                pc += 1;
            }
            StepKind::PutText { sink, text } => {
                let out = expand_vars(text, &ctx.vars);
                match sink {
                    TextSink::Clipboard => {
                        // Another application holding the clipboard open is common
                        // and temporary, so this is a warning rather than a stop.
                        if !platform::set_clipboard_text(&out) {
                            warn!("the clipboard would not take the text");
                        }
                    }
                    TextSink::File { path, append } => {
                        write_text_file(&expand_vars(path, &ctx.vars), &out, *append)
                    }
                }
                pc += 1;
            }
            StepKind::ReadNumber { x, y, w, h, var, prep, expect } => {
                note_sighting(|s| s.text = Some((*x, *y, *w, *h)));
                match ocr::read_region_as(*x, *y, *w, *h, *prep, expect) {
                    Ok(r) => {
                        let all = r.text();
                        // A reading that does not fit the format asked for is not a
                        // small error, it is a different number. Leaving the variable
                        // alone lets the script see the old value and decide, which
                        // beats silently writing a zero.
                        match ocr::value_of(expect, &all) {
                            Some(value) if ocr::accepts(expect, &all) => {
                                info!(
                                    "ocr read '{}' [{:?} q{:.2}] -> {var} = {value}",
                                    all.replace('\n', " / "),
                                    r.prep,
                                    r.quality
                                );
                                ctx.vars.insert(var.clone(), Value::Num(value));
                            }
                            _ => warn!(
                                "ocr read '{}' [{:?} q{:.2}] does not fit {:?} - {var} kept",
                                all.replace('\n', " / "),
                                r.prep,
                                r.quality,
                                expect
                            ),
                        }
                        ctx.vars.insert(format!("{var}.quality"), Value::Num(r.quality));
                        ctx.last_text = all;
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
    let _live = selftest::enter_playback();
    if data.is_empty() {
        state.playing.store(false, Ordering::Relaxed);
        return;
    }

    virtual_desktop::init_thread();
    platform::begin_high_res_timer();
    state.pixel_triggered.store(false, Ordering::Relaxed);
    state.fg_added_us.store(0, Ordering::Relaxed);

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
        let mut guard = FrameGuard::new(&state);
        let mut ctx = ScriptCtx {
            state: &state,
            data: &data,
            generation,
            map,
            vars: data.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            templates: Default::default(),
            last_text: String::new(),
            last_hit: std::collections::HashMap::new(),
            latched: std::collections::HashMap::new(),
            history: std::collections::HashMap::new(),
            depth: 0,
            called: std::collections::HashMap::new(),
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
            match run_script(&mut ctx, &mut pressed, &mut mover, &mut guard) {
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
        // A full-screen grab holds 14 MB of committed bitmap; there is no reason
        // for it to survive the run that asked for it.
        platform::release_capture_cache();
        // The variables stay on screen after the run - the last values are usually
        // the interesting ones - but the window has to stop claiming it is live.
        note_script_view(|v| {
            v.running = false;
            v.waiting = false;
        });
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
    let mut guard = FrameGuard::new(&state);
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
        let on_desktop = virtual_desktop::is_app_on_active_desktop_cached(platform::app_hwnd())
            && !virtual_desktop::shell_switcher_in_front();
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

        // Falling behind is normal in small amounts. Falling a long way behind means
        // the machine stalled, and firing the whole backlog at once would put several
        // events into a single frame - the one thing a slow game cannot take. Slip
        // the schedule to now instead of racing it.
        let now_late = elapsed_us!();
        if guard.slip_us > 0 && now_late > due.saturating_add(guard.slip_us) {
            let late = now_late - due;
            info!("playback is {} ms late - slipping the schedule", late / 1000);
            cycle_start_us = cycle_start_us.saturating_add(late);
            due = due.saturating_add(late);
            selftest::note_slip(late);
        }

        // Anything the guard adds shifts the schedule with it, so the events after
        // this one keep their spacing instead of being fast-forwarded.
        guard.retune(&state);
        let extra = guard.extra_wait(&ev.kind, now_us());
        if extra > 0 {
            state.fg_added_us.fetch_add(extra, Ordering::Relaxed);
            if !guard_sleep(&state, generation, extra) {
                break;
            }
            cycle_start_us = cycle_start_us.saturating_add(extra);
            due = due.saturating_add(extra);
        }

        if selftest::dry() {
            selftest::note(index, due, elapsed_us!());
        }

        #[cfg(windows)]
        unsafe {
            send_input_event(&ev.kind, &state, &mut pressed, map, &mut mover);
        }
        guard.note_sent(&ev.kind, now_us());
        // Drawing a curved path costs real time. Charging it to the schedule stops
        // the events behind it from bunching up to make the difference back.
        cycle_start_us = cycle_start_us.saturating_add(mover.take_spent());

        prev_scaled_t = scaled_t;
        index += 1;
    }

    pressed.release_all(&state);
    platform::end_high_res_timer();
    platform::release_capture_cache();
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
    selftest::note_input(kind);
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
                if !crate::selftest::dry() {
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
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
                    if !crate::selftest::dry() {
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
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
                if !crate::selftest::dry() {
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
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
                if !crate::selftest::dry() {
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
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
    state.click_shots.lock().clear();
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
    // A "let one step through" left over from a previous run would spend itself on
    // this run's first step, which is the one moment somebody stepping through a
    // script most wants to see.
    state.step_once.store(false, Ordering::Relaxed);
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
    // Set once this thread has taken a screen grab, which is the only reason it
    // would be holding a bitmap - and, with the fast path, a Direct3D device and a
    // full-screen texture. Events stop arriving the moment recording stops, so a
    // plain `recv` would block here forever with all of that still held; the
    // timeout is what gives the thread somewhere to notice and let go.
    let mut holding_capture = false;
    loop {
        let event = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(e) => e,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if holding_capture && !state.recording.load(Ordering::Relaxed) {
                    platform::release_capture_cache();
                    holding_capture = false;
                }
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        if !state.recording.load(Ordering::Relaxed) {
            continue;
        }
        let index = {
            let mut data = state.macro_data.lock();
            if data.events.len() >= MAX_EVENTS {
                continue;
            }
            data.events.push(event);
            data.events.len() - 1
        };
        // The square is cut here rather than in the hook. A low-level hook holds up
        // every keystroke and click on the machine until it returns, and a screen
        // grab is milliseconds; taking one in there would make the mouse stutter
        // for everybody. This thread is one channel hop behind, which is fast
        // enough that the button is still drawn the way it was clicked.
        if let InputEventKind::MouseButton { button, down: true, x, y } = event.kind {
            holding_capture |= take_click_shot(&state, index, button, x, y);
        }
    }
}

/// Cuts the square around one click, if the setting asks for it.
///
/// Returns whether the screen was actually touched, so the caller knows whether it is
/// now holding a capture cache worth releasing.
fn take_click_shot(
    state: &AppState,
    index: usize,
    button: MouseButton,
    x: i32,
    y: i32,
) -> bool {
    if !state.record_click_shots.load(Ordering::Relaxed) {
        return false;
    }
    if state.click_shots.lock().len() >= MAX_CLICK_SHOTS {
        return false;
    }
    let side = state.click_shot_size.load(Ordering::Relaxed).clamp(16, 512) as i32;
    let (vx, vy, vw, vh) = platform::virtual_screen_rect();
    // Centred on the click and then pushed inside the desktop, so a click near an
    // edge still gets a full square rather than a sliver.
    let left = (x - side / 2).clamp(vx, (vx + vw - side).max(vx));
    let top = (y - side / 2).clamp(vy, (vy + vh - side).max(vy));
    let w = side.min(vw);
    let h = side.min(vh);
    let Some(frame) = platform::capture(left, top, w, h) else {
        return true; // it tried, so the cache may well exist
    };
    state.click_shots.lock().push(ClickShot {
        index,
        button,
        x,
        y,
        left,
        top,
        w: frame.w,
        h: frame.h,
        rgba: frame.to_rgba(),
        dpi: platform::current_dpi(),
    });
    true
}

// ============================================================================
// Debug overlay
// ============================================================================

/// A see-through window over everything, drawing what the script just looked at.
///
/// A layered Win32 window rather than a second eframe viewport, for three reasons.
/// The first is decisive: a transparent viewport needs the GL config chosen at
/// start-up to carry an alpha channel, and on a machine whose driver does not offer
/// one eframe says so in the log and creates the window *opaque* - which for a
/// full-screen overlay means covering the desktop in grey. A colour-keyed layered
/// window has no such dependency. The second is that this costs no GL surface and
/// no second render loop. The third is that GDI draws in physical pixels, which is
/// what every rectangle here is already measured in, so a mixed-DPI desktop needs no
/// conversion and nothing drifts on the second monitor.
#[cfg(windows)]
mod overlay {
    use super::win32::*;
    use super::{SIGHTING, wide};
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    /// Painted where nothing should be seen. Windows makes every pixel of exactly
    /// this colour transparent, and a transparent pixel is also not clickable.
    const KEY: u32 = 0x0010_0F0E;

    static RUNNING: AtomicBool = AtomicBool::new(false);
    static WANTED: AtomicBool = AtomicBool::new(false);
    static HWND_SLOT: AtomicIsize = AtomicIsize::new(0);

    /// Turns the overlay on or off. Cheap and idempotent; safe to call every frame.
    pub fn set_enabled(on: bool) {
        let was = WANTED.swap(on, Ordering::Relaxed);
        if on {
            // Not skipped when it was already wanted: switching it off and straight
            // back on can catch the thread mid-teardown, and then nothing would ever
            // put the window back while the box stayed ticked. Two atomics a frame.
            if !RUNNING.swap(true, Ordering::Relaxed) {
                let _ = std::thread::Builder::new()
                    .name("overlay".into())
                    .spawn(|| unsafe { run() });
            }
            return;
        }
        if !was {
            return;
        }
        // The thread notices `WANTED` and takes its own window down: a window may
        // only be destroyed by the thread that created it. The message is only to
        // wake it sooner than its next tick.
        let h = HWND_SLOT.load(Ordering::Relaxed);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(h as *mut std::ffi::c_void)),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    pub fn shutdown() {
        set_enabled(false);
    }

    unsafe fn run() {
        unsafe {
            let hinst = GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default();
            let class = w!("MacroRecorderOverlayWnd");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                lpszClassName: class,
                // Erased in the key colour, so the window is invisible from the
                // moment it appears rather than from its first paint. Without this
                // the surface starts as whatever was in memory, and a full-screen
                // window showing that for one frame is startling.
                hbrBackground: CreateSolidBrush(COLORREF(KEY)),
                ..Default::default()
            };
            RegisterClassW(&wc);

            let (vx, vy, vw, vh) = super::platform::virtual_screen_rect();
            let hwnd = CreateWindowExW(
                // Layered for the colour key, transparent so clicks fall through to
                // whatever is underneath, no-activate and tool-window so it never
                // takes focus or appears in Alt-Tab.
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE,
                class,
                PCWSTR(wide("Macro Recorder overlay").as_ptr()),
                WS_POPUP,
                vx,
                vy,
                vw,
                vh,
                None,
                None,
                Some(hinst),
                None,
            );
            let hwnd = match hwnd {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("overlay window could not be created: {e}");
                    RUNNING.store(false, Ordering::Relaxed);
                    return;
                }
            };
            tracing::info!("overlay window up at {vx},{vy} {vw}x{vh}");
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(KEY), 0, LWA_COLORKEY);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            HWND_SLOT.store(hwnd.0 as isize, Ordering::Relaxed);

            let mut seen = u64::MAX;
            let mut msg = MSG::default();
            while WANTED.load(Ordering::Relaxed) {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        WANTED.store(false, Ordering::Relaxed);
                        break;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                // Repaint only when there is something new to draw. A layered window
                // redrawn ten times a second for no reason is a visible flicker and a
                // pointless slice of a core.
                let now = SIGHTING.lock().seq;
                if now != seen {
                    seen = now;
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    let _ = UpdateWindow(hwnd);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            HWND_SLOT.store(0, Ordering::Relaxed);
            let _ = DestroyWindow(hwnd);
            RUNNING.store(false, Ordering::Relaxed);
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                WM_PAINT => {
                    let mut ps = PAINTSTRUCT::default();
                    let hdc = BeginPaint(hwnd, &mut ps);
                    paint(hwnd, hdc);
                    let _ = EndPaint(hwnd, &ps);
                    LRESULT(0)
                }
                WM_CLOSE => {
                    WANTED.store(false, Ordering::Relaxed);
                    LRESULT(0)
                }
                // Nothing should ever hit this window, but say so anyway.
                WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
    }

    /// Draws into an off-screen bitmap and blits it once.
    ///
    /// Straight onto the window would flash: the key-coloured fill and the
    /// rectangles over it are two separate presentations of a layered surface.
    unsafe fn paint(hwnd: HWND, hdc: HDC) {
        unsafe {
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_err() {
                return;
            }
            let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
            if w <= 0 || h <= 0 {
                return;
            }
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, w, h);
            let old = SelectObject(mem, HGDIOBJ(bmp.0));

            let key_brush = CreateSolidBrush(COLORREF(KEY));
            FillRect(mem, &rc, key_brush);
            let _ = DeleteObject(HGDIOBJ(key_brush.0));

            let seen = SIGHTING.lock().clone();
            let (ox, oy, _, _) = super::platform::virtual_screen_rect();
            let frame = |x: i32, y: i32, rw: i32, rh: i32, colour: u32, width: i32| {
                let pen = CreatePen(PS_SOLID, width, COLORREF(colour));
                let old_pen = SelectObject(mem, HGDIOBJ(pen.0));
                let old_brush = SelectObject(mem, GetStockObject(NULL_BRUSH));
                let _ = Rectangle(mem, x - ox, y - oy, x - ox + rw, y - oy + rh);
                SelectObject(mem, old_brush);
                SelectObject(mem, old_pen);
                let _ = DeleteObject(HGDIOBJ(pen.0));
            };

            // Blue: where it was allowed to look. Amber: where text was read.
            // Violet: the interface element. Green or red: the match itself.
            if let Some((x, y, rw, rh)) = seen.area {
                frame(x, y, rw, rh, 0x00FF_A05A, 1);
            }
            if let Some((x, y, rw, rh)) = seen.text {
                frame(x, y, rw, rh, 0x003C_BEFF, 1);
            }
            if let Some((x, y, rw, rh)) = seen.element {
                frame(x, y, rw, rh, 0x00FF_78B4, 2);
            }
            if let Some((x, y, rw, rh, score)) = seen.hit {
                // The colour is the answer and the number under it is why.
                let col = if score >= 0.85 { 0x0078_DC50 } else { 0x005A_5AF0 };
                frame(x, y, rw, rh, col, 2);
                let label = wide(&format!("{score:.3}"));
                SetBkMode(mem, TRANSPARENT);
                SetTextColor(mem, COLORREF(col));
                let _ = TextOutW(mem, x - ox, y - oy + rh + 2, &label[..label.len() - 1]);
            }
            if !seen.note.is_empty() {
                let label = wide(&seen.note);
                SetBkMode(mem, TRANSPARENT);
                SetTextColor(mem, COLORREF(0x00E6_E6E6));
                let _ = TextOutW(mem, 12, 12, &label[..label.len() - 1]);
            }

            let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(mem);
        }
    }
}

#[cfg(not(windows))]
mod overlay {
    pub fn set_enabled(_on: bool) {}
    pub fn shutdown() {}
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
    // Which slots ended up live, every time the set is rebuilt. The log is the only
    // place that can answer "was it ever registered?" once the moment has passed.
    info!(
        "hotkeys rebuilt, failure bits {failed:#04x}: {}",
        hk.iter()
            .enumerate()
            .map(|(i, k)| format!(
                "{i}={}{}",
                k.label(),
                if failed & (1 << i) != 0 { "!" } else { "" }
            ))
            .collect::<Vec<_>>()
            .join(" ")
    );
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
                WM_HOTKEY_ID => {
                    let id = msg.wParam.0 as i32;
                    // A hotkey that quietly stops working leaves no trace, and the one
                    // question that decides where to look next is whether Windows
                    // delivered the message at all. Hotkeys are pressed a handful of
                    // times a session, so this costs nothing.
                    info!("hotkey {id} delivered");
                    match id {
                        HK_ID_RECORD => toggle_recording(&state),
                        HK_ID_PLAY => toggle_playback(&state),
                        HK_ID_STOP => stop_everything(&state),
                        HK_ID_PAUSE => toggle_pause(&state),
                        HK_ID_FASTER => nudge_speed(&state, 1.25),
                        HK_ID_SLOWER => nudge_speed(&state, 0.8),
                        HK_ID_SKIP => state.skip_step.store(true, Ordering::Relaxed),
                        _ => {}
                    }
                }
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
        // The overlay owns a window on a thread of its own; it has to be told to
        // take it down, or the desktop keeps a dead frame drawn on it.
        overlay::shutdown();
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
/// Clicking moves the caret somewhere the typed-character buffer knows nothing
/// about, so whatever was half-typed stops counting.
#[cfg(windows)]
fn note_mouse_for_expander() {
    expander::reset();
}

fn should_record() -> Option<&'static Arc<AppState>> {
    let state = GLOBAL_STATE.get()?;
    if !state.recording.load(Ordering::Relaxed) {
        return None;
    }
    if !virtual_desktop::is_app_on_active_desktop_cached(platform::app_hwnd())
        || virtual_desktop::shell_switcher_in_front()
    {
        // Recording through the switcher is the same fault seen from the other side:
        // it bakes clicks into the macro that will land in Task View on replay.
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
                        expander::on_key(data.vkCode as u16, data.scanCode as u16, down);
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
        if wp.0 as u32 != 0x0200 {
            note_mouse_for_expander();
        }
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
    fg_cb, fg_fps, fg_added, tip_frame_guard, fg_auto, fg_measured, fg_manual,
    sec_perf, perf_cb, perf_none, perf_frametime, perf_avg, perf_low1, perf_low01,
    perf_stutter, tip_perf, tgt_from_rec, grp_anchor, grp_speed,
    sec_expander, exp_enable, exp_count, exp_reload, exp_open, tip_expander,
    exp_add, exp_abbr, exp_text, exp_prefix, exp_default_trigger, exp_delims,
    exp_excluded_lbl, exp_tr_inherit, exp_tr_delim, exp_tr_prefix, exp_tr_instant,
    exp_in_type, exp_in_paste,
    k_findimg, f_area, a_full, a_window, a_rect, a_near, f_margin, f_into, f_find_hint,
    exp_ac_text, exp_ac_play, exp_ac_stop, exp_ac_run, f_lose_at, f_stable,
    f_prep, p_none, p_ui, p_small, p_game, p_digits, p_auto,
    f_expect, x_any, x_int, x_dec, x_time, x_pattern, tip_pattern, ocr_quality,
    v_number, v_text, tip_value_text,
    k_readtext, k_gettext, k_puttext, c_process, tip_process,
    t_clipboard, t_wintitle, t_process, t_file, f_append,
    a_anchor, f_anchor, f_edge, tip_edge,
    c_element, k_findelem, k_clickelem, f_name, f_autoid, f_control, f_any,
    f_in_front, f_invoke, tip_invoke, tip_uia, img_overlay, tip_overlay,
    // 1.5.0
    m_onmiss, m_continue, m_stop, m_break, m_retry, m_times, m_delay, tip_onmiss,
    k_call, f_macro_file, tip_call, call_depth,
    fast_capture, tip_fast_capture,
    sec_vars, vars_open, vars_title, vars_none, vars_name, vars_value, vars_step,
    vars_stepmode, vars_stepnext, tip_stepmode, vars_running, vars_idle,
    rec_shots, rec_shots_ask, rec_shots_make, rec_shots_skip, rec_shots_done,
    rec_shots_cb, tip_rec_shots, rec_shots_size, rec_shots_miss,
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
    tip_human: "Draws a curved path, with a new arc every time, whenever the pointer has to jump more than about 24 px. Recorded movement is replayed exactly as recorded, so this changes nothing unless 'Capture mouse movement' is off or a script clicks by coordinate or by image.",
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
    sec_target: "🖥 Target window", tgt_title: "Title contains",
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
    fg_cb: "Frame-rate guard", fg_fps: "Slowest expected FPS",
    fg_added: "guard added {} s this run",
    tip_frame_guard: "A game at 15 FPS reads the mouse and keyboard once every 67 ms, so a click that lasted 8 ms is never seen. This holds every press long enough for the slowest frame you expect. It can only make a macro slower, never faster.",
    fg_auto: "Set it from the window automatically", fg_manual: "not measured yet — the figure below is used",
    fg_measured: "measured frame ≈ {} ms",
    sec_perf: "📊 Window responsiveness", perf_cb: "Keep measuring the target window",
    perf_none: "no data — set a target window title above",
    perf_frametime: "Frame time: {} ms", perf_avg: "Average: {} FPS",
    perf_low1: "1 % low: {} FPS", perf_low01: "0.1 % low: {} FPS",
    perf_stutter: "Stutters: {} in the last 10 s",
    tip_perf: "Timed by sending an empty message through the window's own loop, not by counting frames — no driver hooks, no administrator rights. A game that drains its queue once per frame answers in about one frame, which is exactly the delay the guard has to cover. For true frame statistics use PresentMon or RTSS.",
    tgt_from_rec: "\u{2935} From the recording", grp_anchor: "Coordinate anchoring",
    grp_speed: "How well the window keeps up",
    sec_expander: "⌨ Text expander", exp_enable: "Expand abbreviations as I type",
    exp_count: "{} entries enabled", exp_reload: "Reload", exp_open: "Edit expansions.json",
    tip_expander: "Type a short abbreviation and it becomes the longer text you saved for it. Entries live in expansions.json; edit it, then press Reload. Never expands while recording or replaying a macro.",
    exp_add: "+ Add", exp_abbr: "short", exp_text: "becomes this", exp_prefix: "mark",
    exp_default_trigger: "Default trigger", exp_delims: "Delimiters",
    exp_excluded_lbl: "Never in windows", exp_tr_inherit: "default",
    exp_tr_delim: "after a delimiter", exp_tr_prefix: "behind a marker",
    exp_tr_instant: "immediately", exp_in_type: "type", exp_in_paste: "paste",
    k_findimg: "Find image", f_area: "Area", a_full: "whole screen",
    a_window: "active window", a_rect: "a rectangle", a_near: "near the last match",
    f_margin: "margin", f_into: "into", f_find_hint: "sets {}.found .x .y .w .h .score",
    exp_ac_text: "types text", exp_ac_play: "plays a macro", exp_ac_stop: "stops everything",
    exp_ac_run: "runs a program",
    f_lose_at: "lost below", f_stable: "stable",
    f_prep: "Prep", p_none: "none", p_ui: "interface", p_small: "small text",
    p_game: "game HUD", p_digits: "digits", p_auto: "try each",
    f_expect: "Expect", x_any: "anything", x_int: "whole number", x_dec: "decimal",
    x_time: "clock", x_pattern: "pattern",
    tip_pattern: "# a digit, @ a letter, ? one character, * any run",
    ocr_quality: "fit {} ({})",
    v_number: "number", v_text: "text",
    tip_value_text: "{name} is replaced by what that variable holds; {{ is a brace",
    k_readtext: "Read text", k_gettext: "Get text", k_puttext: "Put text",
    c_process: "Process running", tip_process: "part of the name is enough",
    t_clipboard: "clipboard", t_wintitle: "title of the window in front",
    t_process: "program in front", t_file: "file", f_append: "add to the end",
    a_anchor: "relative to another picture", f_anchor: "Anchor", f_edge: "outlines",
    tip_edge: "match shapes rather than shades - survives a theme change",
    c_element: "Element on screen", k_findelem: "Find element",
    k_clickelem: "Press element", f_name: "Name", f_autoid: "Id", f_control: "Kind",
    f_any: "any", f_in_front: "in the window in front", f_invoke: "ask the app",
    tip_invoke: "press it through the application instead of clicking at it",
    tip_uia: "the name a screen reader would read; games draw their own interface and expose none",
    img_overlay: "Show what the script looks at",
    tip_overlay: "a see-through window over everything, drawing the last search area and match",
    m_onmiss: "If not found:", m_continue: "carry on", m_stop: "stop the script",
    m_break: "leave the loop", m_retry: "try again", m_times: "times",
    m_delay: "apart (ms)",
    tip_onmiss: "until 1.5.0 every one of these steps carried on in silence, which is how a night macro ends up clicking at nothing for three hours",
    k_call: "Call macro", f_macro_file: "File", tip_call: "runs another macro's script here; the variables are shared, nesting is capped at 8",
    call_depth: "call nesting is capped at {}",
    fast_capture: "Fast screen capture",
    tip_fast_capture: "Desktop Duplication instead of GDI: about 5x on a whole screen and 20x on a small region. Falls back on its own if this machine will not do it.",
    sec_vars: "🔎 Variables", vars_open: "Watch the run", vars_title: "Variables",
    vars_none: "nothing set yet", vars_name: "name", vars_value: "value",
    vars_step: "step", vars_stepmode: "Pause before each step", vars_stepnext: "▶ Next step",
    tip_stepmode: "the script stops before every step and waits for Next",
    vars_running: "running", vars_idle: "not running",
    rec_shots: "📸 {} clicks with a picture", rec_shots_ask: "Turn {} recorded clicks into steps that look for the picture instead of the coordinates?",
    rec_shots_make: "Make picture steps", rec_shots_skip: "Keep the coordinates",
    rec_shots_done: "{} picture steps written",
    rec_shots_cb: "Snip a picture at every click",
    tip_rec_shots: "while recording, keep a small square from around each click, so the recording can be turned into steps that find the button wherever it has moved to",
    rec_shots_size: "Square size (px)", rec_shots_miss: "If a picture is not found:",
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
    tip_human: "Рисует дугу, каждый раз новую, когда курсору надо прыгнуть больше чем примерно на 24 px. Записанные движения воспроизводятся ровно как записаны, поэтому эффект виден, только если выключено «Записывать движения мыши» или скрипт кликает по координатам либо по картинке.",
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
    sec_target: "🖥 Целевое окно", tgt_title: "Заголовок содержит",
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
    fg_cb: "Защита от просадок FPS", fg_fps: "Минимальный ожидаемый FPS",
    fg_added: "защита добавила {} с за прогон",
    tip_frame_guard: "Игра на 15 FPS опрашивает мышь и клавиатуру раз в 67 мс, поэтому клик длиной 8 мс она просто не видит. Опция удерживает каждое нажатие достаточно долго для самого медленного ожидаемого кадра. Макрос от неё может только замедлиться.",
    fg_auto: "Подбирать автоматически по окну", fg_manual: "измерений пока нет — работает значение ниже",
    fg_measured: "измеренный кадр ≈ {} мс",
    sec_perf: "📊 Отзывчивость окна", perf_cb: "Постоянно измерять целевое окно",
    perf_none: "нет данных — задайте заголовок целевого окна выше",
    perf_frametime: "Время кадра: {} мс", perf_avg: "Средний: {} FPS",
    perf_low1: "1 % низких: {} FPS", perf_low01: "0,1 % низких: {} FPS",
    perf_stutter: "Фризов: {} за последние 10 с",
    tip_perf: "Измеряется временем прохождения пустого сообщения через цикл окна, а не подсчётом кадров — без хуков в драйвер и прав администратора. Игра, которая разбирает очередь раз в кадр, отвечает примерно за кадр, а это ровно та задержка, которую перекрывает защита. Для настоящей статистики кадров используйте PresentMon или RTSS.",
    tgt_from_rec: "\u{2935} Из записи", grp_anchor: "Привязка координат",
    grp_speed: "Насколько окно успевает",
    sec_expander: "⌨ Текстовый расширитель", exp_enable: "Разворачивать сокращения при наборе",
    exp_count: "включено записей: {}", exp_reload: "Перечитать", exp_open: "Открыть expansions.json",
    tip_expander: "Набираете короткое сокращение — оно превращается в сохранённый под него текст. Записи лежат в expansions.json: отредактируйте и нажмите «Перечитать». Во время записи и воспроизведения макроса не срабатывает.",
    exp_add: "+ Добавить", exp_abbr: "кратко", exp_text: "превращается в это", exp_prefix: "знак",
    exp_default_trigger: "Срабатывание по умолчанию", exp_delims: "Разделители",
    exp_excluded_lbl: "Молчать в окнах", exp_tr_inherit: "по умолчанию",
    exp_tr_delim: "после разделителя", exp_tr_prefix: "за префиксом",
    exp_tr_instant: "сразу", exp_in_type: "печатать", exp_in_paste: "вставить",
    k_findimg: "Найти картинку", f_area: "Область", a_full: "весь экран",
    a_window: "активное окно", a_rect: "прямоугольник", a_near: "рядом с прошлым совпадением",
    f_margin: "запас", f_into: "в", f_find_hint: "задаёт {}.found .x .y .w .h .score",
    exp_ac_text: "печатает текст", exp_ac_play: "запускает макрос", exp_ac_stop: "останавливает всё",
    exp_ac_run: "запускает программу",
    f_lose_at: "потеряно ниже", f_stable: "стабильно",
    f_prep: "Обработка", p_none: "без неё", p_ui: "интерфейс",
    p_small: "мелкий текст", p_game: "игровой HUD", p_digits: "цифры",
    p_auto: "перебрать",
    f_expect: "Ожидается", x_any: "что угодно", x_int: "целое число",
    x_dec: "дробное число", x_time: "время", x_pattern: "шаблон",
    tip_pattern: "# цифра, @ буква, ? один символ, * любой отрезок",
    ocr_quality: "соответствие {} ({})",
    v_number: "число", v_text: "текст",
    tip_value_text: "{имя} заменяется значением переменной; {{ — сама скобка",
    k_readtext: "Прочитать текст", k_gettext: "Взять текст", k_puttext: "Записать текст",
    c_process: "Процесс запущен", tip_process: "достаточно части имени",
    t_clipboard: "буфер обмена", t_wintitle: "заголовок активного окна",
    t_process: "активная программа", t_file: "файл", f_append: "дописывать в конец",
    a_anchor: "относительно другой картинки", f_anchor: "Якорь", f_edge: "по контурам",
    tip_edge: "сравнивать форму, а не оттенки — переживает смену темы",
    c_element: "Элемент на экране", k_findelem: "Найти элемент",
    k_clickelem: "Нажать элемент", f_name: "Имя", f_autoid: "Id", f_control: "Тип",
    f_any: "любой", f_in_front: "в активном окне", f_invoke: "через приложение",
    tip_invoke: "попросить приложение нажать, а не кликать по координатам",
    tip_uia: "имя, которое произнёс бы экранный диктор; игры рисуют интерфейс сами и ничего не сообщают",
    img_overlay: "Показывать, куда смотрит скрипт",
    tip_overlay: "прозрачное окно поверх всего: последняя область поиска и последнее совпадение",
    m_onmiss: "Если не найдено:", m_continue: "идти дальше", m_stop: "остановить скрипт",
    m_break: "выйти из цикла", m_retry: "повторить", m_times: "раз",
    m_delay: "интервал (мс)",
    tip_onmiss: "до 1.5.0 все эти шаги молча шли дальше — так ночной макрос три часа кликает в пустоту",
    k_call: "Вызвать макрос", f_macro_file: "Файл", tip_call: "выполняет скрипт другого макроса здесь же; переменные общие, вложенность не глубже 8",
    call_depth: "глубина вызовов ограничена {}",
    fast_capture: "Быстрый захват экрана",
    tip_fast_capture: "Desktop Duplication вместо GDI: примерно в 5 раз быстрее на весь экран и в 20 раз на маленькой области. Если машина не умеет — сам вернётся к GDI.",
    sec_vars: "🔎 Переменные", vars_open: "Следить за прогоном", vars_title: "Переменные",
    vars_none: "пока ничего не задано", vars_name: "имя", vars_value: "значение",
    vars_step: "шаг", vars_stepmode: "Пауза перед каждым шагом", vars_stepnext: "▶ Следующий шаг",
    tip_stepmode: "скрипт останавливается перед каждым шагом и ждёт «Следующий шаг»",
    vars_running: "идёт", vars_idle: "не идёт",
    rec_shots: "📸 Кликов с картинкой: {}", rec_shots_ask: "Превратить {} записанных кликов в шаги, которые ищут картинку, а не координаты?",
    rec_shots_make: "Сделать шаги по картинке", rec_shots_skip: "Оставить координаты",
    rec_shots_done: "Создано шагов по картинке: {}",
    rec_shots_cb: "Вырезать картинку на каждом клике",
    tip_rec_shots: "во время записи сохранять небольшой квадрат вокруг каждого клика, чтобы потом превратить запись в шаги, которые найдут кнопку там, куда она переехала",
    rec_shots_size: "Размер квадрата (px)", rec_shots_miss: "Если картинка не найдена:",
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
    tip_human: "Малює дугу, щоразу нову, коли курсору треба стрибнути більше ніж приблизно на 24 px. Записані рухи відтворюються точно як записані, тож ефект помітний, лише якщо вимкнено «Записувати рухи миші» або скрипт клікає за координатами чи за картинкою.",
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
    sec_target: "🖥 Цільове вікно", tgt_title: "Заголовок містить",
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
    fg_cb: "Захист від просідань FPS", fg_fps: "Мінімальний очікуваний FPS",
    fg_added: "захист додав {} с за прогін",
    tip_frame_guard: "Гра на 15 FPS опитує мишу та клавіатуру раз на 67 мс, тож клік завдовжки 8 мс вона просто не бачить. Опція утримує кожне натискання достатньо довго для найповільнішого очікуваного кадру. Макрос від неї може лише сповільнитися.",
    fg_auto: "Підбирати автоматично за вікном", fg_manual: "вимірювань поки немає — працює значення нижче",
    fg_measured: "виміряний кадр ≈ {} мс",
    sec_perf: "📊 Чутливість вікна", perf_cb: "Постійно вимірювати цільове вікно",
    perf_none: "немає даних — задайте заголовок цільового вікна вище",
    perf_frametime: "Час кадру: {} мс", perf_avg: "Середній: {} FPS",
    perf_low1: "1 % низьких: {} FPS", perf_low01: "0,1 % низьких: {} FPS",
    perf_stutter: "Фризів: {} за останні 10 с",
    tip_perf: "Вимірюється часом проходження порожнього повідомлення через цикл вікна, а не підрахунком кадрів — без хуків у драйвер і прав адміністратора. Гра, яка розбирає чергу раз на кадр, відповідає приблизно за кадр, а це саме та затримка, яку перекриває захист. Для справжньої статистики кадрів використовуйте PresentMon або RTSS.",
    tgt_from_rec: "\u{2935} Із запису", grp_anchor: "Прив'язка координат",
    grp_speed: "Наскільки вікно встигає",
    sec_expander: "⌨ Текстовий розширювач", exp_enable: "Розгортати скорочення під час набору",
    exp_count: "увімкнено записів: {}", exp_reload: "Перечитати", exp_open: "Відкрити expansions.json",
    tip_expander: "Набираєте коротке скорочення — воно перетворюється на збережений під нього текст. Записи лежать у expansions.json: відредагуйте та натисніть «Перечитати». Під час запису та відтворення макроса не спрацьовує.",
    exp_add: "+ Додати", exp_abbr: "коротко", exp_text: "перетворюється на це", exp_prefix: "знак",
    exp_default_trigger: "Спрацювання за умовчанням", exp_delims: "Роздільники",
    exp_excluded_lbl: "Мовчати у вікнах", exp_tr_inherit: "за умовчанням",
    exp_tr_delim: "після роздільника", exp_tr_prefix: "за префіксом",
    exp_tr_instant: "одразу", exp_in_type: "друкувати", exp_in_paste: "вставити",
    k_findimg: "Знайти картинку", f_area: "Область", a_full: "увесь екран",
    a_window: "активне вікно", a_rect: "прямокутник", a_near: "біля минулого збігу",
    f_margin: "запас", f_into: "у", f_find_hint: "задає {}.found .x .y .w .h .score",
    exp_ac_text: "друкує текст", exp_ac_play: "запускає макрос", exp_ac_stop: "зупиняє все",
    exp_ac_run: "запускає програму",
    f_lose_at: "втрачено нижче", f_stable: "стабільно",
    f_prep: "Обробка", p_none: "без неї", p_ui: "інтерфейс",
    p_small: "дрібний текст", p_game: "ігровий HUD", p_digits: "цифри",
    p_auto: "перебрати",
    f_expect: "Очікується", x_any: "будь-що", x_int: "ціле число",
    x_dec: "дробове число", x_time: "час", x_pattern: "шаблон",
    tip_pattern: "# цифра, @ літера, ? один символ, * будь-який відрізок",
    ocr_quality: "відповідність {} ({})",
    v_number: "число", v_text: "текст",
    tip_value_text: "{ім'я} замінюється значенням змінної; {{ — сама дужка",
    k_readtext: "Прочитати текст", k_gettext: "Узяти текст", k_puttext: "Записати текст",
    c_process: "Процес запущено", tip_process: "достатньо частини імені",
    t_clipboard: "буфер обміну", t_wintitle: "заголовок активного вікна",
    t_process: "активна програма", t_file: "файл", f_append: "дописувати в кінець",
    a_anchor: "відносно іншої картинки", f_anchor: "Якір", f_edge: "за контурами",
    tip_edge: "порівнювати форму, а не відтінки — переживає зміну теми",
    c_element: "Елемент на екрані", k_findelem: "Знайти елемент",
    k_clickelem: "Натиснути елемент", f_name: "Ім'я", f_autoid: "Id", f_control: "Тип",
    f_any: "будь-який", f_in_front: "в активному вікні", f_invoke: "через застосунок",
    tip_invoke: "попросити застосунок натиснути, а не клацати по координатах",
    tip_uia: "ім'я, яке б озвучив екранний диктор; ігри малюють інтерфейс самі й нічого не повідомляють",
    img_overlay: "Показувати, куди дивиться скрипт",
    tip_overlay: "прозоре вікно поверх усього: остання область пошуку й останній збіг",
    m_onmiss: "Якщо не знайдено:", m_continue: "йти далі", m_stop: "зупинити скрипт",
    m_break: "вийти з циклу", m_retry: "повторити", m_times: "разів",
    m_delay: "інтервал (мс)",
    tip_onmiss: "до 1.5.0 всі ці кроки мовчки йшли далі — так нічний макрос три години клікає в порожнечу",
    k_call: "Викликати макрос", f_macro_file: "Файл", tip_call: "виконує скрипт іншого макроса тут; змінні спільні, вкладеність не глибша за 8",
    call_depth: "глибина викликів обмежена {}",
    fast_capture: "Швидкий захват екрана",
    tip_fast_capture: "Desktop Duplication замість GDI: приблизно у 5 разів швидше на весь екран і у 20 разів на малій області. Якщо машина не вміє — сам повернеться до GDI.",
    sec_vars: "🔎 Змінні", vars_open: "Стежити за прогоном", vars_title: "Змінні",
    vars_none: "поки нічого не задано", vars_name: "ім'я", vars_value: "значення",
    vars_step: "крок", vars_stepmode: "Пауза перед кожним кроком", vars_stepnext: "▶ Наступний крок",
    tip_stepmode: "скрипт зупиняється перед кожним кроком і чекає «Наступний крок»",
    vars_running: "триває", vars_idle: "не триває",
    rec_shots: "📸 Кліків із картинкою: {}", rec_shots_ask: "Перетворити {} записаних кліків на кроки, які шукають картинку, а не координати?",
    rec_shots_make: "Зробити кроки за картинкою", rec_shots_skip: "Залишити координати",
    rec_shots_done: "Створено кроків за картинкою: {}",
    rec_shots_cb: "Вирізати картинку на кожному кліку",
    tip_rec_shots: "під час запису зберігати невеликий квадрат навколо кожного кліку, щоб потім перетворити запис на кроки, які знайдуть кнопку там, куди вона переїхала",
    rec_shots_size: "Розмір квадрата (px)", rec_shots_miss: "Якщо картинку не знайдено:",
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
    tip_human: "Desenha uma curva, com um arco novo de cada vez, sempre que o ponteiro tem de saltar mais de cerca de 24 px. O movimento gravado é reproduzido tal como foi gravado, por isto nada muda a menos que 'Capturar movimento do rato' esteja desligado ou um script clique por coordenada ou por imagem.",
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
    sec_target: "🖥 Janela alvo", tgt_title: "Título contém",
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
    fg_cb: "Proteção contra queda de FPS", fg_fps: "FPS mínimo esperado",
    fg_added: "a proteção somou {} s nesta execução",
    tip_frame_guard: "Um jogo a 15 FPS lê o rato e o teclado uma vez a cada 67 ms, por isso um clique de 8 ms nunca é visto. Isto mantém cada toque premido o tempo suficiente para o fotograma mais lento previsto. Só pode tornar o macro mais lento.",
    fg_auto: "Definir automaticamente pela janela", fg_manual: "ainda sem medições — usa-se o valor abaixo",
    fg_measured: "fotograma medido ≈ {} ms",
    sec_perf: "📊 Resposta da janela", perf_cb: "Medir a janela alvo continuamente",
    perf_none: "sem dados — defina o título da janela alvo acima",
    perf_frametime: "Tempo de fotograma: {} ms", perf_avg: "Média: {} FPS",
    perf_low1: "1 % mais baixos: {} FPS", perf_low01: "0,1 % mais baixos: {} FPS",
    perf_stutter: "Engasgos: {} nos últimos 10 s",
    tip_perf: "Medido pelo tempo de uma mensagem vazia no ciclo da própria janela, não por contagem de fotogramas — sem ganchos no controlador nem direitos de administrador. Um jogo que esvazia a fila uma vez por fotograma responde em cerca de um fotograma, que é exatamente o atraso que a proteção cobre. Para estatísticas reais use PresentMon ou RTSS.",
    tgt_from_rec: "\u{2935} Da gravação", grp_anchor: "Ancoragem de coordenadas",
    grp_speed: "Quão bem a janela acompanha",
    sec_expander: "⌨ Expansor de texto", exp_enable: "Expandir abreviaturas ao escrever",
    exp_count: "{} entradas ativas", exp_reload: "Recarregar", exp_open: "Abrir expansions.json",
    tip_expander: "Escreva uma abreviatura curta e ela transforma-se no texto que guardou para ela. As entradas estão em expansions.json: edite e carregue em Recarregar. Nunca expande durante a gravação ou a reprodução de um macro.",
    exp_add: "+ Adicionar", exp_abbr: "curto", exp_text: "torna-se isto", exp_prefix: "marca",
    exp_default_trigger: "Disparo predefinido", exp_delims: "Delimitadores",
    exp_excluded_lbl: "Nunca em janelas", exp_tr_inherit: "predefinido",
    exp_tr_delim: "após delimitador", exp_tr_prefix: "atrás de marca",
    exp_tr_instant: "imediatamente", exp_in_type: "escrever", exp_in_paste: "colar",
    k_findimg: "Procurar imagem", f_area: "Área", a_full: "ecrã inteiro",
    a_window: "janela ativa", a_rect: "um retângulo", a_near: "perto do último acerto",
    f_margin: "margem", f_into: "para", f_find_hint: "define {}.found .x .y .w .h .score",
    exp_ac_text: "escreve texto", exp_ac_play: "reproduz um macro", exp_ac_stop: "para tudo",
    exp_ac_run: "executa um programa",
    f_lose_at: "perdido abaixo de", f_stable: "estável",
    f_prep: "Preparo", p_none: "nenhum", p_ui: "interface",
    p_small: "texto pequeno", p_game: "HUD de jogo", p_digits: "dígitos",
    p_auto: "tentar todos",
    f_expect: "Esperado", x_any: "qualquer coisa", x_int: "número inteiro",
    x_dec: "número decimal", x_time: "relógio", x_pattern: "padrão",
    tip_pattern: "# um dígito, @ uma letra, ? um carácter, * qualquer trecho",
    ocr_quality: "ajuste {} ({})",
    v_number: "número", v_text: "texto",
    tip_value_text: "{nome} é substituído pelo valor da variável; {{ é uma chaveta",
    k_readtext: "Ler texto", k_gettext: "Obter texto", k_puttext: "Gravar texto",
    c_process: "Processo em execução", tip_process: "basta parte do nome",
    t_clipboard: "área de transferência", t_wintitle: "título da janela em primeiro plano",
    t_process: "programa em primeiro plano", t_file: "ficheiro", f_append: "juntar ao fim",
    a_anchor: "relativo a outra imagem", f_anchor: "Âncora", f_edge: "contornos",
    tip_edge: "comparar formas em vez de tons - resiste a uma mudança de tema",
    c_element: "Elemento no ecrã", k_findelem: "Procurar elemento",
    k_clickelem: "Premir elemento", f_name: "Nome", f_autoid: "Id", f_control: "Tipo",
    f_any: "qualquer", f_in_front: "na janela em primeiro plano",
    f_invoke: "pela aplicação",
    tip_invoke: "pedir à aplicação que prima, em vez de clicar nas coordenadas",
    tip_uia: "o nome que um leitor de ecrã leria; os jogos desenham a interface e não expõem nada",
    img_overlay: "Mostrar onde o guião procura",
    tip_overlay: "uma janela transparente sobre tudo, com a última área de procura e o último acerto",
    m_onmiss: "Se não encontrar:", m_continue: "continuar", m_stop: "parar o guião",
    m_break: "sair do ciclo", m_retry: "tentar de novo", m_times: "vezes",
    m_delay: "intervalo (ms)",
    tip_onmiss: "até 1.5.0 todos estes passos seguiam em silêncio — é assim que uma macro nocturna passa três horas a clicar no vazio",
    k_call: "Chamar macro", f_macro_file: "Ficheiro", tip_call: "corre aqui o guião de outra macro; as variáveis são as mesmas, o aninhamento pára aos 8",
    call_depth: "o aninhamento de chamadas está limitado a {}",
    fast_capture: "Captura de ecrã rápida",
    tip_fast_capture: "Desktop Duplication em vez de GDI: cerca de 5x num ecrã inteiro e 20x numa região pequena. Volta sozinho ao GDI se a máquina não souber.",
    sec_vars: "🔎 Variáveis", vars_open: "Ver a execução", vars_title: "Variáveis",
    vars_none: "ainda nada definido", vars_name: "nome", vars_value: "valor",
    vars_step: "passo", vars_stepmode: "Pausa antes de cada passo", vars_stepnext: "▶ Passo seguinte",
    tip_stepmode: "o guião pára antes de cada passo e espera por Passo seguinte",
    vars_running: "a correr", vars_idle: "parado",
    rec_shots: "📸 {} cliques com imagem", rec_shots_ask: "Transformar {} cliques gravados em passos que procuram a imagem em vez das coordenadas?",
    rec_shots_make: "Criar passos por imagem", rec_shots_skip: "Manter as coordenadas",
    rec_shots_done: "{} passos por imagem criados",
    rec_shots_cb: "Recortar uma imagem em cada clique",
    tip_rec_shots: "durante a gravação, guardar um quadrado pequeno à volta de cada clique, para depois transformar a gravação em passos que encontram o botão onde quer que ele esteja",
    rec_shots_size: "Tamanho do quadrado (px)", rec_shots_miss: "Se a imagem não for encontrada:",
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
    tip_human: "Dibuja una curva, con un arco nuevo cada vez, cuando el puntero tiene que saltar más de unos 24 px. El movimiento grabado se reproduce tal cual, así que esto no cambia nada salvo que 'Capturar movimiento del ratón' esté desactivado o un script haga clic por coordenada o por imagen.",
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
    sec_target: "🖥 Ventana objetivo", tgt_title: "El título contiene",
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
    fg_cb: "Protección ante caídas de FPS", fg_fps: "FPS mínimo esperado",
    fg_added: "la protección añadió {} s en esta pasada",
    tip_frame_guard: "Un juego a 15 FPS lee el ratón y el teclado una vez cada 67 ms, así que un clic de 8 ms nunca se ve. Esto mantiene cada pulsación el tiempo suficiente para el fotograma más lento previsto. Solo puede hacer el macro más lento.",
    fg_auto: "Ajustar automáticamente según la ventana", fg_manual: "aún sin mediciones — se usa el valor de abajo",
    fg_measured: "fotograma medido ≈ {} ms",
    sec_perf: "📊 Respuesta de la ventana", perf_cb: "Medir la ventana objetivo continuamente",
    perf_none: "sin datos — indica arriba el título de la ventana objetivo",
    perf_frametime: "Tiempo de fotograma: {} ms", perf_avg: "Media: {} FPS",
    perf_low1: "1 % más bajos: {} FPS", perf_low01: "0,1 % más bajos: {} FPS",
    perf_stutter: "Tirones: {} en los últimos 10 s",
    tip_perf: "Se mide con el tiempo de un mensaje vacío en el propio bucle de la ventana, no contando fotogramas — sin ganchos en el controlador ni permisos de administrador. Un juego que vacía su cola una vez por fotograma responde en torno a un fotograma, que es justo el retardo que cubre la protección. Para estadísticas reales usa PresentMon o RTSS.",
    tgt_from_rec: "\u{2935} De la grabación", grp_anchor: "Anclaje de coordenadas",
    grp_speed: "Cómo va siguiendo la ventana",
    sec_expander: "⌨ Expansor de texto", exp_enable: "Expandir abreviaturas al escribir",
    exp_count: "{} entradas activas", exp_reload: "Recargar", exp_open: "Abrir expansions.json",
    tip_expander: "Escribe una abreviatura corta y se convierte en el texto que guardaste para ella. Las entradas están en expansions.json: edítalo y pulsa Recargar. Nunca expande mientras se graba o reproduce un macro.",
    exp_add: "+ Añadir", exp_abbr: "corto", exp_text: "se convierte en esto", exp_prefix: "marca",
    exp_default_trigger: "Disparo por defecto", exp_delims: "Delimitadores",
    exp_excluded_lbl: "Nunca en ventanas", exp_tr_inherit: "por defecto",
    exp_tr_delim: "tras delimitador", exp_tr_prefix: "tras marca",
    exp_tr_instant: "al instante", exp_in_type: "escribir", exp_in_paste: "pegar",
    k_findimg: "Buscar imagen", f_area: "Área", a_full: "toda la pantalla",
    a_window: "ventana activa", a_rect: "un rectángulo", a_near: "cerca del último acierto",
    f_margin: "margen", f_into: "en", f_find_hint: "define {}.found .x .y .w .h .score",
    exp_ac_text: "escribe texto", exp_ac_play: "reproduce un macro", exp_ac_stop: "detiene todo",
    exp_ac_run: "ejecuta un programa",
    f_lose_at: "perdido por debajo de", f_stable: "estable",
    f_prep: "Preparación", p_none: "ninguna", p_ui: "interfaz",
    p_small: "texto pequeño", p_game: "HUD de juego", p_digits: "dígitos",
    p_auto: "probar todas",
    f_expect: "Se espera", x_any: "cualquier cosa", x_int: "número entero",
    x_dec: "número decimal", x_time: "reloj", x_pattern: "patrón",
    tip_pattern: "# un dígito, @ una letra, ? un carácter, * cualquier tramo",
    ocr_quality: "ajuste {} ({})",
    v_number: "número", v_text: "texto",
    tip_value_text: "{nombre} se sustituye por el valor de la variable; {{ es una llave",
    k_readtext: "Leer texto", k_gettext: "Obtener texto", k_puttext: "Guardar texto",
    c_process: "Proceso en ejecución", tip_process: "basta parte del nombre",
    t_clipboard: "portapapeles", t_wintitle: "título de la ventana activa",
    t_process: "programa activo", t_file: "archivo", f_append: "añadir al final",
    a_anchor: "relativo a otra imagen", f_anchor: "Ancla", f_edge: "contornos",
    tip_edge: "comparar formas en vez de tonos - resiste un cambio de tema",
    c_element: "Elemento en pantalla", k_findelem: "Buscar elemento",
    k_clickelem: "Pulsar elemento", f_name: "Nombre", f_autoid: "Id", f_control: "Tipo",
    f_any: "cualquiera", f_in_front: "en la ventana activa",
    f_invoke: "por la aplicación",
    tip_invoke: "pedir a la aplicación que lo pulse, en vez de hacer clic en las coordenadas",
    tip_uia: "el nombre que leería un lector de pantalla; los juegos dibujan su interfaz y no exponen nada",
    img_overlay: "Mostrar dónde mira el guion",
    tip_overlay: "una ventana transparente sobre todo, con la última área de búsqueda y el último acierto",
    m_onmiss: "Si no se encuentra:", m_continue: "seguir", m_stop: "detener el guion",
    m_break: "salir del bucle", m_retry: "reintentar", m_times: "veces",
    m_delay: "intervalo (ms)",
    tip_onmiss: "hasta 1.5.0 todos estos pasos seguían en silencio — así es como una macro nocturna pasa tres horas haciendo clic en el vacío",
    k_call: "Llamar macro", f_macro_file: "Archivo", tip_call: "ejecuta aquí el guion de otra macro; las variables son las mismas, el anidamiento se corta en 8",
    call_depth: "el anidamiento de llamadas está limitado a {}",
    fast_capture: "Captura de pantalla rápida",
    tip_fast_capture: "Desktop Duplication en vez de GDI: unas 5x en la pantalla entera y 20x en una región pequeña. Vuelve solo a GDI si la máquina no puede.",
    sec_vars: "🔎 Variables", vars_open: "Ver la ejecución", vars_title: "Variables",
    vars_none: "aún no hay nada", vars_name: "nombre", vars_value: "valor",
    vars_step: "paso", vars_stepmode: "Pausa antes de cada paso", vars_stepnext: "▶ Paso siguiente",
    tip_stepmode: "el guion se detiene antes de cada paso y espera a Paso siguiente",
    vars_running: "en marcha", vars_idle: "detenido",
    rec_shots: "📸 {} clics con imagen", rec_shots_ask: "¿Convertir {} clics grabados en pasos que buscan la imagen en lugar de las coordenadas?",
    rec_shots_make: "Crear pasos por imagen", rec_shots_skip: "Mantener las coordenadas",
    rec_shots_done: "{} pasos por imagen creados",
    rec_shots_cb: "Recortar una imagen en cada clic",
    tip_rec_shots: "durante la grabación, guardar un cuadrado pequeño alrededor de cada clic, para luego convertir la grabación en pasos que encuentren el botón donde sea que esté",
    rec_shots_size: "Tamaño del cuadrado (px)", rec_shots_miss: "Si no se encuentra la imagen:",
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
    tip_human: "当指针需要跳跃约 24 像素以上时绘制曲线路径，每次弧线都不同。录制的移动会原样回放，因此除非关闭“记录鼠标移动”，或脚本按坐标、按图片点击，否则看不出区别。",
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
    sec_target: "🖥 目标窗口", tgt_title: "标题包含",
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
    fg_cb: "帧率保护", fg_fps: "预计最低 FPS",
    fg_added: "本次运行保护共增加 {} 秒",
    tip_frame_guard: "15 FPS 的游戏每 67 毫秒才读取一次鼠标和键盘，因此只持续 8 毫秒的点击根本不会被看到。此选项会按你设定的最低帧率延长每次按下的时间。它只会让宏变慢，不会变快。",
    fg_auto: "根据窗口自动调整", fg_manual: "尚无测量结果 — 使用下方数值",
    fg_measured: "实测帧时间 ≈ {} 毫秒",
    sec_perf: "📊 窗口响应", perf_cb: "持续测量目标窗口",
    perf_none: "暂无数据 — 请先在上方填写目标窗口标题",
    perf_frametime: "帧时间：{} 毫秒", perf_avg: "平均：{} FPS",
    perf_low1: "1% 低帧：{} FPS", perf_low01: "0.1% 低帧：{} FPS",
    perf_stutter: "卡顿：最近 10 秒内 {} 次",
    tip_perf: "通过测量一条空消息在窗口自身消息循环中的往返时间得出，而不是统计帧数 — 无需驱动钩子，也无需管理员权限。每帧处理一次消息队列的游戏大约在一帧内回应，而这正是帧率保护需要覆盖的延迟。若需要真实的帧数统计，请使用 PresentMon 或 RTSS。",
    tgt_from_rec: "\u{2935} 取自录制", grp_anchor: "坐标锚定",
    grp_speed: "窗口跟得上的程度",
    sec_expander: "⌨ 文本扩展", exp_enable: "输入时展开缩写",
    exp_count: "已启用条目：{}", exp_reload: "重新载入", exp_open: "打开 expansions.json",
    tip_expander: "输入一个短缩写，它会变成你为它保存的长文本。条目保存在 expansions.json 中：编辑后按“重新载入”。录制或回放宏时不会触发。",
    exp_add: "+ 添加", exp_abbr: "缩写", exp_text: "展开为", exp_prefix: "标记",
    exp_default_trigger: "默认触发方式", exp_delims: "分隔符",
    exp_excluded_lbl: "在这些窗口中不触发", exp_tr_inherit: "默认",
    exp_tr_delim: "分隔符之后", exp_tr_prefix: "标记之后",
    exp_tr_instant: "立即", exp_in_type: "逐字输入", exp_in_paste: "粘贴",
    k_findimg: "查找图片", f_area: "搜索范围", a_full: "整个屏幕",
    a_window: "活动窗口", a_rect: "指定矩形", a_near: "上次命中附近",
    f_margin: "外扩", f_into: "存入", f_find_hint: "写入 {}.found .x .y .w .h .score",
    exp_ac_text: "输入文本", exp_ac_play: "播放宏", exp_ac_stop: "停止全部",
    exp_ac_run: "运行程序",
    f_lose_at: "低于则视为消失", f_stable: "稳定",
    f_prep: "预处理", p_none: "不处理", p_ui: "界面文字", p_small: "小号文字",
    p_game: "游戏 HUD", p_digits: "数字", p_auto: "逐个尝试",
    f_expect: "期望格式", x_any: "任意", x_int: "整数", x_dec: "小数",
    x_time: "时间", x_pattern: "模式",
    tip_pattern: "# 一位数字, @ 一个字母, ? 任意一个字符, * 任意长度",
    ocr_quality: "匹配度 {} ({})",
    v_number: "数值", v_text: "文本",
    tip_value_text: "{名称} 会替换为该变量的值; {{ 表示大括号本身",
    k_readtext: "读取文本", k_gettext: "取得文本", k_puttext: "写出文本",
    c_process: "进程在运行", tip_process: "写出名称的一部分即可",
    t_clipboard: "剪贴板", t_wintitle: "前台窗口标题",
    t_process: "前台程序", t_file: "文件", f_append: "追加到末尾",
    a_anchor: "相对另一张图片", f_anchor: "锚点", f_edge: "按轮廓",
    tip_edge: "比较形状而非明暗, 换主题也能匹配",
    c_element: "界面元素", k_findelem: "查找元素", k_clickelem: "点按元素",
    f_name: "名称", f_autoid: "Id", f_control: "类型", f_any: "任意",
    f_in_front: "仅前台窗口", f_invoke: "交给程序",
    tip_invoke: "请程序自己按下, 而不是按坐标点击",
    tip_uia: "屏幕阅读器会念出的名称; 游戏自绘界面, 不会公开任何元素",
    img_overlay: "显示脚本正在看哪里",
    tip_overlay: "覆盖全屏的透明窗口, 画出最近的搜索范围和命中位置",
    m_onmiss: "找不到时：", m_continue: "继续下一步", m_stop: "停止脚本",
    m_break: "跳出循环", m_retry: "重试", m_times: "次",
    m_delay: "间隔 (毫秒)",
    tip_onmiss: "1.5.0 之前这些步骤都会默默继续 —— 夜间运行的宏就是这样对着空白点击三个小时的",
    k_call: "调用宏", f_macro_file: "文件", tip_call: "在此处运行另一个宏的脚本；变量共享，嵌套上限为 8 层",
    call_depth: "调用嵌套上限为 {}",
    fast_capture: "快速屏幕捕获",
    tip_fast_capture: "用 Desktop Duplication 取代 GDI：整屏约快 5 倍，小区域约快 20 倍。本机不支持时会自动退回 GDI。",
    sec_vars: "🔎 变量", vars_open: "查看运行", vars_title: "变量",
    vars_none: "尚未设置任何变量", vars_name: "名称", vars_value: "值",
    vars_step: "步骤", vars_stepmode: "每步之前暂停", vars_stepnext: "▶ 下一步",
    tip_stepmode: "脚本在每一步之前停下，等待「下一步」",
    vars_running: "运行中", vars_idle: "未运行",
    rec_shots: "📸 带图片的点击：{}", rec_shots_ask: "把录制的 {} 次点击改成按图片查找而不是按坐标的步骤吗？",
    rec_shots_make: "生成图片步骤", rec_shots_skip: "保留坐标",
    rec_shots_done: "已生成 {} 个图片步骤",
    rec_shots_cb: "每次点击时截取图片",
    tip_rec_shots: "录制时保存每次点击周围的一小块方形图像，之后就能把录制变成无论按钮移到哪里都能找到它的步骤",
    rec_shots_size: "方块大小 (px)", rec_shots_miss: "找不到图片时：",
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
    // Saturating rather than plain: the UI bounds `ms`, but a function that is only
    // total because of its callers is a trap for the next caller.
    let us = ms.saturating_mul(1_000);
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
    // Both neighbours are read through `get`; this one used a raw index, and it sits
    // ahead of the `get_mut` below that was meant to cover the whole function. A
    // selection left pointing past a recording that another edit has since trimmed
    // reached it first, and the release profile aborts on panic - so the editor took
    // the whole application with it.
    let lo = index.checked_sub(1).and_then(|i| data.events.get(i)).map_or(0, |e| e.t_us);
    let hi = data.events.get(index + 1).map(|e| e.t_us).unwrap_or(u64::MAX);
    if let Some(ev) = data.events.get_mut(index) {
        // `clamp` panics on an inverted window, which unsorted timestamps would give.
        ev.t_us = t_us.clamp(lo, hi.max(lo));
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
    /// So does the variables watcher.
    vars_open: bool,
    /// Set when a recording ends with squares to offer. Cleared either way, so the
    /// offer is made once per recording and never nags.
    shots_offer: bool,
    /// Whether we were recording on the previous frame, which is how the moment a
    /// recording ends is noticed without the transport knowing about the UI.
    was_recording: bool,
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
    /// What the panel does to the pixels, and what it expects to read. Both are
    /// here so a profile can be tried against a real screen before it is written
    /// into a step, which is the only way to choose one honestly.
    ocr_prep: ocr::Prep,
    ocr_expect: ocr::Expect,
    /// Fit of the last reading, and the profile that produced it.
    ocr_fit: Option<(f64, ocr::Prep)>,
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
    /// The expander's entries while they are being edited. A working copy rather
    /// than the live one: the keyboard hook reads the live book on every keystroke,
    /// and holding that lock across a frame of rendering is exactly how a low-level
    /// hook gets itself unhooked.
    exp_book: expander::Book,
    /// The exclusion list is a `Vec` in the file and one line in the UI.
    exp_excluded: String,
    exp_dirty: bool,
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
        let exp_book = expander::snapshot();
        Self {
            exp_excluded: exp_book.excluded_windows.join(", "),
            exp_book,
            exp_dirty: false,
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
            vars_open: false,
            shots_offer: false,
            was_recording: false,
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
            ocr_prep: ocr::Prep::default(),
            ocr_expect: ocr::Expect::default(),
            ocr_fit: None,
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

    /// Writes the squares as templates and rewrites the recording as a script.
    ///
    /// Everything or nothing: if a PNG will not write, the pictures that did write
    /// are left where they are but the script is not replaced, so the recording the
    /// user just made is never traded for a half-built script.
    fn make_click_image_steps(&mut self) {
        let s = self.strs();
        let shots = self.state.click_shots.lock().clone();
        if shots.is_empty() {
            return;
        }
        let stamp = recording_stamp();
        let mut names = Vec::with_capacity(shots.len());
        let mut failed: Option<String> = None;
        for (n, shot) in shots.iter().enumerate() {
            let name = click_shot_name(&stamp, n + 1);
            match save_click_shot(shot, &name) {
                Ok(()) => names.push(name),
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(e) = failed {
            self.status_msg = s.save_err.replace("{}", &e);
            return;
        }
        let threshold = self.config.img_threshold.clamp(0.3, 1.0);
        let miss = self.config.click_shot_miss;
        let data = self.state.macro_data.lock().clone();
        let (script, made) = script_from_click_shots(&data, &shots, &names, threshold, miss);
        if made == 0 {
            // Every press was a drag or was never released. Saying so beats
            // replacing the script with one `Play events` step and calling it done.
            self.status_msg = s.rec_shots_done.replace("{}", "0");
            return;
        }
        self.edit(|d| d.script = script);
        self.ed_view = 2;
        self.status_msg = s.rec_shots_done.replace("{}", &made.to_string());
        info!("recording turned into {made} picture steps, templates rec_{stamp}_*");
    }

    /// The offer itself, drawn over the main window while it stands.
    fn shots_offer_ui(&mut self, ctx: &egui::Context) {
        if !self.shots_offer {
            return;
        }
        let s = self.strs();
        let count = self.state.click_shots.lock().len();
        if count == 0 {
            self.shots_offer = false;
            return;
        }
        let mut make = false;
        let mut skip = false;
        egui::Window::new(s.rec_shots.replace("{}", &count.to_string()))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(s.rec_shots_ask.replace("{}", &count.to_string()));
                ui.add_space(6.0);
                // The one decision worth making here, because it is the one that
                // decides what a broken macro does at three in the morning.
                let mut miss = self.config.click_shot_miss;
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.rec_shots_miss);
                    egui::ComboBox::from_id_salt("shot_miss")
                        .selected_text(miss.name(s))
                        .width(150.0)
                        .show_ui(ui, |ui| {
                            for i in 0..OnMiss::COUNT {
                                let opt = OnMiss::from_index(i);
                                if ui.selectable_label(miss.index() == i, opt.name(s)).clicked() {
                                    miss = opt;
                                }
                            }
                        });
                });
                self.config.click_shot_miss = miss;
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    make = ui.button(s.rec_shots_make).clicked();
                    skip = ui.button(s.rec_shots_skip).clicked();
                });
            });
        if make {
            self.make_click_image_steps();
            self.shots_offer = false;
        } else if skip {
            // The squares are dropped, not kept: an offer that was declined and
            // then quietly reappears after the next recording is worse than no
            // offer at all.
            self.state.click_shots.lock().clear();
            self.shots_offer = false;
        }
    }

    fn set_template(&mut self, w: u32, h: u32, rgba: Vec<u8>, name: String) {
        self.template = Some(Arc::new(vision::Template { w, h, rgba, name }));
        *LAST_HIT.lock() = None;
    }

    /// Reads the panel's rectangle with the panel's profile.
    ///
    /// One place, because the panel reads from two: the button, and the moment the
    /// second corner is picked. They must agree about which profile was used or the
    /// number shown next to the text is a lie.
    fn read_ocr_panel(&mut self, s: &Strings) {
        let (x, y, w, h) = self.ocr_rect;
        match ocr::read_region_as(x, y, w, h, self.ocr_prep, &self.ocr_expect) {
            Ok(r) => {
                let text = r.text();
                self.ocr_fit = Some((r.quality, r.prep));
                self.ocr_text =
                    if text.is_empty() { s.ocr_empty.to_string() } else { text };
            }
            Err(e) => {
                self.ocr_fit = None;
                self.ocr_text = format!("{} — {e}", s.ocr_off);
            }
        }
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
    let names = [
        s.c_always,
        s.c_var,
        s.c_image,
        s.c_pixel,
        s.c_window,
        s.c_text,
        s.c_process,
        s.c_element,
    ];
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
                changed |= value_ui(ui, s, &format!("{salt}_cv"), value);
            });
        }
        Condition::Image {
            template,
            threshold,
            area,
            lose_at,
            stable_of,
            stable_in,
            edge,
        } => {
            ui.horizontal_wrapped(|ui| {
                ui.label(s.f_template);
                changed |= ui
                    .add(egui::TextEdit::singleline(template).desired_width(150.0))
                    .changed();
                changed |= template_picker(ui, &format!("{salt}_tpl"), template);
                changed |= ui
                    .add(egui::DragValue::new(threshold).range(0.3..=1.0).speed(0.01))
                    .changed();
            });
            changed |= area_ui(ui, s, salt, area);
            ui.horizontal_wrapped(|ui| {
                ui.label(s.f_lose_at);
                changed |= ui
                    .add(egui::DragValue::new(lose_at).range(0.0..=1.0).speed(0.01))
                    .changed();
                ui.label(s.f_stable);
                changed |= ui
                    .add(egui::DragValue::new(stable_of).range(0..=32))
                    .changed();
                ui.label("/");
                changed |= ui
                    .add(egui::DragValue::new(stable_in).range(0..=32))
                    .changed();
                changed |= ui.checkbox(edge, s.f_edge).on_hover_text(s.tip_edge).changed();
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
        Condition::Element { query } => {
            changed |= query_ui(ui, s, salt, query);
        }
        Condition::Process { name } => {
            ui.horizontal_wrapped(|ui| {
                changed |= ui
                    .add(egui::TextEdit::singleline(name).desired_width(200.0))
                    .on_hover_text(s.tip_process)
                    .changed();
            });
        }
        Condition::Text { x, y, w, h, needle, prep } => {
            ui.horizontal_wrapped(|ui| {
                ui.label(s.f_needle);
                changed |= ui
                    .add(egui::TextEdit::singleline(needle).desired_width(180.0))
                    .changed();
                changed |= prep_picker(ui, s, &format!("{salt}_ctxt"), prep);
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
            StepKind::WaitFor { cond, appear, timeout_ms, miss } => {
                changed |= condition_ui(ui, s, "wf", cond, Some(self.ocr_rect));
                ui.horizontal_wrapped(|ui| {
                    changed |= ui.selectable_value(appear, true, s.f_appear).clicked();
                    changed |= ui.selectable_value(appear, false, s.f_gone).clicked();
                    ui.label(s.f_timeout);
                    changed |= ui
                        .add(egui::DragValue::new(timeout_ms).range(0..=3_600_000).speed(50.0))
                        .changed();
                });
                changed |= miss_ui(ui, s, "wf", miss);
            }
            StepKind::ClickImage { template, threshold, button, area, edge, miss } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_template);
                    changed |= ui
                        .add(egui::TextEdit::singleline(template).desired_width(140.0))
                        .changed();
                    changed |= template_picker(ui, "ci_tpl", template);
                    changed |= ui
                        .add(egui::DragValue::new(threshold).range(0.3..=1.0).speed(0.01))
                        .changed();
                    changed |= button_picker(ui, s, "ci", button);
                    changed |= ui.checkbox(edge, s.f_edge).on_hover_text(s.tip_edge).changed();
                });
                changed |= area_ui(ui, s, "ci", area);
                changed |= miss_ui(ui, s, "ci", miss);
            }
            StepKind::FindImage { template, threshold, area, var, edge, miss } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_template);
                    changed |= ui
                        .add(egui::TextEdit::singleline(template).desired_width(140.0))
                        .changed();
                    changed |= template_picker(ui, "fi_tpl", template);
                    changed |= ui
                        .add(egui::DragValue::new(threshold).range(0.3..=1.0).speed(0.01))
                        .changed();
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_into);
                    changed |= ui
                        .add(egui::TextEdit::singleline(var).desired_width(110.0))
                        .changed();
                    changed |= ui.checkbox(edge, s.f_edge).on_hover_text(s.tip_edge).changed();
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(s.f_find_hint.replace("{}", var)).weak().small(),
                    );
                });
                changed |= area_ui(ui, s, "fi", area);
                changed |= miss_ui(ui, s, "fi", miss);
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
                    changed |= value_ui(ui, s, "scr_sv", value);
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
            StepKind::FindElement { query, var, timeout_ms, miss } => {
                changed |= query_ui(ui, s, "fe", query);
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_into);
                    changed |= ui
                        .add(egui::TextEdit::singleline(var).desired_width(110.0))
                        .changed();
                    ui.label(s.f_timeout);
                    changed |= ui
                        .add(egui::DragValue::new(timeout_ms).range(0..=600_000).speed(50))
                        .changed();
                });
                changed |= miss_ui(ui, s, "fe", miss);
            }
            StepKind::ClickElement { query, button, invoke, timeout_ms, miss } => {
                changed |= query_ui(ui, s, "ce", query);
                ui.horizontal_wrapped(|ui| {
                    changed |= button_picker(ui, s, "ce", button);
                    changed |=
                        ui.checkbox(invoke, s.f_invoke).on_hover_text(s.tip_invoke).changed();
                    ui.label(s.f_timeout);
                    changed |= ui
                        .add(egui::DragValue::new(timeout_ms).range(0..=600_000).speed(50))
                        .changed();
                });
                changed |= miss_ui(ui, s, "ce", miss);
            }
            StepKind::Call { path, miss } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_macro_file).on_hover_text(s.tip_call);
                    changed |= ui
                        .add(egui::TextEdit::singleline(path).desired_width(220.0))
                        .on_hover_text(s.tip_call)
                        .changed();
                    if ui.button("…").clicked() {
                        if let Some(picked) = rfd::FileDialog::new()
                            .add_filter("Macro", &["json", "mrz", "gz"])
                            .set_directory(paths::data_dir())
                            .pick_file()
                        {
                            // Kept relative when it sits beside the macro that names
                            // it, so a project folder can be moved or shared whole.
                            let here = self
                                .state
                                .current_path
                                .lock()
                                .as_ref()
                                .and_then(|p| p.parent().map(|d| d.to_path_buf()));
                            *path = match here.and_then(|d| picked.strip_prefix(d).ok().map(|r| r.to_path_buf())) {
                                Some(rel) => rel.to_string_lossy().to_string(),
                                None => picked.to_string_lossy().to_string(),
                            };
                            changed = true;
                        }
                    }
                });
                ui.label(
                    egui::RichText::new(
                        s.call_depth.replace("{}", &MAX_CALL_DEPTH.to_string()),
                    )
                    .weak()
                    .small(),
                );
                changed |= miss_ui(ui, s, "call", miss);
            }
            StepKind::ReadText { x, y, w, h, var, prep } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_var);
                    changed |= ui
                        .add(egui::TextEdit::singleline(var).desired_width(100.0))
                        .changed();
                    changed |= prep_picker(ui, s, "rt", prep);
                });
                changed |= region_ui(ui, s, x, y, w, h, Some(self.ocr_rect));
            }
            StepKind::GetText { source, var } => {
                ui.horizontal_wrapped(|ui| {
                    changed |= source_picker(ui, s, "gt", source);
                    ui.label(s.f_into);
                    changed |= ui
                        .add(egui::TextEdit::singleline(var).desired_width(110.0))
                        .changed();
                });
            }
            StepKind::PutText { sink, text } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_text);
                    changed |= ui
                        .add(egui::TextEdit::singleline(text).desired_width(190.0))
                        .on_hover_text(s.tip_value_text)
                        .changed();
                    changed |= sink_picker(ui, s, "pt", sink);
                });
            }
            StepKind::ReadNumber { x, y, w, h, var, prep, expect } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(s.f_var);
                    changed |= ui
                        .add(egui::TextEdit::singleline(var).desired_width(100.0))
                        .changed();
                    changed |= prep_picker(ui, s, "rn", prep);
                    changed |= expect_picker(ui, s, "rn", expect);
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

    /// The other half of the debug overlay: what the script knows, not where it
    /// is looking.
    ///
    /// Its own window rather than a panel in the main one for the same reason the
    /// editor has one: this is read while another application is in front, and a
    /// table that is only visible when the recorder has focus is a table nobody
    /// reads. Only open costs anything - `set_watching_vars` is what makes the
    /// interpreter publish at all.
    fn vars_viewport(&mut self, ctx: &egui::Context) {
        set_watching_vars(self.vars_open);
        if !self.vars_open {
            // Step mode with the window shut would park a run with no button to
            // free it. Closing the window is therefore also "let it go".
            self.state.step_mode.store(false, Ordering::Relaxed);
            return;
        }
        let s = self.strs();
        let title = s.vars_title;
        let mut close = false;
        let state = self.state.clone();
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("macro_vars"),
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([420.0, 480.0])
                .with_min_inner_size([300.0, 220.0]),
            |ctx, _class| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let view = script_view();
                    let running = view.as_ref().is_some_and(|v| v.running);

                    ui.horizontal_wrapped(|ui| {
                        ui.label(if running { s.vars_running } else { s.vars_idle });
                        ui.separator();
                        let mut stepping = state.step_mode.load(Ordering::Relaxed);
                        if ui
                            .checkbox(&mut stepping, s.vars_stepmode)
                            .on_hover_text(s.tip_stepmode)
                            .changed()
                        {
                            state.step_mode.store(stepping, Ordering::Relaxed);
                            // Turning it off has to release a run already parked;
                            // turning it on has to start from a clean slate, or a
                            // leftover raised flag lets the first step through.
                            state.step_once.store(!stepping, Ordering::Relaxed);
                        }
                        let waiting = view.as_ref().is_some_and(|v| v.waiting);
                        if ui
                            .add_enabled(waiting, egui::Button::new(s.vars_stepnext))
                            .clicked()
                        {
                            state.step_once.store(true, Ordering::Relaxed);
                        }
                    });
                    ui.separator();

                    match &view {
                        Some(v) => {
                            let depth = if v.depth > 0 {
                                format!("  ↳{}", v.depth)
                            } else {
                                String::new()
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} {}{depth}  {}",
                                    s.vars_step, v.pc, v.step
                                ))
                                .monospace(),
                            );
                            ui.separator();
                            if v.vars.is_empty() {
                                ui.label(egui::RichText::new(s.vars_none).weak());
                            } else {
                                egui::ScrollArea::vertical()
                                    .id_salt("vars_rows")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        egui::Grid::new("vars_grid")
                                            .num_columns(2)
                                            .striped(true)
                                            .show(ui, |ui| {
                                                ui.label(
                                                    egui::RichText::new(s.vars_name).strong(),
                                                );
                                                ui.label(
                                                    egui::RichText::new(s.vars_value).strong(),
                                                );
                                                ui.end_row();
                                                for (k, val) in &v.vars {
                                                    ui.label(
                                                        egui::RichText::new(k).monospace(),
                                                    );
                                                    // Newlines from an OCR read
                                                    // would each claim a row.
                                                    ui.label(
                                                        egui::RichText::new(
                                                            val.replace('\n', " ⏎ "),
                                                        )
                                                        .monospace(),
                                                    );
                                                    ui.end_row();
                                                }
                                            });
                                    });
                            }
                        }
                        None => {
                            ui.label(egui::RichText::new(s.vars_none).weak());
                        }
                    }
                });
                // A parked run publishes nothing new, so nothing would wake the
                // window up to notice the button being pressable.
                ctx.request_repaint_after(Duration::from_millis(120));
                if ctx.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
            },
        );
        if close {
            self.vars_open = false;
            self.state.step_mode.store(false, Ordering::Relaxed);
            self.state.step_once.store(true, Ordering::Relaxed);
        }
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
                    self.read_ocr_panel(s);
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
        // The moment a recording stops is the moment the squares are worth
        // something, and it is also the only moment the user is looking at this
        // window rather than at the thing they were recording.
        if self.was_recording && !recording {
            self.shots_offer = !self.state.click_shots.lock().is_empty();
        }
        self.was_recording = recording;

        let ctx = ui.ctx().clone();
        self.editor_viewport(&ctx);
        self.vars_viewport(&ctx);
        self.shots_offer_ui(&ctx);
        // The overlay owns its own window on its own thread; this only says whether
        // it should be there. Idempotent, so calling it every frame costs a load.
        WATCHING.store(self.config.debug_overlay, Ordering::Relaxed);
        overlay::set_enabled(self.config.debug_overlay);

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
                        // Spelled out instead of hidden in a tooltip: the setting does
                        // nothing to a recording that already holds real movement, and
                        // that is indistinguishable from it being broken.
                        ui.label(egui::RichText::new(s.tip_human).weak().small());
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.mouse_jitter);
                        ui.add(
                            egui::DragValue::new(&mut self.config.mouse_jitter_px).range(0..=60),
                        );
                    });
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
                    ui.separator();
                    ui.checkbox(&mut self.config.record_click_shots, s.rec_shots_cb)
                        .on_hover_text(s.tip_rec_shots);
                    if self.config.record_click_shots {
                        ui.horizontal(|ui| {
                            ui.label(s.rec_shots_size);
                            ui.add(
                                egui::DragValue::new(&mut self.config.click_shot_size)
                                    .range(16..=512)
                                    .speed(2.0),
                            );
                        });
                        let n = self.state.click_shots.lock().len();
                        if n > 0 {
                            ui.label(
                                egui::RichText::new(
                                    s.rec_shots.replace("{}", &n.to_string()),
                                )
                                .weak()
                                .small(),
                            );
                        }
                    }
                });

                // ---- target window ---------------------------------------------
                // Everything that depends on which window the macro is aimed at, in
                // one place: what it is, how coordinates follow it, and how well it
                // keeps up. Splitting these across Playback and Recording made three
                // halves of one decision look like three unrelated settings.
                egui::CollapsingHeader::new(s.sec_target).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.tgt_title);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.target_title)
                                .desired_width(200.0),
                        );
                    });
                    // The recording already noted which window was in front when it
                    // started, so the title never has to be typed out by hand.
                    let recorded = self
                        .state
                        .macro_data
                        .lock()
                        .anchor
                        .as_ref()
                        .map(|a| a.title.clone());
                    ui.horizontal_wrapped(|ui| {
                        let hit = ui
                            .add_enabled(
                                recorded.is_some(),
                                egui::Button::new(s.tgt_from_rec),
                            )
                            .clicked();
                        if hit {
                            if let Some(t) = &recorded {
                                self.config.target_title = t.clone();
                            }
                        }
                    });
                    let shown = recorded.unwrap_or_else(|| s.anchor_none.to_string());
                    ui.label(
                        egui::RichText::new(s.anchor_of.replace("{}", &shown)).weak().small(),
                    );
                    ui.checkbox(&mut self.config.target_pause_unfocused, s.tgt_focus);

                    ui.separator();
                    ui.label(egui::RichText::new(s.grp_anchor).strong());
                    ui.checkbox(&mut self.config.record_window_anchor, s.anchor_rec);
                    ui.checkbox(&mut self.config.use_window_anchor, s.anchor_use);
                    if self.config.use_window_anchor {
                        ui.checkbox(&mut self.config.anchor_scale, s.anchor_scale);
                    }

                    ui.separator();
                    ui.label(egui::RichText::new(s.grp_speed).strong());
                    ui.label(egui::RichText::new(s.tip_perf).weak().small());
                    ui.checkbox(&mut self.config.frame_guard, s.fg_cb)
                        .on_hover_text(s.tip_frame_guard);
                    if self.config.frame_guard {
                        ui.checkbox(&mut self.config.frame_guard_auto, s.fg_auto);
                        if self.config.frame_guard_auto {
                            let m = self.state.perf_frame_us.load(Ordering::Relaxed);
                            let text = if m > 0 {
                                s.fg_measured
                                    .replace("{}", &format!("{:.1}", m as f64 / 1000.0))
                            } else {
                                s.fg_manual.to_string()
                            };
                            ui.label(egui::RichText::new(text).weak().small());
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.label(s.fg_fps);
                            ui.add(
                                egui::DragValue::new(&mut self.config.frame_guard_fps)
                                    .range(5..=240),
                            );
                        });
                        // Seeing what the guard cost is what tells you whether the FPS
                        // figure is set sensibly.
                        let added = self.state.fg_added_us.load(Ordering::Relaxed);
                        if added > 0 {
                            ui.label(
                                egui::RichText::new(s.fg_added.replace(
                                    "{}",
                                    &format!("{:.1}", added as f64 / 1_000_000.0),
                                ))
                                .weak()
                                .small(),
                            );
                        }
                    }
                    ui.checkbox(&mut self.config.perf_enabled, s.perf_cb);
                    let st = *self.state.perf_stats.lock();
                    // Under a handful of samples every percentile is the same number,
                    // which would look like a measurement without being one.
                    if !st.found || st.samples < 8 {
                        ui.label(egui::RichText::new(s.perf_none).weak());
                    } else {
                        let fps =
                            |us: u64| if us == 0 { 0.0 } else { 1_000_000.0 / us as f64 };
                        ui.label(s.perf_frametime.replace(
                            "{}",
                            &format!("{:.1}", st.avg_us as f64 / 1000.0),
                        ));
                        ui.label(
                            s.perf_avg.replace("{}", &format!("{:.0}", fps(st.avg_us))),
                        );
                        ui.label(
                            s.perf_low1.replace("{}", &format!("{:.0}", fps(st.p99_us))),
                        );
                        ui.label(
                            s.perf_low01
                                .replace("{}", &format!("{:.0}", fps(st.p999_us))),
                        );
                        ui.label(s.perf_stutter.replace("{}", &st.stutters.to_string()));
                    }
                    if self.config.perf_enabled || self.config.frame_guard {
                        ui.ctx().request_repaint_after(Duration::from_millis(400));
                    }
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

                // ---- text expander ---------------------------------------------------
                egui::CollapsingHeader::new(s.sec_expander).show(ui, |ui| {
                    ui.label(egui::RichText::new(s.tip_expander).weak().small());
                    let triggers =
                        [s.exp_tr_inherit, s.exp_tr_delim, s.exp_tr_prefix, s.exp_tr_instant];
                    let inserts = [s.exp_in_type, s.exp_in_paste];
                    let actions =
                        [s.exp_ac_text, s.exp_ac_play, s.exp_ac_stop, s.exp_ac_run];

                    self.exp_dirty |=
                        ui.checkbox(&mut self.exp_book.enabled, s.exp_enable).changed();

                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.exp_default_trigger);
                        let mut idx = trigger_index(&self.exp_book.default_trigger).max(1);
                        egui::ComboBox::from_id_salt("exp_deftrig")
                            .selected_text(triggers[idx])
                            .show_ui(ui, |ui| {
                                // Inherit is meaningless as the global answer, so the
                                // global list starts at the one after it.
                                for (i, name) in triggers.iter().enumerate().skip(1) {
                                    if ui.selectable_label(idx == i, *name).clicked() {
                                        idx = i;
                                    }
                                }
                            });
                        let picked = trigger_from_index(idx, "");
                        if picked != self.exp_book.default_trigger {
                            self.exp_book.default_trigger = picked;
                            self.exp_dirty = true;
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.exp_delims);
                        self.exp_dirty |= ui
                            .add(
                                egui::TextEdit::singleline(&mut self.exp_book.delimiters)
                                    .desired_width(180.0),
                            )
                            .changed();
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(s.exp_excluded_lbl);
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut self.exp_excluded)
                                    .desired_width(180.0),
                            )
                            .changed()
                        {
                            self.exp_book.excluded_windows = self
                                .exp_excluded
                                .split(',')
                                .map(|x| x.trim().to_string())
                                .filter(|x| !x.is_empty())
                                .collect();
                            self.exp_dirty = true;
                        }
                    });

                    ui.separator();
                    ui.label(
                        egui::RichText::new(s.exp_count.replace(
                            "{}",
                            &self
                                .exp_book
                                .entries
                                .iter()
                                .filter(|e| e.enabled)
                                .count()
                                .to_string(),
                        ))
                        .weak(),
                    );

                    let mut remove: Option<usize> = None;
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        for i in 0..self.exp_book.entries.len() {
                            ui.horizontal_wrapped(|ui| {
                                let e = &mut self.exp_book.entries[i];
                                self.exp_dirty |= ui.checkbox(&mut e.enabled, "").changed();
                                self.exp_dirty |= ui
                                    .add(
                                        egui::TextEdit::singleline(&mut e.abbr)
                                            .hint_text(s.exp_abbr)
                                            .desired_width(80.0),
                                    )
                                    .changed();

                                let mut ti = trigger_index(&e.trigger);
                                egui::ComboBox::from_id_salt(format!("exp_tr{i}"))
                                    .selected_text(triggers[ti])
                                    .width(110.0)
                                    .show_ui(ui, |ui| {
                                        for (k, name) in triggers.iter().enumerate() {
                                            if ui.selectable_label(ti == k, *name).clicked() {
                                                ti = k;
                                            }
                                        }
                                    });
                                let prefix = match &e.trigger {
                                    expander::Trigger::Prefix(p) => p.clone(),
                                    _ => ";;".to_string(),
                                };
                                let picked = trigger_from_index(ti, &prefix);
                                if picked != e.trigger {
                                    e.trigger = picked;
                                    self.exp_dirty = true;
                                }
                                if let expander::Trigger::Prefix(p) = &mut e.trigger {
                                    self.exp_dirty |= ui
                                        .add(
                                            egui::TextEdit::singleline(p)
                                                .hint_text(s.exp_prefix)
                                                .desired_width(44.0),
                                        )
                                        .changed();
                                }

                                let mut ai = action_index(&e.action);
                                egui::ComboBox::from_id_salt(format!("exp_ac{i}"))
                                    .selected_text(actions[ai])
                                    .width(104.0)
                                    .show_ui(ui, |ui| {
                                        for (k, name) in actions.iter().enumerate() {
                                            if ui.selectable_label(ai == k, *name).clicked() {
                                                ai = k;
                                            }
                                        }
                                    });
                                let want_act = action_from_index(ai);
                                if want_act != e.action {
                                    e.action = want_act;
                                    self.exp_dirty = true;
                                }

                                let mut ii = usize::from(e.insert == expander::Insert::Paste);
                                egui::ComboBox::from_id_salt(format!("exp_in{i}"))
                                    .selected_text(inserts[ii])
                                    .width(84.0)
                                    .show_ui(ui, |ui| {
                                        for (k, name) in inserts.iter().enumerate() {
                                            if ui.selectable_label(ii == k, *name).clicked() {
                                                ii = k;
                                            }
                                        }
                                    });
                                let want = if ii == 1 {
                                    expander::Insert::Paste
                                } else {
                                    expander::Insert::Type
                                };
                                if want != e.insert {
                                    e.insert = want;
                                    self.exp_dirty = true;
                                }

                                if ui.button("🗑").clicked() {
                                    remove = Some(i);
                                }
                            });
                            self.exp_dirty |= ui
                                .add(
                                    egui::TextEdit::multiline(
                                        &mut self.exp_book.entries[i].text,
                                    )
                                    .hint_text(s.exp_text)
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY),
                                )
                                .changed();
                            ui.separator();
                        }
                    });
                    if let Some(i) = remove {
                        self.exp_book.entries.remove(i);
                        self.exp_dirty = true;
                    }

                    ui.horizontal_wrapped(|ui| {
                        if ui.button(s.exp_add).clicked() {
                            self.exp_book.entries.push(expander::Entry {
                                enabled: true,
                                abbr: String::new(),
                                text: String::new(),
                                trigger: expander::Trigger::Inherit,
                                insert: expander::Insert::Type,
                                action: expander::Action::Text,
                            });
                            self.exp_dirty = true;
                        }
                        if ui.button(s.exp_reload).clicked() {
                            expander::load();
                            self.exp_book = expander::snapshot();
                            self.exp_excluded = self.exp_book.excluded_windows.join(", ");
                            self.exp_dirty = false;
                        }
                        if ui.button(s.exp_open).clicked() {
                            run_program(&paths::expansions_path().to_string_lossy(), "");
                        }
                    });

                    // Applied at the end of the frame rather than on each keystroke:
                    // an entry is edited a character at a time, and writing the file
                    // that often would be silly.
                    if self.exp_dirty {
                        expander::replace(self.exp_book.clone());
                        if let Err(e) = expander::save_current() {
                            self.status_msg = format!("expansions.json: {e}");
                        }
                        self.exp_dirty = false;
                    }
                });

                // ---- text on screen --------------------------------------------------
                egui::CollapsingHeader::new(s.sec_ocr).show(ui, |ui| {
                    ui.label(egui::RichText::new(s.tip_ocr).weak().small());
                    let (mut rx, mut ry, mut rw, mut rh) = self.ocr_rect;
                    region_ui(ui, s, &mut rx, &mut ry, &mut rw, &mut rh, None);
                    self.ocr_rect = (rx, ry, rw, rh);

                    ui.horizontal_wrapped(|ui| {
                        prep_picker(ui, s, "panel", &mut self.ocr_prep);
                        expect_picker(ui, s, "panel", &mut self.ocr_expect);
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui.button(s.ocr_read).clicked() {
                            self.read_ocr_panel(s);
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

                    if let Some((fit, used)) = self.ocr_fit {
                        // Not a confidence from the engine - a judgement about the
                        // shape of what came back. Shown so a profile can be chosen
                        // by comparing numbers rather than by squinting.
                        let names =
                            [s.p_none, s.p_ui, s.p_small, s.p_game, s.p_digits, s.p_auto];
                        let name = names[used.index().min(names.len() - 1)];
                        ui.label(
                            egui::RichText::new(
                                s.ocr_quality
                                    .replacen("{}", &format!("{fit:.2}"), 1)
                                    .replacen("{}", name, 1),
                            )
                            .weak()
                            .small(),
                        );
                    }
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
                                .set_directory(paths::templates_dir())
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
                                    .set_directory(paths::templates_dir())
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
                                if saved.is_ok() {
                                    // Written now because now is the only moment the
                                    // scale it was cut at is still known.
                                    save_template_meta(
                                        &path,
                                        &TemplateMeta { dpi: platform::current_dpi() },
                                    );
                                }
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
                    ui.checkbox(&mut self.config.debug_overlay, s.img_overlay)
                        .on_hover_text(s.tip_overlay);
                    ui.checkbox(&mut self.config.fast_capture, s.fast_capture)
                        .on_hover_text(s.tip_fast_capture);
                    if ui.button(s.vars_open).on_hover_text(s.tip_stepmode).clicked() {
                        self.vars_open = true;
                    }
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
    selftest: Option<String>,
    help: bool,
    version: bool,
}

fn parse_cli() -> CliArgs {
    let mut args = CliArgs {
        play: None,
        loops: None,
        speed: None,
        no_gui: false,
        selftest: None,
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
            "--selftest" => args.selftest = it.next(),
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
        --selftest <W>   Run a self-test and exit.
                         W: timing, vision, script[=rounds],
                            churn[=secs], soak[=hours, fractions allowed]
    -h, --help           Show this help
    -V, --version        Show the version

Without --no-gui the options simply pre-load the GUI.
";

/// Plays a macro without any window. Shared by `--no-gui` and exported executables.
/// An evenly spaced recording of `n` events: move, press, release, repeating.
///
/// Synthetic on purpose. A real recording has uneven gaps, and uneven gaps make it
/// impossible to tell scheduler jitter from the recording's own shape.
fn synthetic_macro(n: usize, gap_us: u64) -> MacroData {
    let events: Vec<MacroEvent> = (0..n)
        .map(|i| {
            let t = i as u64 * gap_us;
            let x = 200 + (i % 300) as i32;
            let kind = match i % 3 {
                0 => InputEventKind::MouseMove { x, y: 300, dx: 1, dy: 0 },
                1 => InputEventKind::MouseButton {
                    button: MouseButton::Left,
                    down: true,
                    x,
                    y: 300,
                },
                _ => InputEventKind::MouseButton {
                    button: MouseButton::Left,
                    down: false,
                    x,
                    y: 300,
                },
            };
            MacroEvent { t_us: t, kind }
        })
        .collect();
    let dur = events.last().map(|e| e.t_us).unwrap_or(0);
    MacroData::new(events, dur)
}

/// A recording shaped like a person: a stream of small moves with one click every
/// 350 ms, rather than sixty-six clicks a second.
///
/// The evenly spaced macro above is deliberately pathological, and it makes the frame
/// guard look ruinous - a press every 15 ms cannot be stretched to two frames without
/// dominating the run. Real recordings leave far more room than the guard ever asks
/// for, and this one exists to show by how much.
fn human_paced_macro(cycles: usize) -> MacroData {
    let mut events = Vec::new();
    let mut t = 0u64;
    for c in 0..cycles {
        for i in 0..50i32 {
            let x = 300 + (c % 200) as i32 + i;
            events.push(MacroEvent {
                t_us: t,
                kind: InputEventKind::MouseMove { x, y: 400, dx: 1, dy: 0 },
            });
            t += 5_000;
        }
        let x = 300 + (c % 200) as i32;
        events.push(MacroEvent {
            t_us: t,
            kind: InputEventKind::MouseButton { button: MouseButton::Left, down: true, x, y: 400 },
        });
        // A 60 ms press and a 40 ms gap before the next stream of moves.
        t += 60_000;
        events.push(MacroEvent {
            t_us: t,
            kind: InputEventKind::MouseButton { button: MouseButton::Left, down: false, x, y: 400 },
        });
        t += 40_000;
    }
    let dur = events.last().map(|e| e.t_us).unwrap_or(0);
    MacroData::new(events, dur)
}

struct TimingReport {
    label: String,
    dispatched: usize,
    mean_us: f64,
    p50_us: u64,
    p99_us: u64,
    max_us: u64,
    drift_us: i64,
    slips: u64,
    slipped_ms: u64,
    longest_burst: usize,
    guard_added_ms: u64,
    wall_ms: u128,
}

/// Runs one scenario through the real `playback_loop` with the OS calls suppressed.
fn timing_scenario(
    label: &str,
    data: &MacroData,
    speed: f64,
    guard_fps: Option<u64>,
    human: bool,
    stall_at: usize,
    stall_us: u64,
) -> TimingReport {
    let (tx, _rx) = unbounded();
    let state = AppState::new(tx);
    state.loop_play.store(false, Ordering::Relaxed);
    state.play_count_limit.store(1, Ordering::Relaxed);
    state.absolute_mouse.store(true, Ordering::Relaxed);
    state.human_mouse.store(human, Ordering::Relaxed);
    *state.speed.lock() = speed;
    match guard_fps {
        Some(fps) => {
            state.frame_guard.store(true, Ordering::Relaxed);
            state.frame_guard_auto.store(false, Ordering::Relaxed);
            state.frame_guard_fps.store(fps, Ordering::Relaxed);
        }
        None => state.frame_guard.store(false, Ordering::Relaxed),
    }
    state.playing.store(true, Ordering::Relaxed);
    let generation = state.play_generation.fetch_add(1, Ordering::SeqCst) + 1;

    selftest::arm(data.events.len(), stall_at, stall_us);
    let started = Instant::now();
    playback_loop(state.clone(), data.clone(), generation);
    let wall_ms = started.elapsed().as_millis();
    let (trace, slips, slipped_us) = selftest::disarm();

    // Lateness against the schedule the scheduler was actually working to, so a
    // deliberate slip or a guard hold is not counted twice: those move the schedule.
    let mut late: Vec<u64> =
        trace.iter().map(|(due, act)| act.saturating_sub(*due)).collect();
    let drift_us = trace
        .last()
        .map(|(due, act)| *act as i64 - *due as i64)
        .unwrap_or(0);

    // The point of the whole exercise: after a stall, does the backlog go out as a
    // burst? A healthy run has gaps near the recorded spacing; a catch-up storm has
    // a long run of dispatches microseconds apart.
    let mut longest_burst = 0usize;
    let mut current = 0usize;
    for w in trace.windows(2) {
        if w[1].1.saturating_sub(w[0].1) < 500 {
            current += 1;
            longest_burst = longest_burst.max(current);
        } else {
            current = 0;
        }
    }

    let n = late.len().max(1) as f64;
    let mean_us = late.iter().sum::<u64>() as f64 / n;
    late.sort_unstable();
    let at = |q: f64| -> u64 {
        if late.is_empty() {
            0
        } else {
            late[(((late.len() - 1) as f64) * q).round() as usize]
        }
    };

    TimingReport {
        label: label.to_string(),
        dispatched: trace.len(),
        mean_us,
        p50_us: at(0.5),
        p99_us: at(0.99),
        max_us: late.last().copied().unwrap_or(0),
        drift_us,
        slips,
        slipped_ms: slipped_us / 1000,
        longest_burst,
        guard_added_ms: state.fg_added_us.load(Ordering::Relaxed) / 1000,
        wall_ms,
    }
}

/// Median of `n` runs. Screen capture varies enough that one sample is a rumour.
fn median_ms(n: usize, mut f: impl FnMut()) -> f64 {
    let mut v: Vec<u128> = (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_micros()
        })
        .collect();
    v.sort_unstable();
    v[v.len() / 2] as f64 / 1000.0
}

/// The top-left `w` x `h` of a frame, so search cost can be measured against
/// different haystack sizes without re-capturing and inheriting capture's noise.
fn crop_frame(hay: &vision::Frame, w: u32, h: u32) -> vision::Frame {
    let (w, h) = (w.min(hay.w), h.min(hay.h));
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let src = (row * hay.w * 4) as usize;
        px.extend_from_slice(&hay.px[src..src + (w * 4) as usize]);
    }
    vision::Frame { x: hay.x, y: hay.y, w, h, px, order: hay.order }
}

/// A square cut out of whatever is on screen, which will therefore match.
fn crop_template(hay: &vision::Frame, at: u32, size: u32, name: &str) -> vision::Template {
    let size = size.min(hay.w.saturating_sub(at)).min(hay.h.saturating_sub(at)).max(2);
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for row in 0..size {
        let src = (((at + row) * hay.w + at) * 4) as usize;
        px.extend_from_slice(&hay.px[src..src + (size * 4) as usize]);
    }
    // A template is always red-first and always opaque; the mask reads the alpha
    // byte and a screen grab does not have one worth reading.
    vision::Frame { x: 0, y: 0, w: size, h: size, px, order: hay.order }.as_template(name)
}

/// Random pixels, which will not match anything on screen.
fn noise_template(size: u32, name: &str) -> vision::Template {
    let mut rng = Rng::new();
    let mut rgba = vec![255u8; (size * size * 4) as usize];
    for px in rgba.chunks_exact_mut(4) {
        px[0] = rng.below(256) as u8;
        px[1] = rng.below(256) as u8;
        px[2] = rng.below(256) as u8;
    }
    vision::Template { w: size, h: size, rgba, name: name.to_string() }
}

fn run_vision_selftest() -> Result<()> {
    // UI Automation is COM, and COM has to be started on the thread that uses it.
    // The playback thread does this for itself; this one has to be told.
    virtual_desktop::init_thread();
    let (vx, vy, vw, vh) = platform::virtual_screen_rect();
    let mpx = |w: u32, h: u32| (w as f64 * h as f64) / 1_000_000.0;
    println!("Self-test: vision and OCR");
    println!(
        "Virtual screen: {vw}x{vh} at ({vx},{vy}), {:.2} megapixels\n",
        mpx(vw as u32, vh as u32)
    );

    // ---- capture -----------------------------------------------------------
    println!("Screen capture");
    println!("{:<14} {:>9} {:>9} {:>8} {:>10}", "region", "Mpx", "ms", "MB", "ms/Mpx");
    let sizes: Vec<(i32, i32)> = [(320, 240), (640, 480), (1280, 720), (1920, 1080), (vw, vh)]
        .into_iter()
        .filter(|(w, h)| *w <= vw && *h <= vh)
        .collect();
    for (w, h) in &sizes {
        let (w, h) = (*w, *h);
        let ms = median_ms(7, || {
            let _ = platform::capture(vx, vy, w, h);
        });
        let m = mpx(w as u32, h as u32);
        println!(
            "{:<14} {:>9.2} {:>9.1} {:>8.1} {:>10.1}",
            format!("{w}x{h}"),
            m,
            ms,
            (w as f64 * h as f64 * 4.0) / 1_048_576.0,
            if m > 0.0 { ms / m } else { 0.0 }
        );
    }

    // ---- where a capture's time actually goes -------------------------------
    // The claim under test: the GDI object churn and the second copy were the
    // expensive parts. They were not. `BitBlt` from the desktop DC is a readback
    // out of whatever the compositor is holding the screen in, and it is the floor
    // - the DC and the copy out are rounding error beside it. Printed rather than
    // asserted, because the split is a property of the machine's graphics stack.
    println!("\nWhere a capture goes");
    println!(
        "{:<14} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "region", "GetDC", "+BitBlt", "+copy", "old DDB", "mem->mem", "blt %"
    );
    for (w, h) in &sizes {
        let (w, h) = (*w, *h);
        let dc_ms = median_ms(9, || {
            let _ = platform::probe_screen_dc();
        });
        let blt_ms = median_ms(9, || {
            let _ = platform::probe_blt(vx, vy, w, h);
        });
        let all_ms = median_ms(9, || {
            let _ = platform::capture(vx, vy, w, h);
        });
        let ddb_ms = median_ms(9, || {
            let _ = platform::probe_blt_ddb(vx, vy, w, h);
        });
        let mem_ms = median_ms(9, || {
            let _ = platform::probe_blt_mem(w, h);
        });
        println!(
            "{:<14} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>8.0}%",
            format!("{w}x{h}"),
            dc_ms,
            blt_ms,
            all_ms,
            ddb_ms,
            mem_ms,
            if all_ms > 0.0 { 100.0 * blt_ms / all_ms } else { 0.0 }
        );
    }

    // ---- the fast path against the old one ----------------------------------
    // Two questions, in this order: does it give the same pixels, and is it
    // actually faster. The first is the one that can lose a macro - a capture that
    // is quick and wrong finds buttons in the wrong place.
    //
    // "The same pixels" cannot be tested by comparing two captures for equality,
    // and the first version of this table tried. A screen with anything live on it
    // - a game, a video, a blinking caret - differs from itself between any two
    // looks, and the table dutifully reported that 97 % of the frame had changed.
    // It had. So the comparison is against the screen's own rate of change: take
    // two captures the old way, then one of each, and ask whether the two methods
    // disagree more than the old method disagrees with itself over the same
    // interval. If they do not, the difference is the screen moving, not the code.
    //
    // The swap column is the one that would catch a channel order mistake, and it
    // is read backwards on purpose: swapping red and blue has to make the match
    // *worse*. If it ever makes it better, the fast path is handing back BGRA
    // labelled RGBA and every template in the program is being matched against its
    // own mirror.
    /// Compares a fast capture against the old one, only where the screen was
    /// demonstrably holding still.
    ///
    /// Returns (still pixels, of those how many the two methods disagree about,
    /// how many they would disagree about with red and blue swapped).
    ///
    /// This is the shape the test had to take, and it took two goes to get there.
    /// Comparing two captures outright fails on any screen with something live on
    /// it: the first version reported 97 % of the frame changed, and it had - there
    /// was a game running. Sampling only the pixels two consecutive captures agreed
    /// about was better but still not enough, because a pixel in a continuously
    /// re-rendered scene lands on the same value twice often enough to matter.
    /// Three consecutive captures, all three agreeing, is the bar: a pixel that
    /// held still across two whole intervals was not being drawn, and a pixel that
    /// was not being drawn has to read the same whichever interface fetched it.
    fn agreement(
        a: &[&vision::Frame],
        b: &vision::Frame,
    ) -> (u64, u64, u64) {
        let Some(first) = a.first() else { return (0, u64::MAX, 0) };
        if a.iter().any(|f| f.px.len() != b.px.len()) || first.px.is_empty() {
            return (0, u64::MAX, 0);
        }
        let (mut still, mut bad, mut bad_swapped) = (0u64, 0u64, 0u64);
        let n = b.px.len() / 4;
        for i in 0..n {
            let o = i * 4;
            let q = &first.px[o..o + 3];
            if a.iter().skip(1).any(|f| f.px[o..o + 3] != *q) {
                continue; // that pixel was being drawn; it proves nothing
            }
            still += 1;
            let r = &b.px[o..o + 3];
            if q != r {
                bad += 1;
            }
            if !(q[0] == r[2] && q[1] == r[1] && q[2] == r[0]) {
                bad_swapped += 1;
            }
        }
        (still, bad, bad_swapped)
    }

    println!("\nDesktop Duplication against GDI");
    println!(
        "{:<12} {:>8} {:>8} {:>7} {:>9} {:>9} {:>9} {:>7}",
        "region", "GDI ms", "fast ms", "faster", "still px", "disagree", "if swapped", "quiet?"
    );
    let mut any_fast = false;
    for (w, h) in &sizes {
        let (w, h) = (*w, *h);
        platform::set_fast_capture(false);
        let gdi_ms = median_ms(9, || {
            let _ = platform::capture(vx, vy, w, h);
        });
        // Three the old way back to back, so a pixel that held still across both
        // intervals can be told from one that merely repeated a value once.
        let a1 = platform::capture(vx, vy, w, h);
        let a2 = platform::capture(vx, vy, w, h);
        let a3 = platform::capture(vx, vy, w, h);
        platform::reset_capture_counters();
        platform::set_fast_capture(true);
        let fast_ms = median_ms(9, || {
            let _ = platform::capture(vx, vy, w, h);
        });
        let b = platform::capture(vx, vy, w, h);
        let (hits, reused, misses) = platform::capture_counters();
        let used_fast = misses == 0 && (hits + reused) > 0;
        any_fast |= used_fast;

        let (still, bad, bad_swapped) = match (&a1, &a2, &a3, &b) {
            (Some(a1), Some(a2), Some(a3), Some(b)) => agreement(&[a1, a2, a3], b),
            _ => (0, u64::MAX, 0),
        };
        let total = (w as u64) * (h as u64);
        // A pixel that was not being drawn has to read the same either way. One in
        // a hundred is left for a screen that started drawing it again between the
        // third capture and the fourth; a channel order or a row pitch mistake is
        // not one in a hundred, it is most of the frame - which is what the swap
        // column is there to show by contrast.
        let enough = still >= 2_000;
        let rate = if still == 0 { 1.0 } else { bad as f64 / still as f64 };
        let swap_rate = if still == 0 { 0.0 } else { bad_swapped as f64 / still as f64 };
        let agrees = enough && bad != u64::MAX && rate <= 0.01 && bad_swapped >= bad;
        println!(
            "{:<12} {:>8.2} {:>8.2} {:>6.1}x {:>8.0}% {:>8.2}% {:>9.1}% {:>7}",
            format!("{w}x{h}"),
            gdi_ms,
            fast_ms,
            if fast_ms > 0.0 { gdi_ms / fast_ms } else { 0.0 },
            100.0 * still as f64 / total as f64,
            rate * 100.0,
            swap_rate * 100.0,
            if !used_fast {
                "n/a"
            } else if !enough {
                "busy"
            } else if agrees {
                "yes"
            } else {
                "busy"
            }
        );
    }
    if !any_fast {
        println!(
            "  Duplication is not available here, so every row above is GDI twice \n\
             \x20 and `agrees` reads n/a. That is the fallback working as intended."
        );
    }

    // ---- the check that does not care what the screen is doing --------------
    // Counting pixels is only decisive on a screen that is holding still, and the
    // machine this was written on had a game running on it, so it never was. This
    // asks the question the program actually cares about instead: cut a template
    // out of a frame the old way and look for it in a frame taken the new way.
    //
    // Every mistake the fast path could make shows up here, and each one shows up
    // differently. Red and blue swapped: the correlation collapses, because the
    // greys it is built from are computed from the wrong channels. A row pitch
    // ignored: the picture shears and the hit lands somewhere else, or nowhere.
    // Coordinates taken from the wrong monitor: the hit is in the wrong place by a
    // screen's width. A frame served stale: the score drops but the position is
    // exactly right, which is the one answer that is not a fault.
    {
        platform::set_fast_capture(false);
        let old = platform::capture(vx, vy, vw.min(1280), vh.min(720));
        platform::set_fast_capture(true);
        let new = platform::capture(vx, vy, vw.min(1280), vh.min(720));
        match (old, new) {
            (Some(old), Some(new)) => {
                // The busiest 96-pixel square in the frame: a flat one correlates
                // with everything and would prove nothing.
                let side = 96u32;
                let mut best = (0u32, 0u32, -1.0f64);
                let step = 64u32;
                let mut y = 0;
                while y + side < old.h {
                    let mut x = 0;
                    while x + side < old.w {
                        let (mut sum, mut sq) = (0.0f64, 0.0f64);
                        for r in (0..side).step_by(8) {
                            for c in (0..side).step_by(8) {
                                let i = (((y + r) * old.w + x + c) * 4) as usize;
                                let v = old.px[i + 1] as f64;
                                sum += v;
                                sq += v * v;
                            }
                        }
                        let n = ((side / 8) * (side / 8)) as f64;
                        let var = sq / n - (sum / n) * (sum / n);
                        if var > best.2 {
                            best = (x, y, var);
                        }
                        x += step;
                    }
                    y += step;
                }
                let (tx, ty, _) = best;
                let mut px = Vec::with_capacity((side * side * 4) as usize);
                for r in 0..side {
                    let o = (((ty + r) * old.w + tx) * 4) as usize;
                    px.extend_from_slice(&old.px[o..o + (side * 4) as usize]);
                }
                let tpl =
                    vision::Frame { x: 0, y: 0, w: side, h: side, px, order: old.order }
                        .as_template("cross-check");
                match vision::find(&new, &tpl, false) {
                    Some(hit) => {
                        let want_x = new.x + tx as i32 + side as i32 / 2;
                        let want_y = new.y + ty as i32 + side as i32 / 2;
                        let err = (hit.x - want_x).abs().max((hit.y - want_y).abs());
                        println!(
                            "\nA {side}x{side} template cut from a GDI frame, looked for in a \n\
                             duplicated one: found at {},{} - wanted {want_x},{want_y} - \
                             off by {err} px, score {:.3}\n  \
                             {}",
                            hit.x,
                            hit.y,
                            hit.score,
                            if err <= 1 {
                                "Same place. Channel order, row pitch and monitor origin \
                                 are all right."
                            } else {
                                "WRONG PLACE - the duplicated frame is not laid out the \
                                 way the old one is."
                            }
                        );
                    }
                    None => println!("\nThe cross-check template was not found at all."),
                }
            }
            _ => println!("\nThe cross-check needs both capture paths and did not get them."),
        }
    }

    // ---- a frame nobody changed has to come back unchanged ------------------
    // The reuse path is the one that makes a polling loop nearly free, and it is
    // also the one that could quietly serve a stale screen. Two looks at a settled
    // rectangle with no new frame between them must be identical to the byte; if
    // the compositor did send a frame, the check has nothing to say and says so.
    {
        platform::set_fast_capture(true);
        let (w, h) = (320.min(vw), 240.min(vh));
        platform::reset_capture_counters();
        let f1 = platform::capture(vx, vy, w, h);
        let f2 = platform::capture(vx, vy, w, h);
        let (hits, reused, _) = platform::capture_counters();
        let verdict = match (&f1, &f2) {
            _ if hits > 1 => "the screen moved - nothing to conclude".to_string(),
            (Some(a), Some(b)) if a.px == b.px => {
                format!("identical, {reused} of {} looks reused a frame", hits + reused)
            }
            (Some(_), Some(_)) => "DIFFERENT with no new frame - the reuse path is wrong"
                .to_string(),
            _ => "a capture failed".to_string(),
        };
        println!("\nTwo looks at an unchanged rectangle: {verdict}");
    }
    platform::reset_capture_counters();

    // ---- the same rectangle, over and over ----------------------------------
    // What a polling loop does. The cached bitmap is what this row is for: before
    // it, every one of these built and destroyed a DC and a bitmap.
    {
        let (w, h) = (400.min(vw), 300.min(vh));
        let n = 200;
        let t0 = std::time::Instant::now();
        let mut ok = 0usize;
        for _ in 0..n {
            if platform::capture(vx, vy, w, h).is_some() {
                ok += 1;
            }
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let (hits, reused, misses) = platform::capture_counters();
        println!(
            "\n{n} captures of the same {w}x{h} rectangle back to back: \
             {per:.2} ms each, {ok}/{n} succeeded"
        );
        // The interesting column is `unchanged`. A script waiting for something to
        // appear looks at a screen that is not moving, and a frame the compositor
        // never sent is a frame nobody had to read back.
        println!(
            "  new frames {hits}, unchanged {reused}, fell back to GDI {misses}"
        );
    }

    let Some(hay) = platform::capture(vx, vy, vw, vh) else {
        println!("\nCould not capture the screen - the rest of this test needs it.");
        return Ok(());
    };

    // ---- search, by template size -------------------------------------------
    // `find` has no early exit: it sweeps every candidate and returns the best one.
    // So a miss should cost what a hit costs, and if it does not, that is worth
    // knowing - it would mean a script's poll rate depends on what is on screen.
    println!("\nTemplate search, single scale, whole screen as haystack");
    println!(
        "{:<16} {:>9} {:>10} {:>9} {:>8} {:>10}",
        "template", "hit ms", "miss ms", "score", "err px", "vs 1 thread"
    );
    for size in [32u32, 64, 128, 256] {
        if size + 8 >= hay.w.min(hay.h) {
            continue;
        }
        let hit_tpl = crop_template(&hay, size, size, "hit");
        let miss_tpl = noise_template(size, "miss");
        let hit_ms = median_ms(5, || {
            let _ = vision::find(&hay, &hit_tpl, false);
        });
        let miss_ms = median_ms(5, || {
            let _ = vision::find(&hay, &miss_tpl, false);
        });
        // Speed means nothing if the answer moved: the template was cut from a known
        // spot, so the hit has to come back pointing at it.
        let found = vision::find(&hay, &hit_tpl, false);
        let want_x = hay.x + (size + size / 2) as i32;
        let want_y = hay.y + (size + size / 2) as i32;
        let (score, err) = match found {
            Some(h) => (h.score, (h.x - want_x).abs().max((h.y - want_y).abs())),
            None => (0.0, -1),
        };
        // The same search with the threads taken away. A different answer here is a
        // regression; the same answer means a template that matches in many places.
        vision::set_max_threads(1);
        let serial = vision::find(&hay, &hit_tpl, false);
        vision::set_max_threads(0);
        let agree = match (&found, &serial) {
            (Some(a), Some(b)) => {
                if a.x == b.x && a.y == b.y { "same" } else { "DIFFERENT" }
            }
            (None, None) => "same",
            _ => "DIFFERENT",
        };
        println!(
            "{:<16} {:>9.1} {:>10.1} {:>9.3} {:>8} {:>10}",
            format!("{size}x{size}"),
            hit_ms,
            miss_ms,
            score,
            err,
            agree
        );
    }

    // ---- search, by haystack size -------------------------------------------
    println!("\nSearch cost against haystack size, 64x64 template");
    println!("{:<16} {:>9} {:>9} {:>10}", "haystack", "Mpx", "ms", "ms/Mpx");
    let tpl64 = crop_template(&hay, 64, 64, "probe");
    for (w, h) in &sizes {
        let sub = crop_frame(&hay, *w as u32, *h as u32);
        if sub.w < 128 || sub.h < 128 {
            continue;
        }
        let ms = median_ms(5, || {
            let _ = vision::find(&sub, &tpl64, false);
        });
        let m = mpx(sub.w, sub.h);
        println!(
            "{:<16} {:>9.2} {:>9.1} {:>10.1}",
            format!("{}x{}", sub.w, sub.h),
            m,
            ms,
            if m > 0.0 { ms / m } else { 0.0 }
        );
    }

    let multi_ms = median_ms(3, || {
        let _ = vision::find(&hay, &tpl64, true);
    });
    println!("\nThe same 64x64 template with 'try other scales' on: {multi_ms:.1} ms");

    // ---- what a script step actually costs ----------------------------------
    let step_ms = median_ms(5, || {
        if let Some(f) = platform::capture(vx, vy, vw, vh) {
            let _ = vision::find(&f, &tpl64, false);
        }
    });
    println!(
        "\nOne script image step, capture and search over the whole screen: {step_ms:.1} ms\n\
         A `While` loop polling for that picture therefore runs at about {:.1} checks \
         per second.",
        if step_ms > 0.0 { 1000.0 / step_ms } else { 0.0 }
    );

    // ---- what the search area is worth --------------------------------------
    // The point of the release, measured rather than promised: a script step is a
    // capture plus a search, and both scale with the area it is allowed to look at.
    println!("\nOne script image step by search area, 64x64 template");
    println!(
        "{:<16} {:>9} {:>10} {:>9} {:>12}",
        "area", "capture", "search", "total ms", "checks/sec"
    );
    for (w, h) in
        [(vw, vh), (1920, 1080), (1280, 720), (800, 600), (400, 300), (200, 150)]
    {
        if w > vw || h > vh {
            continue;
        }
        let cap_ms = median_ms(5, || {
            let _ = platform::capture(vx, vy, w, h);
        });
        let sub_frame = crop_frame(&hay, w as u32, h as u32);
        let find_ms = median_ms(5, || {
            let _ = vision::find(&sub_frame, &tpl64, false);
        });
        let total = cap_ms + find_ms;
        println!(
            "{:<16} {:>9.1} {:>10.1} {:>9.1} {:>12.1}",
            format!("{w}x{h}"),
            cap_ms,
            find_ms,
            total,
            if total > 0.0 { 1000.0 / total } else { 0.0 }
        );
    }

    // ---- what the vector kernel is worth ------------------------------------
    // Stage 4 found a sevenfold win by measuring rather than guessing, so the same
    // rule applies to the kernel that replaced it: print both numbers.
    println!(
        "\nCorrelation kernel ({})",
        if vision::vectorised() { "AVX2 + FMA available" } else { "no AVX2 on this machine" }
    );
    println!("{:<16} {:>10} {:>10} {:>9} {:>10}", "template", "plain ms", "vector ms", "speed-up", "same spot");
    for size in [32u32, 64, 128] {
        if size + 8 >= hay.w.min(hay.h) {
            continue;
        }
        let tpl = crop_template(&hay, size, size, "kernel");
        vision::set_scalar_only(true);
        let plain_ms = median_ms(3, || {
            let _ = vision::find(&hay, &tpl, false);
        });
        let plain = vision::find(&hay, &tpl, false);
        vision::set_scalar_only(false);
        let vec_ms = median_ms(3, || {
            let _ = vision::find(&hay, &tpl, false);
        });
        let vectored = vision::find(&hay, &tpl, false);
        let same = match (&plain, &vectored) {
            (Some(a), Some(b)) => {
                if a.x == b.x && a.y == b.y { "same" } else { "DIFFERENT" }
            }
            (None, None) => "same",
            _ => "DIFFERENT",
        };
        println!(
            "{:<16} {:>10.1} {:>10.1} {:>9.2} {:>10}",
            format!("{size}x{size}"),
            plain_ms,
            vec_ms,
            if vec_ms > 0.0 { plain_ms / vec_ms } else { 0.0 },
            same
        );
    }

    // ---- what outline matching costs ----------------------------------------
    println!("\nGrey against outline matching, 64x64 template");
    println!("{:<16} {:>10} {:>9} {:>10}", "mode", "ms", "score", "err px");
    {
        let tpl = crop_template(&hay, 64, 64, "edge");
        let want_x = hay.x + 64 + 32;
        let want_y = hay.y + 64 + 32;
        for (name, edge) in [("grey", false), ("outline", true)] {
            let ms = median_ms(3, || {
                let _ = vision::find_mode(&hay, &tpl, false, edge);
            });
            let (score, err) = match vision::find_mode(&hay, &tpl, false, edge) {
                Some(h) => (h.score, (h.x - want_x).abs().max((h.y - want_y).abs())),
                None => (0.0, -1),
            };
            println!("{name:<16} {ms:>10.1} {score:>9.3} {err:>10}");
        }
    }

    // ---- OCR ----------------------------------------------------------------
    println!("\nText recognition");
    println!("{:<14} {:>9} {:>8}", "region", "ms", "lines");
    for (w, h) in [(200, 80), (400, 200), (800, 600)] {
        if w > vw || h > vh {
            continue;
        }
        let mut lines = 0usize;
        let mut failed: Option<String> = None;
        let ms = median_ms(3, || match ocr::read_region(vx, vy, w, h) {
            Ok(boxes) => lines = boxes.len(),
            Err(e) => failed = Some(e.to_string()),
        });
        match &failed {
            Some(e) => {
                println!("{:<14} {:>9} {:>8}   {e}", format!("{w}x{h}"), "-", "-");
                break;
            }
            None => println!("{:<14} {:>9.1} {:>8}", format!("{w}x{h}"), ms, lines),
        }
    }

    // ---- what preparing the pixels costs and buys ---------------------------
    // The claim being tested: preparation is worth more than a second engine would
    // be. A profile that costs three times as much and reads the same text is not.
    println!("\nText recognition by preparation profile, 400x200 region");
    println!("{:<16} {:>9} {:>8} {:>8} {:>10}", "profile", "ms", "lines", "chars", "fit");
    for prep in
        [ocr::Prep::None, ocr::Prep::Ui, ocr::Prep::Small, ocr::Prep::Game, ocr::Prep::Digits]
    {
        let (w, h) = (400.min(vw), 200.min(vh));
        let mut lines = 0usize;
        let mut chars = 0usize;
        let mut fit = 0.0f64;
        let mut failed: Option<String> = None;
        let ms = median_ms(3, || {
            match ocr::read_region_as(vx, vy, w, h, prep, &ocr::Expect::Any) {
                Ok(r) => {
                    lines = r.boxes.len();
                    chars = r.text().chars().count();
                    fit = r.quality;
                }
                Err(e) => failed = Some(e.to_string()),
            }
        });
        match &failed {
            Some(e) => {
                println!("{:<16}   {e}", format!("{prep:?}"));
                break;
            }
            None => println!(
                "{:<16} {:>9.1} {:>8} {:>8} {:>10.2}",
                format!("{prep:?}"),
                ms,
                lines,
                chars,
                fit
            ),
        }
    }

    // ---- what asking Windows costs ------------------------------------------
    // The first rung of the cascade, and only the first rung if it is faster than
    // the picture search. Against a game it finds nothing at all, which is the
    // honest answer rather than a fault.
    println!("\nUI Automation, against whatever window is in front");
    println!("{:<30} {:>9} {:>8}", "query", "ms", "found");
    for (label, q) in [
        (
            "any button, front window",
            uia::Query { control: "Button".into(), in_front: true, ..Default::default() },
        ),
        (
            "any button, whole desktop",
            uia::Query { control: "Button".into(), in_front: false, ..Default::default() },
        ),
        (
            "name substring, front window",
            uia::Query {
                name: "e".into(),
                control: "Button".into(),
                in_front: true,
                ..Default::default()
            },
        ),
    ] {
        let mut found = false;
        let ms = median_ms(3, || found = uia::find(&q, 0).is_some());
        println!("{:<30} {:>9.1} {:>8}", label, ms, if found { "yes" } else { "no" });
    }

    println!(
        "\nHow to read this:\n\
         - The step cost is the one that matters. It is the floor under how often a\n\
           script can look at the screen, and nothing in a `Wait for` or a `While` can\n\
           beat it.\n\
         - hit and miss columns should be near-equal. `find` has no early exit, so a\n\
           script polling for a button that is not there yet pays the same as one that\n\
           finds it immediately. Predictable, if not cheap.\n\
         - ms/Mpx says how the cost scales. Multiply it by 8.29 for a 4K screen or by\n\
           the total area of a multi-monitor desktop, which is what a script step\n\
           actually sweeps.\n\
         - A zero in the lines column means OCR ran but read nothing where it looked,\n\
           so the timing is for an empty region. Point it at some text to price a real\n\
           read.\n\
         - err px is how far the hit landed from where the template was cut out. It has\n\
           to stay at 0 or 1. A faster search that answers in the wrong place is not a\n\
           faster search - unless the last column says the single-threaded sweep gives\n\
           the same answer, in which case the template matches in several places and\n\
           the screen, not the code, decided which one won.\n\
         - The area table is what this release is for. Compare the bottom row against\n\
           the top one: that ratio is what a script gains by being told where to look.\n\
         - The kernel table prices the vector path against the plain one. `same spot`\n\
           has to say `same`: the two differ in the last bits of an f32, and if that\n\
           ever decides which position wins, the release finds pictures in different\n\
           places on different processors.\n\
         - Outline matching costs an extra pass over each plane and scores lower on\n\
           the same picture. That is expected - it is a different measurement - and it\n\
           earns its place when a template stops matching after a theme change.\n\
         - The profile table is the case for preparing the pixels. Compare chars and\n\
           fit, not only ms: a profile that costs three times as much and reads the\n\
           same text is not worth having, and one that reads twice as much for twice\n\
           the cost is.\n\
         - UI Automation belongs at the front of the cascade only if it is faster than\n\
           the picture search here. Zero found against a game is the expected answer."
    );
    Ok(())
}

/// Hammers the playback lifecycle looking for races.
///
/// Everything here is one transition away from another one: start while a previous
/// run is still winding down, pause between the schedule check and the send, stop
/// during a frame-guard hold. None of it shows up in ordinary use, because ordinary
/// use makes one transition every few minutes rather than a hundred a second.
///
/// Recording is deliberately left out. Its lifecycle installs global hooks and would
/// capture whatever the machine's owner did for the next ten minutes, and the races
/// worth hunting - generation cancellation, released presses, the pause clock - all
/// live on the playback side.
fn run_churn_selftest(seconds: u64) -> Result<()> {
    let (tx, _rx) = unbounded();
    let state = AppState::new(tx);
    // Long enough that a run is almost always in progress when the next transition
    // arrives, which is the whole point.
    *state.macro_data.lock() = synthetic_macro(4000, 5_000);
    state.loop_play.store(true, Ordering::Relaxed);
    state.absolute_mouse.store(true, Ordering::Relaxed);
    *state.speed.lock() = 1.0;

    selftest::arm_dry();
    println!("Self-test: lifecycle churn");
    println!("{seconds} s, no input is actually sent.\n");

    // If a transition wedges, nothing below will ever report it - so something has to
    // be watching from outside.
    let beat = Arc::new(AtomicU64::new(0));
    {
        let beat = beat.clone();
        std::thread::spawn(move || {
            let mut last = 0u64;
            let mut idle = 0u32;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let now = beat.load(Ordering::Relaxed);
                idle = if now == last { idle + 1 } else { 0 };
                last = now;
                if idle >= 30 {
                    println!("\nDEADLOCK: no transition completed for 30 s. Aborting.");
                    std::process::exit(2);
                }
            }
        });
    }

    let mut rng = Rng::new();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut transitions = 0u64;
    let mut stuck_presses = 0u64;
    let mut extra_releases = 0u64;
    let mut overlapped = 0u64;
    let mut blamed = [0u64; 7];
    let mut next_report = started + Duration::from_secs(30);

    while Instant::now() < deadline {
        let pick = rng.below(7) as usize;
        match pick {
            0 => start_playback(&state),
            1 => stop_playback(&state),
            2 => toggle_playback(&state),
            3 => toggle_pause(&state),
            4 => state.skip_step.store(true, Ordering::Relaxed),
            5 => *state.speed.lock() = 0.1 + (rng.below(30) as f64) / 10.0,
            _ => {
                let on = rng.below(2) == 1;
                state.frame_guard.store(on, Ordering::Relaxed);
                state.frame_guard_fps.store(5 + rng.below(236), Ordering::Relaxed);
            }
        }
        transitions += 1;
        beat.fetch_add(1, Ordering::Relaxed);

        // Two loops at once would mean a generation escaped cancellation.
        if selftest::live() > 1 {
            overlapped += 1;
        }

        // Every so often, stop properly and check that nothing was left held down.
        if rng.below(20) == 0 {
            stop_playback(&state);
            // Waiting for `playing` alone is not enough: the loop clears that flag
            // and then releases what it held, so a check racing in between would
            // report a press that was about to be let go.
            let settle = Instant::now() + Duration::from_millis(600);
            while (state.playing.load(Ordering::Relaxed) || selftest::live() > 0)
                && Instant::now() < settle
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let held = selftest::held();
            if held > 0 {
                stuck_presses += 1;
                blamed[pick] += 1;
                println!(
                    "  STUCK PRESS: {held} still down after a stop, transition {transitions}"
                );
            } else if held < 0 {
                extra_releases += 1;
                blamed[pick] += 1;
            }
        }

        std::thread::sleep(Duration::from_millis(1 + rng.below(20)));

        if Instant::now() >= next_report {
            println!(
                "  {:>4} s  {transitions} transitions, live {}, peak {}, held {}",
                started.elapsed().as_secs(),
                selftest::live(),
                selftest::peak_live(),
                selftest::held()
            );
            next_report += Duration::from_secs(30);
        }
    }

    stop_playback(&state);
    let settle = Instant::now() + Duration::from_secs(3);
    while (state.playing.load(Ordering::Relaxed) || selftest::live() > 0)
        && Instant::now() < settle
    {
        std::thread::sleep(Duration::from_millis(10));
    }

    let live = selftest::live();
    let peak = selftest::peak_live();
    println!("\n{:<36} {}", "transitions", transitions);
    println!("{:<36} {}", "playback loops still running", live);
    println!("{:<36} {}", "most loops running at once", peak);
    println!("{:<36} {}", "moments with two loops at once", overlapped);
    println!("{:<36} {}", "stops that left a press down", stuck_presses);
    println!("{:<36} {}", "stops with an unmatched release", extra_releases);
    if stuck_presses + extra_releases > 0 {
        const NAMES: [&str; 7] =
            ["start", "stop", "toggle", "pause", "skip", "speed", "guard"];
        print!("  last transition before each:");
        for (i, n) in NAMES.iter().enumerate() {
            if blamed[i] > 0 {
                print!("  {n}={}", blamed[i]);
            }
        }
        println!();
    }

    let serious = live > 0 || peak > 1 || overlapped > 0 || stuck_presses > 0;
    println!(
        "\n{}",
        if serious {
            "NOT clean. A press left down, or a generation that escaped cancellation, \
             is a real fault - the counters above say which."
        } else if extra_releases > 0 {
            "No press was ever left down and no generation escaped cancellation.\n\
             The unmatched releases come from pausing mid-press: the pause lets the \
             button go, and on resume the recording plays its own release for a button \
             that is no longer held. Harmless to Windows, but it is a real deviation \
             from the recording and the count above says how often it happens."
        } else {
            "Clean. Every stop released what it was holding, no generation escaped \
             cancellation, and nothing wedged."
        }
    );
    Ok(())
}

/// Runs for hours doing what a long unattended session does, and records what it
/// costs while doing it.
///
/// Leaks are invisible below an hour, and a soak that needs somebody sitting in front
/// of Task Manager writing numbers down is a soak that does not get run. So it samples
/// itself: private bytes, open handles and GDI objects, which between them cover the
/// three things this application allocates in bulk - replay buffers, the WinRT objects
/// behind OCR, and the bitmaps behind screen capture.
fn run_soak_selftest(hours: f64) -> Result<()> {
    let (tx, _rx) = unbounded();
    let state = AppState::new(tx);
    *state.macro_data.lock() = synthetic_macro(2000, 5_000);
    state.loop_play.store(true, Ordering::Relaxed);
    state.absolute_mouse.store(true, Ordering::Relaxed);
    state.frame_guard.store(true, Ordering::Relaxed);
    state.frame_guard_auto.store(false, Ordering::Relaxed);
    state.frame_guard_fps.store(30, Ordering::Relaxed);
    *state.speed.lock() = 1.0;

    selftest::arm_dry();
    println!("Self-test: soak");
    println!(
        "{hours:.1} h. Replay runs continuously; the screen is captured every 2 s and \n\
         read every 5 s, which is what a script polling for a picture does. No input \n\
         is sent, but the screen is being looked at, so leave something on it.\n"
    );

    // A twelve-hour run that stops doing anything after ten minutes reports nothing
    // useful unless something is watching where it stopped. Phases are numbered so a
    // second thread can name the one the loop is stuck in.
    const PHASES: [&str; 5] =
        ["waiting", "capturing the screen", "reading text", "sampling cost", "restarting"];
    let phase = Arc::new(AtomicUsize::new(0));
    let beat = Arc::new(AtomicU64::new(0));
    {
        let (phase, beat) = (phase.clone(), beat.clone());
        std::thread::spawn(move || {
            let mut last = 0u64;
            let mut idle = 0u64;
            loop {
                std::thread::sleep(Duration::from_secs(30));
                let now = beat.load(Ordering::Relaxed);
                idle = if now == last { idle + 30 } else { 0 };
                last = now;
                if idle >= 120 && idle % 120 == 0 {
                    println!(
                        "  STALLED {idle}s in: {}",
                        PHASES[phase.load(Ordering::Relaxed).min(4)]
                    );
                }
            }
        });
    }

    // The console stops accepting writes while text is selected in it, and a soak
    // whose only record is a console it cannot write to has no record at all.
    let csv = paths::log_dir().join("soak.csv");
    let append = |line: &str| {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&csv) {
            let _ = writeln!(f, "{line}");
        }
    };
    append("minute,private_mb,handles,gdi,peak_gdi,captures,reads,restarts,slept_s,worst_stall_s");
    println!("A copy of every row is appended to {}\n", csv.display());

    start_playback(&state);
    let (vx, vy, vw, vh) = platform::virtual_screen_rect();
    let tpl = platform::capture(vx, vy, vw.min(256), vh.min(256))
        .map(|f| f.as_template("soak"));

    let started = Instant::now();
    let end = started + Duration::from_secs_f64(hours * 3600.0);
    let mut next_capture = started;
    let mut next_ocr = started;
    let mut next_report = started;
    let mut captures = 0u64;
    let mut reads = 0u64;
    let mut restarts = 0u64;
    let mut first: Option<(u64, u32, u32)> = None;

    println!(
        "{:>8} {:>12} {:>10} {:>8} {:>10} {:>9} {:>9} {:>9}",
        "elapsed", "private MB", "handles", "GDI", "captures", "reads", "restarts", "slept s"
    );

    // Two clocks: `Instant` stops while the machine is asleep, wall time does not.
    // Their divergence is how a suspended laptop is told apart from a wedged loop.
    let wall0 = std::time::SystemTime::now();
    let mut worst_stall = 0u64;
    let mut worst_phase = 0usize;
    let mut peak_gdi = 0u32;
    let mut ocr_failures = 0u64;
    let mut ocr_enabled = true;

    while Instant::now() < end {
        let now = Instant::now();
        beat.fetch_add(1, Ordering::Relaxed);

        let mark = |p: usize, phase: &AtomicUsize| {
            phase.store(p, Ordering::Relaxed);
            Instant::now()
        };

        if !state.playing.load(Ordering::Relaxed) {
            let t = mark(4, &phase);
            restarts += 1;
            start_playback(&state);
            let took = t.elapsed().as_secs();
            if took > worst_stall {
                worst_stall = took;
                worst_phase = 4;
            }
        }

        if now >= next_capture {
            let t = mark(1, &phase);
            if let (Some(frame), Some(t2)) = (platform::capture(vx, vy, vw, vh), tpl.as_ref())
            {
                let _ = vision::find(&frame, t2, false);
                captures += 1;
            }
            let took = t.elapsed().as_secs();
            if took > worst_stall {
                worst_stall = took;
                worst_phase = 1;
            }
            next_capture = Instant::now() + Duration::from_secs(2);
        }

        if ocr_enabled && now >= next_ocr {
            let t = mark(2, &phase);
            match ocr::read_region(vx, vy, 400.min(vw), 200.min(vh)) {
                Ok(_) => reads += 1,
                Err(_) => ocr_failures += 1,
            }
            let took = t.elapsed().as_secs();
            if took > worst_stall {
                worst_stall = took;
                worst_phase = 2;
            }
            // One slow read is the machine being busy. A minute is the engine not
            // coming back, and there is no sense spending eleven hours finding that
            // out over and over.
            if took >= 60 {
                ocr_enabled = false;
                println!("  Text recognition took {took}s and has been switched off for the rest of the run.");
            }
            next_ocr = Instant::now() + Duration::from_secs(5);
        }

        let (_, _, gdi_now) = platform::process_cost();
        peak_gdi = peak_gdi.max(gdi_now);

        if now >= next_report {
            let t = mark(3, &phase);
            let (private, handles, gdi) = platform::process_cost();
            let mono = started.elapsed().as_secs();
            let wall = wall0.elapsed().map(|d| d.as_secs()).unwrap_or(mono);
            let slept = wall.saturating_sub(mono);
            if first.is_none() && mono >= 300 {
                // The first five minutes are warm-up: caches fill and allocators
                // settle, and counting that as growth would cry wolf every time.
                first = Some((private, handles, gdi));
            }
            let mb = private as f64 / 1_048_576.0;
            println!(
                "{:>7}m {:>12.1} {:>10} {:>8} {:>10} {:>9} {:>9} {:>9}",
                mono / 60,
                mb,
                handles,
                gdi,
                captures,
                reads,
                restarts,
                slept
            );
            append(&format!(
                "{},{:.1},{},{},{},{},{},{},{},{}",
                mono / 60,
                mb,
                handles,
                gdi,
                peak_gdi,
                captures,
                reads,
                restarts,
                slept,
                worst_stall
            ));
            let _ = t;
            // Every minute for the first ten, then every ten. Ten minutes of silence
            // at the start says nothing about whether the loop is alive, which is the
            // first thing anybody watching a soak wants to know.
            let interval = if mono < 600 { 60 } else { 600 };
            next_report = Instant::now() + Duration::from_secs(interval);
        }

        phase.store(0, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(200));
    }

    stop_playback(&state);
    std::thread::sleep(Duration::from_secs(2));
    let (private, handles, gdi) = platform::process_cost();
    let mono = started.elapsed().as_secs();
    let wall = wall0.elapsed().map(|d| d.as_secs()).unwrap_or(mono);
    let slept = wall.saturating_sub(mono);
    println!("\n{:<30} {}", "captures", captures);
    println!("{:<30} {}", "OCR reads", reads);
    println!("{:<30} {}", "OCR failures", ocr_failures);
    println!("{:<30} {}", "playback restarts", restarts);
    println!("{:<30} {}", "peak GDI objects", peak_gdi);
    println!("{:<30} {} s", "time the machine was asleep", slept);
    println!(
        "{:<30} {} s in: {}",
        "longest single stall", worst_stall, PHASES[worst_phase]
    );

    // A run that spent most of itself asleep or wedged has not soaked anything, and
    // saying so is more use than a growth figure computed from four samples.
    let expected = (mono / 2).max(1);
    if captures * 4 < expected {
        println!(
            "\nOnly {captures} captures in {} minutes - this run did far less work than \
             it should have. Look at the stall and sleep figures above before reading \
             anything into the growth numbers.",
            mono / 60
        );
    }

    match first {
        Some((p0, h0, g0)) => {
            let dp = private as f64 / 1_048_576.0 - p0 as f64 / 1_048_576.0;
            let dh = handles as i64 - h0 as i64;
            let dg = gdi as i64 - g0 as i64;
            println!("\ngrowth since the five-minute mark:");
            println!("  {:<28} {dp:+.1} MB", "private bytes");
            println!("  {:<28} {dh:+}", "handles");
            println!("  {:<28} {dg:+}", "GDI objects");
            // A few MB of allocator drift over hours is normal; a steady climb is not,
            // and handles or GDI objects should be flat to within a handful.
            let ok = dp < 32.0 && dh.abs() < 50 && dg.abs() < 50;
            println!(
                "\n{}",
                if ok {
                    "Flat. Nothing accumulated over the run."
                } else {
                    "Something is accumulating. Compare the per-sample rows above: a \
                     straight climb points at a leak, a step points at one operation."
                }
            );
        }
        None => println!("\nToo short to judge growth - run for at least fifteen minutes."),
    }
    Ok(())
}

fn run_selftest(which: &str) -> Result<()> {
    // `churn=600` runs for ten minutes; plain `churn` for five.
    // Two parses, because the soak is measured in hours and everything else in
    // whole units. `soak=0.35` used to parse as a `u64`, fail, and fall through to
    // the twelve-hour default without a word - which is a very long time to wait
    // for a twenty-minute answer, and it is exactly what happened while this
    // release was being tested.
    let (name, arg, hours) = match which.split_once('=') {
        Some((n, a)) => (n, a.parse::<u64>().ok(), a.parse::<f64>().ok()),
        None => (which, None, None),
    };
    if which.contains('=') && arg.is_none() && hours.is_none() {
        println!("'{which}': the part after '=' is not a number - ignoring it");
    }
    match name {
        "timing" => run_timing_selftest(),
        "vision" => run_vision_selftest(),
        "churn" => run_churn_selftest(arg.unwrap_or(300).clamp(5, 7200)),
        "soak" => run_soak_selftest(hours.unwrap_or(12.0).clamp(0.01, 48.0)),
        "script" => run_script_selftest(arg.unwrap_or(200).clamp(1, 100_000)),
        _ => {
            println!(
                "unknown self-test '{which}'. \
                 Available: timing, vision, churn[=seconds], soak[=hours], \
                 script[=rounds]"
            );
            Ok(())
        }
    }
}

/// Drives the script interpreter through the 1.5.0 paths, on real threads.
///
/// The unit tests check the pieces: that a policy round-trips, that `break_target`
/// counts nesting, that a drag is not a click. What they cannot check is the
/// interpreter actually running - the retry loop sleeping and being cancelled, a
/// call handing its variables down and getting them back, a recursion guard firing
/// eight frames deep, a step gate parking a run and something else releasing it.
/// Those are all about a playback thread and the flags other threads set under it,
/// and the only way to test them is to run one.
///
/// Everything runs dry: `arm_dry` silences all five `SendInput` call sites, so
/// nothing here reaches the operating system.
fn run_script_selftest(rounds: u64) -> Result<()> {
    use std::sync::atomic::AtomicU64;

    virtual_desktop::init_thread();
    selftest::arm_dry();
    println!("Self-test: the script interpreter\n");

    let dir = paths::sub_dir("selftest");
    let _ = std::fs::create_dir_all(&dir);

    let mut failures: Vec<String> = Vec::new();
    let mut checks = 0u32;
    let mut check = |name: &str, ok: bool, detail: String| {
        checks += 1;
        println!("{:<52} {:>8}  {detail}", name, if ok { "ok" } else { "FAILED" });
        if !ok {
            failures.push(name.to_string());
        }
    };

    // A script whose steps can be counted from outside. `Log` is the only step that
    // touches nothing and leaves a trace, so the counting is done by wrapping the
    // run instead: what matters is where the program counter stopped, and that is
    // visible in the variables the run leaves behind.
    let run = |script: Vec<ScriptStep>, vars: Vec<(&str, Value)>| -> (ScriptEnd, std::collections::HashMap<String, Value>, Duration) {
        let (tx, _rx) = unbounded();
        let state = AppState::new(tx);
        // `stopping()` compares the run's generation against the live one, and a
        // fresh AppState starts at zero. A harness that forgot this would see every
        // run cancelled before its first step - which is exactly what it did.
        state.play_generation.store(1, Ordering::Relaxed);
        let mut data = MacroData::new(vec![MacroEvent {
            t_us: 0,
            kind: InputEventKind::Key { vk: 0x20, scan: 0, down: true, extended: false },
        }], 1000);
        data.script = script;
        let mut ctx = ScriptCtx {
            state: &state,
            data: &data,
            generation: 1,
            map: CoordMap::IDENTITY,
            vars: vars.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            templates: Default::default(),
            last_text: String::new(),
            last_hit: Default::default(),
            latched: Default::default(),
            history: Default::default(),
            depth: 0,
            called: Default::default(),
        };
        let mut pressed = PressedInputs::default();
        let mut mover = MoveEngine::new(&state);
        let mut guard = FrameGuard::new(&state);
        let t0 = Instant::now();
        let end = run_script(&mut ctx, &mut pressed, &mut mover, &mut guard);
        (end, ctx.vars, t0.elapsed())
    };

    let bump = |name: &str| {
        ScriptStep::new(StepKind::SetVar {
            name: name.into(),
            op: VarOp::Add,
            value: Value::Num(1.0),
        })
    };
    // A picture that certainly is not on screen: no such template file exists, so
    // `find_image_into` returns false without ever looking.
    let missing = |miss: OnMiss| {
        ScriptStep::new(StepKind::ClickImage {
            template: "no_such_template_ever".into(),
            threshold: 0.99,
            button: MouseButton::Left,
            area: SearchArea::Rect { x: 0, y: 0, w: 32, h: 32 },
            edge: false,
            miss,
        })
    };

    // ---- the four policies, one at a time -----------------------------------
    println!("What a step that finds nothing does\n");
    println!("{:<52} {:>8}  {}", "check", "result", "detail");

    {
        let (end, vars, _) = run(vec![missing(OnMiss::Continue), bump("after")], vec![]);
        let after = vars.get("after").map_or(0.0, |v| v.as_num());
        check(
            "carry on: the next step still runs",
            end == ScriptEnd::Finished && after == 1.0,
            format!("{end:?}, after = {after}"),
        );
    }
    {
        let (end, vars, _) = run(vec![missing(OnMiss::Stop), bump("after")], vec![]);
        let after = vars.get("after").map_or(0.0, |v| v.as_num());
        check(
            "stop: the next step does not run",
            end == ScriptEnd::Finished && after == 0.0,
            format!("{end:?}, after = {after}"),
        );
    }
    {
        // while true { miss(Break); bump(inner) }  then bump(after)
        let script = vec![
            ScriptStep::new(StepKind::While { cond: Condition::Always }),
            missing(OnMiss::Break),
            bump("inner"),
            ScriptStep::new(StepKind::EndWhile),
            bump("after"),
        ];
        let (end, vars, took) = run(script, vec![]);
        let inner = vars.get("inner").map_or(-1.0, |v| v.as_num());
        let after = vars.get("after").map_or(0.0, |v| v.as_num());
        check(
            "leave the loop: an endless loop ends, once",
            end == ScriptEnd::Finished && inner == -1.0 && after == 1.0,
            format!("{end:?}, inner = {inner}, after = {after}, {:.0} ms", took.as_secs_f64() * 1000.0),
        );
    }
    {
        let times = 3;
        let delay = 120u64;
        let (end, vars, took) =
            run(vec![missing(OnMiss::Retry { times, delay_ms: delay }), bump("after")], vec![]);
        let after = vars.get("after").map_or(0.0, |v| v.as_num());
        let ms = took.as_secs_f64() * 1000.0;
        // Three retries at 120 ms each is 360 ms of napping, and `nap` sleeps in
        // 15 ms slices, so the floor is real and the ceiling is generous.
        let waited = ms >= (times as f64 * delay as f64 * 0.8);
        check(
            "try again: it waits, then stops",
            end == ScriptEnd::Finished && after == 0.0 && waited,
            format!("{end:?}, after = {after}, {ms:.0} ms for {times}x{delay} ms"),
        );
    }

    // ---- calling another macro ----------------------------------------------
    println!("\nCalling another macro\n");
    println!("{:<52} {:>8}  {}", "check", "result", "detail");

    // A callee that adds one to `n` and writes a string back.
    let child = {
        let mut d = MacroData::new(vec![], 0);
        d.script = vec![
            ScriptStep::new(StepKind::SetVar {
                name: "n".into(),
                op: VarOp::Add,
                value: Value::Num(1.0),
            }),
            ScriptStep::new(StepKind::SetVar {
                name: "from_child".into(),
                op: VarOp::Set,
                value: Value::Str("yes".into()),
            }),
        ];
        d
    };
    let child_path = dir.join("selftest_child.json");
    save_macro(&child_path, &child)?;

    {
        let script = vec![
            ScriptStep::new(StepKind::Call {
                path: child_path.to_string_lossy().to_string(),
                miss: OnMiss::Stop,
            }),
            bump("after"),
        ];
        let (end, vars, _) = run(script, vec![("n", Value::Num(41.0))]);
        let n = vars.get("n").map_or(0.0, |v| v.as_num());
        let back = vars.get("from_child").map(|v| v.to_string()).unwrap_or_default();
        let after = vars.get("after").map_or(0.0, |v| v.as_num());
        check(
            "variables go in and come back out",
            end == ScriptEnd::Finished && n == 42.0 && back == "yes" && after == 1.0,
            format!("n = {n}, from_child = {back:?}, after = {after}"),
        );
    }
    {
        let script = vec![
            ScriptStep::new(StepKind::Call {
                path: "no_such_macro_anywhere.json".into(),
                miss: OnMiss::Stop,
            }),
            bump("after"),
        ];
        let (end, vars, _) = run(script, vec![]);
        let after = vars.get("after").map_or(0.0, |v| v.as_num());
        check(
            "a file that will not load obeys its policy",
            end == ScriptEnd::Finished && after == 0.0,
            format!("{end:?}, after = {after}"),
        );
    }
    {
        // The one that would take the process down without a cap: a macro that
        // calls itself. Written to disk under its own name so the call really does
        // reload it rather than recursing on an in-memory copy.
        let self_path = dir.join("selftest_recursive.json");
        let mut recursive = MacroData::new(vec![], 0);
        recursive.script = vec![
            ScriptStep::new(StepKind::SetVar {
                name: "depth".into(),
                op: VarOp::Add,
                value: Value::Num(1.0),
            }),
            ScriptStep::new(StepKind::Call {
                path: self_path.to_string_lossy().to_string(),
                miss: OnMiss::Continue,
            }),
        ];
        save_macro(&self_path, &recursive)?;
        let script = vec![ScriptStep::new(StepKind::Call {
            path: self_path.to_string_lossy().to_string(),
            miss: OnMiss::Continue,
        })];
        let (end, vars, took) = run(script, vec![]);
        let depth = vars.get("depth").map_or(0.0, |v| v.as_num());
        // One `SetVar` per level, and the cap stops it entering the ninth.
        let want = MAX_CALL_DEPTH as f64;
        check(
            "a macro that calls itself stops at the cap",
            end == ScriptEnd::Finished && depth == want,
            format!("ran {depth} levels, cap is {want}, {:.0} ms", took.as_secs_f64() * 1000.0),
        );
    }

    // ---- the step gate -------------------------------------------------------
    println!("\nPausing before each step\n");
    println!("{:<52} {:>8}  {}", "check", "result", "detail");
    {
        let (tx, _rx) = unbounded();
        let state = AppState::new(tx);
        state.play_generation.store(1, Ordering::Relaxed);
        let mut data = MacroData::new(vec![], 0);
        data.script = (0..5).map(|_| bump("n")).collect();
        state.step_mode.store(true, Ordering::Relaxed);
        set_watching_vars(true);

        let st = state.clone();
        let done = Arc::new(AtomicU64::new(0));
        let d2 = done.clone();
        let handle = std::thread::Builder::new().name("gate".into()).spawn(move || {
            let mut ctx = ScriptCtx {
                state: &st,
                data: &data,
                generation: 1,
                map: CoordMap::IDENTITY,
                vars: Default::default(),
                templates: Default::default(),
                last_text: String::new(),
                last_hit: Default::default(),
                latched: Default::default(),
                history: Default::default(),
                depth: 0,
                called: Default::default(),
            };
            let mut pressed = PressedInputs::default();
            let mut mover = MoveEngine::new(&st);
            let mut guard = FrameGuard::new(&st);
            let end = run_script(&mut ctx, &mut pressed, &mut mover, &mut guard);
            d2.store(1, Ordering::Relaxed);
            (end, ctx.vars.get("n").map_or(0.0, |v| v.as_num()))
        })?;

        // It must be parked, and it must be publishing what it is parked on.
        std::thread::sleep(Duration::from_millis(250));
        let parked = script_view().is_some_and(|v| v.waiting);
        let still_running = done.load(Ordering::Relaxed) == 0;
        check(
            "the run parks before its first step",
            parked && still_running,
            format!("waiting = {parked}, finished = {}", !still_running),
        );

        // Let it through one step at a time and watch the counter follow.
        let mut seen = Vec::new();
        for _ in 0..5 {
            state.step_once.store(true, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(120));
            seen.push(script_view().map_or(0, |v| v.pc));
        }
        state.step_mode.store(false, Ordering::Relaxed);
        state.step_once.store(true, Ordering::Relaxed);
        let (end, n) = handle.join().map_err(|_| anyhow::anyhow!("the gate thread panicked"))?;
        check(
            "letting it through one step at a time runs them all",
            end == ScriptEnd::Finished && n == 5.0,
            format!("{end:?}, n = {n}, program counter went {seen:?}"),
        );
        set_watching_vars(false);
    }
    {
        // The failure mode that would be worst: a run parked in step mode that Stop
        // cannot reach. The gate has to answer to `stop_play` as well as to the
        // button, or the only way out would be to kill the process.
        let (tx, _rx) = unbounded();
        let state = AppState::new(tx);
        state.play_generation.store(1, Ordering::Relaxed);
        let mut data = MacroData::new(vec![], 0);
        data.script = (0..3).map(|_| bump("n")).collect();
        state.step_mode.store(true, Ordering::Relaxed);
        set_watching_vars(true);
        let st = state.clone();
        let handle = std::thread::Builder::new().name("gate2".into()).spawn(move || {
            let mut ctx = ScriptCtx {
                state: &st,
                data: &data,
                generation: 1,
                map: CoordMap::IDENTITY,
                vars: Default::default(),
                templates: Default::default(),
                last_text: String::new(),
                last_hit: Default::default(),
                latched: Default::default(),
                history: Default::default(),
                depth: 0,
                called: Default::default(),
            };
            let mut pressed = PressedInputs::default();
            let mut mover = MoveEngine::new(&st);
            let mut guard = FrameGuard::new(&st);
            run_script(&mut ctx, &mut pressed, &mut mover, &mut guard)
        })?;
        std::thread::sleep(Duration::from_millis(200));
        let t0 = Instant::now();
        state.stop_play.store(true, Ordering::Relaxed);
        let end = handle.join().map_err(|_| anyhow::anyhow!("the gate thread panicked"))?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        check(
            "Stop releases a run parked in step mode",
            end == ScriptEnd::Stopped && ms < 1000.0,
            format!("{end:?} after {ms:.0} ms"),
        );
        set_watching_vars(false);
    }

    // ---- the stress round ----------------------------------------------------
    // Every policy, in a loop, with calls nested under it, run until something
    // either leaks a press or stops agreeing with itself. The counters here are the
    // same ones stage 5 watches: a press left held is the failure this program can
    // least afford.
    println!("\n{rounds} rounds of every policy, with calls under them\n");
    {
        let mut ends = std::collections::BTreeMap::<String, u64>::new();
        let t0 = Instant::now();
        let mut worst_held = 0i64;
        for r in 0..rounds {
            let miss = match r % 4 {
                0 => OnMiss::Continue,
                1 => OnMiss::Stop,
                2 => OnMiss::Break,
                _ => OnMiss::Retry { times: 1, delay_ms: 0 },
            };
            let script = vec![
                ScriptStep::new(StepKind::While { cond: Condition::Always }),
                ScriptStep::new(StepKind::Call {
                    path: child_path.to_string_lossy().to_string(),
                    miss: OnMiss::Continue,
                }),
                missing(miss),
                ScriptStep::new(StepKind::Break),
                ScriptStep::new(StepKind::EndWhile),
                bump("after"),
            ];
            let (end, _, _) = run(script, vec![("n", Value::Num(0.0))]);
            *ends.entry(format!("{end:?}")).or_default() += 1;
            worst_held = worst_held.max(selftest::held().abs());
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / rounds as f64;
        println!("  ends: {ends:?}");
        check(
            "no press left held across the whole stress run",
            worst_held == 0,
            format!("worst |held| = {worst_held}, {per:.2} ms a round"),
        );
        check(
            "every round ended in a way the interpreter names",
            ends.keys().all(|k| k == "Finished"),
            format!("{} distinct endings", ends.len()),
        );
    }

    let _ = std::fs::remove_file(&child_path);
    let _ = std::fs::remove_file(dir.join("selftest_recursive.json"));
    selftest::disarm();

    println!("\n{checks} checks, {} failed", failures.len());
    if !failures.is_empty() {
        println!("  {}", failures.join("\n  "));
        anyhow::bail!("{} script self-test check(s) failed", failures.len());
    }
    println!(
        "\nHow to read this:\n\
         - The four policies are the whole of what 1.5.0 changed about a step that\n\
           finds nothing, and `carry on` has to still be indistinguishable from\n\
           1.4.0 or every existing macro has changed behaviour.\n\
         - `try again` is timed rather than counted: the retries are only useful if\n\
           they actually wait, and a retry loop that spins is worse than none.\n\
         - The recursion round is the one that would otherwise end the process. With\n\
           `panic = \"abort\"` a stack overflow is not an error anybody handles.\n\
         - `Stop releases a run parked in step mode` is the one to look at first if\n\
           step mode ever feels wrong. A run that only the Next button can free, in\n\
           a program whose whole point is a global stop key, would be a trap."
    );
    Ok(())
}

fn run_timing_selftest() -> Result<()> {
    // 3000 events 5 ms apart is 15 seconds per scenario: long enough for drift to
    // show, short enough that the whole set runs in about two minutes.
    const N: usize = 3000;
    const GAP_US: u64 = 5_000;
    let data = synthetic_macro(N, GAP_US);
    // Same order of length, but paced the way a person clicks.
    let paced = human_paced_macro(45);
    println!("Self-test: replay timing");
    println!("{N} events, {} ms apart, no input is actually sent.\n", GAP_US / 1000);

    let runs = [
        timing_scenario("baseline 1.0x", &data, 1.0, None, false, usize::MAX, 0),
        timing_scenario("slow 0.1x", &data, 0.1, None, false, usize::MAX, 0),
        timing_scenario("fast 3.0x", &data, 3.0, None, false, usize::MAX, 0),
        timing_scenario("400 ms stall", &data, 1.0, None, false, N / 2, 400_000),
        timing_scenario("guard 30 FPS", &data, 1.0, Some(30), false, usize::MAX, 0),
        timing_scenario("guard + stall", &data, 1.0, Some(30), false, N / 2, 400_000),
        timing_scenario("human movement", &data, 1.0, None, true, usize::MAX, 0),
        timing_scenario("paced, no guard", &paced, 1.0, None, false, usize::MAX, 0),
        timing_scenario("paced + guard", &paced, 1.0, Some(30), false, usize::MAX, 0),
    ];

    println!(
        "{:<16} {:>6} {:>9} {:>8} {:>9} {:>9} {:>8} {:>6} {:>8} {:>7} {:>8}",
        "scenario",
        "events",
        "mean us",
        "p50 us",
        "p99 us",
        "max us",
        "drift ms",
        "slips",
        "slip ms",
        "burst",
        "guard ms"
    );
    for r in &runs {
        println!(
            "{:<16} {:>6} {:>9.0} {:>8} {:>9} {:>9} {:>8} {:>6} {:>8} {:>7} {:>8}",
            r.label,
            r.dispatched,
            r.mean_us,
            r.p50_us,
            r.p99_us,
            r.max_us,
            r.drift_us / 1000,
            r.slips,
            r.slipped_ms,
            r.longest_burst,
            r.guard_added_ms
        );
    }

    println!("\nwall clock per scenario:");
    for r in &runs {
        println!("  {:<16} {:>6} ms", r.label, r.wall_ms);
    }

    println!(
        "\nHow to read this:\n\
         - p99 is the honest accuracy figure. Under one frame of whatever the target\n\
           renders at is fine; tens of milliseconds on an idle machine is not.\n\
         - drift is the last event's lateness. It should not grow with the run.\n\
         - burst is the longest run of dispatches under 500 us apart. On a 5 ms\n\
           recording anything above 1 means the backlog went out in a clump, which is\n\
           the failure the slip logic exists to prevent. The stall rows are the ones\n\
           that matter: they should slip once and keep burst low.\n\
         - guard ms is what the frame guard added. Compare the two `paced` rows, not\n\
           the `guard 30 FPS` one: the evenly spaced macro clicks every 15 ms, which no\n\
           person does, so the guard has to stretch every press and the cost is an\n\
           artefact of the test shape rather than a forecast for real use."
    );
    Ok(())
}

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

    if let Some(which) = args.selftest.as_deref() {
        platform::attach_parent_console();
        return run_selftest(which);
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

    paths::ensure_dirs();
    expander::load();
    expander::start_worker();

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

    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("perf".into())
            .spawn(move || perf_thread(st))?;
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

    /// A generator with a fixed seed. `Rng::new` seeds itself from the clock and the
    /// process id, which is right for jitter and wrong for a test that has to fail
    /// the same way twice.
    fn seeded(seed: u64) -> Rng {
        Rng(if seed == 0 { 1 } else { seed })
    }

    /// A macro with a bit of everything, so the operations that filter by event kind
    /// have something to filter.
    fn mixed(n: usize) -> MacroData {
        let events: Vec<MacroEvent> = (0..n)
            .map(|i| {
                let t = i as u64 * 1_000;
                match i % 3 {
                    0 => ev(t),
                    1 => btn(t, i % 2 == 0, 10, 20),
                    _ => key(t, 65, i % 2 == 0),
                }
            })
            .collect();
        let dur = events.last().map(|e| e.t_us).unwrap_or(0);
        MacroData::new(events, dur)
    }

    // ---- fuzzing -----------------------------------------------------------

    #[test]
    fn editor_operations_survive_any_range() {
        // Ranges here are deliberately wrong: reversed, past the end, both. The UI
        // clamps them, but a saved script can name events that a later edit deleted.
        // Tests build without `--release`, so overflow checks are on and any
        // arithmetic that wraps in production panics here instead.
        let mut rng = seeded(0xC0FFEE);
        for round in 0..6_000u32 {
            let mut data = mixed(rng.below(10) as usize + 1);
            let before = data.events.len();
            let from = rng.below(15) as usize;
            let to = rng.below(15) as usize;

            match round % 8 {
                0 => editor_delete_range(&mut data, from, to),
                1 => editor_crop(&mut data, from, to),
                2 => {
                    editor_replace_button(
                        &mut data,
                        from,
                        to,
                        MouseButton::Left,
                        MouseButton::Right,
                    );
                }
                3 => editor_shift_coords(&mut data, from, to, 40, -40),
                4 => editor_insert_delay(&mut data, from, rng.below(600_000)),
                5 => editor_scale(&mut data, rng.below(4000) as f64 / 100.0),
                6 => editor_drop_moves(&mut data),
                _ => editor_trim_lead(&mut data),
            }

            assert!(
                data.events.len() <= before,
                "round {round}: an edit grew the recording"
            );
            assert!(
                data.events.windows(2).all(|w| w[0].t_us <= w[1].t_us),
                "round {round}: timestamps went backwards"
            );
            assert!(
                data.duration_us >= data.last_t(),
                "round {round}: duration {} is behind the last event {}",
                data.duration_us,
                data.last_t()
            );
            assert!(data.cycle_len_us() >= 1, "round {round}: zero-length cycle");
        }
    }

    #[test]
    fn editor_operations_survive_an_empty_recording() {
        // Every one of these can be reached with nothing recorded yet.
        let run = |f: &dyn Fn(&mut MacroData)| {
            let mut d = MacroData::new(Vec::new(), 0);
            f(&mut d);
            assert!(d.events.is_empty());
        };
        run(&|d| editor_delete_range(d, 0, 9));
        run(&|d| editor_crop(d, 3, 1));
        run(&|d| editor_shift_coords(d, 0, 5, 10, 10));
        run(&|d| editor_insert_delay(d, 7, 100));
        run(&|d| editor_scale(d, 2.0));
        run(&editor_drop_moves);
        run(&editor_trim_lead);
        run(&|d| editor_delete_one(d, 4));
        run(&|d| editor_duplicate(d, 4));
        run(&|d| editor_set_time(d, 4, 500));
    }

    #[test]
    fn an_absurd_inserted_pause_saturates_instead_of_wrapping() {
        let mut data = mixed(4);
        editor_insert_delay(&mut data, 0, u64::MAX);
        assert!(data.duration_us >= data.last_t());
    }

    #[test]
    fn single_index_edits_survive_any_index() {
        let mut rng = seeded(0xBADC0DE);
        for _ in 0..2_000 {
            let mut data = mixed(rng.below(8) as usize + 1);
            let i = rng.below(12) as usize;
            match rng.below(4) {
                0 => editor_delete_one(&mut data, i),
                1 => editor_duplicate(&mut data, i),
                2 => editor_set_time(&mut data, i, rng.below(50_000)),
                _ => editor_set_event(&mut data, i, InputEventKind::Key {
                    vk: 32,
                    scan: 0,
                    down: true,
                    extended: false,
                }),
            }
            assert!(data.events.windows(2).all(|w| w[0].t_us <= w[1].t_us));
        }
    }

    // ---- settings ----------------------------------------------------------

    #[test]
    fn a_config_with_every_key_missing_still_loads() {
        // The documented promise for a config written by an older build.
        let c: AppConfig = serde_json::from_str("{}").unwrap();
        let d = AppConfig::default();
        assert_eq!(c.speed, d.speed);
        assert_eq!(c.frame_guard, d.frame_guard);
        assert!(!c.frame_guard, "the guard must stay off unless asked for");
    }

    #[test]
    fn sanitize_pulls_absurd_values_back_into_range() {
        let json = r#"{
            "speed": 1000000.0, "play_count_limit": 900000, "jitter_pct": 900000,
            "human_curve": 900000, "mouse_jitter_px": 900, "frame_guard_fps": 900000,
            "mouse_sample_ms": 900000, "schedule_h": 900, "schedule_m": 900,
            "time_limit_h": 900000, "time_limit_m": 900, "time_limit_s": 900,
            "shutdown_delay_s": 900000, "pixel_tolerance": 900000, "pixel_mode": 900,
            "repeat_delay_ms": 900000000, "img_threshold": 42.0,
            "img_rw": 900000, "img_rh": 900000, "default_theme": 900, "default_lang": 900
        }"#;
        let mut c: AppConfig = serde_json::from_str(json).unwrap();
        c.sanitize();
        assert!((0.05..=10.0).contains(&c.speed));
        assert!((1..=9999).contains(&c.play_count_limit));
        assert!(c.jitter_pct <= 50);
        assert!(c.human_curve <= 100);
        assert!((0..=60).contains(&c.mouse_jitter_px));
        assert!((5..=240).contains(&c.frame_guard_fps));
        assert!((1..=100).contains(&c.mouse_sample_ms));
        assert!(c.schedule_h <= 23 && c.schedule_m <= 59);
        assert!(c.time_limit_h <= 240 && c.time_limit_m <= 59 && c.time_limit_s <= 59);
        assert!(c.shutdown_delay_s <= 600);
        assert!(c.pixel_tolerance <= 255 && c.pixel_mode <= 1);
        assert!(c.repeat_delay_ms <= 600_000);
        assert!((0.3..=1.0).contains(&c.img_threshold));
        assert!(c.default_theme < THEME_NAMES.len());
        assert!(c.default_lang <= 6);
    }

    #[test]
    fn a_broken_float_falls_back_rather_than_poisoning_playback() {
        // JSON cannot carry NaN, but a hand-edited file or a bad merge can.
        let mut c = AppConfig::default();
        c.speed = f64::NAN;
        c.img_threshold = f64::INFINITY;
        c.sanitize();
        assert_eq!(c.speed, 1.0);
        assert_eq!(c.img_threshold, 0.85);
    }

    #[test]
    fn the_time_limit_survives_its_own_maximum() {
        let mut c = AppConfig::default();
        c.time_limit_h = 240;
        c.time_limit_m = 59;
        c.time_limit_s = 59;
        c.sanitize();
        assert_eq!(c.time_limit_us(), (240 * 3600 + 59 * 60 + 59) * 1_000_000);
    }

    // ---- macro files -------------------------------------------------------

    #[test]
    fn a_gzipped_macro_round_trips_through_the_disk() {
        let dir = std::env::temp_dir().join("mr_stage2_gzip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.mrz");
        let mut data = mixed(50);
        data.script.push(ScriptStep {
            kind: StepKind::Wait { ms: 250 },
            enabled: true,
        });
        save_macro(&path, &data).unwrap();
        let back = load_macro(&path).unwrap();
        assert_eq!(back.events.len(), data.events.len());
        assert_eq!(back.script.len(), 1);
        // Worth having a number: this is the claim the documentation makes.
        let raw = serde_json::to_vec(&data).unwrap().len();
        let packed = std::fs::metadata(&path).unwrap().len() as usize;
        assert!(packed < raw, "gzip made it bigger: {packed} vs {raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_macro_files_are_refused_not_survived() {
        assert!(parse_macro("").is_err());
        assert!(parse_macro("{").is_err());
        assert!(parse_macro("[]").is_err(), "an empty macro has nothing to play");
        assert!(parse_macro(r#"{"events":[]}"#).is_err());
        assert!(parse_macro("null").is_err());
        // A script that cannot be resolved must be refused at load, not half-run.
        let bad = r#"{"events":[{"t_us":0,"kind":{"MouseMove":{"x":1,"y":1,"dx":0,"dy":0}}}],
                      "script":[{"kind":{"If":{"cond":"Always"}},"enabled":true}]}"#;
        assert!(parse_macro(bad).is_err());
    }

    // ---- script blocks -----------------------------------------------------

    #[test]
    fn an_empty_script_resolves_to_nothing() {
        let b = resolve_blocks(&[]).unwrap();
        assert!(b.end_of.is_empty() && b.else_of.is_empty() && b.start_of.is_empty());
    }

    #[test]
    fn a_second_else_is_refused() {
        let steps = vec![
            ScriptStep { kind: StepKind::If { cond: Condition::Always }, enabled: true },
            ScriptStep { kind: StepKind::Else, enabled: true },
            ScriptStep { kind: StepKind::Else, enabled: true },
            ScriptStep { kind: StepKind::EndIf, enabled: true },
        ];
        assert!(resolve_blocks(&steps).is_err());
    }

    #[test]
    fn deep_nesting_resolves_without_running_out_of_stack() {
        // Resolution is iterative; this fails loudly if that ever stops being true.
        const DEPTH: usize = 500;
        let mut steps = Vec::new();
        for _ in 0..DEPTH {
            steps.push(ScriptStep {
                kind: StepKind::While { cond: Condition::Always },
                enabled: true,
            });
        }
        for _ in 0..DEPTH {
            steps.push(ScriptStep { kind: StepKind::EndWhile, enabled: true });
        }
        let b = resolve_blocks(&steps).unwrap();
        assert_eq!(b.end_of[0], Some(DEPTH * 2 - 1));
        assert_eq!(b.end_of[DEPTH - 1], Some(DEPTH));
    }

    // ---- frame guard on automatic ------------------------------------------

    #[test]
    fn the_guard_follows_the_measurement_when_on_automatic() {
        let (tx, _rx) = unbounded();
        let state = AppState::new(tx);
        state.frame_guard.store(true, Ordering::Relaxed);
        state.frame_guard_auto.store(true, Ordering::Relaxed);
        state.frame_guard_fps.store(60, Ordering::Relaxed);

        // Nothing measured yet, so the configured 60 FPS stands in: two 16.6 ms frames.
        let mut g = FrameGuard::new(&state);
        assert!((32_000..=34_000).contains(&g.hold_us), "hold was {}", g.hold_us);

        // The window turns out to manage about 15 FPS.
        state.perf_frame_us.store(66_000, Ordering::Relaxed);
        g.retune(&state);
        assert!(g.hold_us > 120_000, "hold was {}", g.hold_us);

        // A 3 % wobble is ignored: the guard must not become a source of jitter.
        let steady = g.hold_us;
        state.perf_frame_us.store(68_000, Ordering::Relaxed);
        g.retune(&state);
        assert_eq!(g.hold_us, steady);

        // A real recovery is followed.
        state.perf_frame_us.store(8_000, Ordering::Relaxed);
        g.retune(&state);
        assert!(g.hold_us < 20_000, "hold was {}", g.hold_us);
    }

    #[test]
    fn a_measurement_is_ignored_when_automatic_is_off() {
        let (tx, _rx) = unbounded();
        let state = AppState::new(tx);
        state.frame_guard.store(true, Ordering::Relaxed);
        state.frame_guard_auto.store(false, Ordering::Relaxed);
        state.frame_guard_fps.store(60, Ordering::Relaxed);
        state.perf_frame_us.store(200_000, Ordering::Relaxed);
        let mut g = FrameGuard::new(&state);
        let before = g.hold_us;
        g.retune(&state);
        assert_eq!(g.hold_us, before);
        assert!(before < 40_000);
    }

    #[test]
    fn a_disabled_guard_ignores_the_measurement_too() {
        let (tx, _rx) = unbounded();
        let state = AppState::new(tx);
        state.frame_guard.store(false, Ordering::Relaxed);
        state.frame_guard_auto.store(true, Ordering::Relaxed);
        state.perf_frame_us.store(200_000, Ordering::Relaxed);
        let mut g = FrameGuard::new(&state);
        g.retune(&state);
        let up = InputEventKind::MouseButton {
            button: MouseButton::Left,
            down: false,
            x: 0,
            y: 0,
        };
        assert_eq!(g.extra_wait(&up, 0), 0);
    }

    // ---- responsiveness maths ----------------------------------------------

    fn book_with(entries: Vec<expander::Entry>) -> expander::Book {
        expander::Book { enabled: true, entries, ..Default::default() }
    }

    fn entry(abbr: &str, text: &str, t: expander::Trigger) -> expander::Entry {
        expander::Entry {
            enabled: true,
            abbr: abbr.into(),
            text: text.into(),
            trigger: t,
            insert: expander::Insert::Type,
            action: expander::Action::Text,
        }
    }

    fn typed_busy(
        book: &expander::Book,
        s: &str,
        allow_text: bool,
    ) -> Option<expander::Fire> {
        let mut buf: Vec<char> = Vec::new();
        let mut last = None;
        for c in s.chars() {
            buf.push(c);
            last = expander::match_at(book, &buf, c, allow_text);
            if last.is_some() {
                break;
            }
        }
        last
    }

    fn typed(book: &expander::Book, s: &str) -> Option<expander::Fire> {
        let mut buf: Vec<char> = Vec::new();
        let mut last = None;
        for c in s.chars() {
            buf.push(c);
            last = expander::match_at(book, &buf, c, true);
            if last.is_some() {
                break;
            }
        }
        last
    }

    #[test]
    fn delimiter_mode_waits_for_the_space() {
        let b = book_with(vec![entry("addr", "Baker Street", expander::Trigger::Inherit)]);
        assert!(typed(&b, "addr").is_none(), "fired before the delimiter");
        let f = typed(&b, "addr ").expect("did not fire on the space");
        // Four characters and the space, and the space comes back after the text.
        assert_eq!(f.backspaces, 5);
        assert_eq!(f.segments, vec![expander::Segment::Text("Baker Street ".into())]);
    }

    #[test]
    fn an_abbreviation_inside_a_word_does_not_fire() {
        let b = book_with(vec![entry("addr", "Baker Street", expander::Trigger::Inherit)]);
        assert!(typed(&b, "readdr ").is_none());
        assert!(typed(&b, "my addr ").is_some());
    }

    #[test]
    fn instant_and_prefix_modes_fire_without_a_delimiter() {
        let b = book_with(vec![
            entry(";sig", "Kind regards", expander::Trigger::Instant),
            entry("me", "Sherlock", expander::Trigger::Prefix(";;".into())),
        ]);
        assert_eq!(typed(&b, ";sig").expect("instant").backspaces, 4);
        assert_eq!(typed(&b, ";;me").expect("prefix").backspaces, 4);
        // Without its prefix the short abbreviation stays inert, which is the whole
        // reason the mode exists.
        assert!(typed(&b, "me ").is_none());
    }

    #[test]
    fn the_longest_abbreviation_wins_when_two_match_at_once() {
        // `;` is a delimiter, so the moment `;sig` is typed both entries match: the
        // short one behind a word boundary, the long one from the start of the buffer.
        // That is the collision the rule is for.
        let b = book_with(vec![
            entry("sig", "short", expander::Trigger::Instant),
            entry(";sig", "long", expander::Trigger::Instant),
        ]);
        let f = typed(&b, ";sig").expect("fired");
        assert_eq!(f.segments, vec![expander::Segment::Text("long".into())]);
        assert_eq!(f.backspaces, 4);
    }

    #[test]
    fn instant_mode_fires_before_a_longer_abbreviation_can_be_finished() {
        // Not a defect but a property worth pinning down: instant expansion cannot
        // wait to find out whether more is coming, so `;sig` goes off halfway through
        // `;signature`.
        let b = book_with(vec![
            entry(";sig", "short", expander::Trigger::Instant),
            entry(";signature", "long", expander::Trigger::Instant),
        ]);
        assert_eq!(
            typed(&b, ";signature").expect("fired").segments,
            vec![expander::Segment::Text("short".into())]
        );

        // Delimiter mode has no such problem, because nothing is decided until the
        // word has ended. Which is why it is the default.
        let d = book_with(vec![
            entry("sig", "short", expander::Trigger::Inherit),
            entry("signature", "long", expander::Trigger::Inherit),
        ]);
        assert_eq!(
            typed(&d, "signature ").expect("fired").segments,
            vec![expander::Segment::Text("long ".into())]
        );
    }

    #[test]
    fn a_disabled_entry_and_a_disabled_book_stay_quiet() {
        let mut b = book_with(vec![entry("addr", "x", expander::Trigger::Inherit)]);
        b.entries[0].enabled = false;
        assert!(typed(&b, "addr ").is_none());
        b.entries[0].enabled = true;
        b.enabled = false;
        assert!(typed(&b, "addr ").is_none());
    }

    /// Writes a solid-colour PNG, which is all these tests need to exist.
    fn write_png(path: &std::path::Path, w: u32, h: u32, shade: u8) {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([shade, shade, shade, 255]));
        img.save(path).unwrap();
    }

    #[test]
    fn a_folder_of_variants_loads_as_a_set() {
        let dir = std::env::temp_dir().join("mr_tpl_variants");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Claim")).unwrap();
        write_png(&dir.join("Claim/normal.png"), 20, 10, 200);
        write_png(&dir.join("Claim/hover.png"), 20, 10, 150);
        write_png(&dir.join("Claim/dark.png"), 20, 10, 40);
        // Something that is not a picture must not become one.
        std::fs::write(dir.join("Claim/notes.txt"), "ignore me").unwrap();

        let set = load_template_set_at(&dir.join("Claim"), "Claim");
        assert_eq!(set.len(), 3, "three PNGs, and only the PNGs");
        // Alphabetical, so a tie between variants does not depend on the file system.
        assert!(set.iter().all(|t| t.w == 20 && t.h == 10));

        // A single file still works, which is what every existing macro uses.
        write_png(&dir.join("Solo.png"), 8, 8, 90);
        assert_eq!(load_template_set_at(&dir.join("Solo"), "Solo").len(), 1);

        // And a name that is neither is empty rather than a panic.
        assert!(load_template_set_at(&dir.join("Nothing"), "Nothing").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_template_is_rescaled_from_the_dpi_it_was_cut_at() {
        let dir = std::env::temp_dir().join("mr_tpl_dpi");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("Btn.png");
        write_png(&png, 90, 30, 128);

        // No sidecar means nothing is known, and nothing known means nothing done.
        // This is the assertion that stops an upgrade from breaking every template
        // that was cut before this release existed.
        let plain = load_template_set_at(&dir.join("Btn"), "Btn");
        assert_eq!(plain.len(), 1);
        assert_eq!((plain[0].w, plain[0].h), (90, 30));

        // Half the dpi it will be looked for on, so it has to come back twice the size.
        save_template_meta(&png, &TemplateMeta { dpi: platform::current_dpi() / 2 });
        let scaled = load_template_set_at(&dir.join("Btn"), "Btn");
        assert_eq!((scaled[0].w, scaled[0].h), (180, 60));

        // The sidecar sits beside the picture, not instead of it.
        assert!(dir.join("Btn.png.json").exists());
        assert!(png.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_thresholds_stop_a_wobbling_score_from_flapping() {
        // The sequence from the report that asked for this: around a single threshold
        // of 0.80 it reads as four state changes.
        let scores = [0.79, 0.81, 0.79, 0.82, 0.78];
        let mut one = false;
        let mut flips_one = 0;
        for sc in scores {
            let now = match_decision(sc, 0.80, 0.0, one);
            if now != one {
                flips_one += 1;
            }
            one = now;
        }
        assert_eq!(flips_one, 4, "a single threshold should flap here");

        // With a lower threshold to lose it, the same sequence settles after one.
        let mut two = false;
        let mut flips_two = 0;
        for sc in scores {
            let now = match_decision(sc, 0.80, 0.70, two);
            if now != two {
                flips_two += 1;
            }
            two = now;
        }
        assert_eq!(flips_two, 1);
        assert!(two, "it appeared at 0.81 and never dropped under 0.70");

        // And it does come back when the picture really goes.
        assert!(!match_decision(0.69, 0.80, 0.70, true));
    }

    #[test]
    fn a_lower_threshold_that_is_not_lower_changes_nothing() {
        // Zero means unset; a value at or above the appear threshold is a mistake, and
        // honouring it would make a found picture impossible to lose.
        for bad in [0.0, 0.80, 0.95] {
            assert!(!match_decision(0.79, 0.80, bad, true), "lose_at {bad} was honoured");
        }
    }

    #[test]
    fn stability_tells_an_object_from_a_flicker() {
        // 82, 84, 83: an object. 83, 51, 74: noise that was briefly plausible.
        let mut steady = 0u32;
        let mut answers = Vec::new();
        for sc in [0.82, 0.84, 0.83] {
            answers.push(stable_enough(&mut steady, sc >= 0.80, 2, 3));
        }
        assert_eq!(answers, vec![false, true, true], "two of three should settle true");

        let mut noisy = 0u32;
        let mut answers = Vec::new();
        for sc in [0.83, 0.51, 0.74] {
            answers.push(stable_enough(&mut noisy, sc >= 0.80, 2, 3));
        }
        assert!(answers.iter().all(|a| !a), "one good frame in three is not an object");
    }

    #[test]
    fn asking_for_no_stability_answers_frame_by_frame() {
        let mut h = 0u32;
        assert!(stable_enough(&mut h, true, 0, 0));
        assert!(!stable_enough(&mut h, false, 1, 1));
        assert!(stable_enough(&mut h, true, 1, 1));
        // And the window cannot be pushed past the width of the mask it lives in.
        let mut wide = 0u32;
        for _ in 0..40 {
            stable_enough(&mut wide, true, 40, 40);
        }
        assert!(stable_enough(&mut wide, true, 40, 40));
    }

    #[test]
    fn a_search_area_stays_inside_the_desktop() {
        // A window can sit half off-screen and a hand-typed rectangle can be nonsense;
        // the capture has to be a real rectangle either way.
        let (fx, fy, fw, fh) = (0, 0, 1920, 1080);
        let clamp = |x: i32, y: i32, w: i32, h: i32| {
            let x = x.clamp(fx, fx + fw - 1);
            let y = y.clamp(fy, fy + fh - 1);
            (x, y, w.clamp(1, fx + fw - x), h.clamp(1, fy + fh - y))
        };
        assert_eq!(clamp(-500, -500, 800, 600), (0, 0, 800, 600));
        assert_eq!(clamp(1800, 1000, 800, 600), (1800, 1000, 120, 80));
        assert_eq!(clamp(0, 0, -5, -5), (0, 0, 1, 1));
    }

    #[test]
    fn a_step_kind_survives_the_round_trip_including_the_new_one() {
        // COUNT and the two index tables have to agree, and adding a kind is exactly
        // where they stop agreeing.
        for i in 0..StepKind::COUNT {
            assert_eq!(StepKind::from_index(i).index(), i, "kind {i} does not round-trip");
        }
        assert!(matches!(
            StepKind::from_index(17),
            StepKind::FindImage { .. }
        ));
    }

    #[test]
    fn an_image_step_keeps_its_area_through_a_save() {
        let mut data = MacroData::new(vec![ev(0)], 1000);
        data.script.push(ScriptStep {
            kind: StepKind::FindImage {
                template: "claim".into(),
                threshold: 0.9,
                area: SearchArea::Rect { x: 10, y: 20, w: 300, h: 200 },
                var: "target".into(),
                edge: false,
                miss: OnMiss::Continue,
            },
            enabled: true,
        });
        let back: MacroData =
            serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        match &back.script[0].kind {
            StepKind::FindImage { area, var, .. } => {
                assert_eq!(var, "target");
                assert_eq!(*area, SearchArea::Rect { x: 10, y: 20, w: 300, h: 200 });
            }
            other => panic!("wrong kind back: {other:?}"),
        }
    }

    #[test]
    fn an_image_step_written_before_areas_existed_still_loads() {
        // Every macro saved up to 1.3.5 has no `area` field at all.
        let json = r#"{"events":[{"t_us":0,"kind":{"MouseMove":{"x":1,"y":1,"dx":0,"dy":0}}}],
            "script":[{"kind":{"ClickImage":{"template":"claim","threshold":0.85,
            "button":"Left"}},"enabled":true}]}"#;
        let data = parse_macro(json).expect("an older macro must still load");
        match &data.script[0].kind {
            StepKind::ClickImage { area, .. } => assert_eq!(*area, SearchArea::FullScreen),
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn a_command_entry_carries_its_action_instead_of_text() {
        let mut e = entry(";farm", "C:/macros/farm.mrz", expander::Trigger::Instant);
        e.action = expander::Action::PlayMacro;
        let b = book_with(vec![e]);
        let f = typed(&b, ";farm").expect("fired");
        assert_eq!(f.action, expander::Action::PlayMacro);
        assert_eq!(f.payload, "C:/macros/farm.mrz");
        // The path is a path, not something to type: no placeholders, no segments.
        assert!(f.segments.is_empty());
        assert_eq!(f.backspaces, 5);
    }

    #[test]
    fn replaying_silences_text_entries_but_not_commands() {
        let mut cmd = entry(";stop", "", expander::Trigger::Instant);
        cmd.action = expander::Action::StopAll;
        let b = book_with(vec![entry(";sig", "text", expander::Trigger::Instant), cmd]);

        // Idle: both work.
        assert!(typed_busy(&b, ";sig", true).is_some());
        assert!(typed_busy(&b, ";stop", true).is_some());

        // Replaying: typing into a running macro would fight with it, but a command
        // that stops it is the whole point of being able to reach for one.
        assert!(typed_busy(&b, ";sig", false).is_none());
        assert_eq!(
            typed_busy(&b, ";stop", false).expect("a command must still fire").action,
            expander::Action::StopAll
        );
    }

    #[test]
    fn placeholders_become_segments() {
        use expander::Segment::*;
        assert_eq!(
            expander::render("a{cursor}b"),
            vec![Text("a".into()), Cursor, Text("b".into())]
        );
        assert_eq!(
            expander::render("name{key:Tab}mail"),
            vec![Text("name".into()), Key(0x09), Text("mail".into())]
        );
        // A backslash is how a replacement contains a literal placeholder.
        assert_eq!(expander::render("\\{date}"), vec![Text("{date}".into())]);
        // An unknown token is left as written rather than swallowed.
        assert_eq!(expander::render("{nope}"), vec![Text("{nope}".into())]);
        // So is an unclosed brace, which is a typo and not a token.
        assert_eq!(expander::render("{oops"), vec![Text("{oops".into())]);
        assert!(expander::render("{random:a|a|a}") == vec![Text("a".into())]);
    }

    #[test]
    fn date_patterns_are_substituted() {
        let out = expander::stamp("yyyy-MM-dd HH:mm:ss");
        assert_eq!(out.len(), 19, "unexpected shape: {out}");
        assert!(out.chars().all(|c| c.is_ascii_digit() || "-: ".contains(c)));
        assert_eq!(expander::key_by_name("enter"), Some(0x0D));
        assert_eq!(expander::key_by_name("nonsense"), None);
    }

    #[test]
    fn setting_a_time_on_a_stale_selection_keeps_the_process_alive() {
        // Found by the fuzz above; kept by name because the fuzz reports a round
        // number and this reports the scenario: a selection outliving its recording.
        let mut data = mixed(3);
        editor_set_time(&mut data, 9, 1_000);
        assert_eq!(data.events.len(), 3);
        let mut empty = MacroData::new(Vec::new(), 0);
        editor_set_time(&mut empty, 4, 1_000);
        assert!(empty.events.is_empty());
    }

    #[test]
    fn one_percent_low_means_the_worst_one_percent() {
        // Ninety-nine good frames and one bad one. What a reader wants from a
        // "1 % low" is the bad one; a 99th percentile would hand back a good one.
        let mut v = vec![10_000u64; 99];
        v.push(200_000);
        assert_eq!(perf::summarize(&v).p99_us, 200_000);

        // Four bad frames in four hundred: the worst 1 % is all four, averaged, so no
        // single sample decides the figure the guard is sized from.
        let mut w = vec![10_000u64; 396];
        w.extend([100_000, 120_000, 140_000, 160_000]);
        assert_eq!(perf::summarize(&w).p99_us, 130_000);
    }

    #[test]
    fn summarize_handles_the_small_cases() {
        let one = perf::summarize(&[5_000]);
        assert_eq!(one.samples, 1);
        assert_eq!(one.avg_us, 5_000);
        assert_eq!(one.p99_us, 5_000);
        assert_eq!(one.stutters, 0);

        // A perfectly steady window has no hitches, however tight the numbers.
        assert_eq!(perf::summarize(&[200; 400]).stutters, 0);

        // Ordering must not matter: the samples arrive in whatever order they arrive.
        let mut v: Vec<u64> = (1..=100).map(|i| i * 1_000).collect();
        let ascending = perf::summarize(&v);
        v.reverse();
        let descending = perf::summarize(&v);
        assert_eq!(ascending.p99_us, descending.p99_us);
        assert_eq!(ascending.avg_us, descending.avg_us);
        assert_eq!(ascending.worst_us, 100_000);
    }

    #[test]
    fn text_matching_stays_forgiving_without_becoming_useless() {
        assert!(ocr::text_matches("YOU  WIN !", "you win"));
        assert!(!ocr::text_matches("you lose", "you win"));
        assert!(!ocr::text_matches("", "claim"));
        assert!(ocr::first_number("no digits here").is_none());
    }

    #[test]
    fn a_saved_macro_comes_back_unchanged() {
        let data = MacroData::new(vec![ev(0), ev(1000)], 5000);
        let back: MacroData = serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        assert_eq!(back.events.len(), 2);
        assert_eq!(back.duration_us, 5000);
        assert_eq!(back.version, format_version());
    }

    #[test]
    fn a_file_with_no_version_field_is_read_as_the_current_one() {
        // What every macro saved before the field existed looks like.
        let data = parse_macro(r#"{"events":[{"t_us":0,"kind":{"MouseMove":{"x":1,"y":2,"dx":0,"dy":0}}}]}"#)
            .unwrap();
        assert_eq!(data.version, format_version());
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
        vision::Frame::rgba(0, 0, w, h, rgba)
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
        let flat = vision::Frame::rgba(0, 0, 200, 200, vec![90u8; 200 * 200 * 4]);
        let hit = vision::find(&flat, &tpl, false);
        // A featureless screen cannot correlate with a patterned template.
        assert!(hit.map(|h| h.score < 0.5).unwrap_or(true));
    }

    #[test]
    fn template_larger_than_the_screen_is_not_a_match() {
        let tpl = checker_template(64, 64);
        let small = vision::Frame::rgba(0, 0, 32, 32, vec![0u8; 32 * 32 * 4]);
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
    fn frame_guard_stretches_a_press_that_is_too_short() {
        // 30 FPS -> a 33 ms frame, so a press must survive about 66 ms.
        let mut g = FrameGuard::for_fps(true, 30);
        let down =
            InputEventKind::MouseButton { button: MouseButton::Left, down: true, x: 0, y: 0 };
        let up =
            InputEventKind::MouseButton { button: MouseButton::Left, down: false, x: 0, y: 0 };
        g.note_sent(&down, 1_000_000);
        // Released after 5 ms: the guard has to hold it.
        assert!(g.extra_wait(&up, 1_005_000) >= 50_000);
        // Released after 100 ms: already long enough, nothing added.
        assert_eq!(g.extra_wait(&up, 1_100_000), 0);
    }

    #[test]
    fn frame_guard_separates_a_click_from_the_move_before_it() {
        let mut g = FrameGuard::for_fps(true, 30);
        let mv = InputEventKind::MouseMove { x: 10, y: 10, dx: 0, dy: 0 };
        let down =
            InputEventKind::MouseButton { button: MouseButton::Left, down: true, x: 0, y: 0 };
        g.note_sent(&mv, 2_000_000);
        assert!(g.extra_wait(&down, 2_000_000) >= 30_000);
        assert_eq!(g.extra_wait(&down, 2_100_000), 0);
    }

    /// egui carries its own emoji font, and that font stops short of Emoji 12.
    /// Anything newer draws as an empty box, which looks like a bug in the app
    /// rather than a gap in a font. Catching it here beats finding it by staring
    /// at the window.
    #[test]
    fn ui_strings_avoid_glyphs_the_bundled_font_lacks() {
        for (lang, table) in
            [("EN", EN), ("RU", RU), ("UK", UK), ("PT", PT), ("ES", ES), ("ZH", ZH)]
        {
            for (key, value) in table.to_map() {
                for ch in value.chars() {
                    let cp = ch as u32;
                    // Symbols and Pictographs Extended-A: everything in here arrived
                    // with Unicode 12 or later.
                    assert!(
                        !(0x1FA70..=0x1FAFF).contains(&cp),
                        "{lang}.{key} uses {ch:?} (U+{cp:04X}), which the bundled \
                         emoji font has no glyph for",
                    );
                }
            }
        }
    }

    #[test]
    fn percentiles_pick_the_worst_samples() {
        // 100 samples: ninety-nine at 10 ms and one 200 ms hitch.
        let mut v = vec![10_000u64; 99];
        v.push(200_000);
        let st = perf::summarize(&v);
        assert_eq!(st.samples, 100);
        assert_eq!(st.worst_us, 200_000);
        // The worst 1 % is the hitch, and it counts as a stutter.
        assert_eq!(st.p99_us, 200_000);
        assert_eq!(st.stutters, 1);
        // A steady window reports no hitches at all.
        assert_eq!(perf::summarize(&[10_000; 50]).stutters, 0);
        assert_eq!(perf::summarize(&[]).samples, 0);
    }

    #[test]
    fn frame_guard_switched_off_never_waits() {
        let mut g = FrameGuard::for_fps(false, 30);
        let down =
            InputEventKind::MouseButton { button: MouseButton::Left, down: true, x: 0, y: 0 };
        let up =
            InputEventKind::MouseButton { button: MouseButton::Left, down: false, x: 0, y: 0 };
        g.note_sent(&down, 0);
        assert_eq!(g.extra_wait(&up, 1), 0);
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
                value: Value::Num(3.0),
            } }),
            step(StepKind::PlayEvents { from: 0, to: 1 }),
            step(StepKind::SetVar {
                name: "n".into(),
                op: VarOp::Add,
                value: Value::Num(1.0),
            }),
            step(StepKind::EndWhile),
        ];
        d.vars.insert("n".into(), Value::Num(0.0));
        let text = serde_json::to_string(&d).unwrap();
        let back = parse_macro(&text).unwrap();
        assert_eq!(back.script.len(), 4);
        assert!(back.has_script());
        assert_eq!(back.vars.get("n"), Some(&Value::Num(0.0)));
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

    // ---- image search --------------------------------------------------------

    /// The same picture, made lighter or darker the way a theme change does.
    fn reshade(f: &vision::Frame, add: i16, gain: f32) -> vision::Frame {
        let mut px = f.px.clone();
        for p in px.chunks_exact_mut(4) {
            for c in p.iter_mut().take(3) {
                let v = (*c as f32 * gain) as i16 + add;
                *c = v.clamp(0, 255) as u8;
            }
        }
        vision::Frame { x: f.x, y: f.y, w: f.w, h: f.h, px, order: f.order }
    }

    #[test]
    fn the_vector_kernel_agrees_with_the_plain_one() {
        // The two have to give the same answer to within rounding, or the release
        // finds pictures in different places depending on the processor.
        let tpl = checker_template(32, 24);
        let hay = haystack(320, 240, &tpl, 100, 60);
        let hit = vision::find(&hay, &tpl, false).expect("should find it");
        assert!(hit.score > 0.95, "score was {}", hit.score);
        assert_eq!((hit.x, hit.y), (100 + 16, 60 + 12));

        // And the one-pass form must still refuse a template with no contrast: it
        // would otherwise correlate with everything equally well.
        let flat = vision::Template {
            w: 16,
            h: 16,
            rgba: vec![128u8; 16 * 16 * 4],
            name: "flat".into(),
        };
        let score = vision::find(&hay, &flat, false).map(|h| h.score).unwrap_or(-1.0);
        assert!(score <= 0.0, "a blank template scored {score}");
    }

    #[test]
    fn correlation_still_ignores_brightness_and_contrast() {
        // What normalised correlation is for, and what the rewritten inner loop
        // must not have lost.
        let tpl = checker_template(32, 24);
        let hay = haystack(320, 240, &tpl, 100, 60);
        let dimmed = reshade(&hay, -40, 0.75);
        let hit = vision::find(&dimmed, &tpl, false).expect("should still find it");
        assert!(hit.score > 0.95, "score fell to {} on a dimmed screen", hit.score);
        assert_eq!((hit.x, hit.y), (100 + 16, 60 + 12));
    }

    #[test]
    fn the_outline_mode_finds_the_same_place() {
        // Edge matching is a different measurement, so the score is its own; what
        // has to survive is where it says the picture is.
        let tpl = checker_template(32, 24);
        let hay = haystack(320, 240, &tpl, 100, 60);
        let hit = vision::find_mode(&hay, &tpl, false, true).expect("should find it");
        assert_eq!((hit.x, hit.y), (100 + 16, 60 + 12));
        // Never quite 1.0, and it should not be: the template's outermost ring has
        // no neighbours outside itself, so its gradients are computed against a
        // replicated border while the screen's are computed against whatever is
        // really there. The interior is what carries the match.
        assert!(hit.score > 0.85, "outline score was {}", hit.score);
    }

    #[test]
    fn a_template_larger_than_the_area_is_still_not_a_match() {
        // The prepared form has more ways to be handed nonsense than the old one.
        let big = checker_template(64, 64);
        let small = vision::Frame::rgba(0, 0, 16, 16, vec![0u8; 16 * 16 * 4]);
        assert!(vision::find(&small, &big, false).is_none());
        assert!(vision::find_mode(&small, &big, false, true).is_none());
    }

    #[test]
    fn an_anchored_area_survives_a_save_and_load() {
        let mut data = MacroData::new(vec![ev(0)], 1000);
        data.script.push(ScriptStep {
            kind: StepKind::ClickImage {
                template: "join".into(),
                threshold: 0.85,
                button: MouseButton::Left,
                area: SearchArea::NearAnchor {
                    anchor: "heading".into(),
                    dx: -150,
                    dy: 40,
                    w: 300,
                    h: 120,
                },
                edge: true,
                miss: OnMiss::Continue,
            },
            enabled: true,
        });
        let back: MacroData =
            serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        match &back.script[0].kind {
            StepKind::ClickImage { area, edge, .. } => {
                assert!(*edge);
                assert_eq!(
                    *area,
                    SearchArea::NearAnchor {
                        anchor: "heading".into(),
                        dx: -150,
                        dy: 40,
                        w: 300,
                        h: 120,
                    }
                );
            }
            other => panic!("wrong kind back: {other:?}"),
        }
    }

    // ---- OCR preparation ---------------------------------------------------

    /// A strip of `shade` pixels with `text` pixels written into the middle third.
    fn strip(bg: u8, fg: u8, n: usize) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(n * 4);
        for i in 0..n {
            let v = if i % 5 == 0 { fg } else { bg };
            rgba.extend_from_slice(&[v, v, v, 255]);
        }
        rgba
    }

    #[test]
    fn otsu_puts_the_cut_between_the_two_populations() {
        // Forty dark pixels and sixty light ones: the threshold has to land in the
        // gap, not on either cluster.
        let mut g = vec![30u8; 40];
        g.extend(std::iter::repeat_n(210u8, 60));
        let t = ocr::otsu(&g);
        assert!((30..210).contains(&t), "threshold {t} is not between the two levels");

        // A flat picture has no gap, and must still answer rather than divide by
        // zero or loop.
        let flat = vec![128u8; 50];
        let _ = ocr::otsu(&flat);
        let _ = ocr::otsu(&[]);
    }

    #[test]
    fn binarising_ends_with_dark_text_whichever_way_it_started() {
        // Light glyphs on a dark panel - the common case on a screen, and the one
        // Windows OCR is worst at. Every version of it has to come out the same way
        // round: the minority is the text, and the text ends up black.
        let light_on_dark = ocr::prepare(&strip(20, 240, 100), ocr::Prep::Game, vision::Order::Rgba);
        let dark_on_light = ocr::prepare(&strip(240, 20, 100), ocr::Prep::Game, vision::Order::Rgba);
        for (name, out) in [("light on dark", light_on_dark), ("dark on light", dark_on_light)]
        {
            let black = out.chunks_exact(4).filter(|p| p[0] == 0).count();
            let white = out.chunks_exact(4).filter(|p| p[0] == 255).count();
            assert_eq!(black + white, 100, "{name}: something is not black or white");
            assert!(black < white, "{name}: the text should be the black minority");
        }
    }

    #[test]
    fn preparing_nothing_changes_nothing() {
        // The default has to be byte-for-byte what 1.3.5 sent to the engine, or
        // every existing macro reads differently after an upgrade.
        let src = strip(20, 240, 64);
        assert_eq!(ocr::prepare(&src, ocr::Prep::None, vision::Order::Rgba), src);
        // And a region too small to hold a pixel is not a panic.
        assert!(ocr::prepare(&[], ocr::Prep::Ui, vision::Order::Rgba).is_empty());
    }

    #[test]
    fn the_contrast_stretch_survives_a_flat_region() {
        // A single colour has no range to stretch, which is a division by zero if
        // nobody checks. The pixels come back grey, not NaN.
        let out = ocr::prepare(&[90, 90, 90, 255, 90, 90, 90, 255], ocr::Prep::Ui, vision::Order::Rgba);
        assert_eq!(out.len(), 8);
        assert_eq!(out[3], 255, "alpha has to stay opaque");
    }

    // ---- expected format ---------------------------------------------------

    #[test]
    fn a_pattern_is_small_enough_to_type_and_still_useful() {
        assert!(ocr::pattern_matches("##:##", "12:34"));
        assert!(!ocr::pattern_matches("##:##", "1:34"));
        assert!(ocr::pattern_matches("*gems*", "Total Gems: 12"), "case is ignored");
        assert!(ocr::pattern_matches("@@@", "abc"));
        assert!(!ocr::pattern_matches("@@@", "ab1"));
        assert!(ocr::pattern_matches("?#", "x7"));
        // Surrounding whitespace is not a failure - OCR adds it constantly.
        assert!(ocr::pattern_matches("##", "  42  "));
        // A run of stars is one star, and must not take exponential time.
        assert!(ocr::pattern_matches("*******x", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaax"));
        assert!(!ocr::pattern_matches("*******x", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaay"));
    }

    #[test]
    fn the_expected_format_refuses_a_wrong_shaped_reading() {
        use ocr::Expect::*;
        assert!(ocr::accepts(&Integer, "Gems: 1,250"));
        assert!(!ocr::accepts(&Integer, "no digits here"));
        assert!(ocr::accepts(&Time, "02:34"));
        assert!(!ocr::accepts(&Time, "1250"));
        assert!(ocr::accepts(&Decimal, "12.5%"));
        assert!(ocr::accepts(&Any, "anything at all"));
        assert!(!ocr::accepts(&Any, "   "));
        // An empty pattern is a pattern nobody finished typing, not a pattern that
        // matches everything.
        assert!(!ocr::accepts(&Pattern(String::new()), "12:34"));
    }

    #[test]
    fn a_decimal_reads_the_point_and_drops_the_thousands_separator() {
        assert_eq!(ocr::first_decimal("1,250.5 gems"), Some(1250.5));
        assert_eq!(ocr::first_decimal("12.5%"), Some(12.5));
        // A trailing stop is a full stop, not a point with nothing after it.
        assert_eq!(ocr::first_decimal("Level 7."), Some(7.0));
        assert_eq!(ocr::first_decimal("nothing"), None);
    }

    #[test]
    fn the_value_read_follows_the_format_asked_for() {
        // The same text means different numbers under different formats, which is
        // the whole reason the format is asked for.
        assert_eq!(ocr::value_of(&ocr::Expect::Time, "02:34"), Some(154.0));
        assert_eq!(ocr::value_of(&ocr::Expect::Integer, "02:34"), Some(2.0));
        // The default keeps the old rule: a clock reads as seconds.
        assert_eq!(ocr::value_of(&ocr::Expect::Any, "02:34"), Some(154.0));
    }

    #[test]
    fn quality_prefers_the_reading_that_fits_the_format() {
        use ocr::Expect::Time;
        // What the ladder is choosing between: a clean clock, a clock with a
        // mis-read digit, and a line of noise.
        let good = ocr::quality("02:34", &Time);
        let smudged = ocr::quality("O2:34", &Time);
        let noise = ocr::quality("~ ##@!", &Time);
        assert!(good > smudged, "{good} should beat {smudged}");
        assert!(smudged > noise, "{smudged} should beat {noise}");
        assert!(good >= 0.999, "a perfect clock has to stop the ladder early");
        assert_eq!(ocr::quality("", &Time), 0.0);
        assert_eq!(ocr::quality("   ", &ocr::Expect::Any), 0.0);
    }

    #[test]
    fn an_ocr_step_written_before_profiles_existed_still_loads() {
        // Every macro saved up to 1.3.5 has neither `prep` nor `expect`, and must
        // keep behaving exactly as it did.
        let json = r#"{"events":[{"t_us":0,"kind":{"MouseMove":{"x":1,"y":1,"dx":0,"dy":0}}}],
            "script":[{"kind":{"ReadNumber":{"x":0,"y":0,"w":300,"h":80,"var":"gold"}},
            "enabled":true},
            {"kind":{"If":{"cond":{"Text":{"x":0,"y":0,"w":10,"h":10,"needle":"go"}}}},
            "enabled":true},
            {"kind":"EndIf","enabled":true}]}"#;
        let data = parse_macro(json).expect("an older macro must still load");
        match &data.script[0].kind {
            StepKind::ReadNumber { prep, expect, var, .. } => {
                assert_eq!(var, "gold");
                assert_eq!(*prep, ocr::Prep::None);
                assert_eq!(*expect, ocr::Expect::Any);
            }
            other => panic!("wrong kind: {other:?}"),
        }
        match &data.script[1].kind {
            StepKind::If { cond: Condition::Text { prep, .. } } => {
                assert_eq!(*prep, ocr::Prep::None)
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    // ---- values that can hold text -----------------------------------------

    #[test]
    fn a_value_reads_as_a_number_whichever_way_it_is_stored() {
        // The point of this: a count read off the screen lands in a variable as
        // text, and then gets compared against a number. That has to work without
        // a conversion step nobody would think to add.
        assert_eq!(Value::Str("42".into()).as_num(), 42.0);
        assert_eq!(Value::Str("  7 ".into()).as_num(), 7.0);
        assert_eq!(Value::Num(3.5).as_num(), 3.5);
        // And something that is not a number is not silently one.
        assert_eq!(Value::Str("Roblox".into()).numeric(), None);
        assert_eq!(Value::Str("Roblox".into()).as_num(), 0.0);
        assert_eq!(Value::Str(String::new()).numeric(), None);
        // A whole number prints without a tail: `7`, never `7.0`.
        assert_eq!(Value::Num(7.0).as_text(), "7");
        assert_eq!(Value::Num(-7.0).as_text(), "-7");
        assert_eq!(Value::Num(0.5).as_text(), "0.5");
    }

    #[test]
    fn a_value_written_before_text_existed_still_loads() {
        // Every macro up to 1.3.5 stored a bare number, and the untagged form has
        // to keep reading those as numbers rather than as the text "10".
        let v: Value = serde_json::from_str("10").unwrap();
        assert_eq!(v, Value::Num(10.0));
        let t: Value = serde_json::from_str("\"10\"").unwrap();
        assert_eq!(t, Value::Str("10".into()));
        // And a whole older macro, with vars and a condition in it.
        let json = r#"{"events":[{"t_us":0,"kind":{"MouseMove":{"x":1,"y":1,"dx":0,"dy":0}}}],
            "vars":{"count":3.0},
            "script":[{"kind":{"While":{"cond":{"Var":{"name":"count","cmp":"Lt",
            "value":10.0}}}},"enabled":true},{"kind":"EndWhile","enabled":true}]}"#;
        let data = parse_macro(json).expect("an older macro must still load");
        assert_eq!(data.vars.get("count"), Some(&Value::Num(3.0)));
        match &data.script[0].kind {
            StepKind::While { cond: Condition::Var { value, .. } } => {
                assert_eq!(*value, Value::Num(10.0))
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn comparison_falls_back_to_text_only_when_it_has_to() {
        let num = Value::Num(10.0);
        let as_text = Value::Str("10".into());
        let word = Value::Str("Roblox".into());

        // Two things that read as numbers are compared as numbers, whichever way
        // round they are stored.
        assert!(Cmp::Eq.test_values(&num, &as_text));
        assert!(Cmp::Lt.test_values(&Value::Str("9".into()), &num));

        // Text is compared as text, trimmed and without case.
        assert!(Cmp::Eq.test_values(&word, &Value::Str("  roblox ".into())));
        assert!(Cmp::Ne.test_values(&word, &Value::Str("minecraft".into())));

        // And `has` is the forgiving containment screen text needs.
        assert!(Cmp::Has.test_values(
            &Value::Str("Roblox - Level 7".into()),
            &Value::Str("roblox".into())
        ));
        assert!(!Cmp::Has.test_values(&word, &Value::Str("minecraft".into())));
    }

    #[test]
    fn adding_to_text_joins_it_and_adding_to_numbers_adds_them() {
        let join = VarOp::Add
            .apply_values(&Value::Str("Level ".into()), &Value::Num(7.0));
        assert_eq!(join, Value::Str("Level 7".into()));

        // Two numbers that happen to be stored as text are still two numbers: a
        // count read from the screen and then incremented must not become "31".
        let sum = VarOp::Add.apply_values(&Value::Str("3".into()), &Value::Num(1.0));
        assert_eq!(sum, Value::Num(4.0));

        // Set replaces, whatever the kinds are.
        assert_eq!(
            VarOp::Set.apply_values(&Value::Num(1.0), &Value::Str("x".into())),
            Value::Str("x".into())
        );
        // Taking away from text is meaningless, so it answers with a number.
        assert_eq!(
            VarOp::Sub.apply_values(&Value::Str("hello".into()), &Value::Num(1.0)),
            Value::Num(-1.0)
        );
    }

    #[test]
    fn placeholders_in_step_text_are_filled_from_the_variables() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("gold".to_string(), Value::Num(1250.0));
        vars.insert("who".to_string(), Value::Str("Watson".into()));

        assert_eq!(expand_vars("{who} has {gold}", &vars), "Watson has 1250");
        // A brace that was meant literally stays one.
        assert_eq!(expand_vars("{{gold}}", &vars), "{gold}");
        // A name nobody set is left as written rather than vanishing: a message
        // that silently loses a word is much harder to diagnose.
        assert_eq!(expand_vars("{missing}", &vars), "{missing}");
        // An unclosed brace is text, not an error.
        assert_eq!(expand_vars("100% {sure", &vars), "100% {sure");
        assert_eq!(expand_vars("", &vars), "");
    }

    #[test]
    fn the_new_text_steps_survive_a_save_and_load() {
        let mut data = MacroData::new(vec![ev(0)], 1000);
        data.script.push(ScriptStep {
            kind: StepKind::GetText {
                source: TextSource::File("C:/tmp/in.txt".into()),
                var: "line".into(),
            },
            enabled: true,
        });
        data.script.push(ScriptStep {
            kind: StepKind::PutText {
                sink: TextSink::File { path: "C:/tmp/out.txt".into(), append: true },
                text: "{line}".into(),
            },
            enabled: true,
        });
        data.vars.insert("who".into(), Value::Str("Watson".into()));

        let back: MacroData =
            serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        assert_eq!(back.vars.get("who"), Some(&Value::Str("Watson".into())));
        match (&back.script[0].kind, &back.script[1].kind) {
            (
                StepKind::GetText { source: TextSource::File(p), var },
                StepKind::PutText { sink: TextSink::File { append, .. }, text },
            ) => {
                assert_eq!(p, "C:/tmp/in.txt");
                assert_eq!(var, "line");
                assert!(*append);
                assert_eq!(text, "{line}");
            }
            other => panic!("wrong kinds back: {other:?}"),
        }
    }

    #[test]
    fn an_element_step_survives_a_save_and_load() {
        let mut data = MacroData::new(vec![ev(0)], 1000);
        data.script.push(ScriptStep {
            kind: StepKind::ClickElement {
                query: uia::Query {
                    name: "Save".into(),
                    automation_id: "btnSave".into(),
                    control: "Button".into(),
                    in_front: true,
                },
                button: MouseButton::Left,
                invoke: true,
                timeout_ms: 2000,
                miss: OnMiss::Continue,
            },
            enabled: true,
        });
        let back: MacroData =
            serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        match &back.script[0].kind {
            StepKind::ClickElement { query, invoke, timeout_ms, .. } => {
                assert_eq!(query.name, "Save");
                assert_eq!(query.control, "Button");
                assert!(*invoke);
                assert_eq!(*timeout_ms, 2000);
            }
            other => panic!("wrong kind back: {other:?}"),
        }
        // A control type nobody recognises is "any", not a panic.
        assert_eq!(uia::control_id("Button"), 50000);
        assert_eq!(uia::control_id("button"), 50000);
        assert_eq!(uia::control_id("Nonsense"), 0);
        assert_eq!(uia::control_id(""), 0);
        // A query that names nothing must match nothing. Without this, the three
        // conditions collapse to "true", a subtree search for "true" returns the
        // root, and `Press element` clicks the middle of the window - which is
        // exactly what a step freshly added from the menu would have done.
        assert!(uia::Query::default().is_empty());
        assert!(uia::Query { in_front: false, ..Default::default() }.is_empty());
        assert!(uia::Query { control: "Nonsense".into(), ..Default::default() }.is_empty());
        assert!(!uia::Query { name: "Save".into(), ..Default::default() }.is_empty());
        assert!(!uia::Query { control: "Button".into(), ..Default::default() }.is_empty());
        assert!(
            !uia::Query { automation_id: "btnSave".into(), ..Default::default() }.is_empty()
        );
        // Whitespace is not a name.
        assert!(uia::Query { name: "   ".into(), ..Default::default() }.is_empty());

        // And the flag that says "ask the application" defaults to on for a query
        // written before it existed.
        let q: uia::Query = serde_json::from_str(
            r#"{"name":"Save","automation_id":"","control":""}"#,
        )
        .unwrap();
        assert!(q.in_front);
    }

    #[test]
    fn a_file_read_into_a_variable_is_capped() {
        let dir = std::env::temp_dir().join("mr_text_file");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");

        // Naming the wrong path must not be able to pull an unbounded file into a
        // variable, so the read stops at the cap.
        let big = "x".repeat(TEXT_FILE_CAP as usize + 500);
        std::fs::write(&path, &big).unwrap();
        let read = read_text_file(path.to_str().unwrap());
        assert_eq!(read.len() as u64, TEXT_FILE_CAP);

        // A file that is not there is empty, not a panic.
        assert!(read_text_file(dir.join("nothing.txt").to_str().unwrap()).is_empty());

        // Writing replaces by default and adds to the end when asked.
        let out = dir.join("out.txt");
        let p = out.to_str().unwrap();
        write_text_file(p, "one", false);
        write_text_file(p, "-two", true);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "one-two");
        write_text_file(p, "fresh", false);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "fresh");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_step_and_condition_kind_still_round_trips() {
        // Adding a kind is exactly where COUNT and the two index tables stop
        // agreeing, and the "Add" menu then offers a step nobody can edit.
        for i in 0..StepKind::COUNT {
            assert_eq!(StepKind::from_index(i).index(), i, "step kind {i}");
        }
        for i in 0..8 {
            assert_eq!(Condition::from_index(i).kind_index(), i, "condition kind {i}");
        }
        // Each of the three new kinds has to be reachable from the "Add" menu, which
        // is what the index tables are for.
        assert!(matches!(StepKind::from_index(21), StepKind::FindElement { .. }));
        assert!(matches!(StepKind::from_index(22), StepKind::ClickElement { .. }));
        assert!(matches!(Condition::from_index(7), Condition::Element { .. }));
        for i in 0..4 {
            assert_eq!(TextSource::from_index(i).index(), i);
        }
        for i in 0..2 {
            assert_eq!(TextSink::from_index(i).index(), i);
        }
    }

    // ---- 1.5.0: channel order ------------------------------------------------

    #[test]
    fn a_bgra_frame_and_its_rgba_twin_are_the_same_picture() {
        // The capture path stopped swapping red and blue and started saying which
        // way round they are instead. If `Order` is ever read wrongly the search
        // correlates a picture against its own mirror, which does not fail loudly -
        // it just stops finding things. So: build one frame both ways and require
        // the same template to land in the same place with the same score.
        let (w, h) = (160u32, 120u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        // Two properties this pattern has to have. Not grey, because equal
        // channels would pass whatever the order - which is exactly how a swap
        // survives a careless test. And not periodic, because a repeating pattern
        // matches in several places and the position assertion below would then be
        // measuring which tie the sweep happened to break.
        let mut rng = Rng(0x5150_1509_A5A5_1234);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = (rng.next_u64() & 0xFF) as u8;
                rgba[i + 1] = (rng.next_u64() & 0xFF) as u8;
                rgba[i + 2] = (rng.next_u64() & 0xFF) as u8;
                rgba[i + 3] = 255;
            }
        }
        let mut bgra = rgba.clone();
        for p in bgra.chunks_exact_mut(4) {
            p.swap(0, 2);
            p[3] = 0; // and GDI leaves the alpha at zero, as the real thing does
        }
        let a = vision::Frame::rgba(0, 0, w, h, rgba);
        let b = vision::Frame { x: 0, y: 0, w, h, px: bgra, order: vision::Order::Bgra };

        // `to_rgba` is the way back out, and it has to undo both the order and the
        // missing alpha.
        assert_eq!(a.to_rgba(), b.to_rgba(), "to_rgba must reconcile the two orders");

        let tpl = vision::Frame::rgba(
            0,
            0,
            24,
            24,
            {
                let mut t = vec![0u8; 24 * 24 * 4];
                for y in 0..24u32 {
                    for x in 0..24u32 {
                        let src = (((y + 40) * w + x + 50) * 4) as usize;
                        let dst = ((y * 24 + x) * 4) as usize;
                        t[dst..dst + 4].copy_from_slice(&a.px[src..src + 4]);
                    }
                }
                t
            },
        )
        .as_template("probe");

        let ha = vision::find(&a, &tpl, false).expect("red-first haystack");
        let hb = vision::find(&b, &tpl, false).expect("blue-first haystack");
        assert_eq!((ha.x, ha.y), (hb.x, hb.y), "the two orders found different places");
        assert!((ha.score - hb.score).abs() < 1e-4, "{} vs {}", ha.score, hb.score);
        assert_eq!((ha.x, ha.y), (62, 52), "and it is not where the template was cut");
    }

    #[test]
    fn a_capture_with_no_alpha_still_matches() {
        // A screen grab has an alpha of zero everywhere. The mask reads the alpha,
        // so a haystack whose mask were consulted would be entirely masked out and
        // every search would come back with nothing. The search throws the
        // haystack's mask away for exactly this reason; this is the test that says
        // so, because the symptom would otherwise be "image steps stopped working".
        let (w, h) = (96u32, 96u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let v = (((x / 8) + (y / 8)) % 2) as u8 * 200 + 20;
                px[i] = v;
                px[i + 1] = v;
                px[i + 2] = v;
                px[i + 3] = 0; // no alpha at all, as GDI leaves it
            }
        }
        let hay = vision::Frame { x: 0, y: 0, w, h, px, order: vision::Order::Bgra };
        let tpl = checker_template(16, 16);
        assert!(
            vision::find(&hay, &tpl, false).is_some(),
            "a frame with no alpha found nothing at all"
        );
    }

    // ---- 1.5.0: what happens when a step finds nothing -----------------------

    #[test]
    fn a_missing_miss_policy_reads_as_carry_on() {
        // Every macro written before 1.5.0 has no `miss` field anywhere, and every
        // one of them has to keep behaving exactly as it did. This is the whole
        // compatibility promise of the feature in one assertion.
        let old = r#"{"version":3,"duration_us":10,"events":[],"script":[
            {"kind":{"ClickImage":{"template":"a","threshold":0.9,"button":"Left"}},
             "enabled":true},
            {"kind":{"WaitFor":{"cond":"Always","appear":true,"timeout_ms":5}},
             "enabled":true}]}"#;
        let data = parse_macro(old).expect("a 1.4.0 script must still load");
        for st in &data.script {
            assert_eq!(st.kind.miss(), OnMiss::Continue, "{:?}", st.kind);
        }
    }

    #[test]
    fn every_miss_policy_round_trips() {
        for i in 0..OnMiss::COUNT {
            assert_eq!(OnMiss::from_index(i).index(), i, "policy {i}");
        }
        let mut data = MacroData::new(vec![ev(0)], 1000);
        for m in [
            OnMiss::Continue,
            OnMiss::Stop,
            OnMiss::Break,
            OnMiss::Retry { times: 7, delay_ms: 250 },
        ] {
            data.script.push(ScriptStep::new(StepKind::ClickImage {
                template: "t".into(),
                threshold: 0.9,
                button: MouseButton::Left,
                area: SearchArea::default(),
                edge: false,
                miss: m,
            }));
        }
        let back: MacroData =
            serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        assert_eq!(back.script[1].kind.miss(), OnMiss::Stop);
        assert_eq!(back.script[2].kind.miss(), OnMiss::Break);
        assert_eq!(back.script[3].kind.miss(), OnMiss::Retry { times: 7, delay_ms: 250 });
    }

    #[test]
    fn leaving_a_loop_lands_in_the_same_place_however_it_was_asked_for() {
        // `Break` the step and `Break` the miss policy are two spellings of one
        // jump. If they ever disagreed about nesting, a macro would leave the wrong
        // loop - and it would do so only sometimes, which is the worst kind.
        let k = |kind| ScriptStep::new(kind);
        let steps = vec![
            k(StepKind::While { cond: Condition::Always }), // 0
            k(StepKind::While { cond: Condition::Always }), // 1
            k(StepKind::Break),                             // 2 - inner
            k(StepKind::EndWhile),                          // 3
            k(StepKind::Break),                             // 4 - outer
            k(StepKind::EndWhile),                          // 5
            k(StepKind::Log { text: String::new() }),       // 6
        ];
        assert_eq!(break_target(&steps, 2), 4, "the inner break lands after its own end");
        assert_eq!(break_target(&steps, 4), 6, "the outer break lands after the outer end");
        // A break with no loop around it runs off the end rather than anywhere odd.
        let loose = vec![k(StepKind::Break), k(StepKind::Log { text: String::new() })];
        assert_eq!(break_target(&loose, 0), loose.len());
    }

    #[test]
    fn a_retry_policy_reports_how_many_extra_looks_it_wants() {
        assert_eq!(OnMiss::Continue.retries(), (0, 0));
        assert_eq!(OnMiss::Stop.retries(), (0, 0));
        assert_eq!(OnMiss::Retry { times: 4, delay_ms: 750 }.retries(), (4, 750));
    }

    // ---- 1.5.0: calling another macro ---------------------------------------

    #[test]
    fn a_call_step_survives_a_save_and_load() {
        let mut data = MacroData::new(vec![ev(0)], 1000);
        data.script.push(ScriptStep::new(StepKind::Call {
            path: "sub/login.json".into(),
            miss: OnMiss::Stop,
        }));
        let back: MacroData =
            serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        match &back.script[0].kind {
            StepKind::Call { path, miss } => {
                assert_eq!(path, "sub/login.json");
                assert_eq!(*miss, OnMiss::Stop);
            }
            other => panic!("wrong kind back: {other:?}"),
        }
    }

    #[test]
    fn the_call_depth_cap_is_small_enough_to_be_reached_before_the_stack_is() {
        // Under `panic = "abort"` a stack overflow is the process gone with keys
        // held. The cap is the only thing standing between that and a macro that
        // names itself, so it has to be a small number and it has to be checked
        // before the recursion, not after.
        assert!(MAX_CALL_DEPTH >= 2, "nesting has to be worth having");
        assert!(MAX_CALL_DEPTH <= 32, "a cap this deep is not a cap");
    }

    // ---- 1.5.0: recording into picture steps --------------------------------

    fn click_pair(t: u64, x: i32, y: i32, up_x: i32, up_y: i32, up_t: u64) -> Vec<MacroEvent> {
        vec![
            MacroEvent {
                t_us: t,
                kind: InputEventKind::MouseButton {
                    button: MouseButton::Left,
                    down: true,
                    x,
                    y,
                },
            },
            MacroEvent {
                t_us: up_t,
                kind: InputEventKind::MouseButton {
                    button: MouseButton::Left,
                    down: false,
                    x: up_x,
                    y: up_y,
                },
            },
        ]
    }

    #[test]
    fn a_click_is_told_apart_from_a_drag() {
        // A drag is press, move, release, and turning one into "find the picture
        // and click it" would drop the drag without saying so. The distance and the
        // hold are both checked because a slow press in place is still a click and
        // a fast flick across the screen is not.
        let click = click_pair(0, 100, 100, 102, 101, 50_000);
        assert_eq!(matching_release(&click, 0, MouseButton::Left), Some(1));

        let drag = click_pair(0, 100, 100, 400, 300, 50_000);
        assert_eq!(matching_release(&drag, 0, MouseButton::Left), None, "a drag");

        let held = click_pair(0, 100, 100, 100, 100, 5_000_000);
        assert_eq!(matching_release(&held, 0, MouseButton::Left), None, "a long hold");

        // A press that is never released is the end of the recording, not a click.
        assert_eq!(matching_release(&click[..1], 0, MouseButton::Left), None);
    }

    #[test]
    fn turning_clicks_into_pictures_keeps_every_event() {
        // The promise of the feature: the clicks become pictures and *nothing else
        // changes*. The keystrokes between them, the timing, the scrolling - all of
        // it has to still be replayed, or the macro that comes back is not the
        // macro that was recorded.
        let mut events = Vec::new();
        events.extend(click_pair(0, 50, 50, 50, 50, 10_000));
        events.push(MacroEvent {
            t_us: 20_000,
            kind: InputEventKind::Key { vk: 0x41, scan: 0, down: true, extended: false },
        });
        events.push(MacroEvent {
            t_us: 21_000,
            kind: InputEventKind::Key { vk: 0x41, scan: 0, down: false, extended: false },
        });
        events.extend(click_pair(30_000, 300, 200, 301, 200, 40_000));
        let total = events.len();
        let data = MacroData::new(events, 40_000);

        let shots: Vec<ClickShot> = [0usize, 4]
            .iter()
            .map(|i| ClickShot {
                index: *i,
                button: MouseButton::Left,
                x: 0,
                y: 0,
                left: 0,
                top: 0,
                w: 2,
                h: 2,
                rgba: vec![255u8; 16],
                dpi: 96,
            })
            .collect();
        let names = vec!["a".to_string(), "b".to_string()];
        let (script, made) =
            script_from_click_shots(&data, &shots, &names, 0.85, OnMiss::Stop);
        assert_eq!(made, 2, "both clicks should have become picture steps");

        // Two picture steps, and the keystrokes in a `Play events` range between
        // them. Nothing after the last click here, so no trailing range.
        let images = script
            .iter()
            .filter(|s| matches!(s.kind, StepKind::ClickImage { .. }))
            .count();
        assert_eq!(images, 2);

        // The ranges, taken together, must cover every event that is not part of a
        // converted click, exactly once and in order.
        let mut covered: Vec<usize> = Vec::new();
        for st in &script {
            if let StepKind::PlayEvents { from, to } = st.kind {
                assert!(from <= to, "a backwards range: {from}..{to}");
                covered.extend(from..=to);
            }
        }
        assert_eq!(covered, vec![2, 3], "only the keystrokes should be replayed raw");
        assert!(covered.iter().all(|i| *i < total));

        // And the generated steps carry the policy they were asked for, which is
        // the whole reason the offer has a combo box on it.
        for st in &script {
            if matches!(st.kind, StepKind::ClickImage { .. }) {
                assert_eq!(st.kind.miss(), OnMiss::Stop);
            }
        }
    }

    #[test]
    fn a_drag_is_left_alone_rather_than_turned_into_a_picture() {
        let events = click_pair(0, 50, 50, 400, 400, 10_000);
        let data = MacroData::new(events, 10_000);
        let shots = vec![ClickShot {
            index: 0,
            button: MouseButton::Left,
            x: 50,
            y: 50,
            left: 0,
            top: 0,
            w: 2,
            h: 2,
            rgba: vec![255u8; 16],
            dpi: 96,
        }];
        let (script, made) =
            script_from_click_shots(&data, &shots, &["d".into()], 0.85, OnMiss::Continue);
        assert_eq!(made, 0, "a drag must not become a picture step");
        // And the whole recording still plays.
        assert_eq!(script.len(), 1);
        assert!(matches!(script[0].kind, StepKind::PlayEvents { from: 0, to: 1 }));
    }

    #[test]
    fn a_shot_pointing_at_an_event_that_was_deleted_is_ignored() {
        // The squares are taken during recording and used after it, and the editor
        // sits between the two. An index that no longer names a click has to be
        // skipped rather than panic or generate a step for something else.
        let data = MacroData::new(click_pair(0, 10, 10, 10, 10, 5_000), 5_000);
        let shots = vec![ClickShot {
            index: 99,
            button: MouseButton::Left,
            x: 0,
            y: 0,
            left: 0,
            top: 0,
            w: 2,
            h: 2,
            rgba: vec![255u8; 16],
            dpi: 96,
        }];
        let (script, made) =
            script_from_click_shots(&data, &shots, &["gone".into()], 0.85, OnMiss::Stop);
        assert_eq!(made, 0);
        assert_eq!(script.len(), 1, "the recording still plays whole");
    }

    #[test]
    fn generated_template_names_are_unique_and_ordered() {
        let stamp = "20260101_1200";
        let names: Vec<String> = (1..=12).map(|n| click_shot_name(stamp, n)).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "the names have to sort into step order");
        let unique: std::collections::BTreeSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
    }

    // ---- 1.5.0: the self-running executable's footer -------------------------

    #[test]
    fn a_footer_length_that_overflows_is_refused() {
        // The hole this closes: `16 + len` wraps to zero in a release build for a
        // length near u64::MAX, and the `checked_sub` underneath it then succeeds.
        // The subtraction looked careful; the addition it depended on was not.
        let mut image = vec![0u8; 4096];
        image.extend_from_slice(&(u64::MAX - 15).to_le_bytes());
        image.extend_from_slice(PAYLOAD_MAGIC);
        assert_eq!(payload_offset(&image), None, "a wrapping length must be refused");

        // And the plain lie: more bytes than the file holds.
        let mut image = vec![0u8; 4096];
        image.extend_from_slice(&1_000_000u64.to_le_bytes());
        image.extend_from_slice(PAYLOAD_MAGIC);
        assert_eq!(payload_offset(&image), None, "a length past the end of the file");

        // A length past what any real macro could be, but inside the file.
        let mut image = vec![0u8; 200 * 1024 * 1024];
        let n = (MAX_PAYLOAD + 1).to_le_bytes();
        image.extend_from_slice(&n);
        image.extend_from_slice(PAYLOAD_MAGIC);
        assert_eq!(payload_offset(&image), None, "a length past the cap");
    }

    #[test]
    fn an_honest_footer_still_parses() {
        // The hardening must not have closed the door on the feature itself.
        let body = b"hello, payload";
        let mut image = vec![7u8; 512];
        let start = image.len();
        image.extend_from_slice(body);
        image.extend_from_slice(&(body.len() as u64).to_le_bytes());
        image.extend_from_slice(PAYLOAD_MAGIC);
        assert_eq!(payload_offset(&image), Some(start));
        // No magic at all is not an error, it is an ordinary executable.
        assert_eq!(payload_offset(&vec![7u8; 512]), None);
        // And a file too short to hold a footer must not index backwards past zero.
        assert_eq!(payload_offset(&[]), None);
        assert_eq!(payload_offset(&[1, 2, 3]), None);
    }

    #[test]
    fn a_compression_bomb_is_refused_rather_than_allocated() {
        // A gigabyte of zeroes compresses to about a megabyte, and `read_to_end`
        // would have committed the gigabyte. Under `panic = "abort"` a failed
        // allocation is not something anybody catches.
        let huge = vec![0u8; (MAX_INFLATED + 4096) as usize];
        let squashed = gzip(&huge).expect("gzip");
        assert!(
            (squashed.len() as u64) < MAX_INFLATED / 100,
            "the bomb should be much smaller than what it expands to"
        );
        let err = gunzip(&squashed).expect_err("the bomb must be refused");
        assert!(
            err.to_string().contains("expands past"),
            "refused for the wrong reason: {err}"
        );
        // And something of an ordinary size still round-trips.
        let ordinary = b"a macro, compressed".to_vec();
        assert_eq!(gunzip(&gzip(&ordinary).unwrap()).unwrap(), ordinary);
    }

    #[test]
    fn every_prep_and_format_round_trips_through_its_index() {
        for i in 0..6 {
            assert_eq!(ocr::Prep::from_index(i).index(), i);
        }
        for i in 0..5 {
            assert_eq!(ocr::Expect::from_index(i).index(), i);
        }
        // The ladder must not contain the rung that means "climb the ladder".
        assert!(!ocr::Prep::LADDER.contains(&ocr::Prep::Auto));
    }
}
