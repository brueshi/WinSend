//! A fake desktop, so the UI and every state transition can be exercised on a
//! machine that is not Windows.
//!
//! It models the situation that makes this app necessary: a main Zoom meeting
//! window and a dual-monitor video window that share a window class and differ
//! only by title. Windows can be made to vanish on demand to exercise the
//! "confirmed window is gone" path without needing Zoom to cooperate.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::hotkey::{Action, Hotkey, Hotkeys};
use crate::platform::{Bounds, MonitorInfo, Platform, PlatformError, Thumbnail, WindowCandidate};
use crate::shell::{HotkeyReport, Shell, ShellEvent, Waker};

pub struct MockPlatform {
    windows: RefCell<Vec<WindowCandidate>>,
    /// Flips the Zoom windows out of existence to test the reconfirm prompt.
    zoom_present: RefCell<bool>,
    /// Bounds from before a window was minimised, so restoring puts them back
    /// the way Windows does.
    pre_minimize: RefCell<HashMap<u64, Bounds>>,
}

impl Default for MockPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPlatform {
    pub fn new() -> Self {
        Self {
            windows: RefCell::new(default_windows()),
            zoom_present: RefCell::new(true),
            pre_minimize: RefCell::new(HashMap::new()),
        }
    }

    /// Minimise a window the way Windows does, including the off-screen bounds
    /// it reports for iconic windows. Those bounds are the reason Send has to
    /// un-minimise before capturing a restore point.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn minimize(&self, handle: u64) {
        let mut windows = self.windows.borrow_mut();
        if let Some(window) = windows.iter_mut().find(|w| w.handle == handle) {
            self.pre_minimize.borrow_mut().insert(handle, window.bounds);
            window.minimized = true;
            window.bounds = Bounds::new(-32000, -32000, 160, 28);
        }
    }

    pub fn zoom_present(&self) -> bool {
        *self.zoom_present.borrow()
    }

    /// Driven by the mock-only debug checkbox, which is compiled out on
    /// Windows even though the mock itself remains reachable there via
    /// `WINSEND_MOCK=1`.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn set_zoom_present(&self, present: bool) {
        *self.zoom_present.borrow_mut() = present;
    }
}

/// A fake desktop shell, so the hotkey and tray paths can be walked end to end
/// without Windows.
///
/// It has no thread of its own: events are injected by the debug controls on
/// the main screen, or directly by tests. The waker is still called on every
/// injection, since getting that wiring wrong is what would make a real hotkey
/// press appear to do nothing.
pub struct MockShell {
    waker: Waker,
    queue: RefCell<Vec<ShellEvent>>,
    /// The bindings that actually took, which is not the same as the ones
    /// asked for once a combination has been refused.
    registered: RefCell<Hotkeys>,
    /// Combinations that will refuse to register, standing in for one already
    /// owned by another application.
    unavailable: RefCell<Vec<Hotkey>>,
}

impl MockShell {
    pub fn new(waker: Waker) -> Self {
        Self {
            waker,
            queue: RefCell::new(Vec::new()),
            registered: RefCell::new(Hotkeys::default()),
            unavailable: RefCell::new(Vec::new()),
        }
    }

    /// Stand in for a hotkey press. Driven by the mock-only debug buttons,
    /// which are compiled out on Windows even though the mock shell itself
    /// remains reachable there via `WINSEND_MOCK=1`.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn trigger(&self, action: Action) {
        self.emit(ShellEvent::Trigger(action));
    }

    /// Make these combinations refuse to register, so the failure path can be
    /// exercised without persuading another application to take one.
    #[cfg(test)]
    pub fn set_unavailable(&self, hotkeys: Vec<Hotkey>) {
        *self.unavailable.borrow_mut() = hotkeys;
    }

    #[cfg(test)]
    pub fn registered(&self) -> Hotkeys {
        *self.registered.borrow()
    }

    fn emit(&self, event: ShellEvent) {
        self.queue.borrow_mut().push(event);
        (self.waker)();
    }
}

impl Shell for MockShell {
    fn apply_hotkeys(&self, hotkeys: Hotkeys) {
        let mut registered = Hotkeys::default();
        let mut report = HotkeyReport::default();

        for action in Action::ALL {
            let Some(hotkey) = hotkeys.binding(action) else {
                continue;
            };
            if self.unavailable.borrow().contains(&hotkey) {
                report.rejected.push((
                    action,
                    format!("{hotkey} is already in use by another application"),
                ));
                continue;
            }
            // Cannot fail: the set being applied already satisfies the rules.
            let _ = registered.set(action, Some(hotkey));
        }

        *self.registered.borrow_mut() = registered;
        self.emit(ShellEvent::HotkeysApplied(report));
    }

    fn poll(&self) -> Vec<ShellEvent> {
        self.queue.borrow_mut().drain(..).collect()
    }

    #[cfg(not(windows))]
    fn as_mock(&self) -> Option<&MockShell> {
        Some(self)
    }
}

fn window(
    handle: u64,
    process_name: &str,
    class_name: &str,
    title: &str,
    bounds: Bounds,
    monitor_id: &str,
    likely_zoom: bool,
) -> WindowCandidate {
    WindowCandidate {
        handle,
        process_name: process_name.into(),
        class_name: class_name.into(),
        title: title.into(),
        bounds,
        monitor_id: monitor_id.into(),
        likely_zoom,
        minimized: false,
    }
}

fn default_windows() -> Vec<WindowCandidate> {
    vec![
        // The main meeting window and the video window are indistinguishable by
        // description: same process, same class, same title. Observed on real
        // Windows, and the reason a remembered handle is needed to tell them
        // apart at all.
        window(
            0x1001,
            "Zoom.exe",
            "ZPContentViewWndClass",
            "Zoom Workplace",
            Bounds::new(120, 80, 1280, 800),
            r"\\.\DISPLAY1",
            true,
        ),
        // The target.
        window(
            0x1002,
            "Zoom.exe",
            "ZPContentViewWndClass",
            "Zoom Workplace",
            Bounds::new(2700, 200, 960, 540),
            r"\\.\DISPLAY2",
            true,
        ),
        window(
            0x2001,
            "chrome.exe",
            "Chrome_WidgetWin_1",
            "Production runsheet - Google Docs",
            Bounds::new(300, 150, 1440, 900),
            r"\\.\DISPLAY1",
            false,
        ),
        window(
            0x2002,
            "obs64.exe",
            "Qt5152QWindowIcon",
            "OBS 30.0.2 - Profile: Live",
            Bounds::new(0, 0, 1200, 760),
            r"\\.\DISPLAY1",
            false,
        ),
        // Long title, to keep the picker layout honest about overflow.
        window(
            0x2003,
            "explorer.exe",
            "CabinetWClass",
            "Q3 Broadcast Assets — Final — Approved — Do Not Move Or Rename",
            Bounds::new(400, 300, 1100, 700),
            r"\\.\DISPLAY1",
            false,
        ),
    ]
}

impl Platform for MockPlatform {
    fn monitors(&self) -> Vec<MonitorInfo> {
        vec![
            MonitorInfo {
                id: r"\\.\DISPLAY1".into(),
                bounds: Bounds::new(0, 0, 2560, 1440),
                work_area: Bounds::new(0, 0, 2560, 1400),
                is_primary: true,
            },
            MonitorInfo {
                id: r"\\.\DISPLAY2".into(),
                bounds: Bounds::new(2560, 0, 1920, 1080),
                work_area: Bounds::new(2560, 0, 1920, 1040),
                is_primary: false,
            },
        ]
    }

    fn candidate_windows(&self) -> Vec<WindowCandidate> {
        let present = self.zoom_present();
        self.windows
            .borrow()
            .iter()
            .filter(|w| present || w.process_name != "Zoom.exe")
            .cloned()
            .collect()
    }

    fn thumbnail(&self, handle: u64) -> Option<Thumbnail> {
        let windows = self.windows.borrow();
        let window = windows.iter().find(|w| w.handle == handle)?;
        Some(synthetic_thumbnail(window))
    }

    fn unminimize(&self, handle: u64) -> Result<(), PlatformError> {
        if !self.zoom_present() {
            return Err(PlatformError::WindowGone);
        }
        let mut windows = self.windows.borrow_mut();
        let window = windows
            .iter_mut()
            .find(|w| w.handle == handle)
            .ok_or(PlatformError::WindowGone)?;
        if window.minimized {
            window.minimized = false;
            if let Some(bounds) = self.pre_minimize.borrow_mut().remove(&handle) {
                window.bounds = bounds;
            }
        }
        Ok(())
    }

    fn window_bounds(&self, handle: u64) -> Result<Bounds, PlatformError> {
        if !self.zoom_present() {
            return Err(PlatformError::WindowGone);
        }
        self.windows
            .borrow()
            .iter()
            .find(|w| w.handle == handle)
            .map(|w| w.bounds)
            .ok_or(PlatformError::WindowGone)
    }

    fn set_window_bounds(
        &self,
        handle: u64,
        bounds: Bounds,
        _borderless: bool,
    ) -> Result<(), PlatformError> {
        if !self.zoom_present() {
            return Err(PlatformError::WindowGone);
        }
        let mut windows = self.windows.borrow_mut();
        let window = windows
            .iter_mut()
            .find(|w| w.handle == handle)
            .ok_or(PlatformError::WindowGone)?;
        window.bounds = bounds;
        Ok(())
    }

    #[cfg(not(windows))]
    fn as_mock(&self) -> Option<&MockPlatform> {
        Some(self)
    }
}

/// Distinguishable placeholder art, so the picker's thumbnail rendering can be
/// developed without a real capture backend. Zoom-ish windows get a "video"
/// look (a bright centre block on dark) and everything else a flat panel.
fn synthetic_thumbnail(window: &WindowCandidate) -> Thumbnail {
    const WIDTH: u32 = 192;
    const HEIGHT: u32 = 108;

    // Spread hues across handles so two windows never look identical.
    let hue = ((window.handle.wrapping_mul(2654435761)) % 360) as f32;
    let (r, g, b) = hsv_to_rgb(hue, 0.55, 0.85);

    let mut rgba = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let in_centre = window.likely_zoom
                && x > WIDTH / 4
                && x < WIDTH * 3 / 4
                && y > HEIGHT / 5
                && y < HEIGHT * 4 / 5;
            let (pr, pg, pb) = if window.likely_zoom {
                if in_centre {
                    (r, g, b)
                } else {
                    (18, 18, 22)
                }
            } else {
                let shade = 40 + (y * 60 / HEIGHT) as u8;
                (shade, shade, shade + 8)
            };
            rgba.extend_from_slice(&[pr, pg, pb, 255]);
        }
    }

    Thumbnail { width: WIDTH, height: HEIGHT, rgba }
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}
