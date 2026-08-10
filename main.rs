#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tracing::info;

#[cfg(windows)]
mod win32 {
    pub use windows::core::w;
    pub use windows::Win32::Foundation::*;
    pub use windows::Win32::Graphics::Dwm::*;
    pub use windows::Win32::Graphics::Gdi::HRGN;
    pub use windows::Win32::Media::*;
    pub use windows::Win32::System::LibraryLoader::*;
    pub use windows::Win32::System::Registry::*;
    pub use windows::Win32::UI::HiDpi::*;
    pub use windows::Win32::UI::Input::KeyboardAndMouse::*;
    pub use windows::Win32::UI::WindowsAndMessaging::*;
    pub use windows::Win32::Globalization::GetUserDefaultUILanguage;
}

// ============================================================================
// Configuration & Persistence
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug)]
struct AppConfig {
    default_lang: usize,
    default_theme: usize,
    transparent_ui: bool,
    time_limit_enabled: bool,
    time_limit_h: u64,
    time_limit_m: u64,
    time_limit_s: u64,
    action_on_completion: usize,
    loop_play: bool,
    play_count_limit: u64,
    speed: f64,
    absolute_mouse: bool,
    always_on_top: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_lang: 0,
            default_theme: 0,
            transparent_ui: true,
            time_limit_enabled: false,
            time_limit_h: 0,
            time_limit_m: 0,
            time_limit_s: 0,
            action_on_completion: 0,
            loop_play: true,
            play_count_limit: 1,
            speed: 1.0,
            absolute_mouse: true,
            always_on_top: true,
        }
    }
}

const CONFIG_PATH: &str = "config.json";
const MACRO_PATH: &str = "macro.json";

fn load_config() -> AppConfig {
    std::fs::read_to_string(CONFIG_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &AppConfig) -> Result<()> {
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(CONFIG_PATH, json)?;
    Ok(())
}

// ============================================================================
// Macro Event Model
// ============================================================================

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum MouseButton { Left, Right, Middle, X1, X2 }

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

// ============================================================================
// Shared Application State
// ============================================================================

struct AppState {
    recording: AtomicBool,
    playing: AtomicBool,
    stop_play: AtomicBool,
    loop_play: AtomicBool,
    absolute_mouse: AtomicBool,
    rec_start_us: AtomicU64,
    last_move_us: AtomicU64,
    recorded_time_us: AtomicU64,
    play_count: AtomicU64,
    play_count_limit: AtomicU64,
    time_limit_enabled: AtomicBool,
    time_limit_us: AtomicU64,
    action_on_completion: AtomicU64,
    speed: Mutex<f64>,
    last_x: Mutex<i32>,
    last_y: Mutex<i32>,
    macro_data: Mutex<Vec<MacroEvent>>,
    event_tx: Option<Sender<MacroEvent>>,
}

impl AppState {
    fn new(tx: Sender<MacroEvent>) -> Arc<Self> {
        Arc::new(Self {
            recording: AtomicBool::new(false),
            playing: AtomicBool::new(false),
            stop_play: AtomicBool::new(false),
            loop_play: AtomicBool::new(true),
            absolute_mouse: AtomicBool::new(true),
            rec_start_us: AtomicU64::new(0),
            last_move_us: AtomicU64::new(0),
            recorded_time_us: AtomicU64::new(0),
            play_count: AtomicU64::new(0),
            play_count_limit: AtomicU64::new(1),
            time_limit_enabled: AtomicBool::new(false),
            time_limit_us: AtomicU64::new(0),
            action_on_completion: AtomicU64::new(0),
            speed: Mutex::new(1.0),
            last_x: Mutex::new(i32::MIN),
            last_y: Mutex::new(i32::MIN),
            macro_data: Mutex::new(Vec::new()),
            event_tx: Some(tx),
        })
    }
}

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn init_epoch() { EPOCH.set(Instant::now()).ok(); }

fn now_us() -> u64 {
    EPOCH.get().map(|e| e.elapsed().as_micros() as u64).unwrap_or(0)
}

fn current_rec_time_us(state: &AppState) -> u64 {
    let start = state.rec_start_us.load(Ordering::Relaxed);
    now_us().saturating_sub(start)
}

// ============================================================================
// Modern Windows Backdrop & DPI Management
// ============================================================================

#[cfg(windows)]
mod platform {
    use super::win32::*;
    use std::ffi::c_void;

    pub fn apply_system_backdrop(hwnd: HWND, use_acrylic: bool) {
        unsafe {
            let dark_mode: i32 = 1;
            let _ = DwmSetWindowAttribute(
                hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark_mode as *const i32 as *const c_void, std::mem::size_of::<i32>() as u32,
            );

            let backdrop_type: i32 = if use_acrylic { 3 } else { 2 };
            let result = DwmSetWindowAttribute(
                hwnd, DWMWINDOWATTRIBUTE(38), // DWMWA_SYSTEMBACKDROP_TYPE
                &backdrop_type as *const i32 as *const c_void, std::mem::size_of::<i32>() as u32,
            );

            if result.is_err() {
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

    pub unsafe fn send_absolute_mouse_move(x: i32, y: i32) {
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);

            let nx = ((x - vx) as f64 / vw as f64 * 65535.0).round() as i32;
            let ny = ((y - vy) as f64 / vh as f64 * 65535.0).round() as i32;

            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: nx, dy: ny, mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        time: 0, dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    pub fn begin_high_res_timer() { unsafe { let _ = timeBeginPeriod(1); } }
    pub fn end_high_res_timer() { unsafe { let _ = timeEndPeriod(1); } }

    pub fn initiate_system_shutdown(reason: &str) -> anyhow::Result<()> {
        std::process::Command::new("shutdown")
            .args(["/s", "/t", "60", "/c", reason])
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn shutdown command: {}", e))?;
        Ok(())
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn apply_system_backdrop(_: (), _: bool) {}
    pub unsafe fn send_absolute_mouse_move(_: i32, _: i32) {}
    pub fn begin_high_res_timer() {}
    pub fn end_high_res_timer() {}
    pub fn initiate_system_shutdown(_: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("Shutdown not supported on this platform"))
    }
}

// ============================================================================
// High-Precision Playback Engine
// ============================================================================

fn playback_loop(state: Arc<AppState>, events: Vec<MacroEvent>) {
    if events.is_empty() { return; }

    platform::begin_high_res_timer();

    let speed = *state.speed.lock();
    let speed = speed.clamp(0.05, 10.0);
    let last_t = events.last().map(|e| e.t_us).unwrap_or(0);
    let cycle_us = ((last_t + 10_000) as f64 / speed) as u64;

    let loop_play = state.loop_play.load(Ordering::Relaxed);
    let max_count = if loop_play { u64::MAX } else { state.play_count_limit.load(Ordering::Relaxed).max(1) };

    let start = Instant::now();
    let mut cycle_start_us: u64 = 0;
    let mut index: usize = 0;
    let mut count: u64 = 0;

    while !state.stop_play.load(Ordering::Relaxed) {
        if state.time_limit_enabled.load(Ordering::Relaxed) {
            let limit = state.time_limit_us.load(Ordering::Relaxed);
            if start.elapsed().as_micros() as u64 >= limit {
                let action = state.action_on_completion.load(Ordering::Relaxed);
                if action == 1 {
                    let _ = platform::initiate_system_shutdown("Macro Recorder: Time limit reached.");
                }
                break;
            }
        }

        if index >= events.len() {
            count += 1;
            state.play_count.store(count, Ordering::Relaxed);
            if count >= max_count { break; }
            cycle_start_us = cycle_start_us.saturating_add(cycle_us);
            index = 0;
            continue;
        }

        let ev = &events[index];
        let due = cycle_start_us + ((ev.t_us as f64 / speed) as u64);
        let now = start.elapsed().as_micros() as u64;

        if due > now {
            let remaining = due - now;
            if remaining > 2000 {
                std::thread::sleep(Duration::from_micros(remaining - 1000));
            } else {
                spin_sleep::sleep(Duration::from_micros(remaining));
            }
            continue;
        }

        #[cfg(windows)]
        unsafe { send_input_event(&ev.kind, &state); }

        index += 1;
    }

    state.playing.store(false, Ordering::Relaxed);
    platform::end_high_res_timer();
}

#[cfg(windows)]
unsafe fn send_input_event(kind: &InputEventKind, state: &AppState) {
    use win32::*;
    unsafe {
        match kind {
            InputEventKind::Key { vk, scan, down, extended } => {
                let mut flags = KEYBD_EVENT_FLAGS(0);
                if !down { flags |= KEYEVENTF_KEYUP; }
                if *extended { flags |= KEYEVENTF_EXTENDEDKEY; }
                let ki = if *scan != 0 {
                    flags |= KEYEVENTF_SCANCODE;
                    KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: *scan, dwFlags: flags, time: 0, dwExtraInfo: 0 }
                } else {
                    KEYBDINPUT { wVk: VIRTUAL_KEY(*vk), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 }
                };
                let input = INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
            InputEventKind::MouseMove { x, y, dx, dy } => {
                if state.absolute_mouse.load(Ordering::Relaxed) {
                    platform::send_absolute_mouse_move(*x, *y);
                } else {
                    let input = INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: *dx, dy: *dy, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } },
                    };
                    SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                }
            }
            InputEventKind::MouseButton { button, down, x, y } => {
                if state.absolute_mouse.load(Ordering::Relaxed) {
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
                    Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
                };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
            InputEventKind::MouseWheel { delta, horizontal, .. } => {
                let flags = if *horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: *delta as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
                };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
        }
    }
}

// ============================================================================
// Input Hook Thread (with panic safety)
// ============================================================================

#[cfg(windows)]
fn input_hook_thread(state: Arc<AppState>) {
    use win32::*;

    unsafe {
        let hmod = GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default();

        let kb_hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), Some(hmod), 0);
        let ms_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(ms_proc), Some(hmod), 0);

        let _ = RegisterHotKey(None, 1, MOD_NOREPEAT, 0x77); // F8
        let _ = RegisterHotKey(None, 2, MOD_NOREPEAT, 0x78); // F9

        HOOK_STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == 0x0312 { // WM_HOTKEY
                match msg.wParam.0 as i32 {
                    1 => toggle_recording(&state),
                    2 => toggle_playback(&state),
                    _ => {}
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }

        let _ = UnregisterHotKey(None, 1);
        let _ = UnregisterHotKey(None, 2);
        if let Ok(h) = kb_hook { let _ = UnhookWindowsHookEx(h); }
        if let Ok(h) = ms_hook { let _ = UnhookWindowsHookEx(h); }
    }

    #[cfg(windows)]
    unsafe extern "system" fn kb_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        std::panic::catch_unwind(|| {
            if code == 0 {
                HOOK_STATE.with(|s| {
                    if let Some(ref state) = *s.borrow() {
                        if state.recording.load(Ordering::Relaxed) {
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
                                        emit_event(state, InputEventKind::Key {
                                            vk: data.vkCode as u16,
                                            scan: data.scanCode as u16,
                                            down,
                                            extended: data.flags.0 & LLKHF_EXTENDED.0 != 0,
                                        });
                                    }
                                }
                            }
                        }
                    }
                });
            }
        }).ok();
        unsafe { CallNextHookEx(None, code, wp, lp) }
    }

    #[cfg(windows)]
    unsafe extern "system" fn ms_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        std::panic::catch_unwind(|| {
            if code == 0 {
                HOOK_STATE.with(|s| {
                    if let Some(ref state) = *s.borrow() {
                        if state.recording.load(Ordering::Relaxed) {
                            unsafe {
                                let data = &*(lp.0 as *const MSLLHOOKSTRUCT);
                                if data.flags & LLMHF_INJECTED == 0 {
                                    let x = data.pt.x;
                                    let y = data.pt.y;
                                    let wm = wp.0 as u32;

                                    let kind = match wm {
                                        0x0200 => { // WM_MOUSEMOVE
                                            let now = current_rec_time_us(state);
                                            let last = state.last_move_us.load(Ordering::Relaxed);
                                            if last == 0 || now.saturating_sub(last) >= 5000 {
                                                state.last_move_us.store(now, Ordering::Relaxed);
                                                let mut lx = state.last_x.lock();
                                                let mut ly = state.last_y.lock();
                                                let (dx, dy) = if *lx == i32::MIN { (0, 0) } else { (x - *lx, y - *ly) };
                                                *lx = x; *ly = y;
                                                Some(InputEventKind::MouseMove { x, y, dx, dy })
                                            } else { None }
                                        }
                                        0x0201 => Some(InputEventKind::MouseButton { button: MouseButton::Left, down: true, x, y }),
                                        0x0202 => Some(InputEventKind::MouseButton { button: MouseButton::Left, down: false, x, y }),
                                        0x0204 => Some(InputEventKind::MouseButton { button: MouseButton::Right, down: true, x, y }),
                                        0x0205 => Some(InputEventKind::MouseButton { button: MouseButton::Right, down: false, x, y }),
                                        0x0207 => Some(InputEventKind::MouseButton { button: MouseButton::Middle, down: true, x, y }),
                                        0x0208 => Some(InputEventKind::MouseButton { button: MouseButton::Middle, down: false, x, y }),
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

                                    if let Some(k) = kind { emit_event(state, k); }
                                }
                            }
                        }
                    }
                });
            }
        }).ok();
        unsafe { CallNextHookEx(None, code, wp, lp) }
    }
}

#[cfg(windows)]
std::thread_local! {
    static HOOK_STATE: std::cell::RefCell<Option<Arc<AppState>>> = const { std::cell::RefCell::new(None) };
}

fn emit_event(state: &AppState, kind: InputEventKind) {
    let t_us = current_rec_time_us(state);
    let event = MacroEvent { t_us, kind };
    if let Some(ref tx) = state.event_tx {
        let _ = tx.send(event);
    }
}

fn toggle_recording(state: &Arc<AppState>) {
    if state.recording.load(Ordering::Relaxed) {
        state.recorded_time_us.store(current_rec_time_us(state), Ordering::Relaxed);
        state.recording.store(false, Ordering::Relaxed);
        info!("Recording stopped");
    } else {
        if state.playing.load(Ordering::Relaxed) { return; }
        state.macro_data.lock().clear();
        *state.last_x.lock() = i32::MIN;
        *state.last_y.lock() = i32::MIN;
        state.last_move_us.store(0, Ordering::Relaxed);
        state.rec_start_us.store(now_us(), Ordering::Relaxed);
        state.recording.store(true, Ordering::Relaxed);
        info!("Recording started");
    }
}

fn toggle_playback(state: &Arc<AppState>) {
    if state.playing.load(Ordering::Relaxed) {
        state.stop_play.store(true, Ordering::Relaxed);
        info!("Playback stop requested");
    } else {
        if state.recording.load(Ordering::Relaxed) { return; }
        let events = state.macro_data.lock().clone();
        if events.is_empty() { return; }
        state.play_count.store(0, Ordering::Relaxed);
        state.stop_play.store(false, Ordering::Relaxed);
        state.playing.store(true, Ordering::Relaxed);
        let s = state.clone();
        std::thread::spawn(move || playback_loop(s, events));
        info!("Playback started");
    }
}

fn collector_thread(rx: Receiver<MacroEvent>, state: Arc<AppState>) {
    while let Ok(event) = rx.recv() {
        if state.recording.load(Ordering::Relaxed) {
            state.macro_data.lock().push(event);
        }
    }
}

// ============================================================================
// Localization
// ============================================================================

#[derive(Clone, Copy, PartialEq)]
enum Lang { En, Ru, Uk, Pt, Es, Zh }

struct Strings {
    record: &'static str, stop_rec: &'static str, play: &'static str, stop_play: &'static str,
    rec_time: &'static str, rec_done: &'static str, play_inf: &'static str, play_lim: &'static str,
    loop_cb: &'static str, play_count: &'static str, speed: &'static str, abs_mouse: &'static str,
    on_top: &'static str, theme: &'static str, language: &'static str, lang_auto: &'static str,
    save: &'static str, load: &'static str, events: &'static str, status_ready: &'static str,
    status_rec: &'static str, status_play: &'static str, saved: &'static str, loaded: &'static str,
    save_err: &'static str, load_err: &'static str, time_limit_cb: &'static str,
    time_limit_h: &'static str, time_limit_m: &'static str, time_limit_s: &'static str, action_on_limit: &'static str,
    action_stop: &'static str, action_shutdown: &'static str, save_settings: &'static str,
    settings_saved: &'static str, transparent_ui: &'static str,
}

const EN: Strings = Strings {
    record: "🔴 Record (F8)", stop_rec: "⏹ Stop Rec", play: "▶ Play (F9)", stop_play: "⏹ Stop Play",
    rec_time: "⏱ Recording: {}…", rec_done: "⏱ Recorded: {} (done)", play_inf: "🔄 Plays: {} (∞)",
    play_lim: "🔄 Plays: {} / {}", loop_cb: "Loop playback", play_count: "Play count:",
    speed: "Speed", abs_mouse: "Absolute mouse (DPI fix)", on_top: "📌 Always on Top",
    theme: "Theme:", language: "Language:", lang_auto: "Auto (system)",
    save: "💾 Save", load: "📂 Load", events: "📦 Events: {}",
    status_ready: "Ready [F8: Record | F9: Play]", status_rec: "Recording... [F8 to stop]",
    status_play: "Playing... [F9 to stop]", saved: "Saved to macro.json", loaded: "Loaded macro.json",
    save_err: "Save error: {}", load_err: "Load error: {}",
    time_limit_cb: "⏱ Stop after time limit", time_limit_h: "Hours", time_limit_m: "Minutes", time_limit_s: "Seconds",
    action_on_limit: "Action on limit:", action_stop: "Stop playback", action_shutdown: "Shutdown system",
    save_settings: "💾 Save Settings", settings_saved: "Settings saved!", transparent_ui: "🪟 Transparent UI",
};

const RU: Strings = Strings {
    record: "🔴 Запись (F8)", stop_rec: "⏹ Стоп запись", play: "▶ Плей (F9)", stop_play: "⏹ Стоп плей",
    rec_time: "⏱ Запись: {}…", rec_done: "⏱ Записано: {} (готово)", play_inf: "🔄 Проигрываний: {} (∞)",
    play_lim: "🔄 Проигрываний: {} / {}", loop_cb: "Циклическое воспроизведение", play_count: "Проигрываний:",
    speed: "Скорость", abs_mouse: "Абсолютная мышь (фикс DPI)", on_top: "📌 Поверх всех окон",
    theme: "Тема:", language: "Язык:", lang_auto: "Авто (система)",
    save: "💾 Сохранить", load: "📂 Загрузить", events: "📦 Событий: {}",
    status_ready: "Готов [F8: запись | F9: плей]", status_rec: "Запись... [F8 — стоп]",
    status_play: "Воспроизведение... [F9 — стоп]", saved: "Сохранено в macro.json", loaded: "Загружено из macro.json",
    save_err: "Ошибка сохранения: {}", load_err: "Ошибка загрузки: {}",
    time_limit_cb: "⏱ Остановить по таймеру", time_limit_h: "Часы", time_limit_m: "Минуты", time_limit_s: "Секунды",
    action_on_limit: "Действие по таймеру:", action_stop: "Остановить", action_shutdown: "Выключить систему",
    save_settings: "💾 Сохранить настройки", settings_saved: "Настройки сохранены!", transparent_ui: "🪟 Прозрачный интерфейс",
};

fn detect_system_lang() -> Lang {
    #[cfg(windows)]
    unsafe {
        let lang = win32::GetUserDefaultUILanguage() as u32;
        match lang & 0x3FF {
            0x19 => Lang::Ru, 0x22 => Lang::Uk, 0x16 => Lang::Pt,
            0x0A => Lang::Es, 0x04 => Lang::Zh, _ => Lang::En,
        }
    }
    #[cfg(not(windows))]
    Lang::En
}

fn get_strings(lang_mode: usize, system_lang: Lang) -> &'static Strings {
    let lang = match lang_mode {
        1 => Lang::En, 2 => Lang::Ru, 3 => Lang::Uk,
        4 => Lang::Pt, 5 => Lang::Es, 6 => Lang::Zh,
        _ => system_lang,
    };
    match lang {
        Lang::Ru => &RU,
        _ => &EN,
    }
}

// ============================================================================
// Theme System
// ============================================================================

#[derive(Clone, Copy, PartialEq)]
enum Theme { Dark, OLED, Material3, Catppuccin, Nord, Dracula, Glass, Neumorphism }

const THEME_NAMES: [&str; 8] = [
    "Dark (default)", "OLED (Pure Black)", "Material Design 3", "Catppuccin Mocha",
    "Nord", "Dracula", "Glassmorphism", "Neumorphism",
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
    backdrop: i32,
}

fn rgb(r: u8, g: u8, b: u8) -> egui::Color32 { egui::Color32::from_rgb(r, g, b) }
fn rgba(r: u8, g: u8, b: u8, a: u8) -> egui::Color32 { egui::Color32::from_rgba_unmultiplied(r, g, b, a) }

#[cfg(windows)]
fn get_system_accent_color() -> Option<egui::Color32> {
    use win32::*;
    unsafe {
        let mut key = HKEY::default();
        let path = windows::core::w!("Software\\Microsoft\\Windows\\DWM");
        // FIX: windows 0.62 changed ulOptions to Option<u32>
        let res = RegOpenKeyExW(HKEY_CURRENT_USER, path, Some(0), KEY_READ, &mut key);
        if res.is_err() { return None; }

        let mut data: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let value_name = windows::core::w!("AccentColor");
        let res = RegQueryValueExW(
            key, value_name, None, None,
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);

        if res.is_ok() {
            // Windows stores DWORD colors in 0xAABBGGRR format
            let r = (data & 0xFF) as u8;
            let g = ((data >> 8) & 0xFF) as u8;
            let b = ((data >> 16) & 0xFF) as u8;
            return Some(egui::Color32::from_rgb(r, g, b));
        }
        None
    }
}

#[cfg(not(windows))]
fn get_system_accent_color() -> Option<egui::Color32> { None }

fn get_palette(theme: Theme) -> Palette {
    match theme {
        Theme::Dark => Palette {
            dark: true, bg: rgb(16, 16, 16), panel: rgb(24, 24, 24), widget: rgb(42, 42, 42), widget_hover: rgb(58, 58, 58), widget_active: rgb(75, 75, 75), active_fg: rgb(255, 255, 255), border: rgb(70, 70, 70), hover_border: rgb(95, 95, 95), text: rgb(230, 230, 230), faint: rgb(130, 130, 130), accent: rgb(70, 130, 255), accent_text: rgb(255, 255, 255), widget_round: 4.0, shadow_blur: 4, shadow_offset: 1, shadow_alpha: 60, item_spacing_y: 5.0, button_padding: 3.0, animation_time: 0.15, backdrop: 1,
        },
        Theme::OLED => Palette {
            dark: true, bg: rgb(0, 0, 0), panel: rgb(0, 0, 0), widget: rgb(20, 20, 20), widget_hover: rgb(35, 35, 35), widget_active: rgb(50, 50, 50), active_fg: rgb(255, 255, 255), border: rgb(40, 40, 40), hover_border: rgb(80, 80, 80), text: rgb(240, 240, 240), faint: rgb(120, 120, 120), accent: rgb(0, 122, 204), accent_text: rgb(255, 255, 255), widget_round: 2.0, shadow_blur: 0, shadow_offset: 0, shadow_alpha: 0, item_spacing_y: 5.0, button_padding: 3.0, animation_time: 0.1, backdrop: 1,
        },
        Theme::Material3 => {
            let sys_accent = get_system_accent_color().unwrap_or_else(|| rgb(208, 188, 255));
            Palette {
                dark: true, bg: rgb(18, 17, 24), panel: rgb(18, 17, 24), widget: rgb(56, 48, 75), widget_hover: rgb(70, 60, 92), widget_active: sys_accent, active_fg: rgb(255, 255, 255), border: rgb(73, 69, 82), hover_border: sys_accent, text: rgb(230, 224, 233), faint: rgb(147, 143, 153), accent: sys_accent, accent_text: rgb(255, 255, 255), widget_round: 20.0, shadow_blur: 8, shadow_offset: 2, shadow_alpha: 80, item_spacing_y: 7.0, button_padding: 6.0, animation_time: 0.4, backdrop: 1,
            }
        },
        Theme::Catppuccin => Palette {
            dark: true, bg: rgb(17, 17, 27), panel: rgb(30, 30, 46), widget: rgb(49, 50, 68), widget_hover: rgb(69, 71, 90), widget_active: rgb(203, 166, 247), active_fg: rgb(17, 17, 27), border: rgb(88, 91, 112), hover_border: rgb(203, 166, 247), text: rgb(205, 214, 244), faint: rgb(166, 172, 200), accent: rgb(203, 166, 247), accent_text: rgb(17, 17, 27), widget_round: 10.0, shadow_blur: 6, shadow_offset: 2, shadow_alpha: 90, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.25, backdrop: 1,
        },
        Theme::Nord => Palette {
            dark: true, bg: rgb(46, 52, 64), panel: rgb(46, 52, 64), widget: rgb(59, 66, 82), widget_hover: rgb(67, 76, 94), widget_active: rgb(136, 192, 208), active_fg: rgb(46, 52, 64), border: rgb(76, 86, 106), hover_border: rgb(136, 192, 208), text: rgb(216, 222, 233), faint: rgb(148, 155, 168), accent: rgb(136, 192, 208), accent_text: rgb(46, 52, 64), widget_round: 6.0, shadow_blur: 5, shadow_offset: 1, shadow_alpha: 80, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.2, backdrop: 1,
        },
        Theme::Dracula => Palette {
            dark: true, bg: rgb(40, 42, 54), panel: rgb(40, 42, 54), widget: rgb(68, 71, 90), widget_hover: rgb(80, 83, 105), widget_active: rgb(255, 121, 198), active_fg: rgb(40, 42, 54), border: rgb(98, 114, 164), hover_border: rgb(255, 121, 198), text: rgb(248, 248, 242), faint: rgb(135, 140, 160), accent: rgb(255, 121, 198), accent_text: rgb(40, 42, 54), widget_round: 8.0, shadow_blur: 6, shadow_offset: 2, shadow_alpha: 90, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.25, backdrop: 1,
        },
        Theme::Glass => Palette {
            dark: true, bg: rgb(24, 28, 40), panel: rgba(40, 46, 64, 110), widget: rgba(255, 255, 255, 45), widget_hover: rgba(255, 255, 255, 75), widget_active: rgba(120, 180, 255, 200), active_fg: rgb(255, 255, 255), border: rgba(255, 255, 255, 110), hover_border: rgba(255, 255, 255, 170), text: rgb(240, 245, 255), faint: rgb(190, 200, 220), accent: rgb(120, 180, 255), accent_text: rgb(255, 255, 255), widget_round: 14.0, shadow_blur: 12, shadow_offset: 3, shadow_alpha: 100, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.3, backdrop: 3,
        },
        Theme::Neumorphism => Palette {
            dark: false, bg: rgb(224, 229, 236), panel: rgb(224, 229, 236), widget: rgb(224, 229, 236), widget_hover: rgb(231, 236, 243), widget_active: rgb(93, 120, 255), active_fg: rgb(255, 255, 255), border: rgb(224, 229, 236), hover_border: rgb(224, 229, 236), text: rgb(60, 70, 90), faint: rgb(120, 130, 150), accent: rgb(93, 120, 255), accent_text: rgb(255, 255, 255), widget_round: 12.0, shadow_blur: 10, shadow_offset: 5, shadow_alpha: 110, item_spacing_y: 6.0, button_padding: 5.0, animation_time: 0.25, backdrop: 1,
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

fn apply_theme(ctx: &egui::Context, theme: Theme, transparent_ui: bool) {
    let p = get_palette(theme);
    
    let mut visuals = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.window_fill = p.panel;
    visuals.panel_fill = p.panel;
    visuals.extreme_bg_color = p.bg;
    visuals.window_shadow = make_shadow(&p);
    visuals.popup_shadow = make_shadow(&p);
    visuals.hyperlink_color = p.accent;
    visuals.selection.bg_fill = p.accent;
    visuals.selection.stroke = egui::Stroke::new(1.0, p.accent_text);

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
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, p.hover_border);
    visuals.widgets.active.bg_fill = p.widget_active;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, p.active_fg);

    let is_transparent_theme = matches!(theme, Theme::Glass);
    if transparent_ui || is_transparent_theme {
        visuals.window_fill = egui::Color32::TRANSPARENT;
        visuals.panel_fill = egui::Color32::from_rgba_unmultiplied(30, 30, 30, 140);
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

    #[cfg(windows)]
    if (is_transparent_theme || transparent_ui) && p.backdrop > 1 {
        let hwnd_res = unsafe { win32::FindWindowW(None, win32::w!("Macro Recorder")) };
        if let Ok(hwnd) = hwnd_res {
            platform::apply_system_backdrop(hwnd, p.backdrop == 3);
        }
    }
}

// ============================================================================
// Main Application
// ============================================================================

struct MacroApp {
    state: Arc<AppState>,
    config: AppConfig,
    system_lang: Lang,
    status_msg: String,
}

impl MacroApp {
    fn new(cc: &eframe::CreationContext<'_>, state: Arc<AppState>, config: AppConfig) -> Self {
        setup_fonts(&cc.egui_ctx);
        let theme = [Theme::Dark, Theme::OLED, Theme::Material3, Theme::Catppuccin,
                     Theme::Nord, Theme::Dracula, Theme::Glass, Theme::Neumorphism]
            .get(config.default_theme).copied().unwrap_or(Theme::Dark);
        apply_theme(&cc.egui_ctx, theme, config.transparent_ui);

        Self {
            state,
            config,
            system_lang: detect_system_lang(),
            status_msg: String::new(),
        }
    }

    fn strs(&self) -> &'static Strings {
        get_strings(self.config.default_lang, self.system_lang)
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
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) { f.push("cjk".into()); }
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Monospace) { f.push("cjk".into()); }
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

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Macro Recorder");
                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button(s.record).clicked() { toggle_recording(&self.state); }
                    if ui.button(s.stop_rec).clicked() { self.state.recording.store(false, Ordering::Relaxed); }
                });
                ui.horizontal(|ui| {
                    if ui.button(s.play).clicked() { toggle_playback(&self.state); }
                    if ui.button(s.stop_play).clicked() { self.state.stop_play.store(true, Ordering::Relaxed); }
                });

                if recording {
                    ui.label(s.rec_time.replace("{}", &format_us(current_rec_time_us(&self.state))));
                } else {
                    let rt = self.state.recorded_time_us.load(Ordering::Relaxed);
                    if rt > 0 { ui.label(s.rec_done.replace("{}", &format_us(rt))); }
                }

                if playing || self.state.play_count.load(Ordering::Relaxed) > 0 {
                    let c = self.state.play_count.load(Ordering::Relaxed);
                    if self.state.loop_play.load(Ordering::Relaxed) {
                        ui.label(s.play_inf.replace("{}", &c.to_string()));
                    } else {
                        let l = self.state.play_count_limit.load(Ordering::Relaxed);
                        ui.label(s.play_lim.replacen("{}", &c.to_string(), 1).replacen("{}", &l.to_string(), 1));
                    }
                }

                ui.separator();
                ui.heading("⚙ Settings");

                let mut lp = self.config.loop_play;
                if ui.checkbox(&mut lp, s.loop_cb).changed() {
                    self.config.loop_play = lp;
                    self.state.loop_play.store(lp, Ordering::Relaxed);
                }

                if !lp {
                    ui.horizontal(|ui| {
                        ui.label(s.play_count);
                        ui.add(egui::DragValue::new(&mut self.config.play_count_limit).range(1..=9999));
                    });
                    self.state.play_count_limit.store(self.config.play_count_limit, Ordering::Relaxed);
                }

                let mut tle = self.config.time_limit_enabled;
                if ui.checkbox(&mut tle, s.time_limit_cb).changed() {
                    self.config.time_limit_enabled = tle;
                    self.state.time_limit_enabled.store(tle, Ordering::Relaxed);
                }

                if tle {
                    ui.horizontal(|ui| {
                        ui.label(s.time_limit_h);
                        ui.add(egui::DragValue::new(&mut self.config.time_limit_h).range(0..=100));
                        ui.label(s.time_limit_m);
                        ui.add(egui::DragValue::new(&mut self.config.time_limit_m).range(0..=59));
                        ui.label(s.time_limit_s);
                        ui.add(egui::DragValue::new(&mut self.config.time_limit_s).range(0..=59));
                    });
                    let us = (self.config.time_limit_h * 3600 + self.config.time_limit_m * 60 + self.config.time_limit_s) * 1_000_000;
                    self.state.time_limit_us.store(us, Ordering::Relaxed);

                    ui.horizontal(|ui| {
                        ui.label(s.action_on_limit);
                        ui.selectable_value(&mut self.config.action_on_completion, 0, s.action_stop);
                        ui.selectable_value(&mut self.config.action_on_completion, 1, s.action_shutdown);
                    });
                    self.state.action_on_completion.store(self.config.action_on_completion as u64, Ordering::Relaxed);
                }

                ui.add(egui::Slider::new(&mut self.config.speed, 0.1..=3.0).text(s.speed));
                *self.state.speed.lock() = self.config.speed;

                let mut am = self.config.absolute_mouse;
                if ui.checkbox(&mut am, s.abs_mouse).changed() {
                    self.config.absolute_mouse = am;
                    self.state.absolute_mouse.store(am, Ordering::Relaxed);
                }

                if ui.checkbox(&mut self.config.transparent_ui, s.transparent_ui).changed() {
                    let themes = [Theme::Dark, Theme::OLED, Theme::Material3, Theme::Catppuccin,
                                  Theme::Nord, Theme::Dracula, Theme::Glass, Theme::Neumorphism];
                    let t = themes.get(self.config.default_theme).copied().unwrap_or(Theme::Dark);
                    apply_theme(ui.ctx(), t, self.config.transparent_ui);
                }

                if ui.checkbox(&mut self.config.always_on_top, s.on_top).changed() {
                    let level = if self.config.always_on_top {
                        egui::viewport::WindowLevel::AlwaysOnTop
                    } else {
                        egui::viewport::WindowLevel::Normal
                    };
                    ui.ctx().send_viewport_cmd(egui::viewport::ViewportCommand::WindowLevel(level));
                }

                ui.separator();

                ui.horizontal(|ui| {
                    ui.label(s.theme);
                    egui::ComboBox::from_id_salt("theme")
                        .selected_text(THEME_NAMES[self.config.default_theme])
                        .show_ui(ui, |ui| {
                            for (i, name) in THEME_NAMES.iter().enumerate() {
                                if ui.selectable_label(self.config.default_theme == i, *name).clicked() {
                                    self.config.default_theme = i;
                                    let themes = [Theme::Dark, Theme::OLED, Theme::Material3, Theme::Catppuccin,
                                                  Theme::Nord, Theme::Dracula, Theme::Glass, Theme::Neumorphism];
                                    apply_theme(ui.ctx(), themes[i], self.config.transparent_ui);
                                }
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label(s.language);
                    egui::ComboBox::from_id_salt("lang")
                        .selected_text(match self.config.default_lang {
                            1 => "English", 2 => "Русский", 3 => "Українська",
                            4 => "Português", 5 => "Español", 6 => "中文", _ => s.lang_auto,
                        })
                        .show_ui(ui, |ui| {
                            for (i, name) in [s.lang_auto, "English", "Русский", "Українська", "Português", "Español", "中文"].iter().enumerate() {
                                if ui.selectable_label(self.config.default_lang == i, *name).clicked() {
                                    self.config.default_lang = i;
                                }
                            }
                        });
                });

                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button(s.save).clicked() {
                        let res: anyhow::Result<()> = (|| {
                            let data = self.state.macro_data.lock().clone();
                            let json = serde_json::to_string_pretty(&data)?;
                            std::fs::write(MACRO_PATH, json)?;
                            Ok(())
                        })();
                        match res {
                            Ok(_) => self.status_msg = s.saved.into(),
                            Err(e) => self.status_msg = s.save_err.replace("{}", &e.to_string()),
                        }
                    }
                    if ui.button(s.load).clicked() {
                        let res: anyhow::Result<Vec<MacroEvent>> = (|| {
                            let text = std::fs::read_to_string(MACRO_PATH)?;
                            let events = serde_json::from_str(&text)?;
                            Ok(events)
                        })();
                        match res {
                            Ok(events) => {
                                let dur = events.last().map(|e: &MacroEvent| e.t_us).unwrap_or(0);
                                self.state.recorded_time_us.store(dur, Ordering::Relaxed);
                                *self.state.macro_data.lock() = events;
                                self.status_msg = s.loaded.into();
                            }
                            Err(e) => self.status_msg = s.load_err.replace("{}", &e.to_string()),
                        }
                    }
                });

                if ui.button(s.save_settings).clicked() {
                    match save_config(&self.config) {
                        Ok(_) => self.status_msg = s.settings_saved.into(),
                        Err(e) => self.status_msg = format!("Error: {}", e),
                    }
                }

                ui.separator();

                let ec = self.state.macro_data.lock().len();
                ui.label(s.events.replace("{}", &ec.to_string()));

                let status = if recording { s.status_rec }
                             else if playing { s.status_play }
                             else if !self.status_msg.is_empty() { &self.status_msg }
                             else { s.status_ready };
                ui.label(format!("ℹ {}", status));

                ui.ctx().request_repaint_after(Duration::from_millis(100));
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.state.stop_play.store(true, Ordering::Relaxed);
        self.state.recording.store(false, Ordering::Relaxed);
        info!("Application exiting gracefully");
    }
}

fn format_us(us: u64) -> String {
    let secs = us / 1_000_000;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

// ============================================================================
// Entry Point
// ============================================================================

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    init_epoch();

    #[cfg(windows)]
    unsafe {
        let _ = win32::SetProcessDpiAwarenessContext(win32::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let (tx, rx) = unbounded();
    let state = AppState::new(tx);

    let st = state.clone();
    std::thread::spawn(move || collector_thread(rx, st));

    #[cfg(windows)]
    {
        let st = state.clone();
        std::thread::spawn(move || input_hook_thread(st));
    }

    let config = load_config();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([420.0, 640.0])
        .with_transparent(true);

    if config.always_on_top {
        viewport = viewport.with_always_on_top();
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let st = state.clone();
    eframe::run_native(
        "Macro Recorder",
        options,
        Box::new(move |cc| Ok(Box::new(MacroApp::new(cc, st, config)))),
    ).map_err(|e| anyhow::anyhow!("{}", e))
}
