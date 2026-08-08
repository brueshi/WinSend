//! Send and Retrieve, independent of any UI.
//!
//! Every operation re-resolves the target window from scratch. Caching a handle
//! across button presses would be faster and wrong: Zoom can close and reopen
//! the video window between presses, and the stale handle might by then belong
//! to something else entirely.

use crate::config::Config;
use crate::identity::{matches_structurally, resolve, Resolution, WindowIdentity};
use crate::platform::{Bounds, MonitorInfo, Platform, WindowCandidate};

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
    /// near the top without anything being hidden.
    pub fn candidates(&self) -> Vec<WindowCandidate> {
        let mut candidates = self.platform.candidate_windows();
        candidates.sort_by_key(|c| (!c.likely_zoom, c.process_name.to_lowercase(), c.handle));
        candidates
    }

    pub fn confirm_window(&mut self, candidate: &WindowCandidate) -> Result<String, String> {
        self.config.zoom_window = Some(WindowIdentity::from_candidate(candidate));
        self.confirmed_handle = Some(candidate.handle);
        self.config.save()?;
        Ok(format!("Confirmed \"{}\"", candidate.title))
    }

    pub fn set_target_monitor(&mut self, monitor: &MonitorInfo) -> Result<String, String> {
        self.config.set_target(monitor);
        self.config.save()?;
        Ok(format!("Target set to {}", monitor.label()))
    }

    pub fn set_borderless(&mut self, borderless: bool) -> Result<(), String> {
        self.config.borderless = borderless;
        self.config.save()
    }

    /// Locate the confirmed window right now, or explain what the user must do.
    fn locate(&self) -> Result<u64, String> {
        let identity = self
            .config
            .zoom_window
            .as_ref()
            .ok_or("No Zoom window confirmed yet. Use Select Zoom Window.")?;

        let candidates = self.platform.candidate_windows();

        // The window the user clicked, if it is still around and still matches
        // what they picked. This is what makes Zoom's identical main and video
        // windows separable at all, and it is checked rather than assumed.
        if let Some(handle) = self.confirmed_handle {
            if candidates
                .iter()
                .any(|c| c.handle == handle && matches_structurally(identity, c))
            {
                return Ok(handle);
            }
        }

        match resolve(identity, &candidates) {
            Resolution::Found(handle) => Ok(handle),
            Resolution::Ambiguous(handles) => Err(format!(
                "{} Zoom windows look identical, so the right one cannot be told apart. Use Select Zoom Window to pick it again.",
                handles.len()
            )),
            Resolution::NotFound => Err(
                "Could not find the confirmed Zoom window. Re-run Select Zoom Window.".to_string(),
            ),
        }
    }

    pub fn send(&mut self) -> Result<String, String> {
        let handle = self.locate()?;

        let monitors = self.platform.monitors();
        let monitor = self
            .config
            .resolve_monitor(&monitors)
            .ok_or("Target monitor is not connected. Pick one in Settings.")?;
        let destination = monitor.bounds;
        let label = monitor.label();

        // Capture before moving, so Retrieve has somewhere to go back to.
        let original = self
            .platform
            .window_bounds(handle)
            .map_err(|e| format!("Could not read the window's position: {e}"))?;

        self.platform
            .set_window_bounds(handle, destination, self.config.borderless)
            .map_err(|e| format!("Could not move the window: {e}"))?;

        self.saved_bounds = Some(original);
        Ok(format!("Sent to {label}"))
    }

    pub fn retrieve(&mut self) -> Result<String, String> {
        let bounds = self
            .saved_bounds
            .ok_or("Nothing has been sent yet, so there is no position to restore.")?;

        let handle = self.locate()?;

        self.platform
            .set_window_bounds(handle, bounds, false)
            .map_err(|e| format!("Could not restore the window: {e}"))?;

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
        let error = core.send().unwrap_err();
        assert!(error.contains("Select Zoom Window"), "got: {error}");
    }

    #[test]
    fn send_prompts_to_reconfirm_when_the_window_vanished() {
        let mut core = core_with_confirmed_video_window();
        core.platform.as_mock().unwrap().set_zoom_present(false);

        let error = core.send().unwrap_err();
        assert!(error.contains("Re-run Select Zoom Window"), "got: {error}");
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

        let error = core.send().unwrap_err();
        assert!(error.contains("Select Zoom Window"), "got: {error}");
    }

    /// A handle that still exists but now belongs to something else must not be
    /// trusted; the OS reissues handles after a window dies.
    #[test]
    fn a_handle_pointing_at_a_different_window_is_rejected() {
        let mut core = core_with_confirmed_video_window();
        core.confirmed_handle = Some(0x2001); // the Chrome window

        let error = core.send().unwrap_err();
        assert!(error.contains("Select Zoom Window"), "got: {error}");
    }

    #[test]
    fn a_dead_handle_falls_back_to_matching_by_description() {
        let mut core = core_with_confirmed_video_window();
        core.confirmed_handle = Some(0xDEAD);

        // The mock's two Zoom windows are identical, so the fallback correctly
        // refuses rather than guessing.
        let error = core.send().unwrap_err();
        assert!(error.contains("Select Zoom Window"), "got: {error}");
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
        let error = core.send().unwrap_err();
        assert!(error.contains("not connected"), "got: {error}");
    }

    #[test]
    fn zoom_like_windows_sort_ahead_of_everything_else() {
        let core = Core::new(Box::new(MockPlatform::new()), Config::default());
        let candidates = core.candidates();
        let first_non_zoom = candidates.iter().position(|c| !c.likely_zoom).unwrap();
        assert!(candidates[..first_non_zoom].iter().all(|c| c.likely_zoom));
    }
}
