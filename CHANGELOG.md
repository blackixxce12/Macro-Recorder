# Changelog

All notable changes to Macro Recorder are recorded here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [Semantic Versioning](https://semver.org/).

[🇷🇺 Русская версия](CHANGELOG_RU.md)

---

## [1.1.0] — unreleased

Playback that survives a target application which cannot keep up.

### Fixed

- The `Target window` and `Window responsiveness` section headers drew as empty
  boxes. Both used emoji added in Unicode 12 and 13 (🩺 U+1FA7A, 🪟 U+1FA9F), and
  the emoji font egui bundles stops before those — every other glyph in the app is
  Emoji 11 or older, which is why nothing else was affected. They are now 🖥 and 📊,
  and a unit test fails the build if a glyph from that range is ever added again.

### Added

- **Frame-rate guard** (`▶ Playback` → *Frame-rate guard*). A game rendering at 15 FPS
  looks at its input queue about once every 67 ms, so a recorded click that lasted 8 ms
  is never seen: the button goes down and back up between two polls. The guard enforces
  three spacings, all derived from one frame time — a press is held for two frames, a
  re-press waits one frame after the release before it, and a click waits one frame
  after the cursor moved so hit-testing has caught up. It only ever lengthens: a macro
  can get slower, never faster.
  **Off by default** — most macros drive ordinary desktop software, which reads its
  queue as fast as the queue fills.
- **Automatic sizing** (*Set it from the window automatically*, on by default once the
  guard is enabled). The guard follows the measured responsiveness of the target window
  instead of a figure you have to guess. The configured FPS is the fallback used until
  a measurement exists.
- **Window responsiveness panel** (`📊 Window responsiveness`) showing frame time,
  average FPS, 1 % low, 0.1 % low and a stutter count over a rolling ten seconds.
  Requires a title under `🖥 Target window`.
- **New settings** in `config.json`:

  | Key | Type | Default | Meaning |
  |---|---|---|---|
  | `frame_guard` | bool | `false` | Enable the guard |
  | `frame_guard_fps` | 5–240 | `30` | Slowest expected frame rate, used when nothing is measured |
  | `frame_guard_auto` | bool | `true` | Size the guard from the measurement instead |
  | `perf_enabled` | bool | `false` | Keep the responsiveness panel updating |

- Three unit tests for the guard's spacing rules and one for the percentile maths.

### Changed

- **Playback no longer bursts to catch up after a stall.** The scheduler was drift-free
  against the start of the run, so a 400 ms hitch left several events already overdue
  and they went out back to back — the whole backlog landing in a single frame, which
  is exactly what a struggling application cannot absorb. Falling more than six frames
  behind now slips the entire schedule to the present instead of racing it. The slip is
  logged.
- **`Click at` and `Click image` script steps** held the button for a hardcoded 30 ms.
  That is under half a frame at 15 FPS. The hold now comes from the guard.
- Those two steps no longer re-send the click coordinates in the button event. The
  cursor has already been moved there, and sending them again moved a second time,
  re-rolling *Aim spread* and landing the click a few pixels from where it was aimed.
- `platform::find_window_rect` is now a thin wrapper over a new internal handle lookup,
  which the probe reuses. Behaviour is unchanged.

### Notes

- **The responsiveness figures are not a frame counter and do not claim to be.** Reading
  another process's real present timings means an ETW session against the DXGI providers
  — what PresentMon does — which needs administrator rights and a schema parser larger
  than this whole program. `DwmGetCompositionTimingInfo` is no substitute either: since
  Windows 8.1 it reports the compositor, which keeps ticking at the monitor's refresh
  rate however badly the game underneath is doing.
  What is measured instead is the round-trip of an empty `WM_NULL` through the target
  window's own message loop. A normal game loop drains its queue once per frame, so the
  answer arrives within about one frame — and that is precisely the delay the guard has
  to cover, because input is handled on the thread that pumps. Where a game renders on
  a separate thread the number tracks input handling rather than the rendered frame
  rate, which for this purpose is the more useful of the two. For true frame statistics,
  run PresentMon, CapFrameX or RTSS alongside.
- The probe sends one message every 25 ms, far below the rate of ordinary mouse input,
  and needs no elevation. It is inert by design: `WM_NULL` makes the window procedure
  return immediately, so what is timed is the wait in the queue rather than any work
  the message caused.
- The guard is sized from the worst 1 % of samples rather than the average. A press has
  to survive the slow frames, not the comfortable ones.

---

## [1.0.0]

First public release.

Recording and replay of mouse, keyboard, wheel and X1/X2 with microsecond timing ·
loop forever, N times or until a time limit, with shutdown/restart/sleep/hibernate/log-off ·
per-monitor DPI awareness · virtual desktop isolation · window anchoring · pixel stop
condition · built-in editor with three views · script engine with 17 step kinds,
6 conditions and variables · image search · OCR through `Windows.Media.Ocr` · scheduler ·
target window · 7 rebindable hotkeys · 9 themes · 6 languages · `.exe` and AutoHotkey
export · settings profiles · headless CLI.
