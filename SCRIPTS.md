# 🧠 Scripts in Macro Recorder

### A guide for someone who has never done this before

There is no programming here. No code to type. There is a list of steps you assemble with the mouse from a dropdown — like a shopping list.

Read it in order. Fifteen minutes from now you'll have a working script.

[🇷🇺 Русская версия](SCRIPTS_RU.md) • [← Back to README](README.md)

---

## 📑 Contents

1. [What a script actually is](#1-what-a-script-actually-is)
2. [Do I even need one?](#2-do-i-even-need-one)
3. [Your first script in five minutes](#3-your-first-script-in-five-minutes)
4. [Reading the script window](#4-reading-the-script-window)
5. [Four rules to remember](#5-four-rules-to-remember)
6. [Every step: the cheat sheet](#6-every-step-the-cheat-sheet)
7. [Every step: in detail](#7-every-step-in-detail)
8. [Conditions: how the macro looks at the screen](#8-conditions-how-the-macro-looks-at-the-screen)
9. [Variables: the script's memory](#9-variables-the-scripts-memory)
10. [Blocks: `If` and `While`](#10-blocks-if-and-while)
11. [Image templates: how to make good ones](#11-image-templates-how-to-make-good-ones)
12. [Three worked examples](#12-three-worked-examples)
13. [Debugging: seeing what's going on](#13-debugging-seeing-whats-going-on)
14. [Common traps](#14-common-traps)
15. [What scripts can't do](#15-what-scripts-cant-do)
16. [What it looks like inside the file](#16-what-it-looks-like-inside-the-file)
17. [One-page cheat sheet](#17-one-page-cheat-sheet)

---

## 1. What a script actually is

Picture two machines.

**The first is a tape recorder.** You press Record, do some clicking, press Stop. Now it repeats exactly that: same movements, same clicks, same delays. Always identical. That's a **plain macro**, and it already works without any script at all.

The tape recorder has one flaw: it's **blind**. It never looks at the screen. If the game loads three seconds slower today, it clicks on schedule anyway — into empty space. The whole run is wasted.

**The second machine is a to-do list with conditions.** Something like:

> 1. Wait until the "Claim" button shows up on screen.
> 2. Press it.
> 3. Replay actions 0 through 240 of the recording.
> 4. Read how many gems I have.
> 5. If it's under 500, go back to line 1.

That's a **script**. It can **wait**, **check** and **repeat the right number of times**. And inside itself it can still replay chunks of your recording — so you don't have to click everything out by hand again.

> **The key idea:** a script doesn't replace the recording. A script **drives** the recording. You record the boring part once, then bolt "wait for this" and "repeat until that" on top.

---

## 2. Do I even need one?

The honest answer: **usually no.** You need a script only when a blind replay isn't enough.

| Your situation | Script needed? |
|---|---|
| "I need the same thing pressed 500 times" | ❌ No. Just record + **Loop playback** |
| "I need it to run 3 hours and then shut the PC down" | ❌ No. That's **⏱ Time limit** in the main window |
| "I need it to stop when the bar turns red" | ❌ No. That's **🎯 Pixel condition** |
| "The button doesn't appear at a fixed time" | ✅ **Yes.** The `Wait for` step |
| "The button is in a different place every time" | ✅ **Yes.** The `Click image` step |
| "Keep going until I have 500 gems" | ✅ **Yes.** `While` + `Read number` |
| "If an error popup appears, close it; otherwise carry on" | ✅ **Yes.** `If` / `Else` |
| "Do exactly 20 rounds, then launch another program" | ✅ **Yes.** `While` + a variable + `Run` |

If you landed in the top three rows, close this guide — you don't need it. It's all checkboxes in the main window already.

---

## 3. Your first script in five minutes

We'll build the simplest possible thing: **a macro that waits 2 seconds, then replays your recording, over and over.**

Useless? Yes. But you'll see where everything lives, and the rest gets easy.

### Step 0. Record literally anything

Press `F6`, click around for a couple of seconds, press `F6` again. The window should show something like **📦 Events: 47**.

A script works without a recording too, but the editor won't open — it has nothing to show.

### Step 1. Open the editor

In the main window expand **✂ Editor** and press **✂ Open editor**.

A separate window opens. Three tabs across the top:

```
📦 Events: 47   ⏳ Length: 3.2 s  |  [ Story ] [ Raw events ] [ Script ]
```

### Step 2. Go to the Script tab

Press **Script**. You'll see:

> No script — the macro just replays the recording

That's correct. There genuinely isn't one yet.

### Step 3. Add the first step

At the bottom there's a dropdown and an **Add** button.

1. Pick **Wait** in the dropdown.
2. Press **Add**.

A line appears:

```
  0 Wait 1000 ms
```

Below, in the inspector, there's an **Insert pause (ms)** field. Put `2000` in it — two seconds.

```
  0 Wait 2000 ms
```

### Step 4. Add the second step

1. Pick **Play events** in the dropdown.
2. Press **Add**.

```
  0 Wait 2000 ms
  1 Play events 0…46  (47/47)
```

Notice it filled in the **whole recording** — event 0 through the last one. That's what you want 90 % of the time.

### Step 5. Run it

Close the editor window (or don't, it doesn't matter) and press **▶ Play** — or `F7`.

The macro waits two seconds and replays the recording. With **Loop playback** ticked in the main window, it keeps going round.

**That's it. You wrote a script.** Everything from here is just new kinds of steps.

> 💡 If anything goes wrong, press `F9` — emergency stop. It stops everything, always.

---

## 4. Reading the script window

A line in the list looks like this:

```
  3     Click image: claim_button ≥ 0.85
  ↑ ↑   ↑
  │ │   └── what the step does
  │ └────── indent = nesting (this step is inside an If or a While)
  └──────── step number
```

What the styling means:

| Appearance | Meaning |
|---|---|
| Normal text | A normal, enabled step |
| ~~Struck-through grey~~ | The step is **disabled** — the engine skips it. Toggle the **on** checkbox |
| 🟠 Orange text | The step **can never run** — it sits after `Quit the app` |
| 🟠 `⚠ Unbalanced blocks: …` | An error: a missing `End if` or `End while` somewhere. **This script will not run at all** |

The buttons under the list:

| Button | What it does |
|---|---|
| Dropdown + **Add** | Inserts a new step **right after the selected one** |
| **Step from selection** | Builds a `Play events` step from the `from`/`to` range set on the **Raw events** tab |
| **▲** / **▼** | Moves the selected step up/down |
| **Delete action** | Removes the selected step |
| **on** | Temporarily disables a step without deleting it |

Below that is the **inspector** — the fields for whichever step is currently selected. `Wait` has one field, `Click image` has three, `If` has a whole condition picker.

> ⚠️ The editor is locked while recording or playing. Press `F9` first, then edit.

---

## 5. Four rules to remember

### Rule 1. Steps run top to bottom

Like reading a book. Step 0, then 1, then 2. `If` and `While` can jump elsewhere, but everything else is strictly in order.

### Rule 2. The whole script is one "round"

**Loop playback** and **Play count: N** in the main window repeat the **entire script**, first step to last.

```
Play count: 3   →   [whole script] [whole script] [whole script]
```

Don't confuse this with the `While` step, which loops **inside** a single round.

### Rule 3. No script means the normal replay

An empty script (or one where **every** step is disabled) behaves exactly like a plain macro. Nothing breaks.

### Rule 4. Some main-window settings don't apply to scripts

This matters, and it's easy to forget:

| Setting | In a script |
|---|---|
| Loop playback / Play count | ✅ Works |
| Delay between loops | ✅ Works |
| Time limit (stop after N hours) | ✅ Works |
| Speed (slider and faster/slower hotkeys) | ⚠️ Affects the `Play events` step **only** |
| Window anchoring | ⚠️ Affects `Play events` and `Click at`, but not `Click image` |
| **Timing jitter** | ❌ Ignored |
| **🎯 Pixel condition** (stop on a pixel) | ❌ Ignored — use a `pixel` condition inside the script |
| **Action when the limit is hit** (shut down, sleep…) | ❌ Ignored — the script just stops |
| **Search area only / Try other scales** | ❌ Ignored — those apply to the test button in the panel |

If you want "shut the PC down when the script finishes", use a `Run` step with the path `shutdown` and the arguments `/s /t 60`.

---

## 6. Every step: the cheat sheet

| Step | In one line |
|---|---|
| **Play events** | Replay the recording from event N to event M |
| **Wait** | Just pause for that many milliseconds |
| **Wait for** | Block until a condition becomes true (or stops being true) |
| **Click image** | Find a picture on screen and click it |
| **Click at** | Click exact coordinates |
| **Key** | Press or release a single key |
| **Set** | Put a number in a variable (`=`, `+=`, `-=`, `*=`) |
| **If** | Start of "if this is true, then…" |
| **Else** | "…otherwise…" |
| **End if** | End of the `If` block |
| **While** | Start of "repeat while this is true" |
| **End while** | End of the loop |
| **Break** | Leave the loop early |
| **Run** | Open a program, file, folder or URL |
| **Quit the app** | Close Macro Recorder itself |
| **Note** | Write text to the log file (for debugging) |
| **Read number** | Recognise a number on screen into a variable |

---

## 7. Every step: in detail

### ▶ Play events

**Fields:** `from` (first event index), `to` (last event index).

Replays a slice of your recording with its original timing. This is the bridge between the tape recorder and the script.

How to find the indices:
1. Switch to the **Raw events** or **Story** tab.
2. Find the spot, read the row number on the left.
3. Or set the `from` / `to` fields at the bottom and press **Step from selection** — the numbers get filled in for you.

**Notes:**
- A freshly added step covers the **whole recording** (0 … last event).
- This is the **only** step affected by the speed slider and the faster/slower hotkeys.
- The "skip step" hotkey abandons the rest of the slice and moves on.
- If you later delete events in the editor, the indices in the step stay as they were. Check them.

---

### ⏸ Wait

**Field:** `Insert pause (ms)` — 0 to 3,600,000 (one hour).

Just waits. 1000 ms = 1 second.

**Note:** the speed slider does **not** affect this step. A 2000 ms wait is two seconds at 0.5× and at 3×.

Stop stays responsive during a wait: the engine sleeps in 15 ms slices and keeps checking whether it should quit.

---

### ⏳ Wait for

**Fields:** condition · `appears` / `disappears` · `Timeout (ms)` (default 10,000).

The most useful step in the whole set. It holds the script until something happens on screen.

- **appears** — wait until the condition becomes **true**;
- **disappears** — wait until it becomes **false** (a loading screen going away, for instance).

The condition is polled about eight times a second.

> ⚠️ **Very important:** when the timeout expires, the script does **not** stop and does **not** complain. It simply moves to the next step as if all were well (the log records `wait timed out`).
>
> So don't write this:
> ```
> 0  Wait for   image: claim  appears  (10000 ms)
> 1  Click at   (960, 540)
> ```
> If the picture never showed up, step 1 clicks anyway. Write this instead:
> ```
> 0  Wait for   image: claim  appears  (10000 ms)
> 1  If         image: claim ≥ 0.85
> 2      Click image: claim ≥ 0.85
> 3  End if
> ```

---

### 🖼 Click image

**Fields:** `Template` (file name) · threshold (0.30–1.00) · mouse button.

Finds a picture on screen and clicks its **centre**.

- The template is the name of a PNG in the `templates/` folder. Both `claim_button` and `claim_button.png` work.
- If the picture is **not found**, the step quietly does nothing and the script carries on.
- There's a 30 ms gap between press and release.
- Window anchoring does **not** apply here: the coordinates come from the actual match, so they're already correct.

How to make templates — [section 11](#11-image-templates-how-to-make-good-ones).

---

### 🖱 Click at

**Fields:** `X` · `Y` · mouse button.

Clicks exact screen coordinates, measured from the top-left corner (they can be negative on a multi-monitor setup).

Where to get the numbers: in **🎯 Pixel condition** press **🎯 Pick in 3 s** and hover the spot — X and Y land in the fields, and you can copy them across.

Window anchoring **does** apply to this step: if the window moved, the click moves with it.

> 💡 Fixed-coordinate clicking is the most fragile kind of automation there is. Prefer `Click image` when you can.

---

### ⌨ Key

**Fields:** key from the list · `press` / `release`.

Sends **one** keyboard event. Not "press and release" — one.

To type a space you need **two steps**:

```
0  Key Space press
1  Key Space release
```

For `Ctrl+C` you need **four**:

```
0  Key Ctrl press
1  Key C press
2  Key C release
3  Key Ctrl release
```

> ⚠️ Forget the release and the key stays held for the rest of the run. Emergency stop (`F9`) force-releases everything, but don't rely on it.

Thirty common keys are available: `Space`, `Enter`, `Tab`, `Esc`, `Backspace`, `Delete`, `Shift`, `Ctrl`, `Alt`, arrows, `A B C D E Q R S W`, `1 2 3`, `F1 F2 F5 F8 F9`. Need something else? Record it normally and drop it in with `Play events`.

---

### 🔢 Set

**Fields:** variable name · operation (`=`, `+=`, `-=`, `*=`) · value.

Puts a number in a variable. Details in [section 9](#9-variables-the-scripts-memory).

```
Set  rounds  =   0     ← reset
Set  rounds  +=  1     ← add one
```

---

### ❓ If / Else / End if

A fork in the road. Details in [section 10](#10-blocks-if-and-while).

---

### 🔁 While / End while / Break

A loop. Details in [section 10](#10-blocks-if-and-while).

---

### 🚀 Run

**Fields:** `Path or URL` · `Arguments`.

Opens just about anything, without waiting for it to finish:

| What you type | What happens |
|---|---|
| `C:\Games\game.exe` | Launches the program |
| `notepad` | Won't work — use a full path or `notepad.exe` |
| `https://example.com` | Opens in your default browser |
| `D:\notes\plan.txt` | Opens in the default application |
| `D:\screenshots` | Opens the folder in Explorer |
| `shutdown` + arguments `/s /t 60` | Shuts the PC down in 60 seconds |

How it works internally: `.exe`, `.bat`, `.cmd` and `.com` are started directly; everything else is handed to the shell via `cmd /C start`, which is why shortcuts, documents and URLs open too. No console window flashes up.

Arguments are split on spaces. An argument containing a space (a path like `C:\My Files\a.txt`) can't be passed correctly — that's a limitation.

---

### 🛑 Quit the app

No fields. Closes Macro Recorder completely.

For the "do the job then exit" pattern:

```
...
8  If         variable  gems >= 500
9      Note: target reached, exiting
10     Quit the app
11 End if
```

> ⚠️ Anything **after** this step at the top level (not inside an `If`/`While`) will never run. The editor colours those lines orange and says: *"⚠ Steps below can never run"*.

---

### 📝 Note

**Field:** `Text`.

Does nothing at all — it just writes your line into the log file. This is the main debugging tool, see [section 13](#13-debugging-seeing-whats-going-on).

---

### 🔤 Read number

**Fields:** `Variable` · `Region` (X, Y, W, H).

Recognises the text in a screen rectangle, pulls a number out of it and stores it in a variable.

How to set the region without counting pixels:
1. In the main window expand **🔤 Text on screen**.
2. Press **🎯 Pick in 3 s**.
3. Hover the **top-left** corner of the region, wait for the countdown.
4. Hover the **bottom-right** corner, wait again.
5. It immediately shows you what it read there.
6. Go back to your script step and press **⤵ from the panel** — all four numbers get copied in.

What the number parser understands:

| On screen | In the variable |
|---|---|
| `1250` | `1250` |
| `Gems: 1,250` | `1250` |
| `1 250` | `1250` |
| `Remaining: 500.` | `500` |
| `02:34` | `154` (a clock becomes **seconds**) |
| `1:02:03` | `3723` |
| `gibberish` | `0` |

> ⚠️ If the OCR engine errors out (a region smaller than 40×40 px, for instance), the variable **keeps its old value**. Reset it with a `Set` step before the first read.

---

## 8. Conditions: how the macro looks at the screen

A condition is a yes/no question. Conditions are used by three steps: `Wait for`, `If` and `While`.

There are six of them.

### 1️⃣ always

The answer is always yes. Useful for:

- `While always` — an infinite loop (leave it with `Break` or `Quit the app`);
- `If always` — temporarily "switch on" a chunk of script without rewriting the condition.

### 2️⃣ variable

**Fields:** name · comparison (`==`, `!=`, `<`, `<=`, `>`, `>=`) · number.

```
While  rounds < 20
If     gems >= 500
```

A variable that was never assigned counts as **0**.

> ⚠️ `==` on fractional numbers is a bad idea. `0.1 + 0.2` isn't exactly `0.3` in a computer. Use `>=` and `<=`.

### 3️⃣ image

**Fields:** `Template` · threshold (0.30–1.00).

"Is this picture on screen right now?"

- Searches the **whole desktop** (all monitors) at 1:1 scale.
- Side effect: it fills the built-in variables `match_x`, `match_y`, `match_score` — handy for debugging.
- 0.85 is a good starting threshold. Lower means more false hits; higher and antialiasing starts breaking matches.

### 4️⃣ pixel

**Fields:** `X` · `Y` · colour · `Tolerance` (0–255).

"Is the point (X, Y) currently this colour?"

Tolerance is ±N on each of the R, G, B channels. `20` is sensible, `0` means an exact colour only, `60` means "roughly that sort of colour".

The fastest condition by far — one pixel instead of scanning the screen. If you need to check something hundreds of times in a loop, use a pixel, not an image.

Where to get the coordinates and colour: **🎯 Pixel condition → 🎯 Pick in 3 s**.

### 5️⃣ window

**Field:** title.

"Is there a window with this title open?"

It first looks for a window whose title matches **exactly**. Failing that, it walks every visible window and takes the first one whose title **contains** what you typed (case-insensitive, first 24 characters).

So `roblox` is enough to catch `Roblox Player`.

Good for: "wait until the game has started", "if an error window is up, close it".

### 6️⃣ text

**Fields:** `Contains` · `Region` (X, Y, W, H).

"Does this word appear in this rectangle of the screen?"

The comparison is deliberately **loose**: case is ignored, extra whitespace is collapsed, punctuation is dropped. Because text recognition never returns exactly what a human reads.

So `You win!` matches a recognised `YOU  WIN !`.

> ⚠️ The slowest condition there is. Don't put it in a loop that spins hundreds of times a second.
>
> ⚠️ It uses the text recognition built into Windows. If your game isn't in English, add the language in *Windows Settings → Time & language*. Stylised game fonts read badly — check first with the **🔤 Read now** button.

---

## 9. Variables: the script's memory

A variable is a labelled box holding **one number**.

- You choose the name: `rounds`, `gems`, `n`, `count`.
- The value is always a number (fractions allowed). **You cannot store text.**
- An unset variable is **0**.

### Using one

Storing a value — the **Set** step:

| Operation | Effect | Example |
|---|---|---|
| `=` | Assign | `rounds = 0` |
| `+=` | Add | `rounds += 1` |
| `-=` | Subtract | `lives -= 1` |
| `*=` | Multiply | `bet *= 2` |

Checking a value — the **variable** condition:

```
While  rounds < 20
If     gems >= 500
```

### The classic counter

Three steps worth memorising:

```
0  Set  rounds = 0             ← reset before the loop
1  While  rounds < 20          ← the exit condition
2      Play events 0…46
3      Set  rounds += 1        ← ⚠️ MUST be inside the loop
4  End while
```

> ⚠️ Forget step 3 and you get an infinite loop: `rounds` stays 0 forever.

### Built-in variables

Filled in automatically whenever the script searches for something:

| Variable | Contents |
|---|---|
| `match_x` | X of the centre of the last image match |
| `match_y` | Y of the centre of the last image match |
| `match_score` | How well it matched: 0.00 … 1.00 |

The most useful application is diagnosis. Picture not being found? Put this after the check:

```
If  variable  match_score < 0.85
    Note: image not found, best match too weak
End if
```

and read the log.

### Starting values

The interface has **no** field for starting values — every run takes them from the `vars` field in the macro file, and everything else starts at zero.

If you really need them, open the `.json` in a text editor and add:

```json
"vars": { "rounds": 0, "gems": 0 }
```

In practice it's easier to make `Set rounds = 0` the first step. Same result, no hand-editing.

> ⚠️ Variables are **not** kept between runs. Every new run — and every round of loop playback — starts fresh.

---

## 10. Blocks: `If` and `While`

Both work the same way: an opening step, a closing step, contents in between. Like brackets.

### If … Else … End if

```
0  If  image: error_popup ≥ 0.85
1      Click image: ok_button ≥ 0.85
2      Wait 500 ms
3  Else
4      Play events 0…46
5  End if
6  Wait 1000 ms
```

Read it as: if an error popup is on screen, run steps 1–2; otherwise run step 4. Either way, step 6 comes next.

**`Else` is optional.** Often you just want "do this if true":

```
0  If  variable  lives <= 0
1      Note: dead, exiting
2      Quit the app
3  End if
```

### While … End while

```
0  Set  rounds = 0
1  While  variable  rounds < 20
2      Play events 0…46
3      Set  rounds += 1
4  End while
```

How it runs:
1. Reach `While` — test the condition.
2. True → run the contents, hit `End while`, **jump back to step 1** to test again.
3. False → jump straight past `End while`.

If the condition is false from the start, the contents never run **at all**.

### Break

Leaves the **innermost** enclosing loop early:

```
0  While  always
1      Play events 0…46
2      If  image: inventory_full ≥ 0.85
3          Break             ← leave the While
4      End if
5      Wait 2000 ms
6  End while
7  Note: inventory filled up
```

### Nesting

Blocks nest as deep as you like. The indentation is drawn automatically, so the structure is visible at a glance:

```
0  While  variable  rounds < 10
1      If  image: bonus ≥ 0.85
2          Click image: bonus ≥ 0.85
3          Wait 300 ms
4      End if
5      Play events 0…46
6      Set  rounds += 1
7  End while
```

### Rules checked before anything runs

The app won't run a malformed script. Before starting it checks:

| Mistake | Message |
|---|---|
| `If` with no `End if` | `⚠ Unbalanced blocks: #0` |
| `End while` with no `While` | `⚠ Unbalanced blocks: End while #4` |
| A `While` closed by `End if` | `⚠ Unbalanced blocks: While #2` |
| Two `Else` for one `If` | `⚠ Unbalanced blocks: Else #5` |
| `Else` outside any `If` | `⚠ Unbalanced blocks: Else #3` |

While that message is showing, **the script does not execute at all** — not partially, not at all. Fix the blocks.

### Runaway protection

If a script executes 50,000,000 steps in a single run, the engine stops it and logs `script exceeded its step budget`. That's the safety net for a typo like `While rounds < 20` with no `rounds += 1` — it won't leave your machine spinning forever.

---

## 11. Image templates: how to make good ones

A template is a small PNG of the button or icon you want found.

### Step by step

1. Open the game the way it will look while the macro runs.
2. Press `Win+Shift+S` (the Windows snipping tool) and select **just the button**.
3. In Macro Recorder expand **🔎 Image search** and press **📋 Paste**.
4. Press **🔍 Find on screen**. You should get `Found at (x, y) — 0.98`.
5. Press **💾 Save PNG…** and save it into the `templates/` folder under a clear name, e.g. `claim_button.png`.
6. In the script step, type the name into the `Template` field: `claim_button`.

The `templates/` folder sits next to `config.json` — the exact path is shown in the main window under **📁 Files**.

### What makes a good template

| ✅ Do | ❌ Don't |
|---|---|
| Crop just the button | Don't capture half the screen |
| Pick something that doesn't change | Don't include timers, counters or animation |
| Aim for 30–150 px per side | Don't use 10×10 — it'll match anything |
| Snip at the resolution the macro will run at | Don't move templates between monitors |
| Verify with **🔍 Find on screen** | Don't assume "it should work" |

### The threshold

| Value | When |
|---|---|
| `1.00` | Pixel-perfect only. Almost never fires in practice |
| `0.90` | Strict. For crisp static graphics |
| **`0.85`** | **The default — start here** |
| `0.75` | Forgiving. For a button that glows or pulses slightly |
| `0.60` | Very forgiving. Expect false matches |

The score is **normalised**, which means overall brightness and contrast don't matter. A template snipped in the game's day theme usually still matches at night.

### The transparency trick

Fully transparent pixels in the PNG are **excluded from the comparison**.

So in any editor (Paint.NET, GIMP, Photoshop) you can erase the background around a round icon to transparency — and it will then be found on any backdrop: grass, stone, water.

### Important for scripts

The **Search area only** and **Try other scales** options in the panel apply to the **test button only**. Inside a script the search is always:

- across the **whole** screen;
- at **1:1** scale.

Practical consequence: if a scripted search feels slow, shrink the template, not the area.

---

## 12. Three worked examples

### Example A. "Click the button the moment it appears"

Goal: a "Claim" button shows up at unpredictable times. Press it immediately, forever.

You'll need: a `claim.png` template in `templates/`.

```
0  Wait for  image: claim ≥ 0.85  appears  (30000 ms)
1  If        image: claim ≥ 0.85
2      Click image: claim ≥ 0.85
3      Wait 500 ms
4  Else
5      Note: button did not appear within 30 seconds
6  End if
```

Tick **Loop playback** in the main window. The script goes round and round: wait → press → wait → press.

Why the `If` after `Wait for`: on timeout, `Wait for` silently gives up, and without the check step 2 would try to click nothing. `Click image` would also do nothing on its own — but with the `Note` you at least see the problem in the log.

---

### Example B. "Do exactly 20 rounds, then quit"

Goal: replay a recorded farm 20 times and exit, with a pause between rounds.

```
0  Set  rounds = 0
1  While  variable  rounds < 20
2      Play events 0…240  (241/241)
3      Set  rounds += 1
4      Note: round finished
5      Wait 2000 ms
6  End while
7  Note: 20 rounds done
8  Quit the app
```

> 💡 You could do the same with no script at all: untick **Loop playback** and set **Play count: 20**. The script is worth it once you need to check something between rounds.

---

### Example C. "Farm until 500 gems, then shut the PC down"

The realistic scenario. You'll need: a `claim.png` template and a screen region showing the gem counter.

```
0   Set  gems = 0
1   While  variable  gems < 500
2       If  image: error_popup ≥ 0.85
3           Click image: ok_button ≥ 0.85
4           Wait 1000 ms
5       End if
6       Play events 0…240
7       Wait for  image: claim ≥ 0.85  appears  (15000 ms)
8       Click image: claim ≥ 0.85
9       Wait 800 ms
10      Read number (1620,40 300x80) → gems
11      Note: another round done
12  End while
13  Note: 500 gems reached
14  Run  shutdown   /s /t 60
15  Quit the app
```

Walking through it:

- **0** — reset the counter; otherwise it inherits an old value (and before the first `Read number` it's unknown anyway).
- **2–5** — the janitor: if an error popup showed up, close it and carry on. If not, the block is skipped.
- **6** — the actual work: replay the recorded slice.
- **7–8** — wait for the button and press it.
- **10** — read the gem counter off the screen into the variable. Get the region from the **🔤 Text on screen** panel with **⤵ from the panel**.
- **14** — the Windows shutdown command with a 60-second delay. Cancel it by running `shutdown /a` in a terminal.
- **15** — close Macro Recorder itself.

> ⚠️ Note that the shutdown is a `Run` step, **not** the "Action when the limit is hit" setting in the main window. That setting does nothing in script mode.

---

## 13. Debugging: seeing what's going on

A script runs silently. Here are four ways to look inside.

### Method 1. Notes in the log — the main tool

Scatter `Note` steps at the interesting points:

```
0  Note: === round start ===
1  If  image: bonus ≥ 0.85
2      Note: bonus found
3      Click image: bonus ≥ 0.85
4  Else
5      Note: no bonus, skipping
6  End if
```

Then open the log file:

```
<data folder>/logs/macro-recorder.log.2026-08-16
```

The exact data-folder path is shown in the main window under **📁 Files**. Any text editor opens it.

Your notes will be in there, alongside the engine's own entries:

| Log line | Meaning |
|---|---|
| `script: your text` | Your `Note` |
| `playing events 0..=240 of 241` | A `Play events` step started |
| `wait timed out` | `Wait for` gave up and moved on |
| `while at #3: condition false, leaving the loop` | The loop ended |
| `ocr read 'Gems: 1,250' -> gems = 1250` | `Read number` worked, and what it read |
| `template 'claim' could not be loaded` | Template file not found — check the name and `templates/` |
| `ocr failed: …` | Recognition failed; the variable was left alone |
| `script rejected: unbalanced blocks near …` | Blocks don't match up; nothing ran |
| `script exceeded its step budget` | The runaway protection kicked in |

### Method 2. Disable steps instead of deleting them

The **on** checkbox switches a step off temporarily. The line goes ~~struck-through~~ and the engine skips it.

That's how you bisect a problem: disable half the script — still broken? Then it's in the other half.

### Method 3. The "skip step" hotkey

It is **unbound** by default. Bind it under **⌨ Hotkeys → Skip step:** (to `F10`, say).

Now pressing it mid-run abandons the current step and moves to the next one. Invaluable when the script is stuck on a `Wait for` and you want to see what happens afterwards.

### Method 4. Test conditions on their own

Before putting a condition in a script, try it by hand in the main window:

- **image** → **🔎 Image search → 🔍 Find on screen**;
- **text** and **Read number** → **🔤 Text on screen → 🔤 Read now**;
- **pixel** → **🎯 Pixel condition → 🎯 Pick in 3 s**.

If it doesn't work in the panel, it certainly won't work in the script.

---

## 14. Common traps

| Symptom | Cause | Fix |
|---|---|---|
| The script won't start, orange warning at the top | Unbalanced blocks | Count them: every `If` needs an `End if`, every `While` an `End while` |
| The script ends instantly, having done nothing | Every step disabled, or `Quit the app` is at the top | Check the **on** boxes and the step order |
| The image is never found | Template not in `templates/`, a typo in the name, or a different screen resolution | Verify with **🔍 Find on screen**. Look for `template … could not be loaded` in the log |
| The wrong thing gets matched | Threshold too low, template too small | Raise it to 0.90, use a bigger and more distinctive crop |
| `Wait for` doesn't wait, it races ahead | The condition is already true, or the timeout expired | Increase the timeout; check `appears` vs `disappears` |
| The `While` loop never ends | Nothing inside it changes the variable | Add `Set rounds += 1` **inside** the loop |
| A key stays held down | A `press` with no matching `release` | Add the pairing step. `F9` force-releases everything |
| Clicks land in the wrong place | The window moved or the resolution changed | Use `Click image` instead of `Click at`, or turn on window anchoring |
| `Read number` gives 0 | OCR recognised nothing | Verify with **🔤 Read now**. Use a bigger region, add the language pack in Windows |
| `Read number` doesn't change the variable | OCR errored (region under 40×40, for instance) | Enlarge the region. The log will say `ocr failed` |
| The speed slider does nothing | In a script it only affects `Play events` | Edit the values in your `Wait` steps |
| The pixel stop condition never fires | The **🎯 Pixel condition** setting is ignored in script mode | Use a `pixel` condition inside `Wait for` / `If` / `While` |
| The PC doesn't shut down after the time limit | "Action when the limit is hit" is ignored in script mode | Use a `Run` step → `shutdown` with `/s /t 60` |
| The script sits still, status says "Waiting for the window…" | **Target window → Pause while it is not in front** is on | Switch to that window, or untick the box |
| Everything froze, status says "Held" | The app is on another virtual desktop | Go back to the desktop where Macro Recorder lives |
| The exported `.exe` can't find images | Templates weren't shipped with it | Copy the `templates/` folder next to the exported `.exe` |
| The `.ahk` export lost all the logic | Working as intended | Only recorded events are translated to AutoHotkey |

---

## 15. What scripts can't do

An honest list, so you don't hunt for buttons that aren't there:

- **No text variables.** Numbers only. There's nowhere to keep a string.
- **No arithmetic between variables.** `rounds += 1` is fine; `total = rounds * gems` is not. Variable and constant only.
- **No functions or subroutines.** A repeated chunk has to be duplicated.
- **No `and` / `or` in conditions.** One condition is one test. Combine them by nesting `If`:
  ```
  If  image: A ≥ 0.85
      If  variable  gems > 100
          ...
      End if
  End if
  ```
- **No `else if`.** Nest an `If` inside the `Else`.
- **No waiting for a launched program.** `Run` starts the process and moves straight on.
- **No file, clipboard or network access.**
- **No way to use `match_x` / `match_y` in a `Click at` step.** Those fields are plain numbers; a variable can't go in them. Clicking a found image is what the separate `Click image` step is for.
- **Variables don't survive a restart.** Every run starts fresh.

If you hit this list, what you probably want is AutoHotkey. That's fine: Macro Recorder is designed as a recorder with conditions, not as a programming language.

---

## 16. What it looks like inside the file

The script is stored inside the macro file (`.json`). Any text editor opens it — edit it there if you prefer.

```json
{
  "version": 3,
  "duration_us": 8000000,
  "events": [ ... your recording ... ],
  "script": [
    { "kind": { "SetVar": { "name": "rounds", "op": "Set", "value": 0.0 } }, "enabled": true },
    { "kind": { "While":  { "cond": { "Var": { "name": "rounds", "cmp": "Lt", "value": 20.0 } } } }, "enabled": true },
    { "kind": { "PlayEvents": { "from": 0, "to": 240 } }, "enabled": true },
    { "kind": { "SetVar": { "name": "rounds", "op": "Add", "value": 1.0 } }, "enabled": true },
    { "kind": "EndWhile", "enabled": true }
  ],
  "vars": { "rounds": 0.0 }
}
```

Name mapping:

| In the UI | In the file |
|---|---|
| Play events | `PlayEvents { from, to }` |
| Wait | `Wait { ms }` |
| Wait for | `WaitFor { cond, appear, timeout_ms }` |
| Click image | `ClickImage { template, threshold, button }` |
| Click at | `Click { x, y, button }` |
| Key | `Key { vk, down }` |
| Set | `SetVar { name, op, value }` |
| If / Else / End if | `If { cond }` / `Else` / `EndIf` |
| While / End while / Break | `While { cond }` / `EndWhile` / `Break` |
| Run | `Run { path, args }` |
| Quit the app | `Exit` |
| Note | `Log { text }` |
| Read number | `ReadNumber { x, y, w, h, var }` |

Conditions: `Always`, `Var { name, cmp, value }`, `Image { template, threshold }`, `Pixel { x, y, r, g, b, tol }`, `Window { title }`, `Text { x, y, w, h, needle }`.

Comparisons: `Eq` `Ne` `Lt` `Le` `Gt` `Ge`. Operations: `Set` `Add` `Sub` `Mul`.

> ⚠️ Save broken JSON and the app will refuse to open the file. Keep a copy before editing.
>
> ⚠️ A `.mrz` file is compressed and won't open in a text editor. Re-save it as `.json` first.

---

## 17. One-page cheat sheet

**Open the script editor:** main window → **✂ Editor** → **✂ Open editor** → **Script** tab

**Add a step:** pick a kind in the dropdown → **Add** (it lands after the selected step)

**Emergency stop:** `F9`

---

**Steps**

```
Play events from…to      replay a slice of the recording
Wait N ms                 just pause
Wait for <condition> appears/disappears (timeout)
Click image <template> <threshold> <button>
Click at X,Y <button>
Key <key> press/release
Set <name> = += -= *= <number>
If <condition> … Else … End if
While <condition> … Break … End while
Run <path> <arguments>
Quit the app
Note <text>
Read number <region> → <variable>
```

**Conditions**

```
always
variable <name> == != < <= > >= <number>
image <template> <threshold 0.30–1.00>
pixel X,Y <colour> <tolerance 0–255>
window <title>
text "word" <region>
```

---

**The three things people forget most**

1. Every `If` needs an `End if`. Every `While` needs an `End while`.
2. Something inside a `While` must change the variable in its condition.
3. Every `Key … press` needs a `Key … release`.

**Threshold:** start at `0.85`; not found → `0.75`; matching the wrong thing → `0.90`.

**Log:** `<data folder>/logs/macro-recorder.log.YYYY-MM-DD` — the folder path is shown under **📁 Files**.

---

Still stuck? Open an [issue](../../issues) with the macro file and the relevant part of the log. The `Note` step writes straight there — scatter a few in first.
