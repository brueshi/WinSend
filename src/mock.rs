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
use crate::platform::{
    Bounds, MonitorInfo, Placement, Platform, PlatformError, Thumbnail, WindowCandidate,
};
use crate::shell::{HotkeyReport, Shell, ShellEvent, TrayState, Waker};

/// Which end of the stacking order to move a window to.
enum Depth {
    Front,
    Back,
}

/// A platform call that changed the fake desktop, as it arrived.
///
/// Every other field on `MockPlatform` answers "what state is it in now". This
/// answers "in what order did it get there", which is the only way to test
/// something whose whole point is the sequence — Retrieve putting the displaced
/// windows back *before* it moves the sent window off them, where both orders
/// leave the desktop looking identical once the dust settles.
///
/// Calls, not effects: a call that reports success and does nothing, the way a
/// window that refuses to minimise does, is still recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Placed(u64),
    Raised(u64),
    Demoted(u64),
    Minimized(u64),
    Unminimized(u64),
    Hidden(u64),
    Shown(u64),
    Activated(u64),
}

pub struct MockPlatform {
    windows: RefCell<Vec<WindowCandidate>>,
    /// Every mutating call, oldest first. See [`Call`].
    calls: RefCell<Vec<Call>>,
    /// Flips the Zoom windows out of existence to test the reconfirm prompt.
    zoom_present: RefCell<bool>,
    /// Bounds from before a window was minimised, so restoring puts them back
    /// the way Windows does.
    pre_minimize: RefCell<HashMap<u64, Bounds>>,
    /// Windows that ignore being demoted.
    sticky: RefCell<Vec<u64>>,
    /// Windows that ignore being minimised, the way a borderless popup with no
    /// minimise behaviour does.
    unminimisable: RefCell<Vec<u64>>,
    /// Windows taken off screen.
    hidden: RefCell<Vec<u64>>,
    /// Windows that were given the foreground.
    activated: RefCell<Vec<u64>>,
    /// Windows that suspend rather than minimise when they lose the display.
    suspends: RefCell<Vec<u64>>,
    /// Windows owning a display exclusively.
    ///
    /// Models what Windows does with a full-screen exclusive window: while it
    /// owns the screen it is not in the window list at all, and it minimises
    /// itself the moment something else takes the foreground. Both halves
    /// matter — the first is why nothing could ever be found to push aside,
    /// the second is the only trace it leaves behind.
    exclusive: RefCell<Vec<u64>>,
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
            calls: RefCell::new(Vec::new()),
            zoom_present: RefCell::new(true),
            pre_minimize: RefCell::new(HashMap::new()),
            sticky: RefCell::new(Vec::new()),
            unminimisable: RefCell::new(Vec::new()),
            hidden: RefCell::new(Vec::new()),
            activated: RefCell::new(Vec::new()),
            exclusive: RefCell::new(Vec::new()),
            suspends: RefCell::new(Vec::new()),
        }
    }

    /// Whether the window is being held above everything else.
    #[cfg(test)]
    pub fn is_topmost(&self, handle: u64) -> bool {
        self.windows
            .borrow()
            .iter()
            .any(|w| w.handle == handle && w.topmost)
    }

    #[cfg(test)]
    pub fn is_minimized(&self, handle: u64) -> bool {
        self.windows
            .borrow()
            .iter()
            .any(|w| w.handle == handle && w.minimized)
    }

    /// Put a window at the front of the stacking order, standing in for
    /// whatever already owns the display.
    #[cfg(test)]
    pub fn bring_to_front(&self, handle: u64) {
        let _ = self.restack(handle, Depth::Front);
    }

    /// Make a window refuse to be pushed back, the way a player that re-asserts
    /// itself does, so the minimise fallback can be exercised.
    #[cfg(test)]
    pub fn set_sticky(&self, handle: u64) {
        self.sticky.borrow_mut().push(handle);
    }

    /// Make a window ignore being minimised, the way a borderless popup does,
    /// so the hide fallback can be exercised.
    #[cfg(test)]
    pub fn set_unminimisable(&self, handle: u64) {
        self.unminimisable.borrow_mut().push(handle);
    }

    /// Add a window to the fake desktop, for cases the default set does not
    /// model.
    #[cfg(test)]
    pub fn add_window(
        &self,
        handle: u64,
        process_name: &str,
        class_name: &str,
        title: &str,
        bounds: Bounds,
    ) {
        self.windows.borrow_mut().push(window(
            handle,
            process_name,
            class_name,
            title,
            bounds,
            r"\\.\DISPLAY2",
            false,
        ));
    }

    #[cfg(test)]
    pub fn set_own_process(&self, handle: u64) {
        if let Some(window) = self.windows.borrow_mut().iter_mut().find(|w| w.handle == handle) {
            window.own_process = true;
        }
    }

    #[cfg(test)]
    pub fn title_of(&self, handle: u64) -> String {
        self.windows
            .borrow()
            .iter()
            .find(|w| w.handle == handle)
            .map(|w| w.title.clone())
            .unwrap_or_default()
    }

    /// Whether `front` sits above `back` in the stacking order.
    #[cfg(test)]
    pub fn is_in_front_of(&self, front: u64, back: u64) -> bool {
        let windows = self.windows.borrow();
        let at = |handle| windows.iter().position(|w| w.handle == handle);
        match (at(front), at(back)) {
            (Some(front), Some(back)) => front < back,
            _ => false,
        }
    }

    /// Give a window the screen exclusively, the way a full-screen media
    /// player does.
    #[cfg(test)]
    pub fn set_exclusive(&self, handle: u64) {
        self.exclusive.borrow_mut().push(handle);
    }

    /// Make a window suspend rather than minimise, the way a packaged
    /// application does.
    #[cfg(test)]
    pub fn set_suspends(&self, handle: u64) {
        self.suspends.borrow_mut().push(handle);
    }

    #[cfg(test)]
    pub fn is_cloaked(&self, handle: u64) -> bool {
        self.windows
            .borrow()
            .iter()
            .any(|w| w.handle == handle && w.cloaked)
    }

    #[cfg(test)]
    pub fn was_activated(&self, handle: u64) -> bool {
        self.activated.borrow().contains(&handle)
    }

    #[cfg(test)]
    pub fn is_hidden(&self, handle: u64) -> bool {
        self.hidden.borrow().contains(&handle)
    }

    /// Every mutating call so far, oldest first.
    #[cfg(test)]
    pub fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    fn record(&self, call: Call) {
        self.calls.borrow_mut().push(call);
    }

    /// Move a window to one end of the stacking order.
    fn restack(&self, handle: u64, depth: Depth) -> Result<(), PlatformError> {
        let mut windows = self.windows.borrow_mut();
        let at = windows
            .iter()
            .position(|w| w.handle == handle)
            .ok_or(PlatformError::WindowGone)?;
        let window = windows.remove(at);
        match depth {
            Depth::Front => windows.insert(0, window),
            Depth::Back => windows.push(window),
        }
        Ok(())
    }

    /// Put a window into the always-on-top band, standing in for whatever a
    /// media player does when it goes full screen.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn set_topmost(&self, handle: u64, topmost: bool) -> Result<(), PlatformError> {
        let mut windows = self.windows.borrow_mut();
        let window = windows
            .iter_mut()
            .find(|w| w.handle == handle)
            .ok_or(PlatformError::WindowGone)?;
        window.topmost = topmost;
        Ok(())
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
    tray: RefCell<TrayState>,
}

impl MockShell {
    pub fn new(waker: Waker) -> Self {
        Self {
            waker,
            queue: RefCell::new(Vec::new()),
            registered: RefCell::new(Hotkeys::default()),
            unavailable: RefCell::new(Vec::new()),
            tray: RefCell::new(TrayState::default()),
        }
    }

    /// Stand in for choosing an item from the tray menu.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn choose(&self, event: ShellEvent) {
        self.emit(event);
    }

    /// What the tray icon would be showing, so the debug controls can display
    /// it and tests can assert on it.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn tray(&self) -> TrayState {
        self.tray.borrow().clone()
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

    fn set_tray_state(&self, state: TrayState) {
        *self.tray.borrow_mut() = state;
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
        topmost: false,
        own_process: false,
        cloaked: false,
        // Overwritten from the vector's order on every enumeration.
        z_order: 0,
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
        // A media player's full-screen output filling the second display, and
        // deliberately untitled. This is what makes Send look like it did
        // nothing: the window that has to move is a bare popup with no caption
        // text, so anything that filtered on having a title never saw it at
        // all. It must be enumerated and must stay out of the picker.
        window(
            0x3001,
            "vlc.exe",
            "Qt5152QWindowIcon",
            "",
            Bounds::new(2560, 0, 1920, 1080),
            r"\\.\DISPLAY2",
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

    /// The vector's order is the stacking order, front first, mirroring what
    /// EnumWindows gives on Windows. Demoting and raising move a window within
    /// it, so the z-order logic above is exercised against something that
    /// actually behaves like a desktop.
    fn candidate_windows(&self) -> Vec<WindowCandidate> {
        let present = self.zoom_present();
        self.windows
            .borrow()
            .iter()
            .filter(|w| present || w.process_name != "Zoom.exe")
            .filter(|w| !self.hidden.borrow().contains(&w.handle))
            // Owning a screen exclusively keeps it out of the window list
            // entirely, until it minimises and rejoins the ordinary world.
            .filter(|w| {
                w.minimized || w.cloaked || !self.exclusive.borrow().contains(&w.handle)
            })
            .enumerate()
            .map(|(depth, window)| WindowCandidate { z_order: depth, ..window.clone() })
            .collect()
    }

    fn thumbnail(&self, handle: u64) -> Option<Thumbnail> {
        let windows = self.windows.borrow();
        let window = windows.iter().find(|w| w.handle == handle)?;
        Some(synthetic_thumbnail(window))
    }

    fn unminimize(&self, handle: u64) -> Result<(), PlatformError> {
        self.record(Call::Unminimized(handle));
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

    /// Minimise the way Windows does, including the off-screen bounds it
    /// reports for iconic windows. Those bounds are the reason Send has to
    /// un-minimise before capturing a restore point.
    /// Recorded rather than acted on. Activation's real effect is on the
    /// foreground, and the thing it exists for — a full-screen exclusive
    /// window that gives way when something else takes focus — is precisely
    /// what a fake desktop cannot model. Pretending otherwise would give
    /// false confidence about the one case it is there to handle.
    fn activate(&self, handle: u64) -> Result<(), PlatformError> {
        self.record(Call::Activated(handle));
        if !self.windows.borrow().iter().any(|w| w.handle == handle) {
            return Err(PlatformError::WindowGone);
        }
        if let Some(window) = self.windows.borrow_mut().iter_mut().find(|w| w.handle == handle) {
            window.cloaked = false;
        }
        self.activated.borrow_mut().push(handle);

        // Anything owning a screen exclusively gives it up the moment
        // something else is focused, and minimises itself doing so.
        let surrendering: Vec<u64> = self
            .exclusive
            .borrow()
            .iter()
            .copied()
            .filter(|owner| *owner != handle)
            .collect();
        for owner in surrendering {
            if self.suspends.borrow().contains(&owner) {
                // A packaged application suspends instead, and its window goes
                // cloaked with the minimised flag never being set.
                if let Some(window) =
                    self.windows.borrow_mut().iter_mut().find(|w| w.handle == owner)
                {
                    window.cloaked = true;
                }
                continue;
            }
            let _ = self.minimize(owner);
        }
        Ok(())
    }

    fn hide(&self, handle: u64) -> Result<(), PlatformError> {
        self.record(Call::Hidden(handle));
        if !self.windows.borrow().iter().any(|w| w.handle == handle) {
            return Err(PlatformError::WindowGone);
        }
        self.hidden.borrow_mut().push(handle);
        Ok(())
    }

    fn show(&self, handle: u64) -> Result<(), PlatformError> {
        self.record(Call::Shown(handle));
        self.hidden.borrow_mut().retain(|hidden| *hidden != handle);
        Ok(())
    }

    fn minimize(&self, handle: u64) -> Result<(), PlatformError> {
        self.record(Call::Minimized(handle));
        if self.unminimisable.borrow().contains(&handle) {
            // Reports success and stays exactly where it was, which is the
            // failure mode the hide fallback exists for.
            return Ok(());
        }
        let mut windows = self.windows.borrow_mut();
        let window = windows
            .iter_mut()
            .find(|w| w.handle == handle)
            .ok_or(PlatformError::WindowGone)?;
        self.pre_minimize.borrow_mut().insert(handle, window.bounds);
        window.minimized = true;
        window.topmost = false;
        window.bounds = Bounds::new(-32000, -32000, 160, 28);
        drop(windows);
        self.restack(handle, Depth::Back)
    }

    fn demote(&self, handle: u64) -> Result<(), PlatformError> {
        self.record(Call::Demoted(handle));
        if self.sticky.borrow().contains(&handle) {
            // Reports success and stays exactly where it was, which is the
            // failure mode the minimise fallback exists for.
            return Ok(());
        }
        self.set_topmost(handle, false)?;
        self.restack(handle, Depth::Back)
    }

    fn raise(&self, handle: u64, topmost: bool) -> Result<(), PlatformError> {
        self.record(Call::Raised(handle));
        self.set_topmost(handle, topmost)?;
        self.restack(handle, Depth::Front)
    }

    fn place_window(&self, handle: u64, placement: Placement) -> Result<(), PlatformError> {
        self.record(Call::Placed(handle));
        if !self.zoom_present() {
            return Err(PlatformError::WindowGone);
        }
        let mut windows = self.windows.borrow_mut();
        let window = windows
            .iter_mut()
            .find(|w| w.handle == handle)
            .ok_or(PlatformError::WindowGone)?;
        window.bounds = placement.bounds;

        window.topmost = placement.topmost;
        // Deliberately no restack. Whether placing a window actually brings it
        // in front of full-screen media is the thing that keeps turning out
        // not to be true, so the mock does not assume it either; the logic
        // above has to measure and react rather than trust the placement.
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
