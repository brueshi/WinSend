# Feature brief: interface rework and self-updating

Hand this file to a fresh session as the starting prompt. It is written to be
self-contained.

## Context

WinSend sends Zoom's dual-monitor video window to a chosen display and brings it
back, driven by a button, a global hotkey or a tray menu. Read `README.md` first
for the architecture. The short version is that all OS access sits behind two
traits — `Platform` in `src/platform.rs` for questions with immediate answers,
`Shell` in `src/shell.rs` for events the OS delivers on its own schedule — each
with a mock implementation, so the entire interface and all of the logic build,
run and are tested on macOS.

**Development happens on macOS.** Build and test with `cargo test` and
`cargo run --bin winsend`; cross-compile with:

```
PATH="$(brew --prefix lld)/bin:$PATH" \
  cargo xwin build --release --target x86_64-pc-windows-msvc --bin winsend
```

UI work is iterated by launching on macOS and screenshotting with
`screencapture -R`. `WINSEND_SCREEN=settings|select` (debug builds) opens
straight onto a screen. `WINSEND_MOCK=1` forces the mock platform and shell on
Windows.

**Whatever you build must not break the macOS build.** That property is what
keeps the Windows feedback loop short, and the previous round of work proved how
much it is worth: six attempts at one Windows-only behaviour were needed even
with it, and would have been far more expensive without.

## Feature 1: interface rework

Three screens that swap and resize the window become one surface with sections,
keeping the native title bar, restyled with pill-shaped controls.

### The design problem worth thinking about

The app has two incompatible jobs. During a broadcast it must be **small and
unmissable** — two large buttons, always on top, occupying as little of the
screen as possible. While being configured it needs **room**: a monitor list,
several toggles, hotkey rows, a diagnostics button.

Today that is resolved by swapping screens and resizing the window
(`COMPACT_SIZE`, `SETTINGS_SIZE`, `PICKER_SIZE` in `src/app.rs`, applied by
`Screen::size` and `go_to`). It works, but the window jumping size on every
navigation is exactly the jarring behaviour the rework is meant to remove.

A single surface that contains everything would either be too tall for live use
or force scrolling past configuration to reach Send. Neither is acceptable.

The shape that resolves it: **compact by default, with configuration in a
disclosure that grows the window when opened and shrinks it when closed.** One
surface, one navigation model, and the live case stays exactly as small as it is
now. The window still changes size, but only when the user asks it to, which is
the difference between a response and a surprise.

**The picker is the exception and should stay one.** It needs around 640px of
height and shows thumbnails to compare, so folding it into the surface would
either dominate it or be unusably cramped. Make it a modal over the surface —
`egui::Window`, or a second viewport — rather than a screen the app navigates
to. That also removes the last caller of the screen-swapping machinery.

### Requirements

- One surface. `Screen` and `go_to` should be gone or reduced to the picker
  modal. Nothing else resizes the window behind the user's back.
- Pill-shaped controls: `Style::visuals.widgets.*.corner_radius` set to half the
  control height, for every widget state (`inactive`, `hovered`, `active`,
  `open`) — missing one produces a button that changes shape on hover.
- The main controls stay large. They are pressed under pressure, on camera; do
  not shrink them to make room for polish.
- Dark stays forced, and the reason is in `apply_style`: this sits on screen
  during a live production, where a white panel spills light and clashes with
  the rest of the kit. Modernising must not quietly become theme-following.
- Native window integration on Windows 11, via `DwmSetWindowAttribute` on the
  eframe window's `HWND`:
  - `DWMWA_USE_IMMERSIVE_DARK_MODE` (20), so the title bar matches the app
    instead of being a white bar above a dark panel.
  - `DWMWA_WINDOW_CORNER_PREFERENCE` (33), for rounded corners.
  - **Mica (`DWMWA_SYSTEMBACKDROP_TYPE`, 38) is deliberately not assumed.** The
    backdrop only shows through a transparent window background, so egui's
    opaque `panel_fill` would have to go translucent — which during a broadcast
    means whatever is behind the window shows through it. Try it, look at it
    over a live desktop, and be willing to reject it.
- The `HWND` comes from eframe via `raw-window-handle`. The Dwm calls are
  Windows-only and belong behind `Platform` (something like
  `apply_window_chrome(handle)`), not scattered through `app.rs` with `cfg`
  attributes, so the mock no-ops and macOS keeps building.
- Status messages currently replace each other in a bare label. With one surface
  there is room to do better; a message that vanished before it was read is a
  message that was not delivered.

### Not requirements

Do not restyle the diagnostics report. It is plain text on purpose, so it can be
pasted into an issue or a message.

## Feature 2: self-updating

Check GitHub Releases on launch, say so unobtrusively, and install when asked.

### The design problem worth thinking about

Everything about this feature is shaped by one fact: **the application is
running during a live broadcast.** An update that restarts the app at the wrong
moment is worse than never updating at all.

That rules out anything automatic. The check runs once per launch, on its own
thread, and its only effect is to make an indicator appear. Downloading and
restarting happen when the user clicks, never otherwise.

There is a subtler hazard. `Core` holds the restore point in memory and it is
session-scoped by design — quitting while a window is sent leaves Zoom's window
on the wrong monitor with nothing to put it back. **Restarting to update while
`can_retrieve()` is true must warn**, and ideally offer to retrieve first. This
is the kind of thing that is obvious in hindsight at 3am during a show.

The second design problem is replacing a running executable. Windows will not
let you overwrite one, but it will let you **rename** it. The sequence:

1. Download beside the current executable, to a temporary name.
2. Rename the running executable out of the way (`winsend-old.exe`).
3. Rename the download into its place.
4. Relaunch and exit.
5. On the next start, delete `winsend-old.exe` if it is there.

Every step must be reversible. If step 3 fails after step 2 succeeded, rename
back. The user must never be left without a working executable because an update
was interrupted.

### Requirements

- **A new seam with a mock**, mirroring `Platform` and `Shell`. Network I/O on
  its own thread, results delivered over a channel, the UI woken the same way
  `Shell` wakes it. That is what makes the whole flow — no update, update
  available, download failed, digest mismatch, install succeeded — exercisable
  on macOS without touching the network.
- **HTTP client:** prefer `ureq` with rustls. It is blocking, which suits a
  dedicated thread, and pulls no async runtime. Deliberately not the Windows
  HTTP APIs: those would be untestable on macOS, and this is precisely the kind
  of logic that benefits from being developed against a mock. Justify the choice
  in the PR description per `CLAUDE.md`.
- **Version comparison must be numeric, not textual.** Tags are `v0.1.14`; a
  string compare puts `v0.1.9` above `v0.1.10`. Three numbers parsed and
  compared is about twenty lines and worth testing rather than pulling in a
  semver crate for.
- **Verify the download.** The GitHub releases API returns a `digest` field
  (`sha256:...`) per asset. Hash what was downloaded and compare. This is trust
  on first use against GitHub over TLS rather than real code signing, and should
  be described honestly as such; signing needs a certificate and is a separate
  decision with a cost attached.
- A failed check is not an error worth showing. No network, GitHub down, rate
  limited: the indicator simply does not appear. Never block startup on it,
  never interrupt with a dialog.
- An opt-out in `Config`, defaulting to on.

### The version-drift trap

`Cargo.toml`'s version is what the running app compares against the latest tag.
It has drifted from the tags before — it said `0.1.0` while `v0.1.2` was
released — and with an updater that stops being untidy and becomes a bug: an app
whose version lags the tags believes it is permanently out of date.

**Bump `Cargo.toml` before creating the tag, in the same commit or immediately
before it.** Consider a check in CI, or a release script that does both.

## Feature 3: fading the window out on Retrieve

Instead of the video window vanishing from the target display in one frame, fade
it out over a couple of hundred milliseconds.

### Why this belongs on Retrieve and not on Send

The two operations are not equally urgent. Send is the one under pressure — the
feed has to be up now, and anything that delays it by even a fifth of a second
is a cost paid at the worst moment. Retrieve is the relaxed half: something is
being taken off air, and there is no one waiting on it.

So fading Retrieve while leaving Send instant is not an inconsistency to be
apologised for. It is the transition going where there is slack for it. If a
fade on Send is wanted later it should be argued separately, and it has to
contend with the fact that Send already produces a hard cut it does not control
— the full-screen player gives up the display the instant focus moves.

### The design problem worth thinking about

**A half-finished fade is far worse than no fade.** A window stuck at forty
percent opacity, on camera, is a visible fault where a hard cut would have been
merely unremarkable. Every failure path — the window closing mid-animation, the
platform call failing, the app quitting, a second Retrieve arriving — has to
land on fully opaque and fully un-layered, never anywhere in between. Build the
guarantee first and the animation second.

**What is behind matters more than what is fading.** Fading Zoom out reveals
whatever is underneath, and today Retrieve puts the window back before restoring
the media player that was displaced. Done in that order the fade reveals a bare
desktop and then the player snaps in on top, which is two transitions where
there was one. The player has to be restored *first*, behind the still-opaque
Zoom window, so the fade reveals the thing that is meant to be there.

That reordering is most of the value and is worth doing whether or not the fade
is implemented.

**Making another application's window translucent is not free.** It needs
`WS_EX_LAYERED` and `SetLayeredWindowAttributes`. Layered windows compose
differently, and Zoom's video window is GPU-composited — the same property that
made thumbnail capture need `PW_RENDERFULLCONTENT`. It may flicker, drop frames,
or refuse. Try it against a real session early, because if it misbehaves the
feature does not survive its own purpose.

Note also that the previous round of work has just finished fixing two faults in
restoring window styles that were taken away. `WS_EX_LAYERED` is another such
style. Add it the same way: record the bits set, put back exactly those, and
never snapshot a whole style word.

### Requirements

- Opacity behind `Platform`, something like `set_window_opacity(handle, alpha)`
  and a way to clear it. The mock records the values, which is what makes the
  timing and the end states testable on macOS.
- The animation itself is portable and belongs above the seam: a small state
  machine driven by the frame loop, using `ctx.request_repaint()` the way the
  rest of the app already does. No sleeping on the UI thread, and no thread that
  outlives the operation.
- `Core::retrieve` is synchronous today and callers depend on that. Either the
  fade is driven entirely from `app.rs` around a `Core` that still completes
  synchronously, or `Core` gains an explicit notion of an operation in progress.
  The first is smaller; take it unless something forces otherwise.
- Restore displaced windows before the fade begins, not after the move.
- A toggle in Settings, and a hard cut when it is off. Do not make it the only
  behaviour.
- Around 200ms. Long enough to read as deliberate, short enough that nobody
  waits for it. Resist making it configurable until someone asks.
- If a hotkey arrives mid-fade, finish immediately rather than queueing. The
  operator pressing a key twice means they want it done, not animated twice.

## Going public

The updater needs the repo public, since the releases API only serves
unauthenticated reads for public repositories.

- No secrets are present. Configuration is user-local under `%APPDATA%`, nothing
  is baked into the binary, and a scan of the history found nothing
  credential-shaped. Re-check before flipping the switch rather than trusting
  this sentence.
- **There is no `LICENSE` file.** A public repository without one is "all rights
  reserved" by default, which is probably not the intent. Decide before making
  it public, since adding one afterwards does not apply retroactively to anyone
  who already took a copy.
- `docs/hotkey-and-tray.md` and this file become public too. Both are written to
  be read by someone else, so that is fine, but read them once with that in
  mind.

## Constraints that apply to both

- Follow `CLAUDE.md`. Conventional commits, one concern per commit, no
  co-authoring, explain non-obvious decisions.
- New OS behaviour belongs behind `Platform`, `Shell`, or a sibling seam with a
  mock, so the UI stays runnable and testable on macOS.
- Anything that can be tested against a mock should have a test. The existing
  suite is in-module under `#[cfg(test)]`; match that style. There are 86 tests
  and no warnings on either target — keep it that way.
- The main window stays small and always on top. It is used live.

## Known state at handoff

- Send, Retrieve, global hotkeys, the tray icon and menu, and the application
  icons are all confirmed working on Windows.
- Getting the sent window in front of full-screen media works, and the mechanism
  is not obvious: such a window is managed outside the normal stacking order and
  never appears in `EnumWindows` at all, so it is reached by taking the
  foreground rather than by any z-order manipulation. `README.md` explains this;
  do not undo it while rearranging the interface.
- Hide-to-tray is still unverified on Windows, as is whether a media player that
  gives up the display is reliably restored on Retrieve.
- Settings has a **Copy diagnostics** button that reports the same window list
  and the same verdicts `Send` acts on. It is the reason the full-screen media
  problem was eventually solved, and it should survive the interface rework.
