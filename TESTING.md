# Testing plan

A staged plan for hardening Macro Recorder. Stages 1 to 6 are complete; stage 7 is
outstanding. Each stage says who did the work and what came out of it.

**The split that shaped everything below.** Through 1.3.5, Claude could read the source
and write test code but could not compile or run anything — no Rust toolchain, no
Windows, no display. That is what made the three kinds below necessary.

**This is no longer true as of 1.4.0.** The work now happens on the target machine with
a toolchain present, so 🔧 stages are run rather than handed over, and every number in
the 1.4.0 and 1.5.0 rows below was measured here. 🖐 has not changed: a real game and a
real night still cannot be simulated.

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
| 7 | [Feature matrix](#stage-7--feature-matrix) | 🖐 | **outstanding** | The checklist exists: [TESTING_MATRIX.md](TESTING_MATRIX.md), 130 rows + 1.5.0's |
| 8 | [1.4.0 regression suite](#stage-8--140-regression-suite) | 🔧 | done | 128 tests, all green; two performance regressions caught before release |
| 9 | [1.5.0: the interpreter and the capture path](#stage-9--150-the-interpreter-and-the-capture-path) | 🔧 | done | 144 tests; a new interpreter harness; stage 1's last debt closed; a plan that was measurably wrong |

---

## Stage 9 — 1.5.0: the interpreter and the capture path

🔧 **Result: 128 → 144 tests, one new harness, and a plan that measurement contradicted.**

### The plan was wrong, and the benchmark said so

1.5.0 set out to make screen capture cheaper, and the plan had three parts, each of
which was real work that was really happening on every single look at the screen: a
device context and a bitmap created and destroyed (GDI object churn, against a
per-process quota of 10 000), a fresh zeroed allocation, a `GetDIBits` that copied and
reformatted the whole frame a second time, and a full pass swapping red with blue —
which `upscale_to_bgra` then swapped straight back on the way to the OCR engine.

All of it was removed. The number did not move.

| Region | 1.4.0 | After all three fixes |
|---|---|---|
| 400×300 | 5.8 ms | 5.7 ms |
| 2560×1440 | 30.2 ms | 30.3 ms |

So the benchmark grew a table that asked where the time actually was, and the answer
was unambiguous:

| Region | `GetDC` | `BitBlt` from the screen | The same blit, memory to memory |
|---|---|---|---|
| 320×240 | 0.01 ms | 6.07 ms | 0.10 ms |
| 640×480 | 0.01 ms | 6.08 ms | 0.22 ms |
| 1280×720 | 0.01 ms | 6.10 ms | 0.68 ms |
| 2560×1440 | 0.01 ms | 23.9 ms | 3.18 ms |

`BitBlt` out of the composited desktop costs about six milliseconds before it has
copied a useful pixel, and it costs that at 320×240 as much as at 640×480. The
destination was never the expensive part — the table prices the new DIB section
against the device bitmap it replaced (`old DDB`) and they come out equal. The
readback was, and no arrangement of GDI objects addresses a readback.

That is what put **Desktop Duplication** in the release. It was not in the plan.

| | GDI | Desktop Duplication |
|---|---|---|
| 400×300, polled 200 times | 6.06 ms each | **0.12 ms each** |
| Whole 2560×1440 screen | 30.2 ms | **4.0 ms** |
| Of 200 polls, frames the compositor never sent | — | 196 |

And what a script step costs end to end, which is the number that decides how often a
macro can look at the screen:

| Search area | 1.4.0 | 1.5.0 | Looks per second |
|---|---|---|---|
| 2560×1440 | 52.2 ms | 32.4 ms | 19 → 31 |
| 1280×720 | 18.6 ms | 9.9 ms | 54 → 101 |
| 400×300 | 7.0 ms | 1.7 ms | 143 → 584 |
| 200×150 | 6.5 ms | 0.6 ms | 154 → 1582 |

The three original fixes stayed. They are still less work, they still removed two full
passes over a fourteen-megabyte buffer that cancelled each other out, and they are what
the fallback path runs when duplication is unavailable.

**Worth stating plainly: this costs memory.** The soak's steady state went from about
12 MB to about 84 MB, which is a D3D11 device, its driver allocations and a full-screen
texture kept on the GPU so that an unchanged frame can be reused. That is the trade,
and it is why the switch to turn it off exists.

### Three attempts at "is it the same picture?"

Speed means nothing if the pixels are wrong, and a capture that is quick and wrong
finds buttons in the wrong place — which does not fail loudly, it just stops working.
Checking it took three goes, and the first two are worth recording because both looked
correct.

1. **Compare the two captures for equality.** Reported that 97 % of the frame differed.
   It did: there was a game running on the machine. The test was measuring the screen,
   not the code.
2. **Compare only the pixels two consecutive captures agree about.** Better — a pixel
   that did not change during that interval was not being drawn. Still not enough: in a
   continuously re-rendered scene a pixel lands on the same value twice often enough to
   matter, and the disagreement rate wandered between 1.6 % and 31 % depending on what
   the game was doing.
3. **Cut a template out of a GDI frame and look for it in a duplicated one.** This is
   the property the program actually depends on, and every way the fast path could be
   wrong has its own signature: channels swapped and the correlation collapses; a row
   pitch ignored and the picture shears; the wrong monitor and the hit is out by a
   screen's width; a stale frame and the score drops but the position is exactly right.

   **0 px off, score 1.000.** And on an idle screen, where the pixel count *can*
   conclude, it agrees: 100 % of pixels still, **0.00 % disagreement** at every
   region size.

The lesson is the same one stage 5 taught about the held-press counter: a test that
reports a dramatic number on the first run is more likely to be measuring itself.

### The new harness: `--selftest script`

The unit tests cover the pieces — a policy round-tripping, `break_target` counting
nesting, a drag being told from a click. What they cannot cover is the interpreter
running: a retry loop sleeping and being cancelled, a call handing its variables down
and getting them back, a recursion guard firing eight frames deep, a step gate parking
a run and something else releasing it. Those are about a playback thread and the flags
other threads set under it.

Twelve checks, all green:

| | |
|---|---|
| The four miss policies | carry on, stop, leave the loop, retry-then-stop |
| Retry is timed, not counted | 366 ms for 3 × 120 ms — a retry loop that spins is worse than none |
| Calls | variables in and back out; a missing file obeying its policy |
| Recursion | a macro naming itself ran 8 levels and stopped at the cap |
| Step mode | parks before the first step; five presses of Next ran five steps |
| Stop while parked | released in 12 ms |
| 3 000 rounds of every policy with calls nested under them | no press left held, every round `Finished` |

One harness fault was found writing it, and it is the sort worth remembering: every run
returned `Stopped` immediately, and it took a moment to see why. `stopping()` compares
the run's generation against the live one, and a fresh `AppState` starts at zero while
the harness was passing generation 1. A harness that reports a clean *failure* is at
least honest; the version of this that set generation 0 and passed everything without
running a step would not have been.

### Stage 1's last debt, closed

The self-running `.exe` footer parser has read a length out of an untrusted tail since
1.2 and been listed as outstanding since stage 1. It is the only place in the program
that takes a number out of bytes it did not write and then acts on it.

`16 + len` was an unchecked addition. In a release build, where overflow checks are
off, a claimed length near `u64::MAX` wraps it to zero — and the `checked_sub`
underneath then succeeds, and `vec![0u8; len]` asks for sixteen exabytes. Under
`panic = "abort"` a failed allocation is the process gone. The subtraction looked
careful; the addition it depended on was not.

Now: the addition is checked, the length is capped at 64 MB before anything is
allocated, the decompressor stops at 512 MB so a compression bomb cannot expand into
memory, and the payload goes through `normalize` — which caps the event count and
rejects unbalanced blocks — exactly like a file opened through the Open dialog. Until
now the one input nobody chose was the one input nobody checked. Three tests cover it,
including a real compression bomb.

### The rest

- **144 tests, all green.** Sixteen new ones: the two channel orders finding the same
  place with the same score, a capture with no alpha still matching (a haystack whose
  mask were consulted would be entirely masked out and every search would come back
  empty), every miss policy round-tripping, a 1.4.0 script loading with `Continue`
  everywhere, the two spellings of `Break` agreeing about nesting, a click told from a
  drag, a conversion covering every event exactly once, a shot pointing at a deleted
  event being skipped, and the three footer tests.
- **`--selftest churn=120`**: 10 516 lifecycle transitions, 0 presses left held, 0
  generations escaped, 0 moments with two loops.
- **`--selftest soak`**: 442 captures and 177 OCR reads over a quarter of an hour.
  Handles 453 → 452, private bytes +2.3 MB against the five-minute mark, 0 GDI
  objects, 0 restarts, 0 OCR failures. The Direct3D device and its full-screen
  texture are allocated once and stay allocated; nothing accumulates per capture,
  which was the question.

One bug was found by the soak rather than in it, and it is a small embarrassment worth
recording: `--selftest soak=0.35` parsed its argument as an integer, failed, and fell
back to the twelve-hour default without a word. Twenty minutes of data arrived after
twenty minutes; the run kept going. It now parses fractional hours and says so when the
argument is not a number at all.

---

## Stage 8 — 1.4.0 regression suite

🔧 **Result: 103 → 128 tests. Two regressions caught by measuring rather than assuming.**

The suite grew with the release. Every 1.4.0 feature that can be tested without a screen
is: the OCR preparation profiles and Otsu's threshold, the expected-format check and its
pattern matcher, the fit score, values that hold text, `{name}` substitution, the
comparison rules, the file cap, the template sets and their scale sidecar, the two
thresholds and the stability window, the search-area clamp, and every step and condition
kind round-tripping through both its index and a save-and-load.

Two things that only measuring could have found, both in `--selftest vision`:

- **The vector kernel was slower than the plain one on small templates** — 0.93×,
  a real regression. Three horizontal sums per row cost more than the multiply-adds
  they replaced once the coarse pass had shrunk a 32-pixel template to eight wide.
  Fixed by keeping the accumulators across the whole window; now 1.1× to 1.6×.
- **Outline matching cost eight times an ordinary search**, not the "one extra pass"
  it should have. The Sobel operator was clamping both coordinates of all eight
  neighbours — sixteen branches a pixel over 3.7 megapixels — where only the one-pixel
  border needed it. Fixed by splitting the interior from the border: now about 1.6×.

The benchmark itself grew four tables: search cost by area, the vector kernel against
the plain one (with a check that they agree about *where* the picture is), grey against
outline matching, and text recognition by preparation profile. Each prints the numbers
that justify the feature above it, or fails to.

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

~~Still outstanding inside this stage: … the self-running `.exe` footer parser, which
reads a length out of an untrusted tail and then trusts it.~~ **Closed in 1.5.0** — see
[stage 9](#stage-9--150-the-interpreter-and-the-capture-path). It was worse than the
note suggested: the guard was a `checked_sub` sitting on top of an unchecked `16 + len`
that wraps to zero in a release build.

Still outstanding inside this stage: the preconditions of each `unsafe` block, and a
written lock ordering across the six threads. Both are documentation debts rather than
suspected faults; the 1.5.0 capture work added one more `unsafe` region (Direct3D and
DXGI) whose preconditions belong in the same document when it is written.

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

130 rows across 15 sections, plus section **S** for 1.5.0, with the short pass at the
end for when time is short. Section D, the frame guard under real input, is still the
one to do first; section S is the one that is entirely untouched, because every feature
in it is new.

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

