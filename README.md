# WinSend

A small Windows utility that sends Zoom's dual-monitor video window to a chosen
display, fills it edge to edge, and puts it back where it was on demand.

When "Use dual monitors" is enabled and a participant is pinned, Zoom opens a
separate floating window containing only that video feed. During a live
production that window has to be dragged onto the right display every time
someone is pinned. WinSend does it in one press.

## Status

The UI, configuration, window-identity matching and the Send/Retrieve logic are
implemented and covered by tests. The Win32 platform layer is written and
compiles, but is **unverified against a real Zoom session**.

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

- **Every Send and Retrieve re-resolves the window from scratch.** Zoom can
  close and reopen the video window mid-meeting, so a cached handle may by then
  refer to something else entirely. Full-screening the wrong window during a
  live broadcast is the failure this guards against.
- **Process name and window class are the identity; the title is only a
  tie-breaker.** Zoom reuses titles across its main meeting window and the
  dual-monitor video window. If a Zoom update changes the class, resolution
  reports "not found" and asks the user to reconfirm rather than falling back to
  some other Zoom window.

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
