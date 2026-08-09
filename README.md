<div align="center">

# 🦀 Macro Recorder

**A modern, open-source alternative to TinyTask.**
*Born from Roblox grind. Forged in Rust.*

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Windows](https://img.shields.io/badge/Windows-10%20%2F%2011-0078D6?logo=windows&logoColor=white)]()
[![Rust](https://img.shields.io/badge/Made%20with-Rust-orange?logo=rust&logoColor=white)]()
[![egui](https://img.shields.io/badge/UI-egui%20%2F%20eframe-blue)]()
[![Latest Release](https://img.shields.io/github/v/release/blackixxce12/Micro-Recorder?label=release&color=green)](https://github.com/blackixxce12/Micro-Recorder/releases)

*Record mouse & keyboard → replay it forever (or exactly N times) → go drink some tea.* ☕

[📥 Download](../../releases) • [✨ Features](#-features) • [🆚 vs TinyTask](#-tinytask-vs-macro-recorder) • [🇷🇺 Русская версия](README_RU.md)

<!-- Put a screenshot file (screenshot.png) next to this README and uncomment:
![screenshot](screenshot.png)
-->

</div>

---

## 📖 The Story: Roblox, anime tower defenses, and a tired hand

I play a lot of **Roblox** — especially anime tower defense games. If you've ever played one, you know *the loop*:

> Place units → wait for the wave → collect gems → upgrade → repeat.
> And again. And again. **Hundreds of times per session.**

One evening, after manually clicking the same "summon / upgrade / claim" buttons for the third hour in a row, my hand said *«no»*. So I did what everyone does — I downloaded **TinyTask**.

It worked… for about 10 minutes. Then the cracks started showing:

- 🦖 **UI straight out of Windows XP** — hasn't aged well since 2007;
- 🐛 **Closed source Win32 blob** with known input-buffer bugs that silently drop events during long recordings (my 2-hour farm macro kept desyncing after ~40 minutes);
- 🎨 **Zero customization** — same gray tray tool everyone else uses;
- 🐌 **No speed control** — want to replay 2× faster to test a macro? *Too bad*;
- 🚫 **No save-as-defaults** — every launch you re-configure it;
- 🔒 **Proprietary format** — want to edit a macro by hand? *Want harder*.

And then I thought: **I want my own macro recorder.** One that:

- looks like a 2026 app, not a 2007 artifact;
- loops **exactly N times** *or* for a **set amount of time** (even shuts down the PC after);
- stays **on top of the game window** with optional **translucent glass UI**;
- and is **fully mine** — open for anyone to read, build and audit.

That weekend project got slightly out of hand. 🦀

---

## 🦀 Why Rust?

| Reason | What it means for you |
|---|---|
| **Single .exe** | No installers, no .NET, no Python runtime — one file, double-click, done |
| **Fearless concurrency** | One thread captures low-level hooks, one replays with microsecond timing, one draws the UI — and they never fight |
| **Memory safety** | A tool that injects input into your system shouldn't crash mid-raid or overflow an event buffer. Rust makes sure of that — no mystery bugs from 17-year-old Win32 code |
| **Tiny & instant** | With LTO + strip the whole app is a few MB and starts instantly |
| **Honest reason** | I wanted a real excuse to learn Rust properly. Best way to learn — build something you actually use |

---

## 🆚 TinyTask vs Macro Recorder

| Feature | TinyTask | Macro Recorder |
|---|:---:|:---:|
| License | Freeware, **closed source** | **MIT, fully open source** |
| Era | 2007 vibes (still) | 2026 |
| UI | Classic gray tray tool, WinForms-era | Modern GPU-rendered GUI, **8 themes** (incl. Windows 11 **Mica / Acrylic blur**), **transparent glass mode** |
| Languages | English only | **6 languages** (EN, RU, UK, PT, ES, ZH) + auto-detection |
| Input capture | Mouse + keyboard | Mouse + keyboard + **wheel + X1/X2 buttons** |
| Loop mode | Infinite **or** N times | Infinite, **N times, or timed** (hours + minutes) |
| Timed playback | ❌ | ✅ stop after H hours M minutes |
| End action | ❌ | **Stop playback OR shut down the PC** |
| Playback speed | Fixed | **0.1× – 3.0×** |
| Recording timer | ❌ | ✅ live + final duration |
| Playback counter | ❌ | ✅ live counter in UI |
| Global hotkeys | Fixed (F8/F9 equivalent) | **F8 / F9** — work in any window, even in-game |
| Always on top | ✅ | ✅ toggleable |
| UI transparency | ❌ | ✅ **full window translucency** (not yet)|
| Settings persistence | ❌ — reconfigure every launch | ✅ **save-as-default** via `config.json` |
| Macro format | Proprietary binary, prone to corruption | **Human-readable JSON**, editable by hand |
| High-DPI / multi-monitor | So-so | Per-monitor DPI aware, absolute-mouse fix |
| Code quality | ~17 years of closed Win32 C with known buffer-overrun bugs | ~1500 lines of auditable Rust, zero unsafe beyond FFI |
| Size | ~40 KB | ~10 MB *(ships a modern GPU UI, 8 themes and 6 translations — the 960 MB `target/` folder stays on my machine, promise)* |
| Price | Free | **Free forever** |

---

## ✨ Features

- 🔴 **Record** everything: mouse moves, clicks, wheel, X-buttons, keyboard
- ▶ **Play back** with precise timing (`spin_sleep` + microsecond scheduling)
- 🔁 **Loop** forever, **exactly N times**, or **until a time limit**
- ⏱ **Time limit**: stop after H hours / M minutes, with optional **auto-shutdown**
- ⚡ **Speed control** 0.1× – 3.0×
- ⌨ **Global hotkeys**: `F8` record, `F9` play — work over any window
- 📌 **Always on Top** mode for gaming
- 🪟 **Transparent UI** — glass-through window via DWM per-pixel alpha
- 🎨 **8 themes**: Dark, Material Design 3, Fluent (Mica), Catppuccin, Nord, Dracula, Glassmorphism (Acrylic), Neumorphism
- 🌍 **6 languages**: English, Русский, Українська, Português, Español, 中文 — auto-detected from system, overridable
- 💾 **Save settings as default** — every future launch remembers your theme / language / hotkeys
- 📦 Save / load macros as **human-readable JSON**

---

## ⌨️ Hotkeys

| Key | Action |
|---|---|
| `F8` | Start / stop recording |
| `F9` | Start / stop playback |

---

## 📥 Download

Grab the latest `.exe` from the **[Releases](../../releases)** page. No installation needed.

Two builds are available:

| File | Requires | Notes |
|---|---|---|
| `MacroRecorder.exe` | Any x86-64 CPU | Universal — runs everywhere |
| `MacroRecorder.v3.exe` | CPU with AVX2 (Intel Haswell 2013+ / AMD Ryzen+) | Faster, smaller binary |

> ⚠️ **Antivirus note:** macro tools inject input, so some antiviruses may flag the unsigned exe as suspicious (false positive). This is normal for this kind of software — that's exactly why the source is open: feel free to build it yourself.

---

## 🛠️ Build from source

```bash
# 1. Install Rust: https://rustup.rs
# 2. Clone & build
git clone https://github.com/blackixxce12/MacroRecorder.git
cd MacroRecorder

# Universal build
cargo build --release

# Optimized build (AVX2, ~5-20% faster on modern CPUs)
# In CMD:
set RUSTFLAGS=-C target-cpu=x86-64-v3 && cargo build --release
# In PowerShell:
$env:RUSTFLAGS="-C target-cpu=x86-64-v3"; cargo build --release
