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
stages. Rows marked 🆕 are new in **1.4.0** and have never been touched by a human at
all. **If you only have an hour, do the 🆕 ones.** They are collected in
[Short pass](#short-pass) at the end.

Rows marked ⚠️ can change system state (shut down, log off, run as administrator).
Read them before running them.

Sections **P**, **Q** and **R** are entirely new. P is the text expander, which arrived
in 1.3.5 and was missed when this document was written — it is the one component that
sees every key you press, so it earns its rows. Q and R are 1.4.0's two additions that
cannot be tested any other way.

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
| G-27 | 🆕 variable, all **seven** comparisons including `has` | Correct each way; `has` is containment, forgiving about case and spacing |
| G-28 | image | Found / not found; sets `match_score` |
| G-29 | pixel | Matches a colour within tolerance |
| G-30 | window | Matches a partial title |
| G-31 | text | Loose match: case, spacing and punctuation ignored |

### Steps new in 1.4.0 🆕

Six new kinds and two new conditions. None of these has ever run outside the test
suite.

| ID | Do | Expect |
|---|---|---|
| G-32 | **Find image** → `target`, picture present | `target.found` is 1; `target.x/.y` are the centre; `target.w/.h` the size |
| G-33 | The same with the picture absent | `target.found` is 0, and `target.score` still carries the best score it saw |
| G-34 | **Read text** → `line` over a label | The whole reading lands in the variable as text, not a number |
| G-35 | **Get text** ← clipboard, after copying something | Variable holds what you copied |
| G-36 | **Get text** ← title of the window in front | Matches the real title |
| G-37 | **Get text** ← program in front | Reads like `notepad.exe` — the name only, no path |
| G-38 | **Get text** ← file, pointing at a text file | Contents land in the variable |
| G-39 | The same pointing at a file that is not there | Empty variable, warning in the log, script continues |
| G-40 | **Put text** → clipboard, then paste it somewhere | Exactly what the step said |
| G-41 | **Put text** → file, *add to the end* off, run twice | File holds one copy, not two |
| G-42 | The same with *add to the end* on | Two copies |
| G-43 | `{name}` inside a **Note**, a **Run** argument and a **Put text** | Replaced by the variable's value |
| G-44 | `{{` in the same places | Comes out as one literal brace |
| G-45 | `{typo}` — a name nothing ever set | Left as written; **not** silently dropped |
| G-46 | **Set** a *text* value, then `+=` another text | Joined end to end |
| G-47 | **Set** `count` = text `"3"`, then `+= 1` | **4**, not `"31"` — two numbers written as text are still numbers |
| G-48 | Compare a variable holding `"10"` against the number `10` | Equal |
| G-49 | Compare a variable holding `"Roblox"` against text `"roblox"` | Equal — text comparison ignores case and surrounding space |
| G-50 | Condition **process running**, part of a name (`roblox`) | True while it runs, false after it closes |
| G-51 | The same for a program that is not running | False, promptly — no long wait |
| G-52 | Condition **element on screen** against Notepad or File Explorer | True |
| G-53 | Save and reload a macro using every step above | All fields survive intact |
| G-54 | 🆕 Load a macro written by **1.3.5** | Loads unchanged: area is the whole screen, prep is none, format is anything, numbers in `vars` are still numbers |

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

### Where to look — new in 1.4.0 🆕

| ID | Do | Expect |
|---|---|---|
| H-13 | Area **whole screen** on a `While` loop, watch the log timing | The baseline: roughly ten looks a second on a 1440p desktop |
| H-14 | The same with area **a rectangle** around the button | Visibly faster; compare against `--selftest vision`, which prints both |
| H-15 | Area **active window** | Finds it while the window is in front |
| H-16 | The same with a different window in front | Does not find it — confirms the area is real |
| H-17 | Area **near the last match**, picture that stays put | Found each time; the log shows a small capture |
| H-18 | The same after moving the window | Still found — it widens to the whole screen when the guess fails |
| H-19 | Area **relative to another picture**, anchor present | Target found in the offset rectangle |
| H-20 | The same with the anchor hidden or removed | Reported as not found. It must **not** quietly search the whole screen |
| H-21 | An anchored search where the anchor is found but the target is not | `match_x/y` belong to the target, not the anchor |
| H-22 | Two identical buttons, anchored to different headings | The right one is pressed each time |

### Staying found — new in 1.4.0 🆕

| ID | Do | Expect |
|---|---|---|
| H-23 | A picture whose score hovers near the threshold, **lost below** at 0 | Flaps: several state changes a second |
| H-24 | The same with **lost below** set well under the threshold | Settles: one state change |
| H-25 | **stable 2 / 3** on a picture that flickers | Ignores the flicker |
| H-26 | The same on a picture that really appears | Reacts after two or three looks, not instantly |
| H-27 | Two steps watching the same template with different settings | They agree about whether it is there — the state is per template |

### One object, several pictures — new in 1.4.0 🆕

| ID | Do | Expect |
|---|---|---|
| H-28 | Make `templates/Claim/` with `normal.png`, `hover.png`, `dark.png`; step names `Claim` | Matches in all three states |
| H-29 | Drop a `.txt` into that folder | Ignored, not treated as a picture |
| H-30 | An empty folder of that name | Step does nothing; log says the folder holds no PNGs |
| H-31 | Check `target.w/.h` when a differently sized variant wins | Reports the winning variant's size, not the first one's |

### Scale and theme — new in 1.4.0 🆕

| ID | Do | Expect |
|---|---|---|
| H-32 | **💾 Save PNG…** a template | `Name.png.json` appears beside it holding the display scale |
| H-33 | Cut a template at 100 %, switch Windows to 150 %, run the step | Still found — the sidecar rescales it |
| H-34 | A template made before 1.4.0, with no sidecar | Used exactly as it is; nothing is guessed |
| H-35 | Delete the sidecar by hand and rerun | Same as H-34, no error |
| H-36 | Tick **outlines** on a template that stopped matching after a theme change | Found again |
| H-37 | **outlines** on an ordinary template | Still found, at the same place, with a slightly lower score — expected |

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

### Preparing the pixels — new in 1.4.0 🆕

Use the **🔤 Text on screen** panel for all of these: it shows the reading and the fit
score side by side, which is the whole point of the feature.

| ID | Do | Expect |
|---|---|---|
| I-8 | Read the same region under each of the five profiles | Different readings; the fit score moves with them |
| I-9 | **game HUD** on pale text over moving artwork | Reads better than **none** — this is the case it exists for |
| I-10 | **digits** on a counter | Reads better than **none** on that, worse on a sentence |
| I-11 | **try each** on the same region | Picks a profile at least as good as your best manual choice; the panel names which |
| I-12 | **try each** on text that already reads perfectly | Stops early — no slower than **none** |
| I-13 | Compare each profile's time in `--selftest vision` | A profile costing three times as much for the same text is not worth using |

### Saying what a reading should look like — new in 1.4.0 🆕

| ID | Do | Expect |
|---|---|---|
| I-14 | **Read number**, expect **clock**, region showing `02:34` | Variable is 154 |
| I-15 | The same expecting **whole number** | Variable is 2 — the format decides the meaning |
| I-16 | Expect **clock** over a region holding `1250` | **Refused**: the variable keeps its old value, and the log says it did not fit |
| I-17 | Expect **pattern** `##:##` over `12:34`, then over `1:34` | Passes, then refused |
| I-18 | Read a region with the engine erroring (under 40×40) | Variable keeps its old value, as before |
| I-19 | Check `<name>.quality` after each of the above | Between 0 and 1, and lower for the readings you can see are wrong |

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
| O-9 | 🆕 **Get text** ← file, pointing at a 500 MB file | Reads the first megabyte and says so; memory does not climb |
| O-10 | 🆕 **Put text** → file in a folder that does not exist | Warning in the log, script continues |
| O-11 | 🆕 An anchored search naming a template that does not exist | Not found, warning in the log, no full-screen fallback |
| O-12 | 🆕 A pattern of nothing but `*` against a long reading | Answers immediately — must not hang |
| O-13 | 🆕 **Press element** straight from the menu, nothing filled in | Does nothing and says so in the log. It must **not** click the middle of the window |

---

## P. Text expander 🆕

Arrived in 1.3.5 and was missed when this document was written. It deserves the most
careful reading here for one reason: **it is the only part of the program that watches
every character you type.** Rows P-9 to P-12 are about that and nothing else.

Entries live in `expansions.json`. The global switch is off until you turn it on.

| ID | Do | Expect |
|---|---|---|
| P-1 | Turn the expander on, type `;sig` in Notepad | Replaced by the saved text |
| P-2 | The same in **delimiter** mode: `addr` then a space | Fires on the space, and the space survives |
| P-3 | **behind a marker** mode with `;` | Fires only when the marker is there |
| P-4 | **immediately** mode | Fires the moment the abbreviation is complete |
| P-5 | An entry using `{date}`, `{time}`, `{datetime}`, `{clipboard}` | All filled in |
| P-6 | An entry ending `{cursor}` | Caret lands where the marker was, not at the end |
| P-7 | `{key:Tab}` and `{random:a\|b\|c}` | A real Tab; a different pick across runs |
| P-8 | The same entry set to **paste** rather than **type** | Text appears at once, and the clipboard is put back afterwards |
| P-9 | Add your terminal and password manager to **Never in windows**, type an abbreviation there | Nothing fires |
| P-10 | Type an abbreviation, then click elsewhere / press a modifier / change window, and finish typing it | Does **not** fire — the buffer is emptied on all three |
| P-11 | Type several abbreviations, then `grep` every file under `logs/` for them | **Nothing found.** The typed buffer must never reach the log at any level |
| P-12 | With an IME or a dead-key layout, type something that commits differently from the keystrokes | Refuses rather than guessing; nothing is mangled |
| P-13 | Start recording a macro, type an abbreviation | Nothing fires, and nothing about it lands in the recording |
| P-14 | 🆕 Play a macro, then type a **text** abbreviation | Silent — it would fight with the macro |
| P-15 | 🆕 Play a macro, then type a `;stop` entry set to **stops everything** | **Playback stops.** This is the whole point of the split |
| P-16 | 🆕 A `;farm` entry set to **plays a macro** with a file path in its text | That file is loaded and started |
| P-17 | 🆕 The same with a path that does not exist | Warning in the log; nothing starts; the app stays up |
| P-18 | 🆕 An entry set to **runs a program** | Opens it, exactly like the `Run` step |
| P-19 | 🆕 A command entry while **recording** | Still silent — recording swallows the expander whole |
| P-20 | Turn the global switch off, type everything above again | Nothing fires at all |

---

## Q. UI Automation 🆕

Windows only tells you about applications that choose to expose themselves. Q-10 is not
a bug report — it is the expected answer, and the reason the picture search still exists.

| ID | Do | Expect |
|---|---|---|
| Q-1 | **Find element** by **Name**, Notepad or File Explorer in front | Found; `elem.x/.y/.w/.h` point at the real control |
| Q-2 | The same with only part of the name | Found — exact first, then substring |
| Q-3 | Narrow by **Kind** = `Button` | Still found, and noticeably faster |
| Q-4 | A name that matches nothing | `elem.found` is 0, variable empty, no crash |
| Q-5 | **Id** filled in from an application that sets one | Found, and it is the most reliable of the three |
| Q-6 | *in the window in front* ticked, then unticked | Both find it; the whole desktop takes longer |
| Q-7 | **Press element** with *ask the app* on | The control is pressed and **the cursor does not move** |
| Q-8 | The same with the window **behind** another one | Still pressed |
| Q-9 | *ask the app* off | A real click at the control's centre |
| Q-10 | Either of those on a control with nothing to invoke | Falls back to a real click rather than doing nothing |
| Q-11 | **timeout** 3000 ms against a dialog that takes a second to draw | Waits for it, then presses |
| Q-12 | timeout 0 against the same | Misses it — confirms the wait is real |
| Q-13 | An element scrolled out of view | Counts as not found. It must **not** report a centre of (0, 0) |
| Q-14 | ⚠️ A window running as administrator, app not elevated | Limited or silent — expected, note what happens |
| Q-15 | **In Roblox, or any Unity / DirectX game** | **Finds nothing.** Expected: the game draws its own interface |
| Q-16 | Time Q-1 and Q-3 against `--selftest vision` | If they are slower than the picture search, the cascade is in the wrong order |

---

## R. Debug overlay 🆕

A layered window, not an eframe viewport. Under **🔎 Image search → Show what the script
looks at**. R-2 is the one that would be unforgivable to get wrong.

| ID | Do | Expect |
|---|---|---|
| R-1 | Tick the box | A window appears over everything — and you cannot see it, because nothing has been searched for yet |
| R-2 | With it on, click your desktop, another app, its own main window | **Every click goes through.** Nothing is blocked anywhere on screen |
| R-3 | Run a script with an image step | Blue rectangle where it looked; green or red rectangle with a score where it found something |
| R-4 | A step whose score is under 0.85 | Rectangle is red; the number under it says why |
| R-5 | A **Read text** step | Amber rectangle over the region it read |
| R-6 | A **Find element** step | Violet rectangle around the control |
| R-7 | Watch it while a `While` loop polls | Redraws when the answer changes, and stays still when it does not — no flicker |
| R-8 | Untick the box | Gone at once, nothing left drawn on the desktop |
| R-9 | Untick and retick it quickly, several times | Comes back every time. It must not end up ticked with no window |
| R-10 | Close the application with it on | The window goes with it — check nothing is left over the desktop |
| R-11 | A second monitor, especially at a different scale | Rectangles land in the right places on both |
| R-12 | A game in **exclusive full screen** | Covered by the game — expected; borderless windowed shows it |
| R-13 | Alt-Tab while it is on | It never appears in the switcher and never takes focus |

---

## Short pass

If time is short, these are the rows that cover code that is either brand new or has
never had real input behind it. **1.4.0 first**, because none of it has been touched by
a human:

**G-32, G-47, G-54** · **H-14, H-20, H-24, H-28, H-33** · **I-9, I-11, I-16** ·
**P-11, P-15** · **Q-7, Q-15** · **R-2, R-8**

Seventeen rows, roughly an hour. Three of them matter more than the rest:

- **P-11** — grep the logs for what you typed. If anything comes back, stop and tell me
  before doing anything else.
- **R-2** — a full-screen window that swallows clicks would be the worst bug this
  release could ship.
- **G-54** — a 1.3.5 macro that no longer loads, or loads and behaves differently,
  breaks every user who upgrades.

### Still outstanding from earlier releases

Never done, and not superseded by anything above:

**A-1, A-6** · **B-10** · **C-1, C-2, C-7, C-9** · **all of D** · **E-1, E-10** ·
**F-15** · **G-6, G-18** · **H-4, H-5, H-8** · **K-2** · **L-2**

Section D still matters most of these: the frame guard is the largest thing added since
1.0.0, and not one of its keystrokes has yet reached Windows.

