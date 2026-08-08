# Feature brief: global hotkey and tray icon

**Delivered.** Kept as a record of what was asked for and why. It describes the
codebase as it was before the work, so parts of it are now out of date — the
probe it refers to has been removed, and its job is done better by the
diagnostics report in Settings. See `README.md` for how things actually stand.

## Context

WinSend is a small Windows utility that sends Zoom's dual-monitor video window
to a chosen display and restores it. It works end to end today. Read `README.md`
first for the architecture; the short version is that all OS access sits behind
the `Platform` trait in `src/platform.rs`, with `MockPlatform` (`src/mock.rs`)
and `Win32Platform` (`src/win32.rs`) implementations. The UI, config and
Send/Retrieve logic build, run and test natively on macOS against the mock.

**Development happens on macOS.** Do not assume a Windows machine is available.
Build and test with `cargo test` and `cargo run`; cross-compile with:

```
PATH="$(brew --prefix lld)/bin:$PATH" \
  cargo xwin build --release --target x86_64-pc-windows-msvc --bin winsend
```

UI work is iterated by launching on macOS and screenshotting with
`screencapture -R`. `WINSEND_SCREEN=settings|select` (debug builds) opens
straight onto a screen. `WINSEND_MOCK=1` forces the mock platform on Windows.

**Whatever you build must not break the macOS build.** That is the property that
keeps the Windows feedback loop short, and it is worth protecting.

## Why these two features

The utility is used live during a broadcast. Both features exist to reduce what
the operator has to do on camera:

- **Global hotkey** — pressing Send currently means finding and focusing the
  WinSend window mid-broadcast. A hotkey makes it a single keystroke from
  inside Zoom.
- **Tray icon** — WinSend is always-on-top and occupies a taskbar slot for a
  window that is mostly idle. It should be able to live in the tray.

## Feature 1: global hotkey

Bind a user-configurable key combination to Send, and ideally a second to
Retrieve. A single toggle binding is also defensible; decide and say why.

### The design problem worth thinking about

`RegisterHotKey` delivers `WM_HOTKEY` to a window's message queue. eframe/winit
owns the main message loop and does not expose raw Windows messages, so hooking
into it is not straightforward.

The approach that fits this codebase is a **dedicated thread owning a
message-only window** (`HWND_MESSAGE`), which calls `RegisterHotKey`, pumps its
own message loop, and forwards hotkey events to the UI over an
`std::sync::mpsc` channel. The UI thread drains the channel in
`eframe::App::update` and must call `ctx.request_repaint()` so a hotkey press
wakes an idle window — without that the action will not fire until the user
moves the mouse over it.

Avoid `SetWindowsHookEx` with a low-level keyboard hook. It is far more
invasive, is treated as suspicious by security tooling, and is not needed for
fixed combinations.

### Requirements

- Configurable, persisted in `Config` (`src/config.rs`) alongside existing
  settings. A serialisable representation of modifiers plus a virtual key.
- `RegisterHotKey` fails if the combination is already taken by another app.
  That failure must surface in the UI as something actionable, not be swallowed.
  The user needs to know their chosen key is unavailable and pick another.
- Capturing a key combination in the settings UI needs care: while capturing,
  ordinary key handling must be suppressed so pressing Escape sets the binding
  rather than closing anything.
- Must degrade cleanly on macOS. The mock should simulate or no-op; the UI must
  still run there.
- Unregister on exit.

## Feature 2: tray icon

Put WinSend in the notification area with a menu, and let the main window be
hidden rather than closed.

### The design problem worth thinking about

`Shell_NotifyIcon` also needs a window to receive its callback messages. **Use
the same hidden message-only window and thread as the hotkey** rather than
standing up a second one. If you build the hotkey first, design that thread to
host both from the start.

Tray menus are built with `CreatePopupMenu` and shown with `TrackPopupMenu`.
There is a well-known quirk: the menu will not dismiss correctly unless the
owning window is brought to the foreground with `SetForegroundWindow` first.

### Requirements

- Menu with at least Send, Retrieve, Settings, and Quit. Retrieve should be
  greyed when there is nothing to restore, mirroring the button.
- Left click shows or hides the main window; closing the window hides to tray
  rather than quitting. Quit must be reachable only from the menu, and must
  actually exit.
- The icon needs a real `.ico` embedded in the binary. There is no icon in the
  repo yet, and none of the existing build steps embed resources.
- Hover tooltip should say something useful, such as the configured target
  monitor.
- Must not break the macOS build.

## Constraints that apply to both

- Follow `CLAUDE.md`. Conventional commits, one concern per commit, no
  co-authoring, explain non-obvious decisions.
- New OS behaviour belongs behind `Platform` or a sibling abstraction with a
  mock implementation, so the UI stays runnable on macOS.
- Anything that can be tested against the mock should have a test. The existing
  suite is in-module under `#[cfg(test)]`; match that style.
- Keep the always-on-top main window small. It is used live.

## Known state at handoff

- Thumbnail capture via `PrintWindow` with `PW_RENDERFULLCONTENT` works against
  Zoom's real video window. Confirmed on Windows.
- Zoom's main meeting window and its video window share process, class and
  title. They are separated by the handle the user clicks, re-validated on each
  use. After an app restart they are genuinely indistinguishable and the user is
  asked to reselect. If you find a durable discriminator in the probe output
  (`src/bin/probe.rs` dumps style bits), that limitation could be removed.
- A hardening pass covering Send/Retrieve state, minimised windows, clearer
  failure messages and window-style cleanup landed before this brief.
