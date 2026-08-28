//! The GPUI front end.
//!
//! Deliberately thin, and deliberately not the shipping binary yet: it exists
//! to be run beside `winsend` against the same mock desktop, so the difference
//! between the two surfaces can be looked at rather than argued about.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use gpui::{
    App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};

use winsend::config::Config;
use winsend::core::Core;
use winsend::gpui_app::{self, Icons, WinSendGpui};
use winsend::{select_platform, update};

fn main() {
    // Before anything else touches the directory the application lives in.
    // The previous version is still sitting beside this one, because a running
    // executable cannot delete itself.
    update::clean_up_previous_install();

    let core = Core::new(select_platform(), Config::load());

    // Set when an update has been installed. Read after `run` returns, which
    // is the only point at which this process has finished putting other
    // applications' windows back — starting the replacement any earlier would
    // race the two of them over the same desktop.
    let relaunch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    gpui_platform::application()
        .with_assets(Icons)
        .run({
            let relaunch = std::sync::Arc::clone(&relaunch);
            move |cx: &mut App| {
                let bounds =
                    Bounds::centered(None, size(px(gpui_app::WIDTH), px(gpui_app::HEIGHT)), cx);
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        // The header is drawn, so the system one is hidden and the
                        // window reads as one object rather than a panel inside a
                        // frame. The OS still rounds and shadows it, which is what
                        // keeps this out of the client-side-decoration business.
                        titlebar: Some(TitlebarOptions {
                            title: Some("WinSend".into()),
                            appears_transparent: true,
                            // Clear of the drawn header's own content, and roughly
                            // where the eye expects them on macOS.
                            // Aligned with the state chip and the window controls
                            // the header draws for itself, rather than with the
                            // titlebar that is no longer there.
                            traffic_light_position: Some(point(px(16.), px(13.))),
                        }),
                        // Resizable, matching the eframe surface: the window is
                        // sized to its content and grows when a panel opens, but
                        // the user still has the final say. It also keeps the
                        // macOS zoom button from rendering as a dead circle.
                        is_resizable: true,
                        ..Default::default()
                    },
                    |window, cx| cx.new(|cx| WinSendGpui::new(core, relaunch, window, cx)),
                )
                .unwrap();
                cx.activate(true);
            }
        });

    if relaunch.load(std::sync::atomic::Ordering::SeqCst) {
        if let Ok(executable) = std::env::current_exe() {
            // The new version, at the path the old one had. Failing to start
            // it is not worth reporting to a window that has already gone; the
            // update is installed either way and the next launch gets it.
            let _ = std::process::Command::new(executable).spawn();
        }
    }
}
