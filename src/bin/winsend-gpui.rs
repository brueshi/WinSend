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
use winsend::select_platform;

fn main() {
    let core = Core::new(select_platform(), Config::load());

    gpui_platform::application()
        .with_assets(Icons)
        .run(move |cx: &mut App| {
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
                        // Vertically centred in the drawn header, which is
                        // 52px tall and about 14px of button.
                        traffic_light_position: Some(point(px(16.), px(19.))),
                    }),
                    // Resizable, matching the eframe surface: the window is
                    // sized to its content and grows when a panel opens, but
                    // the user still has the final say. It also keeps the
                    // macOS zoom button from rendering as a dead circle.
                    is_resizable: true,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| WinSendGpui::new(core)),
            )
            .unwrap();
            cx.activate(true);
        });
}
