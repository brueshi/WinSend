//! The GPUI front end.
//!
//! Deliberately thin, and deliberately not the shipping binary yet: it exists
//! to be run beside `winsend` against the same mock desktop, so the difference
//! between the two surfaces can be looked at rather than argued about.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};

use winsend::config::Config;
use winsend::core::Core;
use winsend::gpui_app::{self, WinSendGpui};
use winsend::select_platform;

fn main() {
    let core = Core::new(select_platform(), Config::load());

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(
            None,
            size(px(gpui_app::WIDTH), px(gpui_app::HEIGHT)),
            cx,
        );
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| WinSendGpui::new(core)),
        )
        .unwrap();
        cx.activate(true);
    });
}
