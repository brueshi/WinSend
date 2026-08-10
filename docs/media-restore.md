# Restoring full-screen media

The last unsolved piece of the full-screen problem, and the reason it stayed
unsolved for a while: it is not a variant of the Send problem, it is its
mirror, and the mirror has no API.

## The problem

Send takes the foreground, because that is the one lever that reaches a
full-screen exclusive player (see the README's design notes; it took six
attempts to learn). The player reacts by minimising or suspending itself.
Retrieve puts it back with focus — which is what clicking it in the taskbar
does — but whether it returns to *full screen* is the application's own
decision. Some players restore themselves. Some come back windowed and stay
that way, and the desktop ends the session visibly different from how Send
found it.

Windows has no API for asking another process to enter its full-screen mode.
That state is internal to each application, keyed to whatever shortcut it
chose: `F` in VLC and mpv, `F11` in the browsers, `Alt+Enter` in Media Player
and MPC. The only honest request from outside is that shortcut, synthesized as
input while the player holds the focus. Everything in this design exists to
make that one keystroke safe to send — or better, unnecessary.

## The automatic path

Send already identifies the player without being told: a window absent from
every enumeration while it owned the display, and stowed (minimised or
cloaked) the moment Send took the foreground, is the player by that very
signature. At that moment — the one time it is reliably enumerable with its
process name attached — its full-screen shortcut is resolved from the keymap
and recorded alongside the displacement.

Retrieve restores displaced windows first and moves the sent window after, as
before. Then the watch starts, one look per frame, and every branch is a
measurement:

- **Gone from the window list** — exclusive full screen again (or closed).
  Done, no key. This is the player that restores itself, and the measurement
  is what stops it being toggled straight back out.
- **Still minimised or cloaked** — still resuming. A suspended packaged
  application takes time to wake, and a window in that state cannot take
  input anyway. Look again next frame.
- **Filling its monitor** (at least 98% coverage, slack for mixed-DPI edges) —
  full screen without needing help. Done, no key.
- **Visibly windowed, twice in a row** — the shortcut, pressed once. The
  second-look rule is a debounce: a player enumerable for a single frame
  mid-restore must not be caught windowed and toggled out of the full screen
  it was entering.

The watch runs for two seconds from the moment Retrieve reports, then says
where every player got to — "wmplayer.exe did not come back in time" — rather
than quietly giving up. A new Send cancels a running watch, since pressing a
player back to full screen mid-Send would fight the Send.

## The guards on the keystroke

`send_key` lives on the platform seam and refuses rather than risks:

- **Foreground or nothing.** `SendInput` delivers to the focus, not to a
  window of the caller's choosing, so the press is refused unless the player
  is the foreground window. The failure mode is a key not sent, never a key
  sent astray into someone's document.
- **Held modifiers corrupt chords.** The user is likely still holding the
  Retrieve hotkey when the watch fires. A synthesized `F` with Ctrl and Alt
  physically down arrives as Ctrl+Alt+F, so the press is refused while any
  modifier outside the chord is held, and retried a frame later when the keys
  are up.
- **At most one press.** Once the key is sent the watch only observes; if the
  player ignores it, the timeout says so.
- **Elevation is surfaced.** UIPI silently swallows input aimed at a
  higher-integrity process. A short injection count is reported as "if the
  player runs as administrator, WinSend must too" rather than as success.

Win is not a valid chord modifier at all — synthesizing Win+anything triggers
operating-system shortcuts.

## The keymap

Three layers, first match wins, resolved at the moment the player is noticed:

1. `media_keys` in the config: process name to chord text, matched
   case-insensitively — `{"vlc.exe": "F"}`. An entry that fails to parse
   yields no key rather than falling back: an override is an instruction, and
   substituting a different key for a typo would send the wrong keystroke on
   purpose.
2. The built-in table: VLC, mpv, Media Player, MPC-HC, PotPlayer, Chrome,
   Edge, Firefox.
3. `media_default_key`, unset by default. A guessed keystroke into an unknown
   application is typing into it, so an unknown player is reported — "no
   full-screen key is known; add one to media_keys" — not experimented on.

There is no settings UI for the keymap on purpose. The table covers the
common players, and for the rest a hand-maintained map in a readable JSON
file beats a grid of text fields.

## The manual path: Restore Media

The automatic watch works by noticing what Send displaced, which means it
cannot see a player that was already sitting in the taskbar before Send —
there is no before-and-after difference to notice — or one that something
other than WinSend pushed aside.

For those, Settings binds a media window the way it binds the Zoom window,
and a third hotkey (or the tray menu's Restore Media) re-finds it by identity,
unminimises it, gives it the foreground, and hands it to the same watch,
guards and all. The binding gets its own picker list: the ordinary picker
filters on having a title, and that filter is the exact reason the player was
invisible for six attempts. The media list includes untitled windows, headed
"(no title)" and identified by process and class — which is what the binding
matches on anyway.

Restore Media deliberately ignores the **Return full-screen video after
Retrieve** setting. That setting governs what Retrieve does on its own;
pressing the hotkey is the explicit request the setting exists to distinguish
from.

## What a real desktop still has to answer

All of the above is tested against the mock, whose players minimise, suspend,
resume windowed, restore themselves, or never come back. What the mock cannot
model is what a real player actually does with the keystroke, and the open
questions are recorded here so the first live run knows what to look at:

- Whether a resumed-from-suspend packaged application processes the shortcut
  immediately or needs longer than the two-second window.
- Whether any player treats the toggle asymmetrically — a key that enters
  full screen but a different one to leave it would make a mistimed press
  worse than none.
- Whether the fade interacts with a player re-entering exclusive full screen
  in the same frames; the watch waits for the fade to finish before pressing
  anything, which should make the answer "no".
