<div align="center">

<img src="assets/icon_256.png" width="128" alt="Macro Recorder">

# 🦀 Macro Recorder

**A modern, open-source alternative to TinyTask.**
*Born from Roblox grind. Forged in Rust.*

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Windows](https://img.shields.io/badge/Windows-10%20%2F%2011-0078D6?logo=windows&logoColor=white)]()
[![Rust](https://img.shields.io/badge/Made%20with-Rust%201.97-orange?logo=rust&logoColor=white)]()
[![egui](https://img.shields.io/badge/UI-egui%20%2F%20eframe%200.36-blue)]()
[![Latest Release](https://img.shields.io/github/v/release/blackixxce12/Macro-Recorder?label=release&color=green)](https://github.com/blackixxce12/macro-recorder/releases)

*Record mouse & keyboard → replay it forever, exactly N times, or until a timer runs out → or write a little program that watches the screen and decides for itself.* ☕

[📥 Download](../../releases) • [✨ Features](#-features) • [🧠 Scripts](SCRIPTS.md) • [🆚 vs TinyTask](#-macro-recorder-vs-tinytask) • [🇷🇺 Русская версия](README_RU.md)

<img src="assets/screenshot.png" width="330" alt="Macro Recorder window">

</div>

---

## 🆕 Highlights

| | |
|---|---|
| 🧠 **Script engine** | 17 step kinds with `If` / `While` / `Break`, variables, and conditions that look at the screen. **[Full guide → SCRIPTS.md](SCRIPTS.md)** |
| 🔎 **Image search** | Paste a snippet with `Win+Shift+S` and the macro can wait for it, or click it wherever it appears |
| 🔤 **Text on screen (OCR)** | Uses the recognition already built into Windows — react to words, or read a number into a variable |
| 📅 **Scheduler** | Start the macro at a set time on chosen weekdays, even while minimised to the tray |
| 🪟 **Target window** | Pause automatically whenever your game or app isn't the one in front |
| 🖱 **Human-like movement** | Curved cursor paths with a random arc, plus an aim-spread in pixels |
| ✂ **Built-in editor** | A plain-English list of what you did, a raw event list, and a per-action inspector — with undo |
| ⚙ **Compile to a standalone .exe** | Export a self-running player — scripts included. No compiler involved |
| ⚓ **Window anchoring** | Follows the target window if it moved *or was resized* |
| ⌨ **Live speed control** | Hotkeys for faster / slower / skip-this-step, usable mid-run |

<details>
<summary>Under the hood</summary>

Pause/resume with a proper schedule clock · 7 rebindable hotkeys + emergency stop · X1/X2 capture ·
Open/Save dialogs and recent files · gzip `.mrz` macros · shutdown/restart/sleep/hibernate/log-off ·
headless CLI · Fluent (Mica) theme · 9 themes · 6 languages · timing jitter · single instance ·
rotating log file · virtual-desktop isolation · per-monitor DPI awareness.

</details>

---

## 📑 Contents

- [The story](#-the-story-roblox-anime-tower-defenses-and-a-tired-hand)
- [Why Rust](#-why-rust)
- [Macro Recorder vs TinyTask](#-macro-recorder-vs-tinytask)
- [Features](#-features)
- [How it works](#-how-it-works)
- [Hotkeys](#️-hotkeys)
- [Scripts](#-scripts)
- [Image search](#-image-search)
- [Text on screen (OCR)](#-text-on-screen-ocr)
- [Editor](#-editor)
- [Schedule & target window](#-schedule--target-window)
- [Exports & extras](#-exports--extras)
- [Themes](#-themes)
- [Languages](#-languages)
- [Files & folders](#-files--folders)
- [Command line](#-command-line)
- [Download](#-download)
- [Build from source](#️-build-from-source)
- [Known limitations](#️-known-limitations)
- [FAQ](#-faq)
- [License & credits](#-license--credits)

---

## 📖 The story: Roblox, anime tower defenses, and a tired hand

I play a lot of **Roblox** — especially anime tower defense games. If you've ever played one, you know *the loop*:

> Place units → wait for the wave → collect gems → upgrade → repeat.
> And again. And again. **Hundreds of times per session.**

One evening, after manually clicking the same "summon / upgrade / claim" buttons for the third hour in a row, my hand said *«no»*. So I did what everyone does — I downloaded **TinyTask**.

And honestly? **It worked.** TinyTask is a genuinely great piece of software: 36 KB of hand-written C that has been quietly automating people's work since the Windows XP era. It's a masterclass in minimalism, and this project would not exist without it.

But minimalism cuts both ways. After a few evenings of farming I kept hitting the same walls:

- ⏰ **No "stop after N hours"** — I wanted to farm while asleep and have the PC shut itself down afterwards. TinyTask can loop forever or loop N times, but it has no concept of *time*;
- 🖥️ **Absolute pixel coordinates, no DPI awareness** — change Windows scaling from 100% to 125%, dock a laptop, or move the game to another monitor, and every click lands in the wrong place;
- 🔒 **Closed source** — a tool that installs global keyboard hooks and injects synthetic input into my system is exactly the kind of tool I'd like to be able to *read*;
- 🧾 **Binary `.rec` files** — I wanted to tweak a macro in a text editor, not re-record it;
- 🌍 **No Russian / Ukrainian / Chinese UI**;
- 🎨 **A fixed toolbar from 2007** — charming, but I stare at this window for hours.

So I built the tool I wanted. And then a second wall showed up: **a blind recording is dumb.** If the wave takes 4 seconds longer than usual, a fixed replay clicks into empty space and the whole run is wasted. What I actually needed was *"wait until the Claim button appears, then click it"*.

That's what the [script engine](SCRIPTS.md) is for. A macro can now look at the screen — for a picture, a pixel colour, a window, or a word of text — and decide what to do. It's still a recorder first: you record the boring part, and add a few conditions on top only where you need them.

That weekend project got slightly out of hand. 🦀

---

## 🦀 Why Rust?

| Reason | What it means for you |
|---|---|
| **Single .exe** | No installer, no .NET, no Python runtime — one file, double-click, done |
| **Fearless concurrency** | Five roles run in parallel — low-level hooks, an event collector, a microsecond-accurate replay engine, a scheduler and the GPU-rendered UI — and the compiler guarantees they don't corrupt each other's state |
| **Memory safety** | A tool that injects input into your system shouldn't scribble over an event buffer mid-raid. Outside the thin `unsafe` Win32 FFI layer, Rust makes whole classes of bugs impossible |
| **Small & instant** | With `opt-level = "z"` + LTO + `strip`, the whole app is a few MB and starts instantly |
| **Honest reason** | I wanted a real excuse to learn Rust properly. Best way to learn — build something you actually use |

---

## 🆚 Macro Recorder vs TinyTask

> **This table is fact-checked** against TinyTask's official website, changelog, FAQ and support pages (see [Sources](#sources-for-the-tinytask-column)). TinyTask is *not* a bad program — it's a deliberately minimal one. Where it wins, this table says so.

### Pick the right tool

| Pick **TinyTask** if… | Pick **Macro Recorder** if… |
|---|---|
| You need the smallest possible footprint (36 KB) | You want timed playback, pause/resume and power actions |
| You need to run on Windows XP / Vista / 7 | You're on Windows 10 / 11 with DPI scaling or multiple monitors |
| You want a 36 KB tool that also compiles macros to 60 KB executables | You want a macro that reacts to the screen instead of clicking blind |
| You want a tool that has been battle-tested for over a decade | You want open source you can audit, fork and extend |
| You just need "record → play", nothing more | You want an editor, conditions, image search, OCR and a scheduler |

### Full comparison

| | **TinyTask 1.77** | **Macro Recorder** |
|---|---|---|
| **License** | Freeware, **closed source** (proprietary) | **MIT, fully open source** |
| **Implementation** | Pure C + raw Win32, self-contained **32-bit** exe | Rust 2024 + `windows-rs`, **64-bit** exe |
| **Binary size** | **~36 KB** 🏆 | ≈5 MB (GPU UI, 9 themes, 6 translations, vision + OCR) |
| **Install** | Portable single exe (optional Inno Setup installer) | Portable single exe |
| **Supported Windows** | **XP → 11** 🏆 | 10 / 11 (Windows 11 for Mica/Acrylic + virtual desktops) |
| **UI** | Fixed Win32 toolbar, user-swappable toolbar bitmaps | GPU-rendered `egui`, **9 themes**, live theme switching |
| **Window translucency** | ❌ | ✅ per-pixel alpha + **DWM Mica / Acrylic** |
| **UI languages** | Separate **localized builds** (FR, DE, IT, PT, ES, SV) since v1.74 — no in-app switch | **6 languages switchable at runtime** (EN, RU, UK, PT, ES, ZH) + auto-detect |
| **Keyboard capture** | ✅ | ✅ virtual key **+ scancode + extended flag** |
| **Mouse move & clicks** | ✅ | ✅ (L / R / M) |
| **Mouse wheel** | ⚠️ documented as unavailable with some mice | ✅ **vertical + horizontal** |
| **X1 / X2 side buttons** | ❌ | ✅ recorded and replayed |
| **Ignores its own injected input** | not documented | ✅ `LLKHF_INJECTED` / `LLMHF_INJECTED` filtered |
| **Hotkeys excluded from recordings** | ✅ by design | ✅ |
| **Repeat playback** | ✅ continuous **or** N times | ✅ continuous **or** N times (1–9999) |
| **Delay between loops** | ❌ (bake it into the recording) | ✅ 0–600 000 ms |
| **Pause / resume** | ❌ | ✅ without losing the position |
| **Stop after a time limit** | ❌ | ✅ **hours : minutes : seconds** |
| **Action when the limit is hit** | ❌ | ✅ stop · **shut down · restart · sleep · hibernate · log off** |
| **Playback speed** | ✅ presets (½×, 1×, 2×, 100×) + custom value | ✅ **0.1× – 3.0×** slider, **changeable mid-run by hotkey** |
| **Timing jitter** | ❌ | ✅ optional 0–50 % per-event randomisation |
| **Human-like cursor paths** | ❌ | ✅ Bézier arcs + aim spread in pixels |
| **Live recording timer** | ❌ (playback shows a countdown since v1.61) | ✅ live timer while recording + final duration |
| **Live playback counter** | ❌ | ✅ `plays: 7 / 50` in the UI |
| **Global hotkeys** | `Ctrl+Alt+Shift+R` / `Ctrl+Shift+Alt+P`, a few alternatives in Prefs | ✅ **7 rebindable slots** (any key × Ctrl/Alt/Shift), applied without restart |
| **Emergency stop key** | ✅ Break / ScrollLock / Pause | ✅ **F9** by default, rebindable |
| **Always on top** | ✅ (since v1.61) | ✅ toggle at runtime |
| **Settings persistence** | ✅ portable `.ini` (since v1.50) | ✅ human-readable **`config.json`** + autosave on exit |
| **Macro format** | Proprietary binary `.rec` | **Plain JSON** with µs timestamps, optional gzip (`.mrz`) |
| **Edit a recording** | ❌ in the classic build (a "With Editor" build exists on the official site) | ✅ **built-in editor** (3 views + inspector) + any text editor |
| **Open / Save dialogs, recent files** | ✅ open & save | ✅ + a recent-files list |
| **Compile macro → standalone .exe** | ✅ ~60 KB output 🏆 | ✅ ~5 MB output (a copy of this exe + the macro **and its script**) |
| **Export to another tool** | ❌ | ✅ **AutoHotkey v2 script** (events only) |
| **Tray icon / minimize to tray** | ❌ | ✅ with a record / play / stop menu |
| **Window anchoring** | ❌ | ✅ follows the target window if it moved **or resized** |
| **Stop on a screen pixel** | ❌ | ✅ colour + tolerance, with a picker |
| **Scripting / conditional logic** | ❌ | ✅ **17 step kinds, `If`/`While`/`Break`, variables** — [SCRIPTS.md](SCRIPTS.md) |
| **Image recognition** | ❌ | ✅ masked normalised cross-correlation, optional multi-scale |
| **Text recognition (OCR)** | ❌ | ✅ via `Windows.Media.Ocr` — no models to download |
| **Scheduler (start at a time)** | ❌ | ✅ time + weekdays, runs from the tray |
| **Pause while another window is in front** | ❌ | ✅ match by window title |
| **Settings profiles** | ❌ | ✅ named, unlimited |
| **User translations without a rebuild** | ❌ | ✅ `lang/xx.json` overrides |
| **Headless / scriptable run** | ❌ (the compiled .exe covers this) | ✅ `--play … --loops … --no-gui` |
| **Single-instance guard** | ❌ | ✅ focuses the running window |
| **Per-monitor DPI awareness** | ❌ raw pixel coordinates; scaling changes shift every click | ✅ **Per-Monitor v2** + coordinates normalized across the whole virtual desktop |
| **Relative (delta) mouse mode** | ❌ | ✅ toggle — useful for FPS-style camera input |
| **Virtual Desktop isolation (Win 11)** | ❌ | ✅ recording & playback pause when the app isn't on the active desktop |
| **Timing model** | Faithful replay of recorded timing | µs timestamps + `timeBeginPeriod(1)` + hybrid sleep / spin-sleep scheduler |
| **Log file** | ❌ | ✅ rotating daily log |
| **Antivirus false positives** | ⚠️ a known, long-standing issue | ⚠️ same — any input injector looks suspicious |
| **Price** | Free | **Free forever** |

### Where TinyTask still wins 🏆

Credit where it's due — two things TinyTask does that this project cannot:

1. **Size and reach.** 36 KB, 32-bit, runs on Windows XP. Its compiled macros are ~60 KB;
   ours are a ~5 MB copy of this executable, because the player *is* the whole app.
   If you need to email a macro to someone on an old machine, TinyTask wins outright.
2. **A decade of field testing.** TinyTask has been used by an enormous number of people for
   many years. Macro Recorder is young — please [file issues](../../issues).

### Sources for the TinyTask column

Facts above were taken from the official TinyTask site rather than SEO mirrors (several of which contradict each other and the vendor's own changelog):

- Official changelog — <https://www.tinytask.net/revision_history.html>
- Official FAQ — <https://www.tinytask.net/faq.html>
- Official support page (hotkeys, emergency stop) — <https://www.tinytask.net/support.html>
- Official downloads (v1.77, "With Editor" builds) — <https://www.tinytask.net/download.html>
- The Portable Freeware Collection entry — <https://www.portablefreeware.com/index.php?id=1853>

---

## ✨ Features

**Capture**

- 🔴 Mouse movement, clicks, wheel (vertical *and* horizontal), **X1/X2 side buttons**, and the full keyboard — including scancodes and extended keys, so layouts and NumPad behave correctly
- 🎚 Movement sampling is configurable (1–100 ms, default 5 ms), or can be **switched off entirely** for click-only macros
- 🚫 The recorder ignores its own synthetic events *and* your own hotkeys, so neither ends up inside the macro

**Replay**

- ▶ Microsecond scheduling: a 1 ms system timer plus a hybrid sleep/spin-sleep loop, so long macros don't drift
- 🔁 Loop forever, **exactly N times** (1–9999), or **until a time limit**, with an optional delay between loops
- ⏸ **Pause and resume** — the schedule clock stops with you, so nothing fast-forwards afterwards
- ⚡ Speed **0.1× – 3.0×** (and live `faster` / `slower` hotkeys), plus optional **timing jitter** (0–50 %)
- 🖱 Absolute or relative mouse mode, optional **human-like curved movement** and per-click aim spread
- 🛟 Stop always releases whatever the macro was holding down — no stuck Shift, no stuck mouse button

**Decide, don't just replay**

- 🧠 A **[script](SCRIPTS.md)** can wait for things, branch, loop and count — while still replaying slices of your recording
- 🔎 **Image search**: find a button anywhere on screen and click it, even if the layout shifted
- 🔤 **OCR**: react to a word on screen, or read a number (gems, timer, HP) into a variable
- 🎯 **Pixel condition**: watch one pixel and stop — or shut the PC down — when it changes

**Automation & safety**

- ⏱ Time limit in `H : M : S`, then: stop · shut down · restart · sleep · hibernate · log off
- ⏳ Shutdown/restart use a visible countdown (0–600 s, default 60) — `shutdown /a` still aborts it
- 📅 **Scheduler** — start at `HH:MM` on the weekdays you tick, even from the tray
- 🪟 **Target window** — automatically pause while your game isn't the window in front
- 🧭 **Per-monitor DPI aware (v2)** — Windows reports true physical pixels, so 125%/150% scaling doesn't silently offset your clicks
- 🪟 **Virtual Desktop isolation (Windows 11)** — if the app lives on Desktop 2, it neither records nor replays while you're working on Desktop 1
- 🔒 **Single instance** — launching it twice just focuses the existing window

**Interface & files**

- ⌨ 7 rebindable global hotkeys, including a dedicated **emergency stop** (default `F6` / `F7` / `F8` / `F9`)
- 📌 Always on Top toggle
- 🎨 **9 themes** + a transparent UI switch, with Windows 11 **Mica** and **Acrylic** backdrops (and a blur fallback on Windows 10)
- 🌍 **6 languages**, auto-detected and switchable at runtime
- 📦 Macros as plain JSON — or gzipped `.mrz` when size matters — with Open/Save dialogs and a recent-files list
- 💾 Settings in a readable `config.json`, saved on demand and on exit
- 📝 A rotating daily log file for when something behaves oddly
- 🖥 A headless CLI for scripts and scheduled tasks

---

## 🧠 How it works

### Architecture

```mermaid
flowchart LR
    subgraph OS["Windows"]
        KB["WH_KEYBOARD_LL hook"]
        MS["WH_MOUSE_LL hook"]
        HK["RegisterHotKey<br/>record / play / stop / …"]
        SI["SendInput"]
        GDI["BitBlt screen capture"]
        OCRW["Windows.Media.Ocr"]
    end

    subgraph APP["macro-recorder.exe"]
        T1["Hook thread<br/>Win32 message loop"]
        T2["Collector thread"]
        T3["Playback / script thread"]
        T4["UI thread — egui / glow"]
        T5["Scheduler thread"]
        ST[("AppState<br/>atomics + parking_lot")]
    end

    FS["Data folder<br/>config.json · macros · templates · logs"]

    KB --> T1
    MS --> T1
    HK --> T1
    T1 -->|"crossbeam channel"| T2
    T2 -->|"push events"| ST
    T4 <-->|"settings, status"| ST
    ST -->|"snapshot"| T3
    T5 -->|"start at HH:MM"| ST
    T3 --> SI
    GDI --> T3
    OCRW --> T3
    ST <--> FS
```

Nothing blocks the UI, and nothing blocks the hook callback — a low-level hook that stalls gets silently dropped by Windows. So the callback does the absolute minimum: two atomic loads, a cached window handle, a cached virtual-desktop answer, then it hands the event off through a lock-free channel.

### The replay scheduler

Naive `sleep()` loops drift badly over a two-hour macro, and a single long `sleep()` makes the Stop key feel broken. The engine schedules every event against one monotonic clock and never sleeps for more than ~15 ms at a time:

```mermaid
flowchart TD
    A["Next event due in Δt"] --> B{"Δt > 2 ms ?"}
    B -->|yes| C["sleep at most 15 ms<br/>then re-check Stop / Pause"]
    B -->|no| D["spin_sleep for Δt<br/>sub-millisecond accuracy"]
    C --> A
    D --> E["SendInput"]
    E --> F{"End of macro?"}
    F -->|no| A
    F -->|yes| G["count += 1<br/>optional delay, next cycle"]
```

`timeBeginPeriod(1)` is requested for the duration of playback only and released afterwards, so the app doesn't keep the whole system on a high-resolution timer while idle.

### Two playback modes

A macro with **no script** is replayed flat: event by event, on the recorded timing. A macro **with a script** hands control to the interpreter instead, and the recording becomes a library of slices the script can play (`Play events 0…240`).

```mermaid
flowchart TD
    P["Play pressed"] --> Q{"Does the macro<br/>have enabled script steps?"}
    Q -->|no| R["Flat replay<br/>+ jitter, pixel stop, end action"]
    Q -->|yes| S["Script interpreter<br/>blocks resolved up front"]
    S --> T["Play events / Wait / Click image /<br/>If / While / Read number / …"]
    T --> U{"Script finished?"}
    U -->|"loop / count left"| S
    U -->|done| V["Stop"]
```

### State machine

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle --> Recording: record hotkey
    Recording --> Idle: record hotkey / emergency stop
    Idle --> Playing: play hotkey / schedule
    Playing --> Idle: play hotkey / emergency stop / count reached
    Playing --> Paused: Pause button
    Paused --> Playing: Resume button
    Playing --> Held: another virtual desktop / target window not in front
    Held --> Playing: back on the app's desktop, window in front
    Playing --> PowerAction: time limit + shutdown/sleep/…
    PowerAction --> [*]
```

Entering **Paused** or **Held** releases anything the macro was holding down and freezes the schedule clock — so returning after ten minutes resumes exactly where you left off instead of replaying ten minutes of backlog at full speed.

---

## ⌨️ Hotkeys

| Action | Default | Notes |
|---|---|---|
| Start / stop **recording** | `F6` | Rebindable |
| Start / stop **playback** | `F7` | Rebindable |
| **Pause / resume** | `F8` | Rebindable, or use the UI button |
| **Emergency stop** | `F9` | Stops recording *and* playback |
| **Faster** | unbound | ×1.25 speed, applied instantly mid-run |
| **Slower** | unbound | ×0.8 speed, applied instantly mid-run |
| **Skip step** | unbound | Abandons the current step (or the rest of the current `Play events` range) |

All slots are registered globally with `MOD_NOREPEAT`, so they work while any application has focus, and they are filtered out of the recording. Each can be combined with **Ctrl**, **Alt** and **Shift**, and changes apply immediately — no restart.

Click the key button and press anything — letters, digits, function keys. While binding, the global
hotkeys are released so you can even swap `F6` and `F7` around; Esc or 15 seconds of silence cancels.
The ▾ list next to it covers keys the window never receives, such as `Pause`, `ScrollLock` and the
NumPad. **Clear** unbinds a slot entirely.

> If another application already owns one of your combinations, the app says so under **⌨ Hotkeys** instead of failing silently — pick a different key or add a modifier.

---

## 🧠 Scripts

A recording replays what you did. A **script** decides *whether* and *how many times* to do it.

```
0  While  gems < 500
1      Wait for  image: claim_button ≥ 0.85  appears  (10000 ms)
2      Click image: claim_button ≥ 0.85
3      Play events 0…240  (241/241)
4      Read number (1620,40 300x80) → gems
5  End while
6  Quit the app
```

Steps live inside the macro file, so a scripted macro is still one `.json` you can save, share and export to `.exe`.

**17 step kinds:** `Play events` · `Wait` · `Wait for` · `Click image` · `Click at` · `Key` · `Set` · `If` · `Else` · `End if` · `While` · `End while` · `Break` · `Run` · `Quit the app` · `Note` · `Read number`

**6 conditions:** `always` · `variable` · `image` · `pixel` · `window` · `text`

> 📘 **The complete, click-by-click guide lives in [SCRIPTS.md](SCRIPTS.md)** — including every step kind, every condition, the built-in variables, three worked examples and a troubleshooting table. Start there; this section is only the summary.

Open the editor (**✂ Editor → Open editor**), switch to the **Script** tab, pick a kind from the dropdown and press **Add**. Blocks are checked before anything runs: an unbalanced `If` is reported in the editor and the script is refused rather than half-executed.

---

## 🔎 Image search

Under **🔎 Image search** in the main window.

1. Snip the button you care about with `Win+Shift+S`, then press **📋 Paste**. (Or **📂 Load PNG…**.)
2. Press **🔍 Find on screen**. The result reads `Found at (x, y) — 0.973` or `Not found (best 0.412)`.
3. Press **💾 Save PNG…** into the `templates/` folder — scripts refer to templates **by file name**.

**Confidence** (0.30–1.00, default 0.85) is a normalised cross-correlation score: `1.00` is pixel-identical, `0.85` tolerates antialiasing and mild colour shifts. Because the score is normalised, a template still matches when the game's brightness or theme changed.

Fully transparent pixels in a PNG are **excluded from the score**, so you can cut a round icon out of its background in any editor and match it on any backdrop.

| Option | Effect |
|---|---|
| **Try other scales** | Also tests 0.8×, 0.9×, 1.1×, 1.25× — for when the window is a different size than when you snipped it |
| **Search area only** | Restricts the sweep to one rectangle, which is much faster on a 4K screen |

> ⚠️ **Both options apply to the test button only.** Inside a script, image steps always sweep the **whole virtual desktop at 1.0× scale**. If you need a scripted search to be fast, use a smaller template.

The search runs on a worker thread, so the window never freezes; a full-screen sweep takes a moment and shows a spinner.

---

## 🔤 Text on screen (OCR)

Under **🔤 Text on screen**. It uses `Windows.Media.Ocr` — the recognition engine already installed with Windows — so there are **no models to download**. If your game isn't in English, add that language pack in Windows Settings and the engine picks it up automatically.

1. Press **🎯 Pick in 3 s**, hover the **top-left** corner of the region, wait for the countdown, then hover the **bottom-right** corner.
2. The rectangle is captured and read immediately, so you can see exactly what the engine sees.
3. In any script step that needs a region, press **⤵ from the panel** to copy those four numbers in.

Small regions are upscaled automatically (Windows OCR returns nothing at all below ~40×40 px). Text matching is deliberately loose: case, extra whitespace and stray punctuation are ignored, because OCR output never matches a human reading character for character.

Numbers are parsed generously too — `Gems: 1,250` and `1 250` both read as `1250`, and a clock like `02:34` is converted to **154 seconds**.

---

## ✂ Editor

**✂ Editor → Open editor** opens a separate window with three views. Every action is undoable one step back.

| View | What it shows |
|---|---|
| **Story** | Plain English: *"Dragged with Left from (120, 340) to (700, 340)"*, *"Typed "hello""*, *"Waited 1.2 s"* |
| **Raw events** | Every event with its microsecond timestamp — the ground truth |
| **Script** | The program: add, reorder, enable/disable and edit steps |

**Per-action inspector** — click a line and edit its time, key, coordinates, delta, horizontal/extended flags; **Duplicate** or **Delete action**.

**Range operations** — pick a range with `from` / `to`, then:

| Action | What it does |
|---|---|
| **Delete** | Removes the range *and pulls the tail back*, so no silent gap is left behind |
| **Keep only** | Crops to the range and rebases it to t = 0 |
| **Drop moves** | Strips every mouse-movement event, leaving clicks and keys |
| **Trim lead-in** | Shifts everything so the first event happens immediately |
| **Insert pause** | Adds N ms at the selection point and shifts the rest |
| **Scale time ×** | Multiplies every timestamp — 2.0 makes the macro permanently twice as slow |
| **Replace in selection** | Swaps one mouse button for another across the range (Left → Right, …) |
| **Shift coordinates** | Adds `dX` / `dY` to every coordinate in the range |

**Insert click at match** — after a successful image search, one button inserts a real click at the found position, right after the selected action.

The editor is disabled while recording or playing.

---

## 📅 Schedule & target window

**📅 Schedule** — tick *Start at a set time*, choose `HH:MM` and the weekdays. A dedicated thread checks every 5 seconds, so it fires even when the window is minimised to the tray and no longer painting. If a recording or playback is already running at that minute, the launch is skipped and logged rather than stacked.

**🪟 Target window** — type a fragment of the window title (matching is case-insensitive and *contains*, so `roblox` matches `Roblox Player`). With *Pause while it is not in front* enabled, playback holds itself whenever something else takes focus, and resumes on its own when you come back. The status line reads **Waiting for the window…**.

Both are unrelated to **⚓ window anchoring**, which is about *coordinates*: turn on *Remember the target window* while recording, and the app stores the foreground window's title and rectangle inside the macro. On playback, *Follow the anchored window* finds it again and shifts every coordinate by however far it moved — and with *Scale with the window size*, stretches them if it was resized too.

---

## 🧰 Exports & extras

### ⚙ Export to a standalone `.exe`

**Files → Export .exe** produces a player that runs on any Windows PC with nothing installed.
It works by copying this executable and appending the macro to it: a PE image ignores trailing
bytes, which is the same trick self-extracting archives use — no compiler or linker is involved.
On startup the player finds its own footer and plays immediately; the emergency-stop hotkey
still works. The current loop count, speed, mouse mode and inter-loop delay are baked in.

**Scripts are included.** If the script uses image templates, copy the `templates/` folder next to
the exported `.exe` — the player looks for templates in its own folder, not in the original one.

### 📜 Export to AutoHotkey

**Files → Export .ahk** writes an AutoHotkey v2 script: `MouseMove` / `Click` / `Send` with
`Sleep` between events, wrapped in a `Loop`, and `Esc` bound to exit. Keys are emitted as
`{vkXX}` so non-US layouts survive the trip.

> ⚠️ This export covers **recorded events only** — script steps, conditions and variables are not translated.

### 🖥 Tray

Enabled in **Appearance**. Left-click toggles the window, right-click opens a menu with
record / play / emergency stop / exit. Turn on *"Close button minimizes to tray"* and the ✕
hides the window instead of quitting — useful for multi-hour unattended runs.

### 🎯 Pixel stop condition

Watch one screen pixel and stop when it matches a colour (or stops matching). Press
**Pick in 3 s**, hover the target, and both the coordinates and the colour are captured.
Tolerance is a per-channel ±value. The condition is polled about four times a second and,
when it fires, runs the same end action as the timer — so *"stop farming when the HP bar
turns red, then shut down"* is two checkboxes.

> ⚠️ This applies to **flat replay only**. In a scripted macro, use a `pixel` condition inside `Wait for` / `If` / `While` instead.

### 🗂 Profiles

Save the entire configuration under a name into `profiles/<name>.json` and switch between
setups with one click. Recent files are kept across switches.

### 🌍 Translations without a rebuild

Press **Export language template** to write `lang/xx.template.json` — a flat key/value dump of
every UI string. Translate the values, rename it to `lang/xx.json` (`en`, `ru`, `uk`, `pt`,
`es`, `zh`), and restart: your strings replace the built-in ones. Empty values and missing keys
fall back to the defaults, so a partial translation is fine.

---

## 🎨 Themes

| # | Theme | Notes |
|---|---|---|
| 0 | **Dark** | The default. Neutral grays, subtle shadows |
| 1 | **OLED (Pure Black)** | `#000000` panels, zero shadows — true black pixels stay off |
| 2 | **Material Design 3** | 20 px rounded widgets, and it **reads your Windows accent colour** from the registry |
| 3 | **Catppuccin Mocha** | The pastel favourite |
| 4 | **Nord** | Cold arctic blues |
| 5 | **Dracula** | Purple/pink on deep gray |
| 6 | **Glassmorphism** | Translucent panels + **DWM Acrylic** system backdrop |
| 7 | **Neumorphism** | The only light theme — soft shadows on `#E0E5EC` |
| 8 | **Fluent (Mica)** | Windows 11 **Mica** backdrop + your system accent colour |

The **Transparent UI** checkbox works on top of any theme. Glass requests Acrylic and Fluent requests Mica through `DwmSetWindowAttribute`; if the attribute isn't supported (Windows 10), the app falls back to classic `DwmEnableBlurBehindWindow`.

---

## 🌍 Languages

`English` · `Русский` · `Українська` · `Português` · `Español` · `中文`

The UI language is detected from `GetUserDefaultUILanguage()` on first launch and can be overridden in the dropdown at any time — no restart. CJK glyphs are loaded from the system fonts (`msyh.ttc`, `simhei.ttf`, `meiryo.ttc`) when present.

---

## 📁 Files & folders

### Where things live

The app picks its data folder at startup and shows the result under **📁 Files**:

1. **Next to the executable** — if that folder is writable (fully portable: USB sticks, `Downloads`, a game folder);
2. otherwise **`%APPDATA%\MacroRecorder\`** — so it still works from `Program Files` or a read-only location.

```
<data folder>/
├── config.json                  settings
├── macro.json                   default macro slot
├── my-farm.mrz                  gzipped macro (optional)
├── templates/
│   └── claim_button.png         pictures the script searches for
├── profiles/
│   └── farming.json             named settings profiles
├── lang/
│   └── ru.json                  optional translation overrides
└── logs/
    └── macro-recorder.log.YYYY-MM-DD
```

### `macro.json` — the recording (format v3)

`t_us` is microseconds since the recording started; `kind` is an externally-tagged enum. `duration_us` is the full length of the recording **including trailing idle time**, which is what makes a "do stuff, then wait 5 seconds" macro loop correctly.

```json
{
  "version": 3,
  "duration_us": 8000000,
  "anchor": { "title": "Roblox", "x": 100, "y": 80, "w": 1280, "h": 720 },
  "events": [
    { "t_us": 0,      "kind": { "MouseMove":   { "x": 960, "y": 540, "dx": 0, "dy": 0 } } },
    { "t_us": 128340, "kind": { "MouseButton": { "button": "Left", "down": true,  "x": 960, "y": 540 } } },
    { "t_us": 190002, "kind": { "MouseButton": { "button": "Left", "down": false, "x": 960, "y": 540 } } },
    { "t_us": 512900, "kind": { "Key":         { "vk": 65, "scan": 30, "down": true,  "extended": false } } },
    { "t_us": 560110, "kind": { "Key":         { "vk": 65, "scan": 30, "down": false, "extended": false } } },
    { "t_us": 900000, "kind": { "MouseWheel":  { "delta": 120, "x": 960, "y": 540, "horizontal": false } } }
  ],
  "script": [
    { "kind": { "While": { "cond": { "Var": { "name": "n", "cmp": "Lt", "value": 10.0 } } } }, "enabled": true },
    { "kind": { "PlayEvents": { "from": 0, "to": 5 } }, "enabled": true },
    { "kind": { "SetVar": { "name": "n", "op": "Add", "value": 1.0 } }, "enabled": true },
    { "kind": "EndWhile", "enabled": true }
  ],
  "vars": { "n": 0.0 }
}
```

| Field | Meaning |
|---|---|
| `version` | `3` — adds `script` and `vars`; v1 and v2 files still load |
| `t_us` | Timestamp in microseconds from the start of the recording |
| `anchor` | Title and rectangle of the window that was in front when recording started (optional) |
| `Key.vk` / `Key.scan` | Virtual-key code and hardware scancode. **Scancode wins on replay** when non-zero — that's what makes games and non-US layouts behave |
| `Key.extended` | Extended-key flag (arrows, NumPad Enter, right Ctrl/Alt…) |
| `MouseMove.x/y` | Absolute screen coordinates (used in absolute mode) |
| `MouseMove.dx/dy` | Delta since the previous sample (used in relative mode) |
| `MouseButton.button` | `Left` · `Right` · `Middle` · `X1` · `X2` |
| `MouseWheel.delta` | 120 per notch, negative = down/left |
| `MouseWheel.horizontal` | `true` for tilt-wheel / horizontal scroll |
| `script` | The program. Empty (or all steps disabled) means "just replay the events" |
| `vars` | Starting values for the script's variables. Anything unset starts at `0` |

**Compatibility:** version 1 files (a bare `[ … ]` array) and version 2 files still load. **Compression:** saving with a `.mrz` (or `.gz`) extension writes gzipped compact JSON, typically 20–40× smaller; both extensions load transparently.

**Validation on load:** unbalanced script blocks, an empty file, or more than 4 000 000 events are rejected with a message. Out-of-order timestamps are sorted rather than rejected.

### `config.json` — the settings

Written by **💾 Save settings** and automatically on exit. Unknown or out-of-range values are clamped instead of crashing, and missing keys fall back to their defaults — so a config from an older version keeps working.

**Appearance**

| Key | Type | Default | Meaning |
|---|---|---|---|
| `default_lang` | 0–6 | `0` | `0` = auto, `1` EN, `2` RU, `3` UK, `4` PT, `5` ES, `6` ZH |
| `default_theme` | 0–8 | `0` | Index into the theme table above |
| `transparent_ui` | bool | `true` | Translucent window |
| `always_on_top` | bool | `true` | Keep the window above others |
| `tray_enabled` / `close_to_tray` | bool | `true` / `true` | Tray icon; ✕ minimizes instead of quitting |

**Playback**

| Key | Type | Default | Meaning |
|---|---|---|---|
| `loop_play` | bool | `true` | Infinite looping |
| `play_count_limit` | 1–9999 | `1` | Used when `loop_play` is `false` |
| `speed` | 0.05–10.0 | `1.0` | Playback speed multiplier |
| `absolute_mouse` | bool | `true` | Absolute vs relative mouse replay |
| `repeat_delay_ms` | 0–600000 | `0` | Pause between loops |
| `jitter_pct` | 0–50 | `0` | Per-event timing randomisation (flat replay only) |
| `human_mouse` | bool | `false` | Curved cursor paths instead of teleporting |
| `human_curve` | 0–100 | `35` | How far the arc bows away from the straight line |
| `mouse_jitter_px` | 0–60 | `0` | Random spread applied to every target point |
| `use_window_anchor` | bool | `false` | Shift coordinates if the anchored window moved |
| `anchor_scale` | bool | `true` | Also stretch them if it was resized |

**Recording**

| Key | Type | Default | Meaning |
|---|---|---|---|
| `capture_mouse_moves` | bool | `true` | Record movement, not just clicks |
| `mouse_sample_ms` | 1–100 | `5` | Movement sampling interval |
| `record_window_anchor` | bool | `false` | Remember the foreground window when recording starts |

**Time limit & power**

| Key | Type | Default | Meaning |
|---|---|---|---|
| `time_limit_enabled` | bool | `false` | Enable the playback time limit |
| `time_limit_h` / `_m` / `_s` | 0–240 / 0–59 / 0–59 | `0` | Hours / minutes / seconds |
| `action_on_completion` | 0–5 | `0` | `0` stop · `1` shut down · `2` restart · `3` sleep · `4` hibernate · `5` log off |
| `shutdown_delay_s` | 0–600 | `60` | Countdown before shutdown/restart |

**Pixel condition**

| Key | Type | Default | Meaning |
|---|---|---|---|
| `pixel_enabled` | bool | `false` | Stop playback on a screen pixel |
| `pixel_x` / `pixel_y` | i32 | `0` | Watched screen coordinate |
| `pixel_r` / `_g` / `_b` | u8 | `255,0,0` | Target colour |
| `pixel_tolerance` | 0–255 | `20` | Per-channel tolerance |
| `pixel_mode` | 0/1 | `0` | `0` stop when it matches · `1` stop when it differs |

**Hotkeys, schedule, target window**

| Key | Type | Default | Meaning |
|---|---|---|---|
| `hotkey_record` / `_play` / `_pause` / `_stop` | object | F6 / F7 / F8 / F9 | `{ "vk": 117, "ctrl": false, "alt": false, "shift": false }`; `vk: 0` means unbound |
| `hotkey_faster` / `_slower` / `_skip` | object | unbound | Live speed control and step skipping |
| `schedule_enabled` | bool | `false` | Start the macro at a set time |
| `schedule_h` / `schedule_m` | 0–23 / 0–59 | `9` / `0` | When |
| `schedule_days` | bitmask | `127` | Bit 0 = Monday … bit 6 = Sunday |
| `target_title` | string | `""` | Window title fragment (max 120 chars) |
| `target_pause_unfocused` | bool | `false` | Pause while that window isn't in front |

**Files & image search**

| Key | Type | Default | Meaning |
|---|---|---|---|
| `recent_files` | array | `[]` | Up to 8 recent macro paths |
| `compress_on_save` | bool | `false` | Default to `.mrz` when saving |
| `img_threshold` | 0.3–1.0 | `0.85` | Confidence for the test search in the panel |
| `img_multiscale` | bool | `false` | Also try 0.8×–1.25× in the panel |
| `img_region_enabled` | bool | `false` | Restrict the panel search to a rectangle |
| `img_rx` / `_ry` / `_rw` / `_rh` | i32 | `0,0,800,600` | That rectangle |

---

## 💻 Command line

```
macro-recorder [OPTIONS]

  -p, --play <FILE>    Load a macro (.json / .mrz) on start
  -n, --loops <N>      Repeat count (0 = infinite)
  -s, --speed <X>      Playback speed multiplier (0.05 - 10.0)
      --no-gui         Play the macro headless and exit
  -h, --help           Show this help
  -V, --version        Show the version
```

Without `--no-gui` the options simply pre-load the GUI, which is handy for shortcuts:

```powershell
# Preload a macro and start the UI with it
macro-recorder.exe --play "D:\macros\farm.mrz"

# Run it 20 times without a window (Task Scheduler, .bat files, …)
macro-recorder.exe --play "D:\macros\farm.mrz" --loops 20 --speed 1.5 --no-gui
```

Scripts run in headless mode too. The emergency-stop hotkey still works.

---

## 📥 Download

Grab the latest `.exe` from the **[Releases](../../releases)** page. No installation needed.

| File | Requires | Notes |
|---|---|---|
| `MacroRecorder.exe` | Any x86-64 CPU | Universal — runs everywhere |
| `MacroRecorder.v3.exe` | AVX2-capable CPU (Intel Haswell 2013+ / AMD Zen+) | Slightly faster on modern CPUs |

> ⚠️ **Antivirus note:** macro tools install global input hooks and inject synthetic input, so unsigned builds get flagged as suspicious. This is a false positive that affects every tool in this category — TinyTask's own changelog has entries about fighting it too. That's exactly why the source is open: [build it yourself](#️-build-from-source) and trust your own binary.

---

## 🛠️ Build from source

```bash
# 1. Install Rust (1.97.1+, edition 2024): https://rustup.rs
# 2. Clone & build
git clone https://github.com/blackixxce12/Macro-Recorder.git
cd Macro-Recorder

# Universal build
cargo build --release

# Optimized build (AVX2, a few % faster on modern CPUs)
# CMD:
set RUSTFLAGS=-C target-cpu=x86-64-v3 && cargo build --release
# PowerShell:
$env:RUSTFLAGS="-C target-cpu=x86-64-v3"; cargo build --release

# Without the OCR backend (if WinRT bindings ever fail to build)
cargo build --release --no-default-features

# Tests (format round-trips, block balancing, config clamping, scheduler math)
cargo test
```

The binary lands in `target/release/`. Release profile: `opt-level = "z"`, fat LTO, one codegen unit, symbols stripped, `panic = "abort"` — which is why the hook callbacks are written to be panic-free rather than relying on `catch_unwind`.

**Features:** `winocr` is on by default and provides text recognition through `Windows.Media.Ocr`. It ships no models — it uses the language packs already installed in Windows. `--no-default-features` disables it; everything else keeps working, and OCR steps report *"This build has no OCR backend"*.

**Icon:** `build.rs` embeds `assets/icon.ico` into the executable using [`winresource`](https://github.com/BenjaminRi/winresource), which needs a resource compiler — `rc.exe` (Windows SDK, comes with the MSVC toolchain) or `windres.exe` (MinGW). If it isn't found the build still succeeds; you just get a `cargo:warning` and no Explorer icon. The window icon comes from `assets/icon.rgba` and always works.

To watch what the app is doing, either read `logs/macro-recorder.log.*` or build in debug mode (which keeps a console attached) and set `RUST_LOG=debug`.

---

## ⚠️ Known limitations

Honest list — please read before filing a bug:

| Limitation | Detail |
|---|---|
| **Windows only** | Every capture/replay path goes through Win32. Non-Windows targets compile, but do nothing |
| **Pausing drops a drag in progress** | Held keys and buttons are released when you pause, so a macro paused mid-drag resumes without the drag |
| **One macro at a time** | Open/Save, recent files and profiles, but no tabs or queue |
| **Exported `.exe` is ~5 MB** | The player is a copy of the whole app. TinyTask's ~60 KB output is smaller by design |
| **Templates aren't embedded** | An exported `.exe` that searches for images needs the `templates/` folder beside it |
| **AHK export ignores scripts** | Only recorded events are translated — conditions, loops and variables are not |
| **Scripted playback skips some flat-replay features** | Timing jitter, the global pixel stop condition and the end-of-run power action apply to flat replay only. Inside a script use a `pixel` condition and the `Quit the app` step instead |
| **No TinyTask `.rec` import** | The format is undocumented; a guessed parser would corrupt macros silently rather than fail loudly |
| **Coordinates are screen-absolute** | DPI awareness stops Windows from lying about pixels, but a macro still assumes the same window layout as when it was recorded. Maximize your target window before recording, or use anchoring |
| **Scripted image search is full-screen** | The "search area" and "other scales" options apply to the test panel only; script steps always sweep the whole desktop at 1.0× |
| **OCR depends on Windows** | Accuracy and available languages come from the language packs installed on your PC. Stylised game fonts read poorly |
| **Elevated windows** | Windows blocks synthetic input into higher-privilege windows. If your target runs as admin, run this as admin too |
| **Anti-cheat** | `SendInput` is standard synthetic input. Many games accept it; kernel-level anti-cheat may detect or block it |
| **Sleep/hibernate depend on the system** | If hibernation is disabled in Windows, that action fails and is logged rather than silently doing something else |

---

## ❓ FAQ

**Is this an auto-clicker / cheat?**
It's a macro recorder: it replays exactly what *you* did. What you automate is your responsibility — many games and services prohibit automation in their terms of service, and some ban for it. Read the rules of whatever you're automating.

**Do I have to learn scripting to use it?**
No. Record → Play works with no script at all, exactly like TinyTask. Scripts are opt-in for when a blind replay isn't enough — see [SCRIPTS.md](SCRIPTS.md).

**My script clicks the wrong place / never finds the image.**
Lower the confidence a little (0.85 → 0.75), re-snip the template *without* the shadow or the animated part around it, and check that the game is at the same resolution as when you snipped. The [troubleshooting table in SCRIPTS.md](SCRIPTS.md) covers this in detail.

**Why is it 5 MB when TinyTask is 36 KB?**
Because it ships a GPU-accelerated UI toolkit, 9 themes, 6 translations, a template matcher and a power/DPI/virtual-desktop layer. Different trade-off, on purpose. If size is your priority, TinyTask is genuinely the better answer.

**Where did my `config.json` go?**
Next to the exe if that folder is writable, otherwise `%APPDATA%\MacroRecorder\`. The app prints the exact path under **📁 Files**.

**Will my macro survive changing the resolution?**
Coordinates are absolute, so no — re-record after a resolution or monitor-layout change. Changing *DPI scaling* is handled, because the process is Per-Monitor v2 aware. A script built on image search survives far more than one built on fixed coordinates.

**Can I stop the auto-shutdown?**
Yes. It uses a system countdown (60 s by default, configurable) with a visible warning. Run `shutdown /a` in a terminal to abort it.

**Does playback record itself into an infinite loop?**
No. Injected events carry the `LLKHF_INJECTED` / `LLMHF_INJECTED` flag and are discarded by the hooks — as are your own hotkeys.

**Does it work in fullscreen games?**
Borderless/windowed-fullscreen works best. Exclusive fullscreen and raw-input games can be inconsistent, as with any `SendInput`-based tool. Screen capture (for image search and OCR) is also more reliable in borderless mode.

**Should I use `.json` or `.mrz`?**
`.json` while you're iterating — you can read and edit it. `.mrz` for long recordings you just want to keep: same data, roughly 20–40× smaller.

**Which language does OCR read?**
Whatever language packs Windows has installed. Add one in *Settings → Time & language* and restart the app.

---

## 🤝 Contributing

Issues and PRs are welcome. If you're reporting a playback bug, please attach the macro file (or a trimmed version of it), the relevant part of `logs/macro-recorder.log.*`, and your Windows version, display scaling and monitor layout. For a script bug, the `Note` step writes straight into the log — sprinkle a few in and attach the result.

---

# 🛡️ Security & VirusTotal Verification

<p align="center">
  <a href="https://www.virustotal.com/gui/file/21cab5702a58699c1b2f14ac4dec322ea591cfed52cde2bb9e361e22496413a7/">
    <img src="https://img.shields.io/badge/VirusTotal-2%2F71%20Safe-brightgreen?style=for-the-badge&logo=virustotal&logoColor=white&color=2e7d32" alt="VirusTotal Build 1">
  </a>
  <a href="https://www.virustotal.com/gui/file/f345b6cf338ec6cf070a60e1cc594ae08fb41510f472e29c931553888e9c29a4/">
    <img src="https://img.shields.io/badge/VirusTotal-2%2F71%20Safe-brightgreen?style=for-the-badge&logo=virustotal&logoColor=white&color=2e7d32" alt="VirusTotal Build 2">
  </a>
  <a href="#-why-do-false-positives-occur">
    <img src="https://img.shields.io/badge/Status-False%20Positives%20Verified-blue?style=for-the-badge&logo=shield&logoColor=white" alt="False Positives Verified">
  </a>
</p>

---

> [!NOTE]
> **Safety Notice:** All release binaries automatically undergo VirusTotal verification prior to every release. Out of **71 antivirus vendors**, 69 confirm the files are completely clean. The 2/71 detections are **100% False Positives**, caused by heuristic analysis of low-level Win32 input APIs and the lack of a paid code-signing certificate.

---

## 📊 VirusTotal Scan Results

| File / Build | SHA-256 Hash | VT Detection | VirusTotal Report |
| :--- | :--- | :---: | :---: |
| **Release Build 1** | `21cab5702a58699c1b2f14ac4dec322ea591cfed52cde2bb9e361e22496413a7` | <mark>**2 / 71**</mark> | [🔍 View Report](https://www.virustotal.com/gui/file/21cab5702a58699c1b2f14ac4dec322ea591cfed52cde2bb9e361e22496413a7/) |
| **Release Build 2** | `f345b6cf338ec6cf070a60e1cc594ae08fb41510f472e29c931553888e9c29a4` | <mark>**2 / 71**</mark> | [🔍 View Report](https://www.virustotal.com/gui/file/f345b6cf338ec6cf070a60e1cc594ae08fb41510f472e29c931553888e9c29a4/) |

---

## ❓ Why Do False Positives Occur?

System-level input automation and simulation utilities frequently trigger heuristic warnings from lesser-known antivirus engines due to the following reasons:

1. **Low-Level Win32 APIs (`SendInput`, `SetWindowsHookEx`)**
   * Standard Windows API functions are used to intercept hotkeys and execute macros or emulate mouse and keyboard actions. Some heuristic scanners mistakenly flag global input hooks as potential keyloggers or autoclickers.
2. **Lack of a Commercial Digital Certificate (Code Signing)**
   * Signing `.exe` files with EV code-signing certificates is expensive. Unsigned binaries from open-source projects receive lower reputation scores from Windows SmartScreen and AI-driven antivirus engines.
3. **Rust Compiler Optimizations**
   * Compiling with target optimization flags (such as LTO and `x86-64-v3` instruction sets) produces machine code patterns that automated scanners sometimes misinterpret as generic unknown threats (`Heur.BKG`, `Trojan.Generic`).

---

## 🔒 Transparency & Verification

This project is fully **Open Source**, giving you full control over what runs on your system:

<details>
<summary><b>🛠️ SHA-256 Checksum Verification</b></summary>

<br>

To verify that your downloaded `.exe` file matches the audited VirusTotal build, run the following command in PowerShell:

```powershell
Get-FileHash -Algorithm SHA256 .\your_file_name.exe
```

</details>

---

## 📄 License & credits

MIT — see [LICENSE](LICENSE).

Built with [egui / eframe](https://github.com/emilk/egui), [windows-rs](https://github.com/microsoft/windows-rs),
[serde](https://serde.rs), [crossbeam](https://github.com/crossbeam-rs/crossbeam),
[parking_lot](https://github.com/Amanieu/parking_lot), [spin_sleep](https://github.com/alexheretic/spin-sleep),
[image](https://github.com/image-rs/image), [rfd](https://github.com/PolyMeilex/rfd) and
[tracing](https://github.com/tokio-rs/tracing).

Inspired by **TinyTask** — thanks for a decade of quietly saving people's hands.
