# Changelog

All notable changes to Macro Recorder are recorded here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [Semantic Versioning](https://semver.org/).

[🇷🇺 Русская версия](CHANGELOG_RU.md)

---

## [1.3.0]

A hardening release. No new user-facing features — this is what a seven-stage test
campaign found, fixed and measured. The plan is in [TESTING.md](TESTING.md).

### Fixed

- **The editor could take the whole application down.** `editor_set_time` read the
  previous event's timestamp with a raw index while reading both of its neighbours
  through `get`, and that read sits ahead of the guard meant to cover the function. A
  selection left pointing past a recording another edit had since trimmed reached it
  first; `panic = "abort"` in the release profile means the process goes, not the
  operation. Found by the new editor fuzzing, which is what it was written for.
- **A small template was searched for pathologically slowly.** The coarse grid was
  chosen from the template alone, ignoring the haystack it would sweep, so a 32 px
  template on a 2560x1440 screen was handed a step of 2 and examined a quarter of every
  pixel position with a 16x16 kernel — 236 million operations against 25 million for a
  64 px one. Measured: 465 ms against 64 ms, seven times slower for the smaller picture.
  The grid is now coarsened until the pass fits a fixed budget, and only then, so a
  small search area keeps the finer grid where it is cheap and its accuracy is worth
  having. The same template now takes 48 ms, matches land in exactly the same place, and
  the multi-scale option dropped from 390 ms to 274 ms with it.
- **"1 % low" did not report the worst 1 %.** It computed a 99th percentile, which for
  a hundred samples stops one sample short of the worst — exactly the sample the label
  promises. It is now the mean of the worst 1 %, matching the frame-time vocabulary the
  name borrows from, and averaging the tail keeps one freak sample from deciding the
  figure the frame guard is sized from.
- **`cargo test` did not pass.** `roundtrip_v2` asserted that a saved macro comes back
  as format version 2, but `MacroData::new` has emitted version 3 since the script
  engine landed. `cargo build` never noticed, because a wrong assertion is still a
  well-typed one. The test now checks against `format_version()` rather than a literal,
  so the next format bump cannot repeat it.
- The `MacroData` doc comment still described the container as "format version 2".

### Changed

- `editor_insert_delay` multiplies its millisecond argument saturatingly. The UI bounds
  that argument, so nothing could reach the overflow — but a function that is total only
  because of its callers is a trap for the next caller.
- Slip events are counted rather than only logged.

### Added

- **21 tests**, taking the suite from 60 to 83, including 8 000 rounds of fuzzing over
  the editor's range and single-index operations with deliberately wrong indices, and
  fuzzing of `sanitize` against absurd values, NaN and infinity. Seeded from fixed
  values, so a failure reproduces exactly. Tests build without `--release`, which means
  overflow checks are on and any arithmetic that would wrap silently in production
  panics there instead.
- **`--selftest timing`.** The scheduler cannot be judged from outside the process — an
  event that fires 40 ms late looks exactly like one that fires on time — so this runs
  the real `playback_loop`, the real frame guard and the real slip logic with every call
  into Windows suppressed, and timestamps each dispatch against the moment it was due.
  Nine scenarios covering speed extremes, a forced stall, the guard, and human-like
  movement.
- **`--selftest vision`.** Times capture and search across region and template sizes,
  prices the multi-scale option, times OCR, and reports the cost of one script image
  step. Costs are given per megapixel so a screen larger than the one under test can be
  worked out rather than guessed at.
- **`--selftest churn[=seconds]`.** Drives the playback lifecycle at about a hundred
  transitions a second while asserting that no generation escapes cancellation, that two
  playback loops never run at once, and that every stop leaves nothing held down. A
  watchdog aborts the run if no transition completes for thirty seconds, because a
  wedged process cannot report on itself.
- **`--selftest soak[=hours]`.** Replays continuously while capturing and reading the
  screen, and samples its own private bytes, handle count and GDI objects. Growth is
  measured from the five-minute mark so allocator warm-up is not reported as a leak, and
  every row is appended to `logs/soak.csv` because the Windows console stops accepting
  writes while text is selected in it.

### Measured

Figures the documentation previously gave in words:

| | |
|---|---|
| Scheduler accuracy, p99 | 4 µs; 103 µs in the worst scenario, measured under load from a running game |
| Drift | none measurable — wall clock matched the recording to the millisecond in all nine scenarios |
| Recovery from a 400 ms stall | one slip, and zero dispatches within 500 µs of each other afterwards: the backlog does not go out as a burst |
| Frame guard cost, human-paced recording | +10 % wall clock |
| Full-screen capture, 2560x1440 | 43 ms |
| Full-screen search, 64x64 template | 68 ms; a miss costs the same as a hit, since `find` has no early exit |
| One script image step | 111 ms, so a `While` polling for a picture runs at about 9 checks a second |
| Lifecycle churn | 33 000 transitions over two runs: no press left held, no generation escaped cancellation, nothing wedged |
| Soak, 2.5 h | 3 832 captures and 1 743 OCR reads; handle count flat at 212, memory settled at 12.8 MB |

### Not done

Stated plainly rather than left for someone to discover.

- **The manual feature matrix ([TESTING_MATRIX.md](TESTING_MATRIX.md)) has not been
  worked through.** Every automated stage ran dry: `SendInput` was suppressed
  throughout. The frame guard, the slip logic and human-like movement have measured
  timings but have never actually pressed a key on a live system.
- **Script image steps still sweep the whole virtual desktop.** Giving them their own
  search region would take the poll rate from about 9 a second to about 70. It changes
  the macro format, so it is held over.
- **The soak's GDI figure measures nothing.** Sampling and capture run on the same
  thread in sequence, so the sample can never land inside a `BitBlt` and the count reads
  zero whether objects leak or not. The absence of a leak is inferred instead from
  3 832 captures completing without exhausting the per-process quota.
- One unexplained event in the first churn run: a single release sent with no press
  behind it, on the harmless side and not reproduced in 33 000 further transitions. The
  mechanism proposed for it predicted roughly a hundred occurrences per run, so that
  explanation is wrong and the cause is still open.

---

## [1.2.0]

Window-related settings gathered into one place, and three things that quietly did not work.

### Added

- **The `🖥 Target window` section now holds everything that depends on which window
  the macro is aimed at**, in three groups: which window it is, how coordinates follow
  it, and how well it keeps up. The frame guard, its automatic mode, the responsiveness
  readout, *Follow the anchored window*, *Scale with the window size* and *Remember the
  target window* have all moved here from `▶ Playback` and `🎬 Recording`, and the
  separate responsiveness section is gone.
- **`⤵ From the recording`** fills the target title from the window the recording was
  made against, so it never has to be typed by hand. It needs *Remember the target
  window* to have been on while recording; the anchored title is shown beside the
  button either way, and the button is disabled when there is none.
- **A dropdown of saved templates** beside the name field in `Click image` steps and in
  `image` conditions. A script using several pictures no longer needs any of their file
  names typed from memory. The list is read when the dropdown opens, so a template saved
  a moment ago is already in it.
- `templates/`, `profiles/`, `lang/` and `logs/` are now created at startup.

### Changed

- Human movement is seeded from where the pointer actually is, so the first jump of a
  run curves like every other one instead of teleporting.
- Time spent drawing a curved path is charged to the playback schedule. Each curve costs
  up to ~60 ms, and until now that was stolen from the events behind it, which then
  bunched up to make the difference back.
- The *Human-like movement* hint is shown under the setting instead of only on hover,
  and says plainly when it applies.

### Fixed

- **Human movement appeared to do nothing.** It draws a curve only when the pointer has
  to jump more than about 24 px, and a recording samples movement every 5 ms — so
  consecutive points are a few pixels apart and the threshold is never reached. That is
  correct behaviour, since a recording already contains real human movement, but it was
  indistinguishable from a broken setting. It applies to click-only macros (*Capture
  mouse movement* off) and to the `Click at` and `Click image` script steps, and the UI
  now says so.
- **`templates/` was not created until a PNG had been saved into it.** Folders were made
  on first use, which is no help to somebody who wanted to drop a picture in beforehand.

### Notes

- A script can use as many pictures and text regions as it likes. Every `Click image`,
  `Wait for`, `If` and `While` step carries its own template name, threshold, region and
  search text, and templates are cached per run, so a chain like *Game Results →
  Claim Rewards → the icon that opens the menu* is just three steps with three different
  templates. The only thing that was awkward was typing the names, which the new
  dropdown solves.

---

## [1.1.0]

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

