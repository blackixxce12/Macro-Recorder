#![cfg(windows)]
#![windows_subsystem = "windows"]

use eframe::egui;
use serde::{Deserialize, Serialize};

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
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
// Configuration
// ============================================================

#[derive(Serialize, Deserialize)]
struct Config {
    default_lang: usize,
    default_theme: usize,
    transparent_ui: bool,
    time_limit_enabled: bool,
    time_limit_h: u64,
    time_limit_m: u64,
    action_on_completion: usize, // 0 = stop, 1 = shutdown
    loop_play: bool,
    play_count_limit: u64,
    speed: f64,
    absolute_mouse: bool,
    always_on_top: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_lang: 0,
            default_theme: 0,
            transparent_ui: false,
            time_limit_enabled: false,
            time_limit_h: 0,
            time_limit_m: 0,
            action_on_completion: 0,
            loop_play: true,
            play_count_limit: 1,
            speed: 1.0,
            absolute_mouse: true,
            always_on_top: true,
        }
    }
}

fn load_config() -> Config {
    std::fs::read_to_string("config.json")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) {
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write("config.json", json);
    }
}

// ============================================================
// Localization
// ============================================================

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    En,
    Ru,
    Uk,
    Pt,
    Es,
    Zh,
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
    time_limit_cb: &'static str,
    time_limit_h: &'static str,
    time_limit_m: &'static str,
    action_on_limit: &'static str,
    action_stop: &'static str,
    action_shutdown: &'static str,
    save_settings: &'static str,
    settings_saved: &'static str,
    transparent_ui: &'static str,
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
    time_limit_cb: "⏱ Stop after time limit",
    time_limit_h: "Hours",
    time_limit_m: "Minutes",
    action_on_limit: "Action on limit:",
    action_stop: "Stop playback",
    action_shutdown: "Shutdown system",
    save_settings: "💾 Save Settings",
    settings_saved: "Settings saved as default!",
    transparent_ui: "🪟 Transparent UI",
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
    time_limit_cb: "⏱ Остановить по таймеру",
    time_limit_h: "Часы",
    time_limit_m: "Минуты",
    action_on_limit: "Действие по таймеру:",
    action_stop: "Остановить воспроизведение",
    action_shutdown: "Выключить систему",
    save_settings: "💾 Сохранить настройки",
    settings_saved: "Настройки сохранены по умолчанию!",
    transparent_ui: "🪟 Прозрачный интерфейс",
};

const UK: Strings = Strings {
    record: "🔴 Запис (F8)",
    stop_rec: "⏹ Зупинити запис",
    play: "▶ Відтворити (F9)",
    stop_play: "⏹ Зупинити відтворення",
    rec_time: "⏱ Час запису: {}…",
    rec_done: "⏱ Час запису: {} (завершено)",
    play_inf: "🔄 Відтворень: {} (нескінченно)",
    play_lim: "🔄 Відтворень: {} / {}",
    loop_cb: "Безперервне відтворення (цикл)",
    play_count: "Кількість відтворень:",
    speed: "Швидкість",
    abs_mouse: "Абсолютна миша (фікс High-DPI)",
    on_top: "📌 Поверх усіх вікон",
    theme: "Тема:",
    language: "Мова:",
    lang_auto: "Авто (система)",
    save: "💾 Зберегти",
    load: "📂 Завантажити",
    events: "📦 Подій записано: {}",
    status_ready: "Готово [F8: запис | F9: плей]",
    status_rec: "Запис... [F8 — стоп]",
    status_play: "Відтворення... [F9 — стоп]",
    saved: "Збережено в macro.json",
    loaded: "Завантажено з macro.json",
    save_err: "Помилка збереження: {}",
    load_err: "Помилка завантаження: {}",
    time_limit_cb: "⏱ Зупинити за таймером",
    time_limit_h: "Години",
    time_limit_m: "Хвилини",
    action_on_limit: "Дія за таймером:",
    action_stop: "Зупинити відтворення",
    action_shutdown: "Вимкнути систему",
    save_settings: "💾 Зберегти налаштування",
    settings_saved: "Налаштування збережено за замовчуванням!",
    transparent_ui: "🪟 Прозорий інтерфейс",
};

const PT: Strings = Strings {
    record: "🔴 Gravar (F8)",
    stop_rec: "⏹ Parar Gravação",
    play: "▶ Reproduzir (F9)",
    stop_play: "⏹ Parar Reprodução",
    rec_time: "⏱ Tempo de gravação: {}…",
    rec_done: "⏱ Tempo gravado: {} (concluído)",
    play_inf: "🔄 Reproduções: {} (infinito)",
    play_lim: "🔄 Reproduções: {} / {}",
    loop_cb: "Reprodução contínua (Loop)",
    play_count: "Nº de reproduções:",
    speed: "Velocidade",
    abs_mouse: "Mouse absoluto (Fix High-DPI)",
    on_top: "📌 Sempre no topo",
    theme: "Tema:",
    language: "Idioma:",
    lang_auto: "Auto (sistema)",
    save: "💾 Salvar",
    load: "📂 Carregar",
    events: "📦 Eventos gravados: {}",
    status_ready: "Pronto [F8: Gravar | F9: Reproduzir]",
    status_rec: "Gravando... [F8 para parar]",
    status_play: "Reproduzindo... [F9 para parar]",
    saved: "Salvo em macro.json",
    loaded: "Carregado macro.json",
    save_err: "Erro ao salvar: {}",
    load_err: "Erro ao carregar: {}",
    time_limit_cb: "⏱ Parar após limite de tempo",
    time_limit_h: "Horas",
    time_limit_m: "Minutos",
    action_on_limit: "Ação no limite:",
    action_stop: "Parar reprodução",
    action_shutdown: "Desligar o sistema",
    save_settings: "💾 Salvar Configurações",
    settings_saved: "Configurações salvas como padrão!",
    transparent_ui: "🪟 Interface Transparente",
};

const ES: Strings = Strings {
    record: "🔴 Grabar (F8)",
    stop_rec: "⏹ Detener Grabación",
    play: "▶ Reproducir (F9)",
    stop_play: "⏹ Detener Reproducción",
    rec_time: "⏱ Tiempo de grabación: {}…",
    rec_done: "⏱ Tiempo grabado: {} (terminado)",
    play_inf: "🔄 Reproducciones: {} (infinito)",
    play_lim: "🔄 Reproducciones: {} / {}",
    loop_cb: "Reproducción continua (Bucle)",
    play_count: "Nº de reproducciones:",
    speed: "Velocidad",
    abs_mouse: "Ratón absoluto (Fix High-DPI)",
    on_top: "📌 Siempre encima",
    theme: "Tema:",
    language: "Idioma:",
    lang_auto: "Auto (sistema)",
    save: "💾 Guardar",
    load: "📂 Cargar",
    events: "📦 Eventos grabados: {}",
    status_ready: "Listo [F8: Grabar | F9: Reproducir]",
    status_rec: "Grabando... [F8 para detener]",
    status_play: "Reproduciendo... [F9 para detener]",
    saved: "Guardado en macro.json",
    loaded: "Cargado macro.json",
    save_err: "Error al guardar: {}",
    load_err: "Error al cargar: {}",
    time_limit_cb: "⏱ Detener por límite de tiempo",
    time_limit_h: "Horas",
    time_limit_m: "Minutos",
    action_on_limit: "Acción en el límite:",
    action_stop: "Detener reproducción",
    action_shutdown: "Apagar el sistema",
    save_settings: "💾 Guardar Ajustes",
    settings_saved: "¡Ajustes guardados por defecto!",
    transparent_ui: "🪟 Interfaz Transparente",
};

const ZH: Strings = Strings {
    record: "🔴 录制 (F8)",
    stop_rec: "⏹ 停止录制",
    play: "▶ 播放 (F9)",
    stop_play: "⏹ 停止播放",
    rec_time: "⏱ 录制时间: {}…",
    rec_done: "⏱ 已录制时间: {} (完成)",
    play_inf: "🔄 播放次数: {} (无限)",
    play_lim: "🔄 播放次数: {} / {}",
    loop_cb: "连续播放 (循环)",
    play_count: "播放次数:",
    speed: "速度",
    abs_mouse: "绝对鼠标 (高DPI修复)",
    on_top: "📌 置顶",
    theme: "主题:",
    language: "语言:",
    lang_auto: "自动 (系统)",
    save: "💾 保存",
    load: "📂 加载",
    events: "📦 已录制事件: {}",
    status_ready: "就绪 [F8: 录制 | F9: 播放]",
    status_rec: "录制中... [F8 停止]",
    status_play: "播放中... [F9 停止]",
    saved: "已保存至 macro.json",
    loaded: "已加载 macro.json",
    save_err: "保存错误: {}",
    load_err: "加载错误: {}",
    time_limit_cb: "⏱ 达到时间限制后停止",
    time_limit_h: "小时",
    time_limit_m: "分钟",
    action_on_limit: "达到限制时的操作:",
    action_stop: "停止播放",
    action_shutdown: "关闭系统",
    save_settings: "💾 保存设置",
    settings_saved: "设置已保存为默认！",
    transparent_ui: "🪟 透明界面",
};

static SYSTEM_LANG: OnceLock<Lang> = OnceLock::new();

fn detect_system_lang() -> Lang {
    unsafe {
        let lang = GetUserDefaultUILanguage() as u32;
        let primary = lang & 0x3FF;
        match primary {
            0x19 => Lang::Ru,
            0x22 => Lang::Uk,
            0x16 => Lang::Pt,
            0x0A => Lang::Es,
            0x04 => Lang::Zh,
            _ => Lang::En,
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
            dark: true, bg: rgb(16, 16, 16), panel: rgb(24, 24, 24), widget: rgb(42, 42, 42), widget_hover: rgb(58, 58, 58), widget_active: rgb(75, 75, 75), active_fg: rgb(255, 255, 255), border: rgb(70, 70, 70), hover_border: rgb(95, 95, 95), text: rgb(230, 230, 230), faint: rgb(130, 130, 130), accent: rgb(70, 130, 255), accent_text: rgb(255, 255, 255), widget_round: 4.0, shadow_blur: 4, shadow_offset: 1, shadow_alpha: 60, item_spacing_y: 5.0, button_padding: 3.0, animation_time: 0.15, backdrop: 1,
        },
        Theme::Material3 => Palette {
            dark: true, bg: rgb(18, 17, 24), panel: rgb(18, 17, 24), widget: rgb(56, 48, 75), widget_hover: rgb(70, 60, 92), widget_active: rgb(208, 188, 255), active_fg: rgb(56, 30, 114), border: rgb(73, 69, 82), hover_border: rgb(208, 188, 255), text: rgb(230, 224, 233), faint: rgb(147, 143, 153), accent: rgb(208, 188, 255), accent_text: rgb(56, 30, 114), widget_round: 20.0, shadow_blur: 8, shadow_offset: 2, shadow_alpha: 80, item_spacing_y: 7.0, button_padding: 6.0, animation_time: 0.4, backdrop: 1,
        },
        Theme::Fluent => Palette {
            dark: true, bg: rgb(16, 16, 16), panel: rgba(24, 26, 30, 140), widget: rgba(255, 255, 255, 28), widget_hover: rgba(255, 255, 255, 48), widget_active: rgb(96, 205, 255), active_fg: rgb(6, 25, 45), border: rgba(255, 255, 255, 55), hover_border: rgb(96, 205, 255), text: rgb(250, 250, 250), faint: rgb(170, 175, 180), accent: rgb(96, 205, 255), accent_text: rgb(6, 25, 45), widget_round: 4.0, shadow_blur: 4, shadow_offset: 1, shadow_alpha: 70, item_spacing_y: 5.0, button_padding: 4.0, animation_time: 0.2, backdrop: 2,
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

fn make_visuals(p: &Palette, see_through: bool, force_transparent: bool) -> egui::Visuals {
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

    // Прозрачная клиентская область: сквозь UI виден Mica/Acrylic или рабочий стол
    if see_through || force_transparent {
        v.window_fill = egui::Color32::TRANSPARENT;
        // Более светлая полупрозрачная панель для лучшей видимости на размытом фоне
        v.panel_fill = egui::Color32::from_rgba_unmultiplied(30, 30, 30, 140); // ~55% непрозрачности
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
            return;
        }
        let value: i32 = kind;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(38), // DWMWA_SYSTEMBACKDROP_TYPE
            &value as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

// ============================================================
// True per-pixel window transparency (Win32, undocumented but stable)
// ============================================================

#[repr(C)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

#[repr(C)]
struct WindowCompositionAttributeData {
    attribute: u32,
    data: *mut std::ffi::c_void,
    size_of_data: usize,
}

#[link(name = "user32")]
extern "system" {
    fn SetWindowCompositionAttribute(
        hwnd: HWND,
        data: *mut WindowCompositionAttributeData,
    ) -> i32;
}

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_DISABLED: u32 = 0;
const ACCENT_ENABLE_TRANSPARENTGRADIENT: u32 = 2;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 3; // Настоящее размытие (Acrylic)

fn set_window_pixel_alpha(enabled: bool) {
    unsafe {
        let hwnd = match FindWindowW(None, w!("Macro Recorder")) {
            Ok(h) => h,
            Err(_) => return,
        };
        if hwnd.is_invalid() {
            return;
        }

        // Сначала включаем Mica/Acrylic через DWM для фона
        // DWMWA_USE_IMMERSIVE_DARK_MODE = 20 (для тёмной темы)
        let dark_mode: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(20), // DWMWA_USE_IMMERSIVE_DARK_MODE
            &dark_mode as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );

        // Включаем Acrylic blur для настоящего размытия фона
        let mut accent = AccentPolicy {
            accent_state: if enabled {
                ACCENT_ENABLE_ACRYLICBLURBEHIND // Размытие вместо простого градиента
            } else {
                ACCENT_DISABLED
            },
            accent_flags: if enabled { 0x20 } else { 0 }, // ACCENT_FLAG_BLUR_REGION_BEHIND
            gradient_color: if enabled {
                // Полупрозрачный чёрный для лучшей видимости UI
                // Формат: 0xAABBGGRR (little-endian)
                0xC0000000
            } else {
                0
            },
            animation_id: 0,
        };
        let mut data = WindowCompositionAttributeData {
            attribute: WCA_ACCENT_POLICY,
            data: &mut accent as *mut AccentPolicy as *mut std::ffi::c_void,
            size_of_data: std::mem::size_of::<AccentPolicy>(),
        };
        let _ = SetWindowCompositionAttribute(hwnd, &mut data);
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyhbd.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\simsun.ttc",
        "C:\\Windows\\Fonts\\meiryo.ttc",
    ];

    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk_fallback".to_owned(),
                egui::FontData::from_owned(data).into(), // <-- добавили .into()
            );
                        if let Some(fams) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                fams.push("cjk_fallback".to_owned());
            }
            if let Some(fams) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                fams.push("cjk_fallback".to_owned());
            }
            break;
        }
    }

    ctx.set_fonts(fonts);
}

fn apply_theme(ctx: &egui::Context, theme: Theme, force_transparent: bool) {
    let p = palette(theme);
    let see_through = matches!(theme, Theme::Fluent | Theme::Glass);

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = make_visuals(&p, see_through, force_transparent);
    style.animation_time = p.animation_time;
    style.spacing.item_spacing = egui::vec2(8.0, p.item_spacing_y);
    style.spacing.button_padding = egui::vec2(p.button_padding, p.button_padding);

    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);

    set_system_backdrop(p.backdrop);
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

static TIME_LIMIT_ENABLED: AtomicBool = AtomicBool::new(false);
static TIME_LIMIT_US: AtomicU64 = AtomicU64::new(0);
static ACTION_ON_COMPLETION: AtomicU64 = AtomicU64::new(0);

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

    let time_limit_enabled = TIME_LIMIT_ENABLED.load(Ordering::Relaxed);
    let time_limit_us = TIME_LIMIT_US.load(Ordering::Relaxed);
    let action_on_completion = ACTION_ON_COMPLETION.load(Ordering::Relaxed);

    std::thread::spawn(move || {
        playback_loop(&events, time_limit_enabled, time_limit_us, action_on_completion);
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

        let _ = RegisterHotKey(None, HOTKEY_ID_RECORD, MOD_NOREPEAT, VK_F8);
        let _ = RegisterHotKey(None, HOTKEY_ID_PLAY, MOD_NOREPEAT, VK_F9);

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

                WM_LBUTTONDOWN => Some(InputEventKind::MouseButton { button: MouseButton::Left, down: true, x, y }),
                WM_LBUTTONUP => Some(InputEventKind::MouseButton { button: MouseButton::Left, down: false, x, y }),
                WM_RBUTTONDOWN => Some(InputEventKind::MouseButton { button: MouseButton::Right, down: true, x, y }),
                WM_RBUTTONUP => Some(InputEventKind::MouseButton { button: MouseButton::Right, down: false, x, y }),
                WM_MBUTTONDOWN => Some(InputEventKind::MouseButton { button: MouseButton::Middle, down: true, x, y }),
                WM_MBUTTONUP => Some(InputEventKind::MouseButton { button: MouseButton::Middle, down: false, x, y }),

                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    let xbutton = ((data.mouseData >> 16) & 0xFFFF) as u16;
                    let button = if xbutton == 1 { MouseButton::X1 } else { MouseButton::X2 };
                    Some(InputEventKind::MouseButton { button, down: wm == WM_XBUTTONDOWN, x, y })
                }

                WM_MOUSEWHEEL => {
                    let delta = ((data.mouseData >> 16) & 0xFFFF) as i16 as i32;
                    Some(InputEventKind::MouseWheel { delta, x, y, horizontal: false })
                }

                WM_MOUSEHWHEEL => {
                    let delta = ((data.mouseData >> 16) & 0xFFFF) as i16 as i32;
                    Some(InputEventKind::MouseWheel { delta, x, y, horizontal: true })
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

fn playback_loop(events: &[MacroEvent], time_limit_enabled: bool, time_limit_us: u64, action_on_completion: u64) {
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
    let mut limit_reached = false;

    while !STOP_PLAY.load(Ordering::Relaxed) {
        if time_limit_enabled && start.elapsed().as_micros() as u64 >= time_limit_us {
            limit_reached = true;
            break;
        }

        if index >= events.len() {
            play_count += 1;
            PLAY_COUNT.store(play_count, Ordering::Relaxed);

            if play_count >= max_count {
                limit_reached = true;
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

    if limit_reached && action_on_completion == 1 {
        let _ = std::process::Command::new("shutdown")
            .args(["/s", "/t", "60", "/c", "Macro Recorder: Limit reached. System shutting down."])
            .spawn();
    }
}

// ============================================================
// SendInput
// ============================================================

#[inline(always)]
unsafe fn send_one_input(input: INPUT) {
    let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
}

#[inline(always)]
unsafe fn make_keyboard_input(vk: u16, scan: u16, down: bool, extended: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);

    if !down { flags |= KEYEVENTF_KEYUP; }
    if extended { flags |= KEYEVENTF_EXTENDEDKEY; }

    if scan != 0 {
        flags |= KEYEVENTF_SCANCODE;
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        }
    } else {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        }
    }
}

#[inline(always)]
unsafe fn make_mouse_input(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

#[inline(always)]
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

#[inline(always)]
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

#[inline(always)]
unsafe fn send_input_event(kind: &InputEventKind) {
    match kind {
        InputEventKind::Key { vk, scan, down, extended } => {
            send_one_input(make_keyboard_input(*vk, *scan, *down, *extended));
        }
        InputEventKind::MouseMove { x, y, dx, dy } => {
            if ABSOLUTE_MOUSE.load(Ordering::Relaxed) {
                send_absolute_mouse_move(*x, *y);
            } else {
                send_one_input(make_mouse_input(MOUSEEVENTF_MOVE, *dx, *dy, 0));
            }
        }
        InputEventKind::MouseButton { button, down, x, y } => {
            if ABSOLUTE_MOUSE.load(Ordering::Relaxed) {
                send_absolute_mouse_move(*x, *y);
            }
            let (flags, data) = mouse_button_flags_data(*button, *down);
            send_one_input(make_mouse_input(flags, 0, 0, data));
        }
        InputEventKind::MouseWheel { delta, horizontal, .. } => {
            let flags = if *horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
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
    SavedSettings,
    SaveErr(String),
    LoadErr(String),
}

impl Status {
    fn text(&self, s: &Strings) -> String {
        match self {
            Status::Ready => s.status_ready.to_string(),
            Status::Saved => s.saved.to_string(),
            Status::Loaded => s.loaded.to_string(),
            Status::SavedSettings => s.settings_saved.to_string(),
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
    lang_mode: usize,
    theme_idx: usize,
    transparent_ui: bool,
    time_limit_enabled: bool,
    time_limit_h: u64,
    time_limit_m: u64,
    action_on_completion: usize,
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
            transparent_ui: false,
            time_limit_enabled: false,
            time_limit_h: 0,
            time_limit_m: 0,
            action_on_completion: 0,
        }
    }
}

impl MacroApp {
    fn lang(&self) -> Lang {
        match self.lang_mode {
            1 => Lang::En,
            2 => Lang::Ru,
            3 => Lang::Uk,
            4 => Lang::Pt,
            5 => Lang::Es,
            6 => Lang::Zh,
            _ => *SYSTEM_LANG.get().unwrap_or(&Lang::En),
        }
    }

    fn strs(&self) -> &'static Strings {
        match self.lang() {
            Lang::En => &EN,
            Lang::Ru => &RU,
            Lang::Uk => &UK,
            Lang::Pt => &PT,
            Lang::Es => &ES,
            Lang::Zh => &ZH,
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
                    if ui.button(s.record).clicked() { toggle_recording(); }
                    if ui.button(s.stop_rec).clicked() { stop_recording(); }
                });

                ui.horizontal(|ui| {
                    if ui.button(s.play).clicked() { toggle_playback(); }
                    if ui.button(s.stop_play).clicked() { stop_playback(); }
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
                        let line = s.play_lim
                            .replacen("{}", &count.to_string(), 1)
                            .replacen("{}", &limit.to_string(), 1);
                        ui.label(line);
                    }
                }

                ui.separator();
                ui.heading("⚙ Settings");

                ui.checkbox(&mut self.loop_play, s.loop_cb);
                LOOP_PLAY.store(self.loop_play, Ordering::Relaxed);

                if !self.loop_play {
                    ui.horizontal(|ui| {
                        ui.label(s.play_count);
                        ui.add(egui::DragValue::new(&mut self.play_count_limit).range(1..=9999).speed(1));
                    });
                    PLAY_COUNT_LIMIT.store(self.play_count_limit, Ordering::Relaxed);
                }

                ui.checkbox(&mut self.time_limit_enabled, s.time_limit_cb);
                TIME_LIMIT_ENABLED.store(self.time_limit_enabled, Ordering::Relaxed);

                if self.time_limit_enabled {
                    ui.horizontal(|ui| {
                        ui.label(s.time_limit_h);
                        ui.add(egui::DragValue::new(&mut self.time_limit_h).range(0..=100).speed(0.1));
                        ui.label(s.time_limit_m);
                        ui.add(egui::DragValue::new(&mut self.time_limit_m).range(0..=59).speed(0.1));
                    });
                    let us = (self.time_limit_h * 3600 + self.time_limit_m * 60) * 1_000_000;
                    TIME_LIMIT_US.store(us, Ordering::Relaxed);

                    ui.horizontal(|ui| {
                        ui.label(s.action_on_limit);
                        ui.selectable_value(&mut self.action_on_completion, 0, s.action_stop);
                        ui.selectable_value(&mut self.action_on_completion, 1, s.action_shutdown);
                    });
                    ACTION_ON_COMPLETION.store(self.action_on_completion as u64, Ordering::Relaxed);
                }

                ui.add(egui::Slider::new(&mut self.speed, 0.1..=3.0).text(s.speed));
                if let Ok(mut speed) = SPEED.lock() { *speed = self.speed; }

                ui.checkbox(&mut self.absolute_mouse, s.abs_mouse);
                ABSOLUTE_MOUSE.store(self.absolute_mouse, Ordering::Relaxed);

               if ui.checkbox(&mut self.transparent_ui, s.transparent_ui).changed() {
    let themes = [
        Theme::Dark, Theme::Material3, Theme::Fluent, Theme::Catppuccin,
        Theme::Nord, Theme::Dracula, Theme::Glass, Theme::Neumorphism,
    ];
    apply_theme(ui.ctx(), themes[self.theme_idx], self.transparent_ui);
    set_window_pixel_alpha(self.transparent_ui);   // <-- включаем/выключаем альфу окна
}
                if ui.checkbox(&mut self.always_on_top, s.on_top).changed() {
                    let level = if self.always_on_top {
                        egui::viewport::WindowLevel::AlwaysOnTop
                    } else {
                        egui::viewport::WindowLevel::Normal
                    };
                    ui.ctx().send_viewport_cmd(egui::viewport::ViewportCommand::WindowLevel(level));
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
                                        Theme::Dark, Theme::Material3, Theme::Fluent, Theme::Catppuccin,
                                        Theme::Nord, Theme::Dracula, Theme::Glass, Theme::Neumorphism,
                                    ];
                                    apply_theme(ui.ctx(), themes[i], self.transparent_ui);
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
                            3 => "Українська",
                            4 => "Português",
                            5 => "Español",
                            6 => "中文",
                            _ => s.lang_auto,
                        })
                        .show_ui(ui, |ui| {
                            let opts = [s.lang_auto, "English", "Русский", "Українська", "Português", "Español", "中文"];
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

                if ui.button(s.save_settings).clicked() {
                    let cfg = Config {
                        default_lang: self.lang_mode,
                        default_theme: self.theme_idx,
                        transparent_ui: self.transparent_ui,
                        time_limit_enabled: self.time_limit_enabled,
                        time_limit_h: self.time_limit_h,
                        time_limit_m: self.time_limit_m,
                        action_on_completion: self.action_on_completion,
                        loop_play: self.loop_play,
                        play_count_limit: self.play_count_limit,
                        speed: self.speed,
                        absolute_mouse: self.absolute_mouse,
                        always_on_top: self.always_on_top,
                    };
                    save_config(&cfg);
                    self.status = Status::SavedSettings;
                }

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

    let cfg = load_config();

    // Создаем базовый билдер
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([400.0, 600.0])
        .with_transparent(true);

    // Условно применяем "поверх всех окон", если это указано в конфиге
    if cfg.always_on_top {
        viewport = viewport.with_always_on_top();
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let result = eframe::run_native(
        "Macro Recorder",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);   // <-- ВОТ ЭТА НОВАЯ СТРОКА

            let themes = [
                            Theme::Dark, Theme::Material3, Theme::Fluent, Theme::Catppuccin,
                Theme::Nord, Theme::Dracula, Theme::Glass, Theme::Neumorphism,
            ];

            // 2. Безопасно берём тему из конфига (защита от битого config.json)
            let theme = themes.get(cfg.default_theme).copied().unwrap_or(Theme::Dark);
            apply_theme(&cc.egui_ctx, theme, cfg.transparent_ui);

            if cfg.transparent_ui {
                set_window_pixel_alpha(true);
            }

            // 3. Сначала создаём app, ПОТОМ заполняем поля
            let mut app = MacroApp::default();
            app.lang_mode = cfg.default_lang;
            app.theme_idx = cfg.default_theme.min(THEME_NAMES.len() - 1); // clamp вместо прямой записи
            app.transparent_ui = cfg.transparent_ui;
            app.time_limit_enabled = cfg.time_limit_enabled;
            app.time_limit_h = cfg.time_limit_h;
            app.time_limit_m = cfg.time_limit_m;
            app.action_on_completion = cfg.action_on_completion;
            app.loop_play = cfg.loop_play;
            app.play_count_limit = cfg.play_count_limit;
            app.speed = cfg.speed;
            app.absolute_mouse = cfg.absolute_mouse;
            app.always_on_top = cfg.always_on_top;

            LOOP_PLAY.store(app.loop_play, Ordering::Relaxed);
            PLAY_COUNT_LIMIT.store(app.play_count_limit, Ordering::Relaxed);
            ABSOLUTE_MOUSE.store(app.absolute_mouse, Ordering::Relaxed);
            TIME_LIMIT_ENABLED.store(app.time_limit_enabled, Ordering::Relaxed);
            let us = (app.time_limit_h * 3600 + app.time_limit_m * 60) * 1_000_000;
            TIME_LIMIT_US.store(us, Ordering::Relaxed);
            ACTION_ON_COMPLETION.store(app.action_on_completion as u64, Ordering::Relaxed);
            if let Ok(mut speed) = SPEED.lock() { *speed = app.speed; }

            Ok(Box::new(app))
        }),
    );

    unsafe {
        let _ = timeEndPeriod(1);
    }

    result.map_err(|e| anyhow::anyhow!(e.to_string()))
}
