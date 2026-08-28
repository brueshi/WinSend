//! The eframe front end.
//!
//! No console window on Windows release builds; this is a GUI utility.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use winsend::app::{self, WinSendApp};
use winsend::config::Config;
use winsend::core::Core;
use winsend::{select_platform, update, WINDOW_ICON, WINDOW_ICON_SIZE};

fn window_icon() -> eframe::egui::IconData {
    eframe::egui::IconData {
        rgba: WINDOW_ICON.to_vec(),
        width: WINDOW_ICON_SIZE,
        height: WINDOW_ICON_SIZE,
    }
}

fn main() -> eframe::Result {
    // Before anything else touches the directory the application lives in.
    // The previous version is still sitting beside this one, because a running
    // executable cannot delete itself.
    update::clean_up_previous_install();

    let core = Core::new(select_platform(), Config::load());

    // Set when an update has been installed. Read after `run_native` returns,
    // which is the only point at which this process has finished putting other
    // applications' windows back — starting the replacement any earlier would
    // race the two of them over the same desktop.
    let relaunch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    #[cfg_attr(not(debug_assertions), allow(unused_mut))]
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("WinSend")
        .with_inner_size([app::DEFAULT_WIDTH, app::COMPACT_HEIGHT])
        .with_min_inner_size([300.0, 200.0])
        // Explicit rather than relying on the default: disclosing the
        // configuration grows the window, and a non-resizable window would
        // leave the user stuck with whatever height that chose.
        .with_resizable(true)
        .with_always_on_top()
        .with_icon(window_icon());

    // Fixed position only in debug, so UI screenshots land in a known place.
    #[cfg(debug_assertions)]
    {
        viewport = viewport.with_position([60.0, 60.0]);
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };

    let outcome = eframe::run_native(
        "WinSend",
        options,
        Box::new({
            let relaunch = std::sync::Arc::clone(&relaunch);
            move |cc| Ok(Box::new(WinSendApp::new(cc, core, relaunch)))
        }),
    );

    if relaunch.load(std::sync::atomic::Ordering::SeqCst) {
        if let Ok(executable) = std::env::current_exe() {
            // The new version, at the path the old one had. Failing to start it
            // is not worth reporting to a window that has already gone; the
            // update is installed either way and the next launch gets it.
            let _ = std::process::Command::new(executable).spawn();
        }
    }

    outcome
}
