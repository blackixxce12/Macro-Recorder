# ⌨ Text expander

### A guide for someone who has never used one

You type `addr` and press space. Those four letters vanish and a full postal address appears in their place, on two lines. That is the whole idea.

There is no programming. There is a list: a short abbreviation on the left, the long text on the right.

[🇷🇺 Русская версия](EXPANDER_RU.md) • [← Back to README](README.md)

---

## 📑 Contents

1. [What it is for](#1-what-it-is-for)
2. [Your first entry in two minutes](#2-your-first-entry-in-two-minutes)
3. [Three ways it can fire](#3-three-ways-it-can-fire)
4. [What else you can put in a replacement](#4-what-else-you-can-put-in-a-replacement)
5. [Typing versus pasting](#5-typing-versus-pasting)
6. [When it stays quiet](#6-when-it-stays-quiet)
7. [The expansions.json file](#7-the-expansionsjson-file)
8. [Worked examples](#8-worked-examples)
9. [Common traps](#9-common-traps)
10. [What it cannot do](#10-what-it-cannot-do)
11. [About privacy](#11-about-privacy)
12. [Cheat sheet](#12-cheat-sheet)

---

## 1. What it is for

There is text you type over and over. An address. A loyalty card number. An email sign-off. A reply template. A commit message skeleton. Bank details.

Each one is twenty or thirty seconds and a chance to make a typo. An expander turns them into four letters.

It works **everywhere**: in a browser, a chat client, an email, a code editor, a game. The program watches what you type, and when it recognises an abbreviation it deletes it and writes the replacement instead.

> **Worth understanding straight away:** this is the only feature in the whole program that involves itself in ordinary typing. Everything else only happens once you press Play. So the expander is **off until you turn it on**, and turning it on should be a decision.

---

## 2. Your first entry in two minutes

### Step 1. Find the section

In the main window, expand **⌨ Text expander**.

### Step 2. Switch it on

Tick **Expand abbreviations as I type**.

### Step 3. Add an entry

Press **+ Add**. An empty row appears.

- In the **short** field type: `ph`
- In the large field below it, type your phone number

### Step 4. Try it

Open Notepad. Type `ph` and press **space**.

The two letters disappear and the number takes their place.

**Done.** Everything else in this guide is detail.

> 💡 There is nothing to save — the entry is written to the file for you.

---

## 3. Three ways it can fire

This is the main setting, and it is chosen **per entry**. It is the dropdown in the entry's row.

### after a delimiter — the default

The abbreviation fires once you have finished typing it **and pressed space** — or Enter, Tab, a full stop, a comma, anything in the delimiter list.

```
addr + space  →  221B Baker Street
```

**Why this is the default.** It is the only mode that stays out of the way of ordinary typing. Until the word has ended, nothing interferes.

It also has a pleasant property: abbreviations may be the beginning of one another. If you have `sig` and `signature`, then `signature` + space expands to the longer one — because nothing is decided until the word is over.

### behind a marker

The abbreviation fires only when a marker precedes it: `;;`, `//`, `:` — whatever you put in the small field beside the dropdown.

```
;;ph  →  +44 20 7946 0000     (fires at once, no space needed)
ph    →  nothing
```

**When to use it.** When the abbreviation is a real word that appears in ordinary text. `me`, `id`, `to` in delimiter mode will fire in the middle of a letter and drive you mad. Behind a marker, never.

### immediately

Fires the moment the last characters match. No space needed.

```
;sig  →  Kind regards, Sherlock     (instantly)
```

**Careful.** This mode has a property worth knowing: it **cannot wait**. If you have `;sig` and `;signature`, the short one goes off halfway through the long one and you physically cannot finish typing. So "immediately" belongs to abbreviations that are not the beginning of any other, ideally behind an unusual character.

### default

"default" inherits from the **Default trigger** setting above the list. Change it there and every entry set to "default" follows.

---

## 4. What else you can put in a replacement

The replacement field takes more than letters. Anything in braces is a placeholder.

| You write | You get |
|---|---|
| `{date}` | `2026-08-18` |
| `{date:dd.MM.yyyy}` | `18.08.2026` |
| `{time}` | `01:23` |
| `{time:HH:mm:ss}` | `01:23:45` |
| `{datetime}` | `2026-08-18 01:23` |
| `{clipboard}` | Whatever is on the clipboard right now |
| `{cursor}` | Where the caret ends up afterwards |
| `{key:Tab}` | A Tab keystroke |
| `{key:Enter}` | An Enter keystroke |
| `{random:yes\|no\|maybe}` | One of the list, picked at random |

### About the date pattern

The letters: `yyyy` year, `MM` month, `dd` day, `HH` hour, `mm` minute, `ss` second. Case matters — `MM` is the month, `mm` is the minute.

Everything else in the pattern is left alone, so `{date:dd.MM.yyyy}` and `{date:dd/MM/yy}` both work.

### About `{cursor}`

Put it where you want to carry on typing.

```
Hello {cursor},

Kind regards,
Sherlock
```

After it fires the caret sits after "Hello ", so you type the name without touching the mouse.

### About `{key:...}`

For filling in forms, where Tab moves to the next field.

```
Sherlock Holmes{key:Tab}sherlock@example.com{key:Tab}
```

Available: `Tab`, `Enter`, `Esc`, `Space`, `Backspace`, `Delete`, `Home`, `End`, `Up`, `Down`, `Left`, `Right`, `PageUp`, `PageDown`.

### Writing a literal brace

Put a backslash in front of it:

```
a literal \{date} stays as written
```

An unclosed brace, or an unknown word inside braces, is left exactly as you wrote it — that is a typo rather than a command, and swallowing it silently would be worse.

### Multi-line text

Just press Enter in the replacement field. The line breaks are kept.

---

## 5. Typing versus pasting

The second dropdown in the entry's row.

| | **type** | **paste** |
|---|---|---|
| How | One character at a time, as though you typed very fast | Puts the text on the clipboard and presses Ctrl+V |
| Works everywhere | Yes | No — in terminals Ctrl+V is often not paste |
| Long text | Noticeably slower | Instant |
| Clipboard | Untouched | Borrowed for a fraction of a second, then given back |

**Which to choose.** Leave it on "type". Switch to "paste" only when a replacement is long — several lines or more — and you can see it being typed out letter by letter.

The clipboard is restored automatically. But if what you had on it was not text — an image, a file — the program cannot put it back. One more argument for keeping "type" as the default.

---

## 6. When it stays quiet

This matters as much as when it fires. A false expansion is far more irritating than one that did not happen.

It does **not** fire when:

| Situation | Why |
|---|---|
| The abbreviation is inside a word: `readdr` | The word boundary is checked |
| The entry's checkbox is off | Obvious |
| The master switch is off | Obvious |
| A macro is **recording** | Otherwise the expansion is written into the macro |
| A macro is **replaying** | Otherwise it fights with the macro |
| You clicked the mouse mid-word | The caret could be anywhere; what was typed is no longer known |
| You pressed an arrow, Home, End, Esc | Same reason |
| You switched away and came back | What was typed belonged to another window |
| You held Ctrl or Alt with a key | That is a command, not typing |
| The window title contains something from the exclusion list | You said so |
| An IME is active (Chinese, Japanese) | The number of characters to delete is unknowable |
| A dead key was used (`´` + `e` → `é`) | Same: two keystrokes, one character |

The last two are a refusal rather than a failure. The expander does not guess where it cannot count.

### The exclusion list

The **Never in windows** field above the entries. Comma-separated, matched anywhere in the title, case-insensitive.

**Put your password manager in it.** And your terminal, if you use one.

```
KeePass, Bitwarden, 1Password, PowerShell, cmd.exe
```

---

## 7. The expansions.json file

Everything lives in `expansions.json` beside the settings. The exact path is shown in the main window under **📁 Files**.

**Edit expansions.json** opens it, and **Reload** picks the changes back up.

Editing it by hand is rarely necessary, but sometimes easier — adding thirty entries at once, or moving your list to another machine.

```json
{
  "enabled": true,
  "default_trigger": "Delimiter",
  "delimiters": " \t\n.,;:!?)]}\"'",
  "excluded_windows": ["KeePass", "PowerShell"],
  "entries": [
    {
      "enabled": true,
      "abbr": "addr",
      "text": "221B Baker Street\nLondon NW1 6XE",
      "trigger": "Inherit",
      "insert": "Type"
    },
    {
      "enabled": true,
      "abbr": "ph",
      "text": "+44 20 7946 0000",
      "trigger": { "Prefix": ";;" },
      "insert": "Type"
    }
  ]
}
```

Name mapping:

| In the UI | In the file |
|---|---|
| default | `"Inherit"` |
| after a delimiter | `"Delimiter"` |
| behind a marker | `{ "Prefix": ";;" }` |
| immediately | `"Instant"` |
| type | `"Type"` |
| paste | `"Paste"` |

> ⚠️ A line break is written as `\n` in JSON, not as a real Enter.
>
> ⚠️ Break the JSON and the program refuses to read it and says so in the log. The file is not overwritten, so your edit is still there to fix.

---

## 8. Worked examples

### An email sign-off

```
Abbreviation:  ;sig         Mode: immediately
Replacement:
Kind regards,
Sherlock Holmes
+44 20 7946 0000
```

### A date for a filename

```
Abbreviation:  dd           Mode: after a delimiter
Replacement:   {date:yyyy-MM-dd}
```

### A reply template with the caret in the right place

```
Abbreviation:  rep          Mode: after a delimiter
Replacement:
Hello,

{cursor}

Do let me know if anything is unclear.
```

### A whole form from one abbreviation

```
Abbreviation:  ;;form       Mode: behind a marker (;;)
Replacement:   Sherlock Holmes{key:Tab}sherlock@example.com{key:Tab}+442079460000
```

### Wrap whatever you just copied in quotes

```
Abbreviation:  q            Mode: after a delimiter
Replacement:   "{clipboard}"
```

### A long contract template

```
Abbreviation:  contract     Mode: after a delimiter
Insert:        paste
Replacement:   ...a page and a half of text...
```

---

## 9. Common traps

| Symptom | Cause | Fix |
|---|---|---|
| Nothing happens | The master switch or the entry is off | Check both boxes |
| Nothing happens | The abbreviation is empty | Empty entries are skipped |
| Nothing happens | The window is on the exclusion list | Check **Never in windows** |
| Nothing happens, but it used to | A macro is recording or replaying | Press `F9` |
| Fires in the middle of a word | "immediately" on a short abbreviation | Switch to "after a delimiter", or add a marker |
| The long abbreviation never fires, the short one does | "immediately" cannot wait | Switch to "after a delimiter" |
| One line where there should be two | The application refused the line break | Try another application; some fields forbid them |
| The wrong thing fired after changing keyboard layout | The abbreviation looked like the start of a word | Known limitation, see below |
| The text types out slowly, letter by letter | A long replacement in "type" mode | Switch to "paste" |
| The clipboard came back wrong | It held something that was not text | Use "type" for that entry |
| `{date}` appeared literally | A typo in the placeholder name | Unknown placeholders are left as written — check the table |

### About changing keyboard layout

If you change layout **with the mouse**, using the language bar, mid-word, the buffer is emptied — the program treats a click as a reason to forget what was typed. The next abbreviation may then look like the start of a word.

Changing layout with the keyboard (Alt+Shift, Win+Space) is handled and causes no such problem.

---

## 10. What it cannot do

An honest list, so you do not go looking.

- **No forms.** `{form:Name}` does not exist. That is a window with fields and validation — perhaps later.
- **No `{selection}`.** There is no way to read selected text in another application: the only route is to send Ctrl+C and take it off the clipboard, which breaks the clipboard and does not work everywhere. Besides, the expander fires on typing, and typing over a selection already replaces it.
- **No undo with Backspace.** If the wrong thing expanded, delete it by hand.
- **No case awareness.** `ADDR` and `addr` are the same abbreviation, and the replacement is always exactly as stored.
- **No nesting.** A replacement cannot contain another abbreviation.
- **No conditions or variables.** [Scripts](SCRIPTS.md) do that — a different job, but they can think.
- **64 characters of memory.** A longer abbreviation will not fire. In practice this is not a limit.

---

## 11. About privacy

Said plainly, because it deserves a plain conversation.

To recognise an abbreviation, the program keeps in memory what you have **just typed**. That is a short step from what a keylogger keeps, and it would be dishonest not to say so.

What is done to keep the line clear:

1. The buffer is capped at **64 characters**.
2. It is **never written to the log** — at any verbosity, including debug.
3. It is **never written to disk**.
4. It is emptied when the foreground window changes, when the mouse is clicked, on arrow keys, and on any Ctrl or Alt combination.
5. It does not exist at all while the expander is off — and it is off by default.

What you can do yourself:

- Put your password manager in **Never in windows**. In such a window the buffer is never filled at all.
- Switch the expander off when you are not using it.
- Check all of the above: [the source is open](https://github.com/blackixxce12/Macro-Recorder), and the module is called `expander`.

---

## 12. Cheat sheet

**Where:** main window → **⌨ Text expander**

**Add:** **+ Add**, abbreviation on the left, replacement below

---

**Trigger modes**

```
after a delimiter   addr + space         safe, use this
behind a marker     ;;addr               for short or ordinary words
immediately         ;sig                 only for unusual abbreviations
default             takes the global setting
```

**Placeholders**

```
{date}  {date:dd.MM.yyyy}  {time}  {datetime}
{clipboard}   {cursor}   {key:Tab}   {random:a|b|c}
\{date}  — literal braces
```

**Insert modes**

```
type    works everywhere, slow for long text
paste   instant, but not everywhere, and borrows the clipboard
```

---

**The three things people forget most**

1. The expander is **off by default** — turn the master switch on.
2. While a macro records or replays it **stays quiet on purpose**.
3. Your password manager belongs in **Never in windows**.

---

Not working? Open an [issue](../../issues) with your `expansions.json`, what you typed, and what came out.
