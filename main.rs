#![cfg(windows)]
#![windows_subsystem = "windows"]

use eframe::egui;
use serde::{Deserialize, Serialize};

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ============================================================
// Localization
// ============================================================

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    En,
    Ru,
}

struct Strings {
    record: &'static str,
    stop_rec: &'static str,
    play: &'static str,
    stop_play: &'static str,
    rec_time: &'static str,
    rec_done: &'static str,
    play_inf: &'static str,
    play_lim: &'static str,
    loop_cb: &'static str,
    play_count: &'static str,
    speed: &'static str,
    abs_mouse: &'static str,
    on_top: &'static str,
    theme: &'static str,
    language: &'static str,
    lang_auto: &'static str,
    save: &'static str,
    load: &'static str,
    events: &'static str,
    status_ready: &'static str,
    status_rec: &'static str,
    status_play: &'static str,
    saved: &'static str,
    loaded: &'static str,
    save_err: &'static str,
    load_err: &'static str,
}

const EN: Strings = Strings {
    record: "🔴 Record (F8)",
    stop_rec: "⏹ Stop Rec",
    play: "▶ Play (F9)",
    stop_play: "⏹ Stop Play",
    rec_time: "⏱ Recording time: {}…",
    rec_done: "⏱ Recorded time: {} (finished)",
    play_inf: "🔄 Play count: {} (infinite)",
    play_lim: "🔄 Play count: {} / {}",
    loop_cb: "Continuous playback (Loop)",
    play_count: "Play count:",
    speed: "Speed",
    abs_mouse: "Absolute mouse (High-DPI fix)",
    on_top: "📌 Always on Top",
    theme: "Theme:",
    language: "Language:",
    lang_auto: "Auto (system)",
    save: "💾 Save",
    load: "📂 Load",
    events: "📦 Events recorded: {}",
    status_ready: "Ready [F8: Record | F9: Play]",
    status_rec: "Recording... [F8 to stop]",
    status_play: "Playing... [F9 to stop]",
    saved: "Saved to macro.json",
    loaded: "Loaded macro.json",
    save_err: "Save error: {}",
    load_err: "Load error: {}",
};

const RU: Strings = Strings {
    record: "🔴 Запись (F8)",
    stop_rec: "⏹ Стоп запись",
    play: "▶ Плей (F9)",
    stop_play: "⏹ Стоп плей",
    rec_time: "⏱ Время записи: {}…",
    rec_done: "⏱ Время записи: {} (завершено)",
    play_inf: "🔄 Проигрываний: {} (бесконечно)",
    play_lim: "🔄 Проигрываний: {} / {}",
    loop_cb: "Непрерывное воспроизведение (цикл)",
    play_count: "Проигрываний:",
    speed: "Скорость",
    abs_mouse: "Абсолютная мышь (фикс High-DPI)",
    on_top: "📌 Поверх всех окон",
    theme: "Тема:",
    language: "Язык:",
    lang_auto: "Авто (система)",
    save: "💾 Сохранить",
    load: "📂 Загрузить",
    events: "📦 Событий записано: {}",
    status_ready: "Готов [F8: запись | F9: плей]",
    status_rec: "Запись... [F8 — стоп]",
    status_play: "Воспроизведение... [F9 — стоп]",
    saved: "Сохранено в macro.json",
    loaded: "Загружено из macro.json",
    save_err: "Ошибка сохранения: {}",
    load_err: "Ошибка загрузки: {}",
};

static SYSTEM_LANG: OnceLock<Lang> = OnceLock::new();

fn detect_system_lang() -> Lang {
    unsafe {
        let lang = GetUserDefaultUILanguage() as u32;
        if lang & 0x3FF == 0x19 {
            Lang::Ru
        } else {
            Lang::En
        }
    }
}

// ============================================================
// Themes
// ============================================================

#[derive(Clone, Copy, PartialEq)]
enum Theme {
    Dark,
    Material3,
    Fluent,
    Catppuccin,
    Nord,
    Dracula,
    Glass,
    Neumorphism,
}

const THEME_NAMES: [&str; 8] = [
    "Dark (default)",
    "Material Design 3",
    "Fluent (Mica / Acrylic)",
    "Catppuccin Mocha",
    "Nord",
    "Dracula",
    "Glassmorphism",
    "Neumorphism",
];

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
    accent_text: egui::Color32,
    widget_round: f32,
    shadow_blur: u8,
    shadow_offset: i8,
    shadow_alpha: u8,
    item_spacing_y: f32,
    button_padding: f32,
    animation_time: f32,
    backdrop: i32, // 1 = none, 2 = Mica, 3 = Acrylic
}

fn rgb(r: u8, g: u8, b: u8) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn palette(theme: Theme) -> Palette {
    match theme {
        Theme::Dark => Palette {
            dark: true,
            bg: rgb(16, 16, 16),
            panel: rgb(24, 24, 24),
            widget: rgb(42, 42, 42),
            widget_hover: rgb(58, 58, 58),
            widget_active: rgb(75, 75, 75),
            active_fg: rgb(255, 255, 255),
            border: rgb(70, 70, 70),
            hover_border: rgb(95, 95, 95),
            text: rgb(230, 230, 230),
            faint: rgb(130, 130, 130),
            accent: rgb(70, 130, 255),
            accent_text: rgb(255, 255, 255),
            widget_round: 4.0,
            shadow_blur: 4,
            shadow_offset: 1,
            shadow_alpha: 60,
            item_spacing_y: 5.0,
            button_padding: 3.0,
            animation_time: 0.15,
            backdrop: 1,
        },
        Theme::Material3 => Palette {
            dark: true,
            bg: rgb(18, 17, 24),
            panel: rgb(18, 17, 24),
            widget: rgb(56, 48, 75),
            widget_hover: rgb(70, 60, 92),
            widget_active: rgb(208, 188, 255),
            active_fg: rgb(56, 30, 114),
            border: rgb(73, 69, 82),
            hover_border: rgb(208, 188, 255),
            text: rgb(230, 224, 233),
            faint: rgb(147, 143, 153),
            accent: rgb(208, 188, 255),
            accent_text: rgb(56, 30, 114),
            widget_round: 20.0,
            shadow_blur: 8,
            shadow_offset: 2,
            shadow_alpha: 80,
            item_spacing_y: 7.0,
            button_padding: 6.0,
            animation_time: 0.4,
            backdrop: 1,
        },
        Theme::Fluent => Palette {
            dark: true,
            bg: rgb(16, 16, 16),
            panel: rgba(24, 26, 30, 140),
            widget: rgba(255, 255, 255, 28),
            widget_hover: rgba(255, 255, 255, 48),
            widget_active: rgb(96, 205, 255),
            active_fg: rgb(6, 25, 45),
            border: rgba(255, 255, 255, 55),
            hover_border: rgb(96, 205, 255),
            text: rgb(250, 250, 250),
            faint: rgb(170, 175, 180),
            accent: rgb(96, 205, 255),
            accent_text: rgb(6, 25, 45),
            widget_round: 4.0,
            shadow_blur: 4,
            shadow_offset: 1,
            shadow_alpha: 70,
            item_spacing_y: 5.0,
            button_padding: 4.0,
            animation_time: 0.2,
            backdrop: 2,
        },
        Theme::Catppuccin => Palette {
            dark: true,
            bg: rgb(17, 17, 27),
            panel: rgb(30, 30, 46),
            widget: rgb(49, 50, 68),
            widget_hover: rgb(69, 71, 90),
            widget_active: rgb(203, 166, 247),
            active_fg: rgb(17, 17, 27),
            border: rgb(88, 91, 112),
            hover_border: rgb(203, 166, 247),
            text: rgb(205, 214, 244),
            faint: rgb(166, 172, 200),
            accent: rgb(203, 166, 247),
            accent_text: rgb(17, 17, 27),
            widget_round: 10.0,
            shadow_blur: 6,
            shadow_offset: 2,
            shadow_alpha: 90,
            item_spacing_y: 5.0,
            button_padding: 4.0,
            animation_time: 0.25,
            backdrop: 1,
        },
        Theme::Nord => Palette {
            dark: true,
            bg: rgb(46, 52, 64),
            panel: rgb(46, 52, 64),
            widget: rgb(59, 66, 82),
            widget_hover: rgb(67, 76, 94),
            widget_active: rgb(136, 192, 208),
            active_fg: rgb(46, 52, 64),
            border: rgb(76, 86, 106),
            hover_border: rgb(136, 192, 208),
            text: rgb(216, 222, 233),
            faint: rgb(148, 155, 168),
            accent: rgb(136, 192, 208),
            accent_text: rgb(46, 52, 64),
            widget_round: 6.0,
            shadow_blur: 5,
            shadow_offset: 1,
            shadow_alpha: 80,
            item_spacing_y: 5.0,
            button_padding: 4.0,
            animation_time: 0.2,
            backdrop: 1,
        },
        Theme::Dracula => Palette {
            dark: true,
            bg: rgb(40, 42, 54),
            panel: rgb(40, 42, 54),
            widget: rgb(68, 71, 90),
            widget_hover: rgb(80, 83, 105),
            widget_active: rgb(255, 121, 198),
            active_fg: rgb(40, 42, 54),
            border: rgb(98, 114, 164),
            hover_border: rgb(255, 121, 198),
            text: rgb(248, 248, 242),
            faint: rgb(135, 140, 160),
            accent: rgb(255, 121, 198),
            accent_text: rgb(40, 42, 54),
            widget_round: 8.0,
            shadow_blur: 6,
            shadow_offset: 2,
            shadow_alpha: 90,
            item_spacing_y: 5.0,
            button_padding: 4.0,
            animation_time: 0.25,
            backdrop: 1,
        },
        Theme::Glass => Palette {
            dark: true,
            bg: rgb(24, 28, 40),
            panel: rgba(40, 46, 64, 110),
            widget: rgba(255, 255, 255, 45),
            widget_hover: rgba(255, 255, 255, 75),
            widget_active: rgba(120, 180, 255, 200),
            active_fg: rgb(255, 255, 255),
            border: rgba(255, 255, 255, 110),
            hover_border: rgba(255, 255, 255, 170),
            text: rgb(240, 245, 255),
            faint: rgb(190, 200, 220),
            accent: rgb(120, 180, 255),
            accent_text: rgb(255, 255, 255),
            widget_round: 14.0,
            shadow_blur: 12,
            shadow_offset: 3,
            shadow_alpha: 100,
            item_spacing_y: 5.0,
            button_padding: 4.0,
            animation_time: 0.3,
            backdrop: 3,
        },
        Theme::Neumorphism => Palette {
            dark: false,
            bg: rgb(224, 229, 236),
            panel: rgb(224, 229, 236),
            widget: rgb(224, 229, 236),
            widget_hover: rgb(231, 236, 243),
            widget_active: rgb(93, 120, 255),
            active_fg: rgb(255, 255, 255),
            border: rgb(224, 229, 236),
            hover_border: rgb(224, 229, 236),
            text: rgb(60, 70, 90),
            faint: rgb(120, 130, 150),
            accent: rgb(93, 120, 255),
            accent_text: rgb(255, 255, 255),
            widget_round: 12.0,
            shadow_blur: 10,
            shadow_offset: 5,
            shadow_alpha: 110,
            item_spacing_y: 6.0,
            button_padding: 5.0,
            animation_time: 0.25,
            backdrop: 1,
        },
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

fn make_visuals(p: &Palette, see_through: bool) -> egui::Visuals {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.window_fill = p.panel;
    v.panel_fill = p.panel;
    v.extreme_bg_color = p.bg;
    v.window_shadow = make_shadow(p);
    v.popup_shadow = make_shadow(p);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent;
    v.selection.stroke = egui::Stroke::new(1.0, p.accent_text);

    let states = [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
    ];
    for w in states {
        w.corner_radius = p.widget_round.into();
        w.bg_stroke = egui::Stroke::new(1.0, p.border);
        w.fg_stroke = egui::Stroke::new(1.0, p.text);
    }
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.faint);
    v.widgets.inactive.bg_fill = p.widget;
    v.widgets.hovered.bg_fill = p.widget_hover;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, p.hover_border);
    v.widgets.active.bg_fill = p.widget_active;
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0, p.active_fg);

    // Прозрачная клиентская область: сквозь UI виден Mica/Acrylic
    if see_through {
        v.window_fill = egui::Color32::TRANSPARENT;
        v.extreme_bg_color = egui::Color32::TRANSPARENT;
        let no_shadow = egui::Shadow {
            offset: [0, 0],
            blur: 0,
            spread: 0,
            color: egui::Color32::TRANSPARENT,
        };
        v.window_shadow = no_shadow;
        v.popup_shadow = no_shadow;
    }

    v
}

fn set_system_backdrop(kind: i32) {
    unsafe {
        let hwnd = match FindWindowW(None, w!("Macro Recorder")) {
            Ok(h) => h,
            Err(_) => GetForegroundWindow(),
        };
        if hwnd.is_invalid() {
            log_debug("set_system_backdrop: HWND not found");
            return;
        }
        let value: i32 = kind;
        if let Err(e) = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(38), // DWMWA_SYSTEMBACKDROP_TYPE
            &value as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        ) {
            log_debug(&format!("DwmSetWindowAttribute failed: {e}"));
        }
    }
}

fn apply_theme(ctx: &egui::Context, theme: Theme) {
    let p = palette(theme);
    let see_through = matches!(theme, Theme::Fluent | Theme::Glass);

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = make_visuals(&p, see_through);
    style.animation_time = p.animation_time;
    style.spacing.item_spacing = egui::vec2(8.0, p.item_spacing_y);
    style.spacing.button_padding = egui::vec2(p.button_padding, p.button_padding);

    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);

    set_system_backdrop(p.backdrop);
    log_debug(&format!("apply_theme: {}", THEME_NAMES[theme as usize]));
}

// ============================================================
// Model
// ============================================================

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum InputEventKind {
    Key {
        vk: u16,
        scan: u16,
        down: bool,
        extended: bool,
    },
    MouseMove {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
        x: i32,
        y: i32,
    },
    MouseWheel {
        delta: i32,
        x: i32,
        y: i32,
        horizontal: bool,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MacroEvent {
    pub t_us: u64,
    pub kind: InputEventKind,
}

// ============================================================
// Globals
// ============================================================

static EPOCH: OnceLock<Instant> = OnceLock::new();
static INPUT_TX: OnceLock<Sender<MacroEvent>> = OnceLock::new();
static MACRO: Mutex<Vec<MacroEvent>> = Mutex::new(Vec::new());

static RECORDING: AtomicBool = AtomicBool::new(false);
static PLAYING: AtomicBool = AtomicBool::new(false);
static STOP_PLAY: AtomicBool = AtomicBool::new(false);
static LOOP_PLAY: AtomicBool = AtomicBool::new(true);
static ABSOLUTE_MOUSE: AtomicBool = AtomicBool::new(true);

static REC_START_US: AtomicU64 = AtomicU64::new(0);
static LAST_MOVE_US: AtomicU64 = AtomicU64::new(0);
static RECORDED_TIME_US: AtomicU64 = AtomicU64::new(0);

static LAST_X: AtomicI32 = AtomicI32::new(i32::MIN);
static LAST_Y: AtomicI32 = AtomicI32::new(i32::MIN);

static SPEED: Mutex<f64> = Mutex::new(1.0);

static PLAY_COUNT: AtomicU64 = AtomicU64::new(0);
static PLAY_COUNT_LIMIT: AtomicU64 = AtomicU64::new(1);

// ============================================================
// Hotkeys
// ============================================================

const WM_HOTKEY: u32 = 0x0312;
const HOTKEY_ID_RECORD: i32 = 1;
const HOTKEY_ID_PLAY: i32 = 2;
const VK_F8: u32 = 0x77;
const VK_F9: u32 = 0x78;

fn toggle_recording() {
    if RECORDING.load(Ordering::Relaxed) {
        stop_recording();
    } else {
        start_recording();
    }
}

fn toggle_playback() {
    if PLAYING.load(Ordering::Relaxed) {
        stop_playback();
    } else {
        start_playback();
    }
}

fn handle_hotkey(id: i32) {
    match id {
        HOTKEY_ID_RECORD => toggle_recording(),
        HOTKEY_ID_PLAY => toggle_playback(),
        _ => {}
    }
}

fn log_debug(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("macro_recorder_log.txt")
    {
        let _ = writeln!(f, "{msg}");
    }
}

// ============================================================
// Timing helpers
// ============================================================

fn now_us() -> u64 {
    EPOCH
        .get()
        .map(|e| e.elapsed().as_micros() as u64)
        .unwrap_or(0)
}

fn current_rec_time_us() -> u64 {
    now_us().saturating_sub(REC_START_US.load(Ordering::Relaxed))
}

fn format_duration_us(us: u64) -> String {
    let total_secs = us / 1_000_000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}

fn emit_event(kind: InputEventKind) {
    let event = MacroEvent {
        t_us: current_rec_time_us(),
        kind,
    };

    if let Some(tx) = INPUT_TX.get() {
        let _ = tx.send(event);
    }
}

// ============================================================
// Record / Playback triggers
// ============================================================

fn start_recording() {
    if PLAYING.load(Ordering::Relaxed) {
        return;
    }

    if let Ok(mut macro_data) = MACRO.lock() {
        macro_data.clear();
    }

    LAST_X.store(i32::MIN, Ordering::Relaxed);
    LAST_Y.store(i32::MIN, Ordering::Relaxed);
    LAST_MOVE_US.store(0, Ordering::Relaxed);
    RECORDED_TIME_US.store(0, Ordering::Relaxed);

    REC_START_US.store(now_us(), Ordering::Relaxed);
    RECORDING.store(true, Ordering::Relaxed);
}

fn stop_recording() {
    if RECORDING.load(Ordering::Relaxed) {
        RECORDED_TIME_US.store(current_rec_time_us(), Ordering::Relaxed);
    }
    RECORDING.store(false, Ordering::Relaxed);
}

fn start_playback() {
    if RECORDING.load(Ordering::Relaxed) || PLAYING.load(Ordering::Relaxed) {
        return;
    }

    let events = match MACRO.lock() {
        Ok(data) => data.clone(),
        Err(_) => Vec::new(),
    };

    if events.is_empty() {
        return;
    }

    PLAY_COUNT.store(0, Ordering::Relaxed);

    STOP_PLAY.store(false, Ordering::Relaxed);
    PLAYING.store(true, Ordering::Relaxed);

    std::thread::spawn(move || {
        playback_loop(&events);
        PLAYING.store(false, Ordering::Relaxed);
    });
}

fn stop_playback() {
    STOP_PLAY.store(true, Ordering::Relaxed);
}

// ============================================================
// Win32 Hook Thread + Hotkeys
// ============================================================

fn input_hook_thread() {
    unsafe {
        let hmod = GetModuleHandleW(None)
            .map(|h| HINSTANCE(h.0))
            .unwrap_or_default();

        let keyboard_hook =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), Some(hmod), 0);
        let mouse_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), Some(hmod), 0);

        match (&keyboard_hook, &mouse_hook) {
            (Ok(_), Ok(_)) => {}
            _ => log_debug("Failed to install low-level hooks"),
        }

        if let Err(e) = RegisterHotKey(None, HOTKEY_ID_RECORD, MOD_NOREPEAT, VK_F8) {
            log_debug(&format!("RegisterHotKey(F8) failed: {e}"));
        }
        if let Err(e) = RegisterHotKey(None, HOTKEY_ID_PLAY, MOD_NOREPEAT, VK_F9) {
            log_debug(&format!("RegisterHotKey(F9) failed: {e}"));
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                handle_hotkey(msg.wParam.0 as i32);
                continue;
            }
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }

        let _ = UnregisterHotKey(None, HOTKEY_ID_RECORD);
        let _ = UnregisterHotKey(None, HOTKEY_ID_PLAY);

        if let Ok(h) = keyboard_hook {
            let _ = UnhookWindowsHookEx(h);
        }
        if let Ok(h) = mouse_hook {
            let _ = UnhookWindowsHookEx(h);
        }
    }
}

// ============================================================
// Hooks
// ============================================================

const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;

unsafe extern "system" fn keyboard_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == 0 && RECORDING.load(Ordering::Relaxed) {
        let data = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let flags = data.flags.0;

        if flags & LLKHF_INJECTED.0 == 0 {
            let vk = data.vkCode as u16;
            let wm = wparam.0 as u32;
            let (down, valid) = match wm {
                WM_KEYDOWN | WM_SYSKEYDOWN => (true, true),
                WM_KEYUP | WM_SYSKEYUP => (false, true),
                _ => (false, false),
            };

            if valid {
                emit_event(InputEventKind::Key {
                    vk,
                    scan: data.scanCode as u16,
                    down,
                    extended: flags & LLKHF_EXTENDED.0 != 0,
                });
            }
        }
    }

    CallNextHookEx(None, code, wparam, lparam)
}

const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_MOUSEHWHEEL: u32 = 0x020E;

unsafe extern "system" fn mouse_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == 0 && RECORDING.load(Ordering::Relaxed) {
        let data = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let flags = data.flags;

        if flags & LLMHF_INJECTED == 0 {
            let x = data.pt.x;
            let y = data.pt.y;

            let last_x = LAST_X.swap(x, Ordering::Relaxed);
            let last_y = LAST_Y.swap(y, Ordering::Relaxed);

            let (dx, dy) = if last_x == i32::MIN || last_y == i32::MIN {
                (0, 0)
            } else {
                (x - last_x, y - last_y)
            };

            let wm = wparam.0 as u32;

            let kind = match wm {
                WM_MOUSEMOVE => {
                    let now = current_rec_time_us();
                    let last = LAST_MOVE_US.load(Ordering::Relaxed);

                    if last == 0 || now.saturating_sub(last) >= 5000 {
                        LAST_MOVE_US.store(now, Ordering::Relaxed);
                        Some(InputEventKind::MouseMove { x, y, dx, dy })
                    } else {
                        None
                    }
                }

                WM_LBUTTONDOWN => Some(InputEventKind::MouseButton {
                    button: MouseButton::Left,
                    down: true,
                    x,
                    y,
                }),
                WM_LBUTTONUP => Some(InputEventKind::MouseButton {
                    button: MouseButton::Left,
                    down: false,
                    x,
                    y,
                }),
                WM_RBUTTONDOWN => Some(InputEventKind::MouseButton {
                    button: MouseButton::Right,
                    down: true,
                    x,
                    y,
                }),
                WM_RBUTTONUP => Some(InputEventKind::MouseButton {
                    button: MouseButton::Right,
                    down: false,
                    x,
                    y,
                }),
                WM_MBUTTONDOWN => Some(InputEventKind::MouseButton {
                    button: MouseButton::Middle,
                    down: true,
                    x,
                    y,
                }),
                WM_MBUTTONUP => Some(InputEventKind::MouseButton {
                    button: MouseButton::Middle,
                    down: false,
                    x,
                    y,
                }),

                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    let xbutton = ((data.mouseData >> 16) & 0xFFFF) as u16;
                    let button = if xbutton == 1 {
                        MouseButton::X1
                    } else {
                        MouseButton::X2
                    };
                    Some(InputEventKind::MouseButton {
                        button,
                        down: wm == WM_XBUTTONDOWN,
                        x,
                        y,
                    })
                }

                WM_MOUSEWHEEL => {
                    let delta = ((data.mouseData >> 16) & 0xFFFF) as i16 as i32;
                    Some(InputEventKind::MouseWheel {
                        delta,
                        x,
                        y,
                        horizontal: false,
                    })
                }

                WM_MOUSEHWHEEL => {
                    let delta = ((data.mouseData >> 16) & 0xFFFF) as i16 as i32;
                    Some(InputEventKind::MouseWheel {
                        delta,
                        x,
                        y,
                        horizontal: true,
                    })
                }

                _ => None,
            };

            if let Some(k) = kind {
                emit_event(k);
            }
        }
    }

    CallNextHookEx(None, code, wparam, lparam)
}

// ============================================================
// Collector / Playback
// ============================================================

fn collector_thread(rx: Receiver<MacroEvent>) {
    while let Ok(event) = rx.recv() {
        if RECORDING.load(Ordering::Relaxed) {
            if let Ok(mut macro_data) = MACRO.lock() {
                macro_data.push(event);
            }
        }
    }
}

fn playback_loop(events: &[MacroEvent]) {
    if events.is_empty() {
        return;
    }

    let speed = SPEED.lock().map(|s| *s).unwrap_or(1.0).clamp(0.05, 10.0);
    let last_event_time = events.last().map(|e| e.t_us).unwrap_or(0);
    let loop_gap_us: u64 = 10_000;
    let cycle_us = ((last_event_time + loop_gap_us) as f64 / speed) as u64;

    let loop_play = LOOP_PLAY.load(Ordering::Relaxed);
    let max_count = if loop_play {
        u64::MAX
    } else {
        PLAY_COUNT_LIMIT.load(Ordering::Relaxed).max(1)
    };

    let start = Instant::now();
    let mut cycle_start_us: u64 = 0;
    let mut index: usize = 0;
    let mut play_count: u64 = 0;

    while !STOP_PLAY.load(Ordering::Relaxed) {
        if index >= events.len() {
            play_count += 1;
            PLAY_COUNT.store(play_count, Ordering::Relaxed);

            if play_count >= max_count {
                break;
            }

            cycle_start_us = cycle_start_us.saturating_add(cycle_us);
            index = 0;
            continue;
        }

        let event = &events[index];
        let due_us = cycle_start_us + ((event.t_us as f64 / speed) as u64);
        let now_us = start.elapsed().as_micros() as u64;

        if due_us > now_us {
            spin_sleep::sleep(Duration::from_micros(due_us - now_us));
            continue;
        }

        unsafe {
            send_input_event(&event.kind);
        }

        index += 1;
    }
}

// ============================================================
// SendInput
// ============================================================

unsafe fn send_one_input(input: INPUT) {
    let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
}

unsafe fn make_keyboard_input(vk: u16, scan: u16, down: bool, extended: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);

    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }

    if scan != 0 {
        flags |= KEYEVENTF_SCANCODE;
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    } else {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
}

unsafe fn make_mouse_input(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

unsafe fn send_absolute_mouse_move(x: i32, y: i32) {
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);

    let normalized_x = ((x - vx) as f64 / vw as f64 * 65535.0).round() as i32;
    let normalized_y = ((y - vy) as f64 / vh as f64 * 65535.0).round() as i32;

    let flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    send_one_input(make_mouse_input(flags, normalized_x, normalized_y, 0));
}

fn mouse_button_flags_data(button: MouseButton, down: bool) -> (MOUSE_EVENT_FLAGS, u32) {
    match (button, down) {
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
    }
}

unsafe fn send_input_event(kind: &InputEventKind) {
    match kind {
        InputEventKind::Key {
            vk,
            scan,
            down,
            extended,
        } => {
            send_one_input(make_keyboard_input(*vk, *scan, *down, *extended));
        }
        InputEventKind::MouseMove { x, y, dx, dy } => {
            if ABSOLUTE_MOUSE.load(Ordering::Relaxed) {
                send_absolute_mouse_move(*x, *y);
            } else {
                send_one_input(make_mouse_input(MOUSEEVENTF_MOVE, *dx, *dy, 0));
            }
        }
        InputEventKind::MouseButton {
            button,
            down,
            x,
            y,
        } => {
            if ABSOLUTE_MOUSE.load(Ordering::Relaxed) {
                send_absolute_mouse_move(*x, *y);
            }
            let (flags, data) = mouse_button_flags_data(*button, *down);
            send_one_input(make_mouse_input(flags, 0, 0, data));
        }
        InputEventKind::MouseWheel {
            delta,
            horizontal,
            ..
        } => {
            let flags = if *horizontal {
                MOUSEEVENTF_HWHEEL
            } else {
                MOUSEEVENTF_WHEEL
            };
            send_one_input(make_mouse_input(flags, 0, 0, *delta as u32));
        }
    }
}

// ============================================================
// File IO
// ============================================================

fn save_macro(path: &str) -> anyhow::Result<()> {
    let macro_data = MACRO.lock().map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let json = serde_json::to_string_pretty(&*macro_data)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn load_macro(path: &str) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let events: Vec<MacroEvent> = serde_json::from_str(&text)?;

    let duration_us = events.last().map(|e| e.t_us).unwrap_or(0);
    RECORDED_TIME_US.store(duration_us, Ordering::Relaxed);

    let mut macro_data = MACRO.lock().map_err(|e| anyhow::anyhow!(e.to_string()))?;
    *macro_data = events;
    Ok(())
}

// ============================================================
// GUI
// ============================================================

enum Status {
    Ready,
    Saved,
    Loaded,
    SaveErr(String),
    LoadErr(String),
}

impl Status {
    fn text(&self, s: &Strings) -> String {
        match self {
            Status::Ready => s.status_ready.to_string(),
            Status::Saved => s.saved.to_string(),
            Status::Loaded => s.loaded.to_string(),
            Status::SaveErr(e) => s.save_err.replace("{}", e),
            Status::LoadErr(e) => s.load_err.replace("{}", e),
        }
    }
}

struct MacroApp {
    status: Status,
    loop_play: bool,
    speed: f64,
    absolute_mouse: bool,
    always_on_top: bool,
    play_count_limit: u64,
    lang_mode: usize, // 0 = auto, 1 = en, 2 = ru
    theme_idx: usize,
}

impl Default for MacroApp {
    fn default() -> Self {
        Self {
            status: Status::Ready,
            loop_play: true,
            speed: 1.0,
            absolute_mouse: true,
            always_on_top: true,
            play_count_limit: 1,
            lang_mode: 0,
            theme_idx: 0,
        }
    }
}

impl MacroApp {
    fn lang(&self) -> Lang {
        match self.lang_mode {
            1 => Lang::En,
            2 => Lang::Ru,
            _ => *SYSTEM_LANG.get().unwrap_or(&Lang::En),
        }
    }

    fn strs(&self) -> &'static Strings {
        match self.lang() {
            Lang::En => &EN,
            Lang::Ru => &RU,
        }
    }
}

impl eframe::App for MacroApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let s = self.strs();
        let recording = RECORDING.load(Ordering::Relaxed);
        let playing = PLAYING.load(Ordering::Relaxed);

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Macro Recorder");
                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button(s.record).clicked() {
                        toggle_recording();
                    }
                    if ui.button(s.stop_rec).clicked() {
                        stop_recording();
                    }
                });

                ui.horizontal(|ui| {
                    if ui.button(s.play).clicked() {
                        toggle_playback();
                    }
                    if ui.button(s.stop_play).clicked() {
                        stop_playback();
                    }
                });

                if recording {
                    ui.label(s.rec_time.replace("{}", &format_duration_us(current_rec_time_us())));
                } else {
                    let recorded_us = RECORDED_TIME_US.load(Ordering::Relaxed);
                    if recorded_us > 0 {
                        ui.label(s.rec_done.replace("{}", &format_duration_us(recorded_us)));
                    }
                }

                if playing || PLAY_COUNT.load(Ordering::Relaxed) > 0 {
                    let count = PLAY_COUNT.load(Ordering::Relaxed);
                    if LOOP_PLAY.load(Ordering::Relaxed) {
                        ui.label(s.play_inf.replace("{}", &count.to_string()));
                    } else {
                        let limit = PLAY_COUNT_LIMIT.load(Ordering::Relaxed);
                        let line = s
                            .play_lim
                            .replacen("{}", &count.to_string(), 1)
                            .replacen("{}", &limit.to_string(), 1);
                        ui.label(line);
                    }
                }

                ui.separator();

                ui.checkbox(&mut self.loop_play, s.loop_cb);
                LOOP_PLAY.store(self.loop_play, Ordering::Relaxed);

                if !self.loop_play {
                    ui.horizontal(|ui| {
                        ui.label(s.play_count);
                        ui.add(
                            egui::DragValue::new(&mut self.play_count_limit)
                                .range(1..=1000)
                                .speed(1),
                        );
                    });
                    PLAY_COUNT_LIMIT.store(self.play_count_limit, Ordering::Relaxed);
                }

                ui.add(egui::Slider::new(&mut self.speed, 0.1..=3.0).text(s.speed));
                if let Ok(mut speed) = SPEED.lock() {
                    *speed = self.speed;
                }

                ui.checkbox(&mut self.absolute_mouse, s.abs_mouse);
                ABSOLUTE_MOUSE.store(self.absolute_mouse, Ordering::Relaxed);

                if ui.checkbox(&mut self.always_on_top, s.on_top).changed() {
                    let level = if self.always_on_top {
                        egui::viewport::WindowLevel::AlwaysOnTop
                    } else {
                        egui::viewport::WindowLevel::Normal
                    };
                    ui.ctx()
                        .send_viewport_cmd(egui::viewport::ViewportCommand::WindowLevel(level));
                }

                ui.separator();

                ui.horizontal(|ui| {
                    ui.label(s.theme);
                    egui::ComboBox::from_id_salt("theme_sel")
                        .selected_text(THEME_NAMES[self.theme_idx])
                        .show_ui(ui, |ui| {
                            for (i, name) in THEME_NAMES.iter().enumerate() {
                                if ui.selectable_label(self.theme_idx == i, *name).clicked() {
                                    self.theme_idx = i;
                                    let themes = [
                                        Theme::Dark,
                                        Theme::Material3,
                                        Theme::Fluent,
                                        Theme::Catppuccin,
                                        Theme::Nord,
                                        Theme::Dracula,
                                        Theme::Glass,
                                        Theme::Neumorphism,
                                    ];
                                    apply_theme(ui.ctx(), themes[i]);
                                }
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label(s.language);
                    egui::ComboBox::from_id_salt("lang_sel")
                        .selected_text(match self.lang_mode {
                            1 => "English",
                            2 => "Русский",
                            _ => s.lang_auto,
                        })
                        .show_ui(ui, |ui| {
                            let opts = [s.lang_auto, "English", "Русский"];
                            for (i, name) in opts.iter().enumerate() {
                                if ui.selectable_label(self.lang_mode == i, *name).clicked() {
                                    self.lang_mode = i;
                                }
                            }
                        });
                });

                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button(s.save).clicked() {
                        match save_macro("macro.json") {
                            Ok(_) => self.status = Status::Saved,
                            Err(e) => self.status = Status::SaveErr(e.to_string()),
                        }
                    }
                    if ui.button(s.load).clicked() {
                        match load_macro("macro.json") {
                            Ok(_) => self.status = Status::Loaded,
                            Err(e) => self.status = Status::LoadErr(e.to_string()),
                        }
                    }
                });

                ui.separator();

                let event_count = MACRO.lock().map(|m| m.len()).unwrap_or(0);

                ui.label(s.events.replace("{}", &event_count.to_string()));
                let status_text = if recording {
                    s.status_rec.to_string()
                } else if playing {
                    s.status_play.to_string()
                } else {
                    self.status.text(s)
                };
                ui.label(format!("ℹ {status_text}"));

                ui.ctx().request_repaint_after(Duration::from_millis(100));
            });
        });
    }
}

// ============================================================
// Main / Init
// ============================================================

fn init() {
    EPOCH.set(Instant::now()).ok();
    SYSTEM_LANG.set(detect_system_lang()).ok();

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = timeBeginPeriod(1);
    }

    let (tx, rx) = unbounded();
    INPUT_TX.set(tx).ok();

    std::thread::spawn(move || collector_thread(rx));
    std::thread::spawn(input_hook_thread);
}

fn main() -> anyhow::Result<()> {
    init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 480.0])
            .with_always_on_top()
            .with_transparent(true),
        ..Default::default()
    };

    let result = eframe::run_native(
        "Macro Recorder",
        options,
        Box::new(|cc| {
            apply_theme(&cc.egui_ctx, Theme::Dark);
            Ok(Box::new(MacroApp::default()))
        }),
    );

    unsafe {
        let _ = timeEndPeriod(1);
    }

    result.map_err(|e| anyhow::anyhow!(e.to_string()))
}