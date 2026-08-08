//! Send and Retrieve, independent of any UI.
//!
//! Every operation re-locates the target window rather than assuming the last
//! one is still valid. The handle the user picked is remembered, because Zoom's
//! main and video windows are otherwise indistinguishable, but it is checked
//! against the live window list on each use: Zoom can close and reopen the
//! video window between presses, and a dead handle can be reissued by the OS to
//! something else entirely.

use crate::config::Config;
use crate::hotkey::{Action, Hotkey};
use crate::identity::{matches_structurally, resolve, Resolution, WindowIdentity};
use crate::platform::{Bounds, MonitorInfo, Placement, Platform, WindowCandidate};

/// Why an action could not complete.
///
/// `needs_selection` exists so the UI can open the picker directly instead of
/// showing an error the user has to decode and act on by navigating to
/// Settings themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub needs_selection: bool,
}

impl Failure {
    fn plain(message: impl Into<String>) -> Self {
        Self { message: message.into(), needs_selection: false }
    }

    fn needs_selection(message: impl Into<String>) -> Self {
        Self { message: message.into(), needs_selection: true }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::plain(message)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub struct Core {
    pub platform: Box<dyn Platform>,
    pub config: Config,
    /// Where the window sat before the last Send. Session-scoped by design:
    /// restoring to bounds captured in some previous run of the app would be
    /// restoring to a layout that no longer exists.
    saved_bounds: Option<Bounds>,
    /// The handle the user actually clicked in the picker.
    ///
    /// Zoom gives its main and video windows the same process, class and title,
    /// which makes them indistinguishable by description alone. The handle is
    /// the only thing that tells them apart, so it is worth keeping — but it is
    /// re-validated on every use rather than trusted, since a dead handle can
    /// be reissued by the OS to an unrelated window.
    confirmed_handle: Option<u64>,
}

impl Core {
    pub fn new(platform: Box<dyn Platform>, config: Config) -> Self {
        Self { platform, config, saved_bounds: None, confirmed_handle: None }
    }

    pub fn can_retrieve(&self) -> bool {
        self.saved_bounds.is_some()
    }

    pub fn monitors(&self) -> Vec<MonitorInfo> {
        self.platform.monitors()
    }

    /// Picker contents, with the Zoom-ish windows first so the likely target is
    /// near the top without anything being hidden. Minimised windows are left
    /// out: they cannot be identified visually and their bounds are nonsense.
    pub fn candidates(&self) -> Vec<WindowCandidate> {
        let mut candidates: Vec<WindowCandidate> = self
            .platform
            .candidate_windows()
            .into_iter()
            .filter(|c| !c.minimized)
            .collect();
        candidates.sort_by_key(|c| (!c.likely_zoom, c.process_name.to_lowercase(), c.handle));
        candidates
    }

    pub fn confirm_window(&mut self, candidate: &WindowCandidate) -> Result<String, Failure> {
        self.config.zoom_window = Some(WindowIdentity::from_candidate(candidate));
        self.confirmed_handle = Some(candidate.handle);
        self.config.save()?;
        Ok(format!("Confirmed \"{}\"", candidate.title))
    }

    pub fn set_target_monitor(&mut self, monitor: &MonitorInfo) -> Result<String, Failure> {
        self.config.set_target(monitor);
        self.config.save()?;
        Ok(format!("Target set to {}", monitor.label()))
    }

    /// Bind or clear a hotkey. Persisted immediately, since a binding the user
    /// has to remember to save is a binding they will lose.
    pub fn set_hotkey(&mut self, action: Action, hotkey: Option<Hotkey>) -> Result<String, Failure> {
        self.config.hotkeys.set(action, hotkey).map_err(Failure::plain)?;
        self.config.save()?;
        Ok(match hotkey {
            Some(hotkey) => format!("{} hotkey set to {hotkey}", action.label()),
            None => format!("{} hotkey cleared", action.label()),
        })
    }

    pub fn set_borderless(&mut self, borderless: bool) -> Result<(), String> {
        self.config.borderless = borderless;
        self.config.save()
    }

    /// Locate the confirmed window right now, or explain what the user must do.
    ///
    /// Returns the whole candidate rather than a handle so callers can see
    /// whether it is minimised without enumerating the desktop a second time.
    fn locate(&self) -> Result<WindowCandidate, Failure> {
        let identity = self.config.zoom_window.as_ref().ok_or_else(|| {
            Failure::needs_selection("No Zoom window confirmed yet. Pick the video window.")
        })?;

        let candidates = self.platform.candidate_windows();

        // The window the user clicked, if it is still around and still matches
        // what they picked. This is what makes Zoom's identical main and video
        // windows separable at all, and it is checked rather than assumed.
        if let Some(handle) = self.confirmed_handle {
            if let Some(found) = candidates
                .iter()
                .find(|c| c.handle == handle && matches_structurally(identity, c))
            {
                return Ok(found.clone());
            }
        }

        match resolve(identity, &candidates) {
            Resolution::Found(handle) => candidates
                .into_iter()
                .find(|c| c.handle == handle)
                .ok_or_else(|| Failure::plain("The window disappeared while being located.")),
            Resolution::Ambiguous(handles) => Err(Failure::needs_selection(format!(
                "{} Zoom windows look identical. Pick the video window again.",
                handles.len()
            ))),
            Resolution::NotFound => {
                // "Zoom is closed" and "the window changed" need different
                // things from the user, so they get different messages and
                // only the second one is worth opening the picker for.
                let process_present = candidates
                    .iter()
                    .any(|c| c.process_name.eq_ignore_ascii_case(&identity.process_name));

                if process_present {
                    Err(Failure::needs_selection(format!(
                        "The confirmed {} window is gone. Pick it again.",
                        identity.process_name
                    )))
                } else {
                    Err(Failure::plain(format!(
                        "{} does not appear to be running.",
                        identity.process_name
                    )))
                }
            }
        }
    }

    /// Bring the window back if it is minimised. Must happen before bounds are
    /// read or set: a minimised window reports off-screen coordinates, and
    /// capturing those as a restore point would send Retrieve nowhere useful.
    fn ensure_visible(&self, window: &WindowCandidate) -> Result<(), Failure> {
        if !window.minimized {
            return Ok(());
        }
        self.platform
            .unminimize(window.handle)
            .map_err(|e| Failure::plain(format!("Could not restore the minimised window: {e}")))
    }

    pub fn send(&mut self) -> Result<String, Failure> {
        let window = self.locate()?;

        let monitors = self.platform.monitors();
        let monitor = self.config.resolve_monitor(&monitors).ok_or_else(|| {
            Failure::plain("Target monitor is not connected. Pick one in Settings.")
        })?;
        let destination = monitor.bounds;
        let label = monitor.label();

        self.ensure_visible(&window)?;

        // Capture only when there is no restore point yet. Pressing Send twice
        // would otherwise record the already-moved position and permanently
        // lose where the window started.
        let captured = if self.saved_bounds.is_none() {
            Some(
                self.platform
                    .window_bounds(window.handle)
                    .map_err(|e| {
                        Failure::plain(format!("Could not read the window's position: {e}"))
                    })?,
            )
        } else {
            None
        };

        self.platform
            .place_window(
                window.handle,
                Placement { bounds: destination, borderless: self.config.borderless },
            )
            .map_err(|e| Failure::plain(format!("Could not move the window: {e}")))?;

        // Commit the restore point only once the move has actually succeeded,
        // so a failed Send does not leave Retrieve pointing somewhere wrong.
        if let Some(bounds) = captured {
            self.saved_bounds = Some(bounds);
        }

        Ok(format!("Sent to {label}"))
    }

    pub fn retrieve(&mut self) -> Result<String, Failure> {
        let bounds = self.saved_bounds.ok_or_else(|| {
            Failure::plain("Nothing has been sent yet, so there is no position to restore.")
        })?;

        let window = self.locate()?;
        self.ensure_visible(&window)?;

        self.platform
            .place_window(window.handle, Placement { bounds, borderless: false })
            .map_err(|e| Failure::plain(format!("Could not restore the window: {e}")))?;

        // Consumed: the next Send captures a fresh restore point rather than
        // reusing a position that may no longer mean anything.
        self.saved_bounds = None;

        Ok("Restored to its original position".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockPlatform;

    /// The two Zoom windows in the mock are identical in every respect the
    /// config records, so tests address them by handle.
    const MAIN_WINDOW: u64 = 0x1001;
    const VIDEO_WINDOW: u64 = 0x1002;

    fn window(core: &Core, handle: u64) -> WindowCandidate {
        core.candidates()
            .into_iter()
            .find(|c| c.handle == handle)
            .expect("mock provides this window")
    }

    /// Mirrors what the picker does, without `confirm_window`'s disk write.
    fn core_with_confirmed_video_window() -> Core {
        let platform = MockPlatform::new();
        let monitors = platform.monitors();
        let candidate = platform
            .candidate_windows()
            .into_iter()
            .find(|c| c.handle == VIDEO_WINDOW)
            .expect("mock provides a video window");

        let mut config = Config::default();
        config.zoom_window = Some(WindowIdentity::from_candidate(&candidate));
        config.set_target(&monitors[1]);

        let mut core = Core::new(Box::new(platform), config);
        core.confirmed_handle = Some(VIDEO_WINDOW);
        core
    }

    #[test]
    fn send_fills_the_target_monitor() {
        let mut core = core_with_confirmed_video_window();
        assert!(core.send().is_ok());
        assert_eq!(
            window(&core, VIDEO_WINDOW).bounds,
            Bounds::new(2560, 0, 1920, 1080)
        );
    }

    #[test]
    fn send_moves_the_picked_window_not_its_identical_twin() {
        let mut core = core_with_confirmed_video_window();
        let main_before = window(&core, MAIN_WINDOW).bounds;

        core.send().unwrap();

        assert_eq!(
            window(&core, MAIN_WINDOW).bounds,
            main_before,
            "the main meeting window must not be touched"
        );
    }

    #[test]
    fn retrieve_restores_the_pre_send_bounds() {
        let mut core = core_with_confirmed_video_window();
        let before = window(&core, VIDEO_WINDOW).bounds;

        core.send().unwrap();
        core.retrieve().unwrap();

        assert_eq!(before, window(&core, VIDEO_WINDOW).bounds);
    }

    #[test]
    fn sending_twice_does_not_lose_the_original_position() {
        let mut core = core_with_confirmed_video_window();
        let original = window(&core, VIDEO_WINDOW).bounds;

        core.send().unwrap();
        core.send().unwrap();
        core.retrieve().unwrap();

        assert_eq!(window(&core, VIDEO_WINDOW).bounds, original);
    }

    #[test]
    fn retrieve_clears_the_restore_point() {
        let mut core = core_with_confirmed_video_window();
        core.send().unwrap();
        core.retrieve().unwrap();

        assert!(!core.can_retrieve(), "a consumed restore point must not linger");
        assert!(core.retrieve().is_err());
    }

    #[test]
    fn a_send_after_a_retrieve_captures_a_fresh_restore_point() {
        let mut core = core_with_confirmed_video_window();
        core.send().unwrap();
        core.retrieve().unwrap();

        let moved_by_hand = Bounds::new(300, 300, 640, 360);
        core.platform
            .place_window(VIDEO_WINDOW, Placement { bounds: moved_by_hand, borderless: false })
            .unwrap();

        core.send().unwrap();
        core.retrieve().unwrap();

        assert_eq!(window(&core, VIDEO_WINDOW).bounds, moved_by_hand);
    }

    #[test]
    fn a_failed_send_leaves_no_restore_point() {
        let mut core = core_with_confirmed_video_window();
        core.platform.as_mock().unwrap().set_zoom_present(false);

        assert!(core.send().is_err());
        assert!(
            !core.can_retrieve(),
            "Retrieve must not be offered after a Send that never moved anything"
        );
    }

    #[test]
    fn a_minimized_window_is_restored_before_its_position_is_captured() {
        let mut core = core_with_confirmed_video_window();
        let original = window(&core, VIDEO_WINDOW).bounds;

        core.platform.as_mock().unwrap().minimize(VIDEO_WINDOW);
        core.send().unwrap();
        core.retrieve().unwrap();

        // Without un-minimising first, the captured bounds would be the
        // off-screen coordinates Windows reports for iconic windows.
        assert_eq!(window(&core, VIDEO_WINDOW).bounds, original);
    }

    #[test]
    fn minimized_windows_are_kept_out_of_the_picker() {
        let core = core_with_confirmed_video_window();
        core.platform.as_mock().unwrap().minimize(VIDEO_WINDOW);

        assert!(core.candidates().iter().all(|c| c.handle != VIDEO_WINDOW));
    }

    #[test]
    fn a_closed_zoom_says_so_instead_of_blaming_the_window() {
        let mut core = core_with_confirmed_video_window();
        core.confirmed_handle = None;
        core.platform.as_mock().unwrap().set_zoom_present(false);

        let failure = core.send().unwrap_err();
        assert!(
            failure.message.contains("does not appear to be running"),
            "got: {}",
            failure.message
        );
        assert!(
            !failure.needs_selection,
            "there is nothing to pick when Zoom is closed"
        );
    }

    #[test]
    fn ambiguity_asks_the_ui_to_open_the_picker() {
        let mut core = core_with_confirmed_video_window();
        core.confirmed_handle = None;

        let failure = core.send().unwrap_err();
        assert!(failure.needs_selection, "got: {}", failure.message);
    }

    #[test]
    fn a_disconnected_monitor_does_not_ask_for_a_window() {
        let mut core = core_with_confirmed_video_window();
        core.config.set_target(&MonitorInfo {
            id: r"\\.\DISPLAY9".into(),
            bounds: Bounds::new(9000, 0, 800, 600),
            work_area: Bounds::new(9000, 0, 800, 600),
            is_primary: false,
        });

        let failure = core.send().unwrap_err();
        assert!(!failure.needs_selection, "the window is fine, the monitor is not");
    }

    #[test]
    fn retrieve_is_unavailable_before_any_send() {
        let mut core = core_with_confirmed_video_window();
        assert!(!core.can_retrieve());
        assert!(core.retrieve().is_err());
    }

    #[test]
    fn can_retrieve_only_after_a_successful_send() {
        let mut core = core_with_confirmed_video_window();
        core.send().unwrap();
        assert!(core.can_retrieve());
    }

    #[test]
    fn send_without_a_confirmed_window_asks_the_user_to_select_one() {
        let platform = MockPlatform::new();
        let monitors = platform.monitors();
        let mut config = Config::default();
        config.set_target(&monitors[1]);

        let mut core = Core::new(Box::new(platform), config);
        let failure = core.send().unwrap_err();
        assert!(failure.needs_selection, "got: {failure}");
    }

    /// The regression for the ambiguity failure seen on Windows: two Zoom
    /// windows identical in process, class and title. Description alone cannot
    /// separate them, so the remembered handle has to.
    #[test]
    fn identical_windows_are_separated_by_the_remembered_handle() {
        let mut core = core_with_confirmed_video_window();
        assert!(core.send().is_ok());
        assert_eq!(
            window(&core, VIDEO_WINDOW).bounds,
            Bounds::new(2560, 0, 1920, 1080)
        );
    }

    /// Without a remembered handle — a restart, say — identical windows are
    /// genuinely indistinguishable, and guessing could full-screen the main
    /// meeting window mid-broadcast.
    #[test]
    fn identical_windows_without_a_handle_ask_for_reselection() {
        let mut core = core_with_confirmed_video_window();
        core.confirmed_handle = None;

        let failure = core.send().unwrap_err();
        assert!(failure.needs_selection, "got: {failure}");
    }

    /// A handle that still exists but now belongs to something else must not be
    /// trusted; the OS reissues handles after a window dies.
    #[test]
    fn a_handle_pointing_at_a_different_window_is_rejected() {
        let mut core = core_with_confirmed_video_window();
        core.confirmed_handle = Some(0x2001); // the Chrome window

        let failure = core.send().unwrap_err();
        assert!(failure.needs_selection, "got: {failure}");
    }

    #[test]
    fn a_dead_handle_falls_back_to_matching_by_description() {
        let mut core = core_with_confirmed_video_window();
        core.confirmed_handle = Some(0xDEAD);

        // The mock's two Zoom windows are identical, so the fallback correctly
        // refuses rather than guessing.
        let failure = core.send().unwrap_err();
        assert!(failure.needs_selection, "got: {failure}");
    }

    #[test]
    fn send_without_the_target_monitor_connected_is_an_error() {
        let platform = MockPlatform::new();
        let candidate = platform
            .candidate_windows()
            .into_iter()
            .find(|c| c.handle == VIDEO_WINDOW)
            .unwrap();

        let mut config = Config::default();
        config.zoom_window = Some(WindowIdentity::from_candidate(&candidate));
        config.set_target(&MonitorInfo {
            id: r"\\.\DISPLAY9".into(),
            bounds: Bounds::new(9000, 0, 800, 600),
            work_area: Bounds::new(9000, 0, 800, 600),
            is_primary: false,
        });

        let mut core = Core::new(Box::new(platform), config);
        core.confirmed_handle = Some(VIDEO_WINDOW);
        let failure = core.send().unwrap_err();
        assert!(failure.message.contains("not connected"), "got: {failure}");
    }

    #[test]
    fn zoom_like_windows_sort_ahead_of_everything_else() {
        let core = Core::new(Box::new(MockPlatform::new()), Config::default());
        let candidates = core.candidates();
        let first_non_zoom = candidates.iter().position(|c| !c.likely_zoom).unwrap();
        assert!(candidates[..first_non_zoom].iter().all(|c| c.likely_zoom));
    }
}
