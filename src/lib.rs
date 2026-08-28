//! WinSend's shared library: everything except the front end that draws it.
//!
//! Split out of `main.rs` so that more than one front end can be built against
//! the same `Core`, `Platform`, `Shell` and `Updater`. The binaries under
//! `src/bin` are thin: they choose a platform, build a `Core`, and hand it to
//! whichever interface they are.
//!
//! The UI modules stay in this crate rather than beside their binaries so that
//! the `crate::` paths throughout them keep resolving, and are feature-gated so
//! a binary can be built without linking the other front end's toolkit.

pub mod config;
pub mod core;
pub mod hotkey;
pub mod identity;
pub mod mock;
pub mod platform;
pub mod shell;
pub mod update;

#[cfg(feature = "eframe-ui")]
pub mod app;

#[cfg(windows)]
pub mod github;
#[cfg(windows)]
pub mod win32;
#[cfg(windows)]
pub mod win32_shell;

/// The window and taskbar icon, as raw RGBA rather than an encoded image.
///
/// Written out already decoded by `tools/make_icon.py`, so setting it costs an
/// include and two constants instead of an image decoder pulled in to unpack
/// one 64-pixel square at startup. This is separate from the icon compiled into
/// the executable's resources, which is what Explorer shows on the file.
///
/// It lives here rather than in a binary because both front ends need it, and
/// `include_bytes!` resolves relative to the file it is written in.
pub const WINDOW_ICON: &[u8] = include_bytes!("../assets/winsend-64.rgba");
pub const WINDOW_ICON_SIZE: u32 = 64;

/// Real Win32 on Windows, the fake desktop everywhere else. Setting
/// `WINSEND_MOCK=1` forces the mock on Windows too, which is useful for
/// working on the UI without a meeting running.
pub fn select_platform() -> Box<dyn platform::Platform> {
    let forced_mock = std::env::var("WINSEND_MOCK").is_ok_and(|v| v == "1");

    #[cfg(windows)]
    if !forced_mock {
        return Box::new(win32::Win32Platform::new());
    }

    let _ = forced_mock;
    Box::new(mock::MockPlatform::new())
}
