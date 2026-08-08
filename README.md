<div align="center">

# 🦀 Macro Recorder

**A modern, open-source alternative to TinyTask.**
*Born from Roblox grind. Forged in Rust.*

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Windows](https://img.shields.io/badge/Windows-10%20%2F%2011-0078D6?logo=windows&logoColor=white)]()
[![Rust](https://img.shields.io/badge/Made%20with-Rust-orange?logo=rust&logoColor=white)]()
[![egui](https://img.shields.io/badge/UI-egui%20%2F%20eframe-blue)]()
[![Latest Release](https://img.shields.io/github/v/release/blackixxce12/MacroRecorder?label=release&color=green)](../../releases)

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

It worked… but it felt like a relic from another era:

- ️ UI straight from the Windows XP days;
- 🔁 loop forever **or nothing** — no «play exactly 5 times»;
- 🐌 no playback speed control;
- 📦 closed source — want a feature? *Want harder.*

And then I thought: **I want my own macro recorder.** One that:

- looks like a 2026 app, not a 2007 artifact;
- loops **exactly N times** so I can sleep while my units farm;
- stays **on top of the game window**;
- and is **fully mine** — open for anyone to read, build and improve.

That weekend project got slightly out of hand. 🦀

---

## 🦀 Why Rust?

| Reason | What it means for you |
|---|---|
| **Single .exe** | No installers, no .NET, no Python runtime — one file, double-click, done |
| **Fearless concurrency** | One thread captures low-level hooks, one replays with microsecond timing, one draws the UI — and they never fight |
| **Memory safety** | A tool that injects input into your system shouldn't crash mid-raid. Rust makes sure of that |
| **Tiny & instant** | With LTO + strip the whole app is a few MB and starts instantly |
| **Honest reason** | I wanted a real excuse to learn Rust properly. Best way to learn — build something you actually use |

---

## 🆚 TinyTask vs Macro Recorder

| Feature | TinyTask | Macro Recorder |
|---|:---:|:---:|
| License | Freeware, closed source | **MIT, fully open source** |
| Era | 2007 vibes | 2025 |
| UI | Classic tray tool | Modern GPU-rendered GUI, **8 themes** (incl. Windows 11 **Mica / Acrylic blur**) |
| Languages | English | **EN + RU**, auto-detected |
| Input capture | Mouse + keyboard | Mouse + keyboard + **wheel + X1/X2 buttons** |
| Loop mode | Infinite only | **Infinite or exactly N times** |
| Playback speed | Fixed | **0.1× – 3.0×** |
| Recording timer | ❌ | ✅ live + final duration |
| Playback counter | ❌ | ✅ |
| Global hotkeys | Fixed | **F8 / F9 — work in any window**, even in-game |
| Always on top | ✅ | ✅ with a toggle |
| Macro format | Proprietary binary | **Human-readable JSON** |
| High-DPI / multi-monitor | So-so | Per-monitor DPI aware, absolute-mouse fix |
| Size | ~40 KB | ~10 MB *(yes, bigger — it ships a modern GPU UI and 8 themes; the 960 MB `target/` folder stays on my machine, promise)* |
| Price | Free | **Free forever** |

---

## ✨ Features

- 🔴 **Record** everything: mouse moves, clicks, wheel, X-buttons, keyboard
- ▶ **Play back** with precise timing (`spin_sleep` + microsecond scheduling)
- 🔁 **Loop** forever or **exactly N times**
- ⚡ **Speed control** 0.1× – 3.0×
- ⌨ **Global hotkeys**: `F8` record, `F9` play — work over any window
- 📌 **Always on Top** mode for gaming
- ⏱ Recording timer & 🔁 play counter right in the UI
- 🎨 **8 themes**: Dark, Material Design 3, Fluent (Mica), Catppuccin, Nord, Dracula, Glassmorphism (Acrylic), Neumorphism
- 🌍 **EN / RU** interface with system auto-detection
- 💾 Save / load macros as **JSON**

---

## ⌨️ Hotkeys

| Key | Action |
|---|---|
| `F8` | Start / stop recording |
| `F9` | Start / stop playback |

---

## 📥 Download

Grab the latest `MacroRecorder.exe` from the **[Releases](../../releases)** page. No installation needed.

> ⚠️ **Antivirus note:** macro tools inject input, so some antiviruses may flag the unsigned exe as suspicious (false positive). This is normal for this kind of software — that's exactly why the source is open: feel free to build it yourself.

---

## 🛠️ Build from source

```bash
# 1. Install Rust: https://rustup.rs
# 2. Clone & build
git clone https://github.com/blackixxce12/MacroRecorder.git
cd MacroRecorder
cargo build --release
