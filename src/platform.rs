//! The seam between WinSend's logic and the operating system.
//!
//! Everything above this trait is portable and testable on any machine. The
//! only thing that genuinely needs Windows is an implementation of `Platform`.

use serde::{Deserialize, Serialize};

use crate::hotkey::KeyChord;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Bounds {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self { x, y, width, height }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// Device name such as `\\.\DISPLAY2`. Stable enough to persist, though it
    /// can shuffle when displays are physically replugged, so resolution falls
    /// back to geometry when the name no longer matches.
    pub id: String,
    pub bounds: Bounds,
    pub work_area: Bounds,
    pub is_primary: bool,
}

impl MonitorInfo {
    /// Human-readable label for the settings list, e.g.
    /// `2560x1440 at (2560, 0) — secondary`.
    pub fn label(&self) -> String {
        format!(
            "{}x{} at ({}, {}){}",
            self.bounds.width,
            self.bounds.height,
            self.bounds.x,
            self.bounds.y,
            if self.is_primary { " — primary" } else { "" }
        )
    }
}

/// A window the user could plausibly be trying to target, as presented in the
/// Select Zoom Window picker.
#[derive(Debug, Clone)]
pub struct WindowCandidate {
    /// Opaque OS handle. An `HWND` on Windows, a synthetic id under the mock.
    /// Never persisted: handles do not survive a process restart.
    pub handle: u64,
    pub process_name: String,
    pub class_name: String,
    pub title: String,
    pub bounds: Bounds,
    pub monitor_id: String,
    /// True when the process or class looked Zoom-ish. Only used to sort the
    /// picker so likely candidates surface first, never to exclude anything.
    pub likely_zoom: bool,
    /// Minimised windows are still enumerated so the confirmed window can be
    /// found and un-minimised, but they are kept out of the picker: their
    /// bounds are meaningless and they cannot be identified visually.
    pub minimized: bool,
    /// Whether the window holds itself above ordinary windows.
    ///
    /// Recorded so a window pushed aside can be put back in the band it came
    /// from, rather than assumed into the wrong one.
    pub topmost: bool,
    /// Cloaked windows are composed by the shell but not shown. Reported so
    /// the diagnostics can say a window exists and is being skipped, never
    /// acted on, since it is not on screen to be in anyone's way.
    pub cloaked: bool,
    /// True when the window belongs to WinSend itself. Never something to
    /// move, hide or offer in the picker.
    pub own_process: bool,
    /// Position in the front-to-back stacking order: 0 is frontmost.
    ///
    /// This is measured, not inferred. Whether one window is in front of
    /// another is exactly what this answers, and it needs no theory about how
    /// any particular application implements full screen.
    pub z_order: usize,
}

impl Bounds {
    /// How much of `monitor` this rectangle covers, from 0 to 1.
    ///
    /// Used to tell a full-screen video from a window that merely happens to
    /// sit on the same display.
    pub fn coverage_of(&self, monitor: Bounds) -> f32 {
        let overlap_width = (self.x + self.width).min(monitor.x + monitor.width) - self.x.max(monitor.x);
        let overlap_height =
            (self.y + self.height).min(monitor.y + monitor.height) - self.y.max(monitor.y);
        if overlap_width <= 0 || overlap_height <= 0 {
            return 0.0;
        }
        let monitor_area = (monitor.width as f32) * (monitor.height as f32);
        if monitor_area <= 0.0 {
            return 0.0;
        }
        (overlap_width as f32 * overlap_height as f32) / monitor_area
    }
}

/// How a window should be placed.
///
/// A struct rather than a list of positional flags: the call sites read as
/// `borderless: false` instead of a bare `false` whose meaning has to be looked
/// up, and placement has more than one dimension to it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub bounds: Bounds,
    /// Strip the window's frame so it fills the monitor edge to edge.
    pub borderless: bool,
    /// Hold the window above ordinary windows.
    ///
    /// Moving a window onto a monitor that already has something full-screen on
    /// it otherwise leaves it behind that content. Raising this window is the
    /// only way to fix that without touching the other application, which
    /// matters: whatever is playing underneath should keep playing, and still
    /// be there when the window is retrieved.
    pub topmost: bool,
}

/// What to ask for next, given what was asked for and what arrived.
///
/// Position is corrected by the difference and size by the ratio, because the
/// two go wrong in different ways: an offset is added to a position, while a
/// scale multiplies a size. Asking for the square of the request over the
/// result cancels a scale factor exactly in one step, where adding the
/// difference would only close part of the gap.
///
/// Lives on this side of the seam rather than with the Win32 code that calls
/// it because it is arithmetic, not an operating system call, and arithmetic
/// that decides where a window ends up should be testable on any machine.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn corrected_request(wanted: Bounds, actual: Bounds) -> Bounds {
    let scale = |wanted: i32, actual: i32| {
        if actual <= 0 || wanted <= 0 {
            wanted
        } else {
            ((wanted as i64 * wanted as i64) / actual as i64) as i32
        }
    };
    Bounds::new(
        wanted.x + (wanted.x - actual.x),
        wanted.y + (wanted.y - actual.y),
        scale(wanted.width, actual.width),
        scale(wanted.height, actual.height),
    )
}

/// RGBA8 preview of a window, sized by the platform layer.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformError {
    WindowGone,
    /// Only ever produced by the Win32 platform, so it reads as dead code on
    /// other targets.
    #[cfg_attr(not(windows), allow(dead_code))]
    Denied(String),
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WindowGone => write!(f, "the window no longer exists"),
            Self::Denied(detail) => write!(f, "permission denied: {detail}"),
        }
    }
}

pub trait Platform {
    fn monitors(&self) -> Vec<MonitorInfo>;

    /// Every visible top-level window worth showing in the picker. Deliberately
    /// broad rather than pre-filtered to "zoom" — see `likely_zoom`.
    fn candidate_windows(&self) -> Vec<WindowCandidate>;

    /// Best-effort preview. Returning `None` is normal and must not be treated
    /// as an error: GPU-composited windows can refuse to be captured.
    fn thumbnail(&self, handle: u64) -> Option<Thumbnail>;

    fn window_bounds(&self, handle: u64) -> Result<Bounds, PlatformError>;

    /// Bring a minimised window back so its real bounds can be read. Called
    /// before capturing a restore point, since a minimised window reports
    /// off-screen coordinates that would make Retrieve useless.
    fn unminimize(&self, handle: u64) -> Result<(), PlatformError>;

    /// Move and resize a window according to `placement`.
    fn place_window(&self, handle: u64, placement: Placement) -> Result<(), PlatformError>;

    /// Drop a window out of the always-on-top band and to the back.
    ///
    /// Deliberately not a minimise: the window keeps rendering and the
    /// application never learns it was covered, so a video playing behind the
    /// sent window carries on and is genuinely still there afterwards.
    fn demote(&self, handle: u64) -> Result<(), PlatformError>;

    /// Bring a window back to the front of the band it belongs in.
    ///
    /// `topmost` is what the window was before it was pushed aside, not a
    /// choice: restoring an ordinary window into the always-on-top band would
    /// leave it pinned over everything the user owns.
    fn raise(&self, handle: u64, topmost: bool) -> Result<(), PlatformError>;

    /// Fallback for a window that will not stay demoted.
    fn minimize(&self, handle: u64) -> Result<(), PlatformError>;

    /// Facts about this platform worth putting in the diagnostics report.
    /// Empty where there is nothing platform-specific to say.
    fn diagnostic_notes(&self) -> Vec<String> {
        Vec::new()
    }

    /// Ask the OS to draw our own window's frame to match the application.
    ///
    /// The title bar is the one part of the interface the application cannot
    /// paint for itself, and a white bar above a dark panel is exactly the
    /// mismatch this exists to remove. Everything it does is advisory: an OS
    /// that does not recognise the request leaves the frame as it was, which
    /// is the same outcome as not asking.
    ///
    /// Defaulted to nothing so the mock and every non-Windows target need say
    /// nothing about a question only Windows has an answer to.
    fn apply_window_chrome(&self, _handle: u64) {}

    /// Give the window the foreground.
    ///
    /// The only lever that reaches a full-screen exclusive window. Those are
    /// managed outside the normal stacking order, so they never appear in
    /// `candidate_windows` and nothing done to other windows affects them —
    /// but they give way when something else takes focus, which is exactly
    /// what clicking another window does by hand.
    fn activate(&self, handle: u64) -> Result<(), PlatformError>;

    /// Press a key combination in the window, as if typed.
    ///
    /// The one mechanism that reaches another application's full-screen mode.
    /// Windows has no API for asking a process to enter it — full screen is
    /// internal state each application manages for itself — so the only honest
    /// request is the application's own toggle shortcut, delivered as input.
    /// Input lands wherever the focus is, so this refuses unless `handle`
    /// holds the foreground, and refuses while a physically held modifier
    /// would corrupt the chord. A `Denied` from either guard means "not right
    /// now" rather than "never": the caller retries on its next look.
    fn send_key(&self, handle: u64, chord: KeyChord) -> Result<(), PlatformError>;

    /// Take a window off screen entirely, and put it back.
    ///
    /// The genuine last resort. A borderless full-screen popup often has no
    /// minimise behaviour at all, so `minimize` can report success and leave it
    /// exactly where it was. Only reached when the user has explicitly asked
    /// for the monitor to be cleared, because hiding another application's
    /// window is more than it is reasonable to do uninvited.
    fn hide(&self, handle: u64) -> Result<(), PlatformError>;

    fn show(&self, handle: u64) -> Result<(), PlatformError>;

    /// Make a window translucent, from 1.0 for opaque down to 0.0 for
    /// invisible.
    ///
    /// Not free on Windows: it needs the window in the layered band, which
    /// composes differently and which a GPU-composited window such as Zoom's
    /// may not take kindly to. An error here means the window will not go
    /// translucent, and the only correct response is to stop trying and cut.
    fn set_window_opacity(&self, handle: u64, alpha: f32) -> Result<(), PlatformError>;

    /// Put the window back to fully opaque, and undo whatever was needed to
    /// make it translucent.
    ///
    /// Must be safe to call on a window that was never faded, and on one that
    /// has since closed, because it is the last thing every failure path does.
    /// A window left part-way transparent on camera is a visible fault, where
    /// the hard cut this replaced was merely unremarkable.
    fn clear_window_opacity(&self, handle: u64) -> Result<(), PlatformError>;

    /// Escape hatch for the mock-only debug controls. Absent from Windows
    /// builds entirely, so it cannot leak into the shipped binary.
    #[cfg(not(windows))]
    fn as_mock(&self) -> Option<&crate::mock::MockPlatform> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scale_factor_is_cancelled_in_one_step() {
        // Asked for 1000 wide, arrived 1500: the window is being scaled by
        // 1.5, so asking for 667 lands on 1000.
        let corrected = corrected_request(
            Bounds::new(100, 100, 1000, 600),
            Bounds::new(100, 100, 1500, 900),
        );
        assert_eq!((corrected.width, corrected.height), (666, 400));
        // Which is what lands on the size actually wanted once the window
        // scales it by the same 1.5 again.
        assert_eq!(corrected.height * 3 / 2, 600);
    }

    #[test]
    fn an_offset_is_corrected_by_the_difference() {
        let corrected = corrected_request(
            Bounds::new(300, 200, 400, 300),
            Bounds::new(280, 190, 400, 300),
        );
        assert_eq!((corrected.x, corrected.y), (320, 210));
    }

    #[test]
    fn a_window_that_arrived_with_no_size_is_asked_for_the_same_size_again() {
        // A minimised or closing window reports nonsense rather than a scale
        // factor, and dividing by it would produce nonsense of our own.
        let wanted = Bounds::new(10, 20, 800, 600);
        let corrected = corrected_request(wanted, Bounds::new(10, 20, 0, 0));
        assert_eq!((corrected.width, corrected.height), (800, 600));
    }
}
