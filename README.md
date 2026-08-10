# WinSend

A small Windows utility that sends Zoom's dual-monitor video window to a chosen
display, fills it edge to edge, and puts it back where it was on demand. It can
be driven by a global hotkey and lives in the notification area.

When "Use dual monitors" is enabled and a participant is pinned, Zoom opens a
separate floating window containing only that video feed. During a live
production that window has to be dragged onto the right display every time
someone is pinned. WinSend does it in one press, without leaving Zoom.

## Status

Confirmed working against a real Zoom session on Windows: Send and Retrieve,
thumbnail capture, global hotkeys, the tray icon and its menu, hide-to-tray, the
application icons, getting the sent window in front of a full-screen media
player on the target display, the single surface and its disclosure, the
Windows 11 title bar and corner styling, and the fade on Retrieve.

The update check is confirmed against the live API. Its download and swap are
not, and could not have been until now: there has to be a release newer than the
one running before that path can be walked at all, and until this one there was
not.

The full-screen restore — pressing a player that gave up the display back to
full screen after Retrieve, and the Restore Media binding for one the automatic
path cannot see — is built and tested against the mock, and unverified against
a real player. `docs/media-restore.md` records the design and what a real
desktop still has to answer.

**Fade out when retrieving**, **Return full-screen video after Retrieve** and
**Check for updates on startup** in Settings turn off the three pieces that
touch anything outside this application.

Everything above the platform seam — the UI, configuration, window identity,
binding rules, and the whole of Send and Retrieve including which windows are in
the way — is covered by tests that run on any platform.

Known limitation: Zoom's main and video windows are identical in process, class
and title, so after restarting WinSend the window has to be picked again. Within
a session the remembered handle separates them.

## Design

Everything outside the process sits behind one of three traits:

```
src/platform.rs   Platform: what WinSend asks the OS, answered on the spot
src/shell.rs      Shell: what the OS tells WinSend, on its own schedule
src/update.rs     Updater: whether a newer release exists, and installing it
src/mock.rs       a fake desktop, shell and release feed, for developing away
                  from Windows
src/win32.rs      the real Platform
src/win32_shell.rs  the real Shell: message-only window, hotkeys, tray icon
src/github.rs     the real Updater: GitHub over HTTPS, on threads of its own
src/hotkey.rs     key combinations and the rules for binding them
src/identity.rs   persisting and re-finding the confirmed window
src/config.rs     settings, stored as JSON under %APPDATA%
src/core.rs       Send and Retrieve, free of any UI
src/app.rs        egui front end
```

`Updater` is a sibling of `Shell` rather than of `Platform`, for the same
reason: its answer arrives on somebody else's schedule, from a thread of its
own, and the UI has to be woken when it does.

Those seams exist for a practical reason rather than a stylistic one: they let
the entire interface and all of the logic build, run and be tested on a machine
that is not Windows, which reduces the Windows feedback loop to just the three
adapters behind them.

`Platform` and `Shell` are separate because the dependency runs in opposite
directions. A platform call is a question with an immediate answer. The shell
delivers events when the OS decides to, from a thread of its own, and that
difference shapes the whole of `shell.rs`.

The decisions worth knowing:

- **The picked window's handle is remembered, but re-validated on every use.**
  Zoom's main meeting window and its video window share process, class and
  title, so nothing in the persisted description separates them — only the
  handle does. A handle alone cannot be trusted either, since Zoom can close and
  reopen the video window and the OS can reissue a dead handle to something
  else, so before each use it is checked against the live window list for both
  existence and a matching process and class.
- **When the window genuinely cannot be identified, the app asks rather than
  guesses.** Full-screening the main meeting window mid-broadcast is the failure
  worth avoiding, and it is worse than a prompt. Ambiguity opens the picker
  instead of reporting an error the user has to decode.
- **Hotkeys run on a thread with its own message-only window.** `RegisterHotKey`
  delivers `WM_HOTKEY` to a message queue, and eframe owns the main one without
  exposing raw Windows messages. The thread forwards presses over a channel and
  wakes the UI, which an idle eframe window needs or the action would not fire
  until the mouse moved over it. The tray icon shares that window, because
  `Shell_NotifyIcon` needs one too.
- **Send and Retrieve get separate bindings rather than one toggle.** They are
  not symmetric: Retrieve is only valid once a restore point exists, so a
  toggle's meaning would depend on state the operator cannot see at the moment
  they press it. Pressing Send twice is already a no-op, where double-tapping a
  toggle would bounce the window mid-broadcast.

- **The interface is one surface, and only the user resizes it.** Send,
  Retrieve and the status strip share a window with the configuration, which
  folds into a disclosure that grows the window when opened and shrinks it when
  closed. Three screens that swapped and resized on every navigation meant the
  window jumped size when nobody had asked it to. The picker is the exception:
  it needs around 640px of height to compare thumbnails, which is more than the
  live surface should ever be, so it opens as a window of its own rather than
  as somewhere the app navigates to.
- **Updating is offered, never taken.** The application runs during a live
  broadcast, so an update that restarts it at the wrong moment is worse than
  never updating at all. The check runs once per launch on a thread of its own
  and its only effect is to make a button appear; downloading and restarting
  happen on a click. A check that fails — no network, GitHub down, rate
  limited — says nothing, because none of that is something the operator asked
  about or can act on. Asking for the update while a window is still sent
  warns first and offers to retrieve on the way, since the restore point lives
  in memory and restarting would strand Zoom on the target display.
- **Retrieve fades, Send cuts.** Send is the half under pressure, where a fifth
  of a second is a cost paid at the worst moment; Retrieve is the relaxed half,
  where something is coming off air and nobody is waiting. The fade also
  depends on the ordering above: displaced windows go back while the sent
  window is still opaque and still covering them, so the fade uncovers what
  belongs there rather than a bare desktop the player then snaps onto. Because
  a window stuck part-way transparent on camera is far worse than no fade at
  all, every path out of the animation — finishing, the window closing, another
  press, quitting — runs through one place that restores full opacity
  unconditionally, and a window that will not go translucent falls back to a
  hard cut before anything on screen has changed.
- **Getting in front of full-screen media is a focus problem, not a z-order
  one.** This took six attempts to learn and is the least obvious thing in the
  codebase. A full-screen media player is managed outside the normal stacking
  order: it does not appear in `EnumWindows` at all, so nothing done to other
  windows reaches it, and every rule about raising or demoting windows was
  selecting from a list it had never been in. It gives way to exactly one thing,
  which is something else taking the foreground — which is why clicking any
  other application makes it minimise. So Send takes the foreground, and the
  z-order work below it only ever mattered for ordinary windows.
- **Coming back is measured, the same as going out.** Retrieve gives a
  displaced player focus back, but whether it returns to full screen is the
  application's own decision, and Windows has no API for asking another
  process to make it. So Retrieve watches: gone from the window list means
  exclusive full screen again and nothing to do; still minimised or cloaked
  means still resuming; visibly windowed on two consecutive looks means the
  player's own full-screen shortcut, pressed once while it holds the
  foreground. The shortcut comes from a per-process table, overridable in the
  config, and no key is ever guessed for an unknown process. A player the
  automatic path cannot see — one already stowed before Send — can be bound
  like the Zoom window and brought back with the Restore Media hotkey. The
  design and its guards are in `docs/media-restore.md`.
- **A placement is measured, not assumed.** A window does not always end up
  where it was put: coordinates can be scaled on a display whose DPI differs
  from the one the process was told about, and an application can resize itself
  in response to being moved. The result is read back and corrected once, by the
  difference for position and by the ratio for size, so the correction carries no
  assumption about any resolution or scale factor.

Ordinary windows in the way are still handled, because not everything on a
display is a full-screen player. A window counts as in the way when it is in
front of the sent window and covers at least 15% of it — both measured, one from
the stacking order and one from the two rectangles. It is dropped out of the
always-on-top band and to the back rather than minimised, so anything playing
carries on; a window that will not stay put is minimised instead. Everything
moved is put back on Retrieve, into the band it came from.

Settings also offers minimising everything on the target display outright. It is
off by default, since minimising can pause a player, but it is the blunt
instrument for when working out what is in the way gets it wrong.

Restore points are captured only on the first Send, so pressing it twice cannot
overwrite the original position, and are cleared once Retrieve consumes them.
Minimised windows are un-minimised before their bounds are read, because Windows
reports off-screen coordinates for iconic windows.

Bindings are stored as the text they read as, so `%APPDATA%` config stays
editable by hand. An ordinary key needs at least one modifier, since a global
binding swallows its key everywhere; F13 to F24 are exempt, because no real
keyboard has them and they are what a Stream Deck emits. Nothing is bound by
default, so nothing is quietly taken from another application on first run.

## Building

Native, for development against the mock platform:

```
cargo test
cargo run
```

Cross-compiling to Windows from macOS requires `lld` and `cargo-xwin`:

```
brew install lld
cargo install cargo-xwin
rustup target add x86_64-pc-windows-msvc

PATH="$(brew --prefix lld)/bin:$PATH" \
  cargo xwin build --release --target x86_64-pc-windows-msvc --bin winsend
```

`WINSEND_MOCK=1` forces the mock platform and shell on Windows, which is useful
for working on the interface without a meeting running and without taking real
hotkeys off the machine. In debug builds, `WINSEND_SCREEN=settings|select` opens
straight onto the configuration or the picker, and the surface carries mock
controls that inject hotkey presses and tray menu choices through the same path
a real one takes.

The tray icon is drawn by a script rather than committed as an opaque binary:

```
python3 tools/make_icon.py                    # rewrites assets/winsend.ico
python3 tools/make_icon.py --preview /tmp/icon.png
```

It produces three things from one description: `winsend.ico` for the tray and
the executable's resources, and `winsend-64.rgba` for the window and taskbar
icon, which eframe wants as raw pixels.

The tray and window icons carry their bytes inline, so only the icon Explorer
shows on the file needs `assets/winsend.rc` and the resource step in `build.rs`.
That step is a no-op for non-Windows targets and only ever a warning when a
resource compiler is missing, so it cannot break either build above.

## Diagnostics

Settings has a **Copy diagnostics** button. It reports every window with its
depth in the stacking order, whether it is always-on-top, minimised, cloaked or
ours, how much of the target monitor and of the Zoom window it covers, and the
verdict Send would reach about it — the same list and the same filters that Send
acts on, not a separate tool answering a nearby question.

That distinction matters. Six attempts at getting in front of full-screen media
were each built on a guess about what was on screen, and a standalone probe that
applied its own filters could not have settled it. The report is what showed the
window in question was not being enumerated at all.

It goes to the clipboard and to `%APPDATA%\WinSend\diagnostics.txt`.

## Releasing

```
python3 tools/release.py --patch          # 0.1.15 -> 0.1.16
python3 tools/release.py 0.2.0 --notes "what changed"
```

The bump, the commit and the tag happen together or not at all. `Cargo.toml`
is what a running copy compares against the tags, and it has drifted from them
before — saying `0.1.0` while `v0.1.2` was released. That was untidy until the
updater existed; now a copy whose version lags the tags believes it is
permanently out of date and offers an update to what it is already running.

Releases are official by default. `--prerelease` publishes one the updater will
not offer, which is how a build reaches a particular machine without being
pushed to everyone.

Releases are read from the releases list rather than from `/releases/latest`.
Both exclude pre-releases, but they are not the same question: `latest` is the
most recently published release, where what matters here is the highest version
number, and a patch against an older line would make those disagree. The asset
must be named `winsend.exe` and must carry a `digest`, which the API supplies;
a release without one is passed over rather than trusted, since a download that
cannot be checked is not one to offer.

That check is a SHA-256 against what GitHub published, over TLS. It catches a
truncated or altered download; it is not code signing and does not pretend to
be. Signing needs a certificate and is a separate decision with a cost.

## Known risk

Thumbnails in the window picker use `PrintWindow` with `PW_RENDERFULLCONTENT`.
GPU-composited surfaces can refuse to be captured, and Zoom's video window may
well be one. A failed capture is treated as normal rather than as an error: the
picker falls back to a placeholder and the window is identified by title, size
and monitor instead. If previews do come back black, the alternative is
Windows.Graphics.Capture, which is considerably more involved.

Getting in front of a full-screen player works by taking the foreground, which
means the player gives up the display and, being suspended or minimised, stops
playing. There is no way to be in front of such a window and leave it running:
those are the same thing from its point of view.

The way back is just as indirect, and carries the newer risk. Full screen is
internal state each application manages for itself, so the only way to restore
it from outside is to synthesize the application's own shortcut — which is
typing into another program. The guards are listed in `docs/media-restore.md`;
the shape of the risk is that every one of them is a measurement taken moments
before the press, and the desktop can change between the measurement and the
keystroke. The press targets the foreground and is refused when the player is
not it, so the failure mode is a key not sent, not a key sent astray. Injected
input is also swallowed silently when the target runs elevated: if the player
runs as administrator, WinSend must too, and the status line says so.

A window hidden to the tray is not guaranteed to be told to redraw, and eframe
only runs a frame when it is. A hotkey that worked only while the window was on
screen would defeat the point, so while hidden a repaint is requested on a
100 ms timer as a floor under the waker. If that turns out to be unnecessary on
Windows it can go; if it turns out to be insufficient, the window is being
hidden by a mechanism that needs replacing rather than tuning.

## Licence

MIT. See `LICENSE`.

Zoom is named here only as the application WinSend works with. Nothing in this
repository is affiliated with or endorsed by Zoom.
