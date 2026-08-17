# Stage 7 — feature matrix

The manual pass. Everything here needs a real machine, a real screen and real
synthetic input actually reaching Windows.

**Why this stage carries more weight than its position suggests.** Stages 2 to 6 were
all run *dry*: `arm_dry()` silenced every one of the five `SendInput` call sites, so
not one synthetic keystroke or click has yet reached the operating system in any of
this testing. The scheduler's timing is measured, the frame guard's arithmetic is
measured, the slip logic is proven — but none of them has ever actually pressed a key.
That is what this stage is for.

## How to use it

Work top to bottom; the sections are ordered so setup happens once. Tick a row when it
behaves; when it does not, record **the row ID, what you saw, and the matching lines
from `logs/macro-recorder.log.*`**. A row ID is enough for me to find the code.

Rows marked 🔥 are new in 1.1.0–1.3.0 or were deliberately excluded from the automated
stages. **If you only have an hour, do those.** They are collected in
[Short pass](#short-pass) at the end.

Rows marked ⚠️ can change system state (shut down, log off, run as administrator).
Read them before running them.

---

## A. First run and files

| ID | Do | Expect |
|---|---|---|
| A-1 | 🔥 Copy the exe to an empty writable folder and run it | `templates/`, `profiles/`, `lang/`, `logs/` all exist immediately, before anything is saved |
| A-2 | Check the path shown under **📁 Files** | Points at the folder next to the exe |
| A-3 | Copy the exe into `C:\Program Files\...` and run | Data folder falls back to `%APPDATA%\MacroRecorder\`, and the panel says so |
| A-4 | Close the app, reopen it | Settings survived; no `config.json` parse errors in the log |
| A-5 | Delete `config.json`, reopen | Starts with defaults, does not crash |
| A-6 | 🔥 Confirm **Frame-rate guard** is unticked on a fresh config | Off by default |
| A-7 | Hand-edit `config.json` to contain `{}` only, reopen | Loads with defaults |
| A-8 | Hand-edit `config.json` with `"speed": 999`, reopen | Clamped to 10.0, no crash |

---

## B. Recording

| ID | Do | Expect |
|---|---|---|
| B-1 | Record mouse movement, clicks, wheel | Event count climbs; **⏱** timer runs |
| B-2 | Record X1/X2 side buttons | They appear in the raw event list |
| B-3 | Record horizontal wheel (tilt or trackpad) | `horizontal: true` events present |
| B-4 | Record while pressing your own hotkeys (F6/F7/F8/F9) | None of them end up in the recording |
| B-5 | Record with a non-US keyboard layout | Replay produces the same characters, not mojibake |
| B-6 | Record NumPad digits and Enter | Replay hits NumPad, not the top row |
| B-7 | Record arrows, right Ctrl, right Alt | Extended-key flags correct on replay |
| B-8 | Untick **Capture mouse movement**, record clicks | Only clicks recorded; cursor teleports on replay |
| B-9 | Set sampling to 1 ms and to 100 ms | Event density changes accordingly |
| B-10 | 🔥 Tick **Remember the target window**, record over a game | Anchor title shown beside **⤵ From the recording** |
| B-11 | Start recording, then stop with the emergency key | Recording stops, nothing held |

---

## C. Replay — real input at last

| ID | Do | Expect |
|---|---|---|
| C-1 | 🔥 Replay a recording into Notepad | Same text appears, same order |
| C-2 | 🔥 Replay a drag in a paint program | The drag is drawn, not two separate clicks |
| C-3 | Loop forever, then stop with `F7` | Stops immediately, nothing left held |
| C-4 | Play count 5 | Runs exactly 5 times, counter reads `5 / 5` |
| C-5 | Delay between loops 2000 ms | Visible pause between cycles |
| C-6 | Speed 0.1× then 3.0× | Visibly slower / faster |
| C-7 | 🔥 Bind **Faster** and **Slower**, press mid-run | Speed changes without stopping |
| C-8 | Pause mid-run, wait a minute, resume | Resumes where it left off; no burst of catching up |
| C-9 | 🔥 Pause while a key is held down | The key is released; nothing stuck afterwards |
| C-10 | Press `F9` mid-run with Shift held by the macro | Shift is released |
| C-11 | Relative mouse mode in a first-person game | Camera moves; absolute mode is wrong there |
| C-12 | Timing jitter 30 % | Timings visibly vary between cycles |
| C-13 | Replay a 30-minute recording | Ends within a second or two of the recorded length |

---

## D. Frame-rate guard — never yet tested live 🔥

Every row here exercises code that has only ever run dry.

| ID | Do | Expect |
|---|---|---|
| D-1 | Guard off, macro of very fast clicks into a game | Note how many clicks the game registers |
| D-2 | Guard on at 30 FPS, same macro | The game registers more of them; the run takes longer |
| D-3 | Watch the **guard added** line | Non-zero, and roughly matches the extra wall-clock time |
| D-4 | Tick **Set it from the window automatically** with no target title | Says no measurement yet, falls back to the FPS field |
| D-5 | Set a target title, wait a few seconds | **measured frame ≈ N ms** appears and moves as the window's load changes |
| D-6 | Cap the game at 30 FPS, then 144 | The measured figure follows in the right direction |
| D-7 | Guard on, macro that types a paragraph | Text still comes out correct, just slower |
| D-8 | Guard at 5 FPS (the extreme) | Very slow but correct; nothing hangs |

---

## E. Target window and anchoring

| ID | Do | Expect |
|---|---|---|
| E-1 | 🔥 Press **⤵ From the recording** | Title field fills from the anchor |
| E-2 | The same with no anchor recorded | Button is disabled |
| E-3 | Type a partial title in lowercase | Matches the real window regardless of case |
| E-4 | Tick **Pause while it is not in front**, alt-tab away mid-run | Status reads **Waiting for the window…**; replay holds |
| E-5 | Alt-tab back | Resumes on its own |
| E-6 | Close the target window mid-run | Replay holds rather than crashing |
| E-7 | Record with anchor, move the window, replay with **Follow the anchored window** | Clicks land in the right places |
| E-8 | Resize the window, replay with **Scale with the window size** | Clicks still land correctly |
| E-9 | The same with scaling unticked | Clicks are offset — confirms the setting does something |
| E-10 | 🔥 Confirm all of the above live in one section | Nothing window-related left in Playback or Recording |

---

## F. Editor

| ID | Do | Expect |
|---|---|---|
| F-1 | Open the editor while replaying | Disabled |
| F-2 | **Story** view of a drag, a double-click, typed text | Described in plain language, not as raw events |
| F-3 | **Raw events** view | Microsecond timestamps, one row per event |
| F-4 | Click a row, change its time | Clamped between its neighbours |
| F-5 | Change a key, a coordinate, a delta | Applied; **Undo** reverts one step |
| F-6 | **Duplicate**, then **Delete action** | Tail shifts correctly both ways |
| F-7 | **Delete** a range | Tail pulled back, no silent gap |
| F-8 | **Keep only** a range | Cropped and rebased to zero |
| F-9 | **Drop moves** | Clicks and keys survive |
| F-10 | **Trim lead-in** | First event happens immediately |
| F-11 | **Insert pause** 5000 ms | Gap appears, rest shifts |
| F-12 | **Scale time ×2** | Replay takes twice as long |
| F-13 | **Replace in selection**, Left → Right | Only inside the range |
| F-14 | **Shift coordinates** by 100/100 | Only inside the range |
| F-15 | 🔥 **Insert click at match** after an image search | A real click appears at the found position |
| F-16 | Reversed range (`from` > `to`) | Handled without complaint |

---

## G. Script — every step kind

Build one script that uses each kind at least once, or several small ones.

| ID | Step | Expect |
|---|---|---|
| G-1 | Play events | Plays the named slice |
| G-2 | Wait | Pauses; speed slider does **not** change it |
| G-3 | Wait for … appears | Blocks until true |
| G-4 | Wait for … disappears | Blocks until false |
| G-5 | Wait for, timed out | Continues silently; log says `wait timed out` |
| G-6 | 🔥 Click image | Clicks the picture's centre |
| G-7 | Click at | Clicks the coordinates; follows the anchor |
| G-8 | Key press + Key release | One keystroke; press alone leaves it held until stop |
| G-9 | Set `=`, `+=`, `-=`, `*=` | Arithmetic correct |
| G-10 | If / End if | Branch taken only when true |
| G-11 | If / Else / End if | Both branches reachable |
| G-12 | While / End while | Loops; exits when false |
| G-13 | Break | Leaves the innermost loop |
| G-14 | Break inside an If inside a While | Still leaves the While |
| G-15 | Run — an exe, a URL, a folder | All three open; no console flash |
| G-16 | Quit the app | Closes the application |
| G-17 | Note | Text appears in the log |
| G-18 | 🔥 Read number | Variable takes the value; a clock reads as seconds |
| G-19 | Unbalanced `If` | Editor warns; **the script does not run at all** |
| G-20 | Step after `Quit the app` | Shown orange as unreachable |
| G-21 | Disable a step | Struck through and skipped |
| G-22 | Script with all steps disabled | Behaves as a plain recording |
| G-23 | `While` with no counter increment | Stops on its own; log says step budget exceeded |
| G-24 | Save and reload a scripted macro | Script survives intact |
| G-25 | Save as `.mrz`, reload | Same, and the file is much smaller |

### Conditions — each in `Wait for`, `If` and `While`

| ID | Condition | Expect |
|---|---|---|
| G-26 | always | Always true |
| G-27 | variable, all six comparisons | Correct each way |
| G-28 | image | Found / not found; sets `match_score` |
| G-29 | pixel | Matches a colour within tolerance |
| G-30 | window | Matches a partial title |
| G-31 | text | Loose match: case, spacing and punctuation ignored |

---

## H. Image search

| ID | Do | Expect |
|---|---|---|
| H-1 | `Win+Shift+S`, then **📋 Paste** | Template appears |
| H-2 | **🔍 Find on screen** | Reports position and a score near 1.0 |
| H-3 | **💾 Save PNG…** into `templates/` | File written |
| H-4 | 🔥 Open the dropdown beside a step's template field | Lists the PNGs in `templates/` |
| H-5 | 🔥 Save a new template, reopen the dropdown | The new one is already there |
| H-6 | Pick one from the dropdown | Name filled in; the step then finds it |
| H-7 | Name a template that does not exist | Step does nothing; log says it could not be loaded |
| H-8 | 🔥 A 32×32 template on the full screen | Fast now — under ~50 ms, not ~470 |
| H-9 | A template with a transparent background | Matches over different backdrops |
| H-10 | Threshold 1.00 | Almost never matches |
| H-11 | Threshold 0.60 | Matches something wrong — confirms it is doing work |
| H-12 | **Try other scales** with a resized window | Finds it; noticeably slower |

---

## I. Text on screen

| ID | Do | Expect |
|---|---|---|
| I-1 | **🎯 Pick in 3 s**, both corners | Region captured and read straight away |
| I-2 | **⤵ from the panel** in a script step | Four numbers copied in |
| I-3 | Read `Gems: 1,250` | Parses as 1250 |
| I-4 | Read `02:34` | Parses as 154 |
| I-5 | A region under 40×40 | Fails gracefully; variable keeps its old value |
| I-6 | A non-English game with the language pack installed | Reads it |
| I-7 | A stylised game font | Note what it does — expected to be poor |

---

## J. Schedule

| ID | Do | Expect |
|---|---|---|
| J-1 | Set a time two minutes out, today's weekday | Fires |
| J-2 | The same with the window minimised to tray | Still fires |
| J-3 | Untick today | Does not fire |
| J-4 | Schedule while already replaying | Skipped, and the log says so |

---

## K. Exports

| ID | Do | Expect |
|---|---|---|
| K-1 | **Export .exe**, run it on this machine | Plays; emergency stop works |
| K-2 | 🔥 Export a **scripted** macro to .exe | Script runs too |
| K-3 | The same, using image templates, on a clean folder | Fails until `templates/` is copied beside it |
| K-4 | Copy `templates/` beside it, run again | Works |
| K-5 | Run the exported exe on another PC | Plays with nothing installed |
| K-6 | **Export .ahk**, run in AutoHotkey v2 | Events replay; `Esc` exits |
| K-7 | Export a scripted macro to .ahk | Only events translated — known limitation |

---

## L. Settings, look and language

| ID | Do | Expect |
|---|---|---|
| L-1 | Each of the 9 themes | Applies at once, text stays readable |
| L-2 | 🔥 The **🖥 Target window** and **📊** headers | Real icons, not empty boxes |
| L-3 | Transparent UI on and off | Works on any theme |
| L-4 | Fluent (Mica) and Glassmorphism on Windows 11 | System backdrop appears |
| L-5 | Each of the 6 languages | Switches without restart; Chinese glyphs render |
| L-6 | **Export language template**, edit, rename to `lang/xx.json`, restart | Your strings replace the built-ins |
| L-7 | A partial translation with empty values | Falls back per string |
| L-8 | Save, switch and reload a profile | All settings restored |
| L-9 | A profile name with `/`, `\`, `:` | Sanitised, no crash |
| L-10 | Rebind every hotkey slot, including to `Pause` and NumPad | All register |
| L-11 | Bind a combination another app owns | Reports the clash instead of failing silently |
| L-12 | Swap `F6` and `F7` with each other | Possible — binding releases the globals |
| L-13 | Clear a slot | Unbound |

---

## M. System integration

| ID | Do | Expect |
|---|---|---|
| M-1 | Change display scaling 100 % → 150 % mid-session | Clicks still land correctly |
| M-2 | Move the target window to a second monitor | Anchoring follows it |
| M-3 | A monitor to the left of the primary (negative coordinates) | Handled |
| M-4 | Unplug a monitor mid-replay | Does not crash |
| M-5 | Windows 11: put the app on desktop 2, work on desktop 1 | Replay holds |
| M-6 | Minimise to tray, use the tray menu | Record / play / stop all work |
| M-7 | **Close button minimizes to tray** | ✕ hides instead of quitting |
| M-8 | Launch a second instance | Focuses the first |
| M-9 | ⚠️ Replay into a window running as administrator, app not elevated | Input is blocked — expected |
| M-10 | ⚠️ The same with the app elevated | Works |
| M-11 | ⚠️ Time limit 1 minute, action **Shut down** | Countdown shows; `shutdown /a` aborts it |
| M-12 | ⚠️ Action **Log off** | Logs off |
| M-13 | Pixel stop condition on a flat replay | Stops when the colour matches |
| M-14 | The same on a **scripted** macro | Ignored — known limitation |
| M-15 | Lock the screen mid-replay | Behaviour noted, no crash |

---

## N. Command line

| ID | Do | Expect |
|---|---|---|
| N-1 | `--help`, `--version` | Print and exit |
| N-2 | `--play file.mrz` | Preloads into the GUI |
| N-3 | `--play … --loops 3 --speed 1.5 --no-gui` | Plays 3× headless and exits |
| N-4 | A scripted macro headless | Script runs |
| N-5 | Emergency stop during a headless run | Stops |
| N-6 | `--play` with a missing file | Clear error, no panic |
| N-7 | `--selftest nonsense` | Lists the available tests |

---

## O. Things that should fail well

| ID | Do | Expect |
|---|---|---|
| O-1 | Load a truncated `.json` | Refused with a message |
| O-2 | Load a `.json` whose script has an unbalanced block | Refused at load |
| O-3 | Load a `.mrz` that is not gzip | Refused |
| O-4 | Load a macro recorded at a different resolution | Plays, lands wrong — the documented limitation |
| O-5 | Replay with the target window closed and anchoring on | Does not crash |
| O-6 | Fill `templates/` with 200 PNGs, open the dropdown | Still usable |
| O-7 | A recording of 100 000+ events | Loads, edits, replays |
| O-8 | Free disk space exhausted while saving | Reports the failure |

---

## Short pass

If time is short, these are the rows that cover code that is either brand new or has
never had real input behind it:

**A-1, A-6** · **B-10** · **C-1, C-2, C-7, C-9** · **all of D** · **E-1, E-10** ·
**F-15** · **G-6, G-18** · **H-4, H-5, H-8** · **K-2** · **L-2**

Twenty-three rows, roughly an hour. Section D matters most: the frame guard is the
largest thing added since 1.0.0 and not one of its keystrokes has yet reached Windows.
