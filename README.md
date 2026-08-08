# WinSend

A small Windows utility that sends Zoom's dual-monitor video window to a chosen
display, fills it edge to edge, and puts it back where it was on demand.

When "Use dual monitors" is enabled and a participant is pinned, Zoom opens a
separate floating window containing only that video feed. During a live
production that window has to be dragged onto the right display every time
someone is pinned. WinSend does it in one press.

## Status

Working end to end against a real Zoom session on Windows, including thumbnail
capture. The UI, configuration, window-identity matching and the Send/Retrieve
logic are covered by tests that run on any platform.

Known limitation: Zoom's main and video windows are identical in process, class
and title, so after restarting WinSend the window has to be picked again. Within
a session the remembered handle separates them.

## Design

Everything the operating system provides sits behind one trait, `Platform`:

```
src/platform.rs   trait and shared types
src/mock.rs       a fake desktop, for developing away from Windows
src/win32.rs      the real implementation
src/identity.rs   persisting and re-finding the confirmed window
src/config.rs     settings, stored as JSON under %APPDATA%
src/core.rs       Send and Retrieve, free of any UI
src/app.rs        egui front end
```

That seam exists for a practical reason rather than a stylistic one: it lets the
entire interface and all of the logic build, run and be tested on a machine that
is not Windows, which reduces the Windows feedback loop to just the Win32
adapter.

Two decisions worth knowing:

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

Restore points are captured only on the first Send, so pressing it twice cannot
overwrite the original position, and are cleared once Retrieve consumes them.
Minimised windows are un-minimised before their bounds are read, because Windows
reports off-screen coordinates for iconic windows.

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

`WINSEND_MOCK=1` forces the mock platform on Windows, which is useful for
working on the interface without a meeting running. In debug builds,
`WINSEND_SCREEN=settings|select` opens straight onto a screen.

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
