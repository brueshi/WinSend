//! The seam between WinSend's logic and the operating system.
//!
//! Everything above this trait is portable and testable on any machine. The
//! only thing that genuinely needs Windows is an implementation of `Platform`.

use serde::{Deserialize, Serialize};

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

    /// Last resort for a window that will not stay demoted.
    fn minimize(&self, handle: u64) -> Result<(), PlatformError>;

    /// Escape hatch for the mock-only debug controls. Absent from Windows
    /// builds entirely, so it cannot leak into the shipped binary.
    #[cfg(not(windows))]
    fn as_mock(&self) -> Option<&crate::mock::MockPlatform> {
        None
    }
}
