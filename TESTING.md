# Testing plan

A staged plan for hardening Macro Recorder. Stages 1 to 6 are complete; stage 7 is
outstanding. Each stage says who did the work and what came out of it.

**The split that shaped everything below.** Claude can read the source and write test
code, but cannot compile or run anything — no Rust toolchain, no Windows, no display.
So every stage is one of three kinds:

| Kind | Meaning |
|---|---|
| 📖 **Static** | Done by reading the source. No build needed. |
| 🔧 **Harness** | Claude writes the code, you run it and report the numbers. |
| 🖐 **Manual** | Only a real Windows box, a real game and a real night will do. |

`cargo build` succeeding proves the types line up. It proves nothing about timing under
load, thread interleaving, or hour nine — which is what the rest of this was for.

---

## Status

| # | Stage | Kind | State | Outcome |
|---|---|---|---|---|
| 1 | [Panic and safety audit](#stage-1--panic-and-safety-audit) | 📖 | done | Hot paths clean; the one crash was found later by stage 2 |
| 2 | [Deterministic tests](#stage-2--deterministic-tests) | 🔧 | done | 21 tests added; a crash and a red test found |
| 3 | [Timing under load](#stage-3--timing-under-load) | 🔧 | done | p99 4 µs, no drift, no burst after a stall |
| 4 | [Vision and OCR benchmark](#stage-4--vision-and-ocr-benchmark) | 🔧 | done | A 7× performance bug found and fixed |
| 5 | [Concurrency churn](#stage-5--concurrency-churn) | 🔧 | done | 33 000 transitions, nothing left held |
| 6 | [Long-run soak](#stage-6--long-run-soak) | 🔧 | done | 2.5 h, handles flat, memory settled |
| 7 | [Feature matrix](#stage-7--feature-matrix) | 🖐 | **outstanding** | See [TESTING_MATRIX.md](TESTING_MATRIX.md) |

---

## Stage 1 — Panic and safety audit

📖 **Result: hot paths clean.**

`panic = "abort"` is set in the release profile, so any panic anywhere kills the process
instantly — mid-macro, with keys still held. That makes every reachable panic a
total-loss bug, which is why this went first.

Checked and found correctly guarded: `play_event_range` against a script step naming
events a later edit deleted; all six editor range operations against reversed, empty and
out-of-range selections; `vision::find_at_scale` against a template larger than the
search area; `CoordMap::build` against a zero-sized anchor; `perf::summarize` against an
empty sample set.

Reading did **not** find `editor_set_time`, which indexed one of its three neighbours
raw. Stage 2's fuzzing did, within seconds. Worth remembering: a careful read of a
9 000-line file is no substitute for a machine trying ten thousand inputs.

Still outstanding inside this stage: the preconditions of each `unsafe` block, a written
lock ordering across the six threads, and the self-running `.exe` footer parser, which
reads a length out of an untrusted tail and then trusts it.

---

## Stage 2 — Deterministic tests

🔧 **Result: 60 → 83 tests. Two real faults.**

- **`cargo test` had never been run, and did not pass.** `roundtrip_v2` asserted format
  version 2 against code that has emitted 3 since the script engine landed. A wrong
  assertion is still a well-typed one, so the build never complained.
- **`editor_set_time` panicked on a stale selection**, taking the whole process with it.
  Found by fuzzing 8 000 rounds of range and single-index operations with deliberately
  wrong indices.

Also added: fuzzing of `sanitize` against absurd values, NaN and infinity; macro-format
round-trips including v1, gzip and malformed input; block resolution 500 levels deep;
and the frame guard's automatic retuning. All seeded, so failures reproduce exactly.

Tests build without `--release`, so overflow checks are on — arithmetic that would wrap
silently in production panics in the suite instead.

---

## Stage 3 — Timing under load

🔧 **Result: clean, with room to spare.** `--selftest timing`

Run with a game in the background, so these are loaded figures.

| | |
|---|---|
| p99 lateness | 4 µs baseline, 103 µs worst scenario |
| Drift | none; wall clock matched the recording to the millisecond in all nine scenarios |
| 400 ms stall | one slip, `burst 0` — no two dispatches within 500 µs of each other afterwards |
| Frame guard, human-paced recording | +10 % wall clock, and p99 *improved* to 4 µs |

The stall row is the one that mattered. Before 1.1.0 a 400 ms freeze would have pushed
roughly eighty overdue events out back-to-back; it now slips in real time instead.

A confirmation arrived unasked: the 0.1× scenario recorded a slip of 258 ms with no
stall injected. The operating system genuinely starved the thread, the threshold fired,
and the wall clock grew by exactly 258 ms.

One harness fault was found mid-stage: `due` was read before the guard moved it, so a
guard hold was recorded as lateness and the guard rows read 61 681 µs instead of 103.

---

## Stage 4 — Vision and OCR benchmark

🔧 **Result: a 7× performance bug, found and fixed.** `--selftest vision`

Measured on 2560x1440. The documentation had said a full-screen sweep "takes a moment".

| | |
|---|---|
| Full-screen capture | 43 ms |
| Full-screen search, 64x64 template | 68 ms |
| One script image step | 111 ms → about 9 checks a second |
| Multi-scale | 6.1× a single pass, not 5× |
| Miss versus hit | identical, since `find` has no early exit |

**The bug:** the coarse grid was chosen from the template size alone, ignoring the
haystack it would sweep. A 32 px template got a step of 2 and examined a quarter of
every pixel position with a 16x16 kernel — 465 ms, seven times slower than a 64 px
template. The grid now coarsens until the pass fits a budget, and only then. Same
template: 48 ms, matches landing in exactly the same place.

This also contradicted the project's own advice. `SCRIPTS.md` recommended 30–150 px
templates; 32 px was the worst possible choice.

**Held over:** giving script image steps their own search region would take the poll
rate from 9 a second to about 70. It changes the macro format, so it waits.

---

## Stage 5 — Concurrency churn

🔧 **Result: clean over 33 000 transitions.** `--selftest churn[=seconds]`

Two ten-minute runs at about a hundred lifecycle transitions a second.

| | |
|---|---|
| Presses left held after a stop | 0 |
| Generations that escaped cancellation | 0 |
| Moments with two playback loops | 0 |
| Watchdog trips | 0 |

The first run reported one release sent with no press behind it — the harmless
direction, not a stuck key. The mechanism proposed for it predicted roughly a hundred
occurrences per run; observation was one, then zero. Two orders of magnitude means the
explanation was wrong, and **the cause is still open**. It is now instrumented: a
recurrence will name the transition that preceded it.

Two harness faults surfaced here as well. The held-press counter was a running total, so
a single incident reported itself on 181 later checks; and the check waited only on
`playing`, which the loop clears *before* releasing what it holds, which could have
manufactured a phantom stuck press.

Recording was deliberately excluded: its lifecycle installs global hooks and would
capture whatever the machine's owner did for ten minutes. Record-versus-replay races
belong in stage 7.

---

## Stage 6 — Long-run soak

🔧 **Result: no leak.** `--selftest soak[=hours]`

2.5 hours, 3 832 screen captures, 1 743 OCR reads.

| | |
|---|---|
| Handle count | 212 at every one of 25 samples |
| Private bytes | 8.0 → 11.7 MB over 40 min, then 11.7 → 12.8 over 110 min |
| Playback restarts | 0 |
| Machine asleep | 0 s |

The memory curve is an allocator settling, not a leak: the rate fell ninefold. Each
capture allocates 14.7 MB and releases it, and after 3 832 of them the whole process
holds less than one frame's worth.

The flat handle count is the strongest single result. WinRT behind OCR was the main
suspect — an unreleased COM object would accumulate once per call, and 1 743 calls would
have made that obvious.

**A metric that does not work:** the GDI object count reads zero always. Sampling and
capture run on the same thread in sequence, so the sample can never land inside a
`BitBlt`. Absence of a GDI leak is inferred instead from 3 832 captures completing
without exhausting the per-process quota of 10 000 objects.

A first 12-hour attempt produced three usable rows out of an expected 72 and was
discarded: the harness had no way to say whether it had stalled, the machine had slept,
or the console had blocked. It now distinguishes all three, and writes every row to
`logs/soak.csv` as well as the console.

---

## Stage 7 — Feature matrix

🖐 **Outstanding.** The checklist is [TESTING_MATRIX.md](TESTING_MATRIX.md).

This stage carries more weight than its position suggests. **Every automated stage ran
dry** — `arm_dry()` suppressed all five `SendInput` call sites throughout — so not one
synthetic keystroke has reached the operating system anywhere in this testing. The
scheduler's timing is measured, the frame guard's arithmetic is measured, the slip logic
is proven, and none of them has ever actually pressed a key.

130 rows across 15 sections, with 23 marked as a short pass for when time is short.
Section D, the frame guard under real input, is the one to do first.

---

## What the campaign cost and returned

Six stages produced four fixes, two of which mattered: a crash that took the whole
process down, and a search seven times slower than it needed to be. Both were in code
that had been read carefully and looked right.

Three claims that had been assertions became measurements: the scheduler does not drift,
a stalled schedule slips rather than bursting, and curve-drawing time is charged to the
schedule rather than stolen from the events behind it.

Four faults were found in the test harnesses themselves, every one of which would
otherwise have produced a confident and wrong conclusion. That ratio is worth
remembering: a test that has never failed has not been tested either.
