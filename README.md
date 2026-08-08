# WinSend

A small Windows utility that sends Zoom's dual-monitor video window to a chosen
display, fills it edge to edge, and puts it back where it was on demand. It can
be driven by a global hotkey and lives in the notification area.

When "Use dual monitors" is enabled and a participant is pinned, Zoom opens a
separate floating window containing only that video feed. During a live
production that window has to be dragged onto the right display every time
someone is pinned. WinSend does it in one press, without leaving Zoom.

## Status

Send and Retrieve work end to end against a real Zoom session on Windows,
including thumbnail capture. The UI, configuration, window-identity matching,
binding rules and the Send/Retrieve logic are covered by tests that run on any
platform.

The global hotkey and the tray icon compile for Windows but have not yet been
exercised there, in the same position `win32.rs` was in before its first run.

Known limitation: Zoom's main and video windows are identical in process, class
and title, so after restarting WinSend the window has to be picked again. Within
a session the remembered handle separates them.

## Design

Everything the operating system provides sits behind one of two traits:

```
src/platform.rs   Platform: what WinSend asks the OS, answered on the spot
src/shell.rs      Shell: what the OS tells WinSend, on its own schedule
src/mock.rs       a fake desktop and shell, for developing away from Windows
src/win32.rs      the real Platform
src/win32_shell.rs  the real Shell: message-only window, hotkeys, tray icon
src/hotkey.rs     key combinations and the rules for binding them
src/identity.rs   persisting and re-finding the confirmed window
src/config.rs     settings, stored as JSON under %APPDATA%
src/core.rs       Send and Retrieve, free of any UI
src/app.rs        egui front end
```

Those seams exist for a practical reason rather than a stylistic one: they let
the entire interface and all of the logic build, run and be tested on a machine
that is not Windows, which reduces the Windows feedback loop to just the two
Win32 adapters.

`Platform` and `Shell` are separate because the dependency runs in opposite
directions. A platform call is a question with an immediate answer. The shell
delivers events when the OS decides to, from a thread of its own, and that
difference shapes the whole of `shell.rs`.

Four decisions worth knowing:

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
straight onto a screen, and the main screen carries mock controls that inject
hotkey presses and tray menu choices through the same path a real one takes.

The tray icon is drawn by a script rather than committed as an opaque binary:

```
python3 tools/make_icon.py                    # rewrites assets/winsend.ico
python3 tools/make_icon.py --preview /tmp/icon.png
```

It is embedded with `include_bytes!` and built at runtime, not compiled in as a
`.rc` resource, because a resource compiler in the build is what would break the
cross-compile above. The cost is that Explorer shows no icon on the `.exe`.

## The probe

`src/bin/probe.rs` is a throwaway diagnostic that dumps the monitor layout and
every visible top-level window with its class, title, owning process, bounds and
style bits. It exists to identify Zoom's video window with certainty before any
detection logic is trusted, and should be deleted once that is settled.

It deliberately dumps every window rather than only processes matching "zoom",
because Zoom has historically hosted windows under process names that give
nothing away.

```
cargo xwin build --release --target x86_64-pc-windows-msvc \
  --features probe --bin probe
probe.exe > windows.txt
```

## Known risk

Thumbnails in the window picker use `PrintWindow` with `PW_RENDERFULLCONTENT`.
GPU-composited surfaces can refuse to be captured, and Zoom's video window may
well be one. A failed capture is treated as normal rather than as an error: the
picker falls back to a placeholder and the window is identified by title, size
and monitor instead. If previews do come back black, the alternative is
Windows.Graphics.Capture, which is considerably more involved.

A window hidden to the tray is not guaranteed to be told to redraw, and eframe
only runs a frame when it is. A hotkey that worked only while the window was on
screen would defeat the point, so while hidden a repaint is requested on a
100 ms timer as a floor under the waker. If that turns out to be unnecessary on
Windows it can go; if it turns out to be insufficient, the window is being
hidden by a mechanism that needs replacing rather than tuning.
