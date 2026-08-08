// No console window on Windows release builds; this is a GUI utility.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod config;
mod core;
mod hotkey;
mod identity;
mod mock;
mod platform;
mod shell;
#[cfg(windows)]
mod win32;
#[cfg(windows)]
mod win32_shell;

use app::WinSendApp;
use config::Config;
use core::Core;
use platform::Platform;

/// Real Win32 on Windows, the fake desktop everywhere else. Setting
/// `WINSEND_MOCK=1` forces the mock on Windows too, which is useful for
/// working on the UI without a meeting running.
fn select_platform() -> Box<dyn Platform> {
    let forced_mock = std::env::var("WINSEND_MOCK").is_ok_and(|v| v == "1");

    #[cfg(windows)]
    if !forced_mock {
        return Box::new(win32::Win32Platform::new());
    }

    let _ = forced_mock;
    Box::new(mock::MockPlatform::new())
}

/// The window and taskbar icon, as raw RGBA rather than an encoded image.
///
/// Written out already decoded by `tools/make_icon.py`, so setting it costs an
/// include and two constants instead of an image decoder pulled in to unpack
/// one 64-pixel square at startup. This is separate from the icon compiled into
/// the executable's resources, which is what Explorer shows on the file.
const WINDOW_ICON: &[u8] = include_bytes!("../assets/winsend-64.rgba");
const WINDOW_ICON_SIZE: u32 = 64;

fn window_icon() -> eframe::egui::IconData {
    eframe::egui::IconData {
        rgba: WINDOW_ICON.to_vec(),
        width: WINDOW_ICON_SIZE,
        height: WINDOW_ICON_SIZE,
    }
}

fn main() -> eframe::Result {
    let core = Core::new(select_platform(), Config::load());

    #[cfg_attr(not(debug_assertions), allow(unused_mut))]
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("WinSend")
        .with_inner_size([340.0, 260.0])
        .with_min_inner_size([300.0, 200.0])
        // Explicit rather than relying on the default: the window is resized
        // programmatically when the picker opens, and a non-resizable window
        // would leave the user stuck with whatever size it chose.
        .with_resizable(true)
        .with_always_on_top()
        .with_icon(window_icon());

    // Fixed position only in debug, so UI screenshots land in a known place.
    #[cfg(debug_assertions)]
    {
        viewport = viewport.with_position([60.0, 60.0]);
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        "WinSend",
        options,
        Box::new(|cc| Ok(Box::new(WinSendApp::new(cc, core)))),
    )
}
