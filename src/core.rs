//! Send and Retrieve, independent of any UI.
//!
//! Every operation re-resolves the target window from scratch. Caching a handle
//! across button presses would be faster and wrong: Zoom can close and reopen
//! the video window between presses, and the stale handle might by then belong
//! to something else entirely.

use crate::config::Config;
use crate::identity::{resolve, Resolution, WindowIdentity};
use crate::platform::{Bounds, MonitorInfo, Platform, WindowCandidate};

pub struct Core {
    pub platform: Box<dyn Platform>,
    pub config: Config,
    /// Where the window sat before the last Send. Session-scoped by design:
    /// restoring to bounds captured in some previous run of the app would be
    /// restoring to a layout that no longer exists.
    saved_bounds: Option<Bounds>,
}

impl Core {
    pub fn new(platform: Box<dyn Platform>, config: Config) -> Self {
        Self { platform, config, saved_bounds: None }
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

        match resolve(identity, &self.platform.candidate_windows()) {
            Resolution::Found(handle) => Ok(handle),
            Resolution::Ambiguous(handles) => Err(format!(
                "{} windows match the confirmed one. Re-run Select Zoom Window to pick the right one.",
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

    fn core_with_confirmed_video_window() -> Core {
        let platform = MockPlatform::new();
        let monitors = platform.monitors();
        let candidate = platform
            .candidate_windows()
            .into_iter()
            .find(|c| c.title == "Zoom Workplace")
            .expect("mock provides a video window");

        let mut config = Config::default();
        config.zoom_window = Some(WindowIdentity::from_candidate(&candidate));
        config.set_target(&monitors[1]);

        Core::new(Box::new(platform), config)
    }

    #[test]
    fn send_fills_the_target_monitor() {
        let mut core = core_with_confirmed_video_window();
        assert!(core.send().is_ok());

        let moved = core
            .candidates()
            .into_iter()
            .find(|c| c.title == "Zoom Workplace")
            .unwrap();
        assert_eq!(moved.bounds, Bounds::new(2560, 0, 1920, 1080));
    }

    #[test]
    fn retrieve_restores_the_pre_send_bounds() {
        let mut core = core_with_confirmed_video_window();
        let before = core
            .candidates()
            .into_iter()
            .find(|c| c.title == "Zoom Workplace")
            .unwrap()
            .bounds;

        core.send().unwrap();
        core.retrieve().unwrap();

        let after = core
            .candidates()
            .into_iter()
            .find(|c| c.title == "Zoom Workplace")
            .unwrap()
            .bounds;
        assert_eq!(before, after);
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
        let platform = MockPlatform::new();
        let monitors = platform.monitors();
        let candidate = platform
            .candidate_windows()
            .into_iter()
            .find(|c| c.title == "Zoom Workplace")
            .unwrap();
        platform.set_zoom_present(false);

        let mut config = Config::default();
        config.zoom_window = Some(WindowIdentity::from_candidate(&candidate));
        config.set_target(&monitors[1]);

        let mut core = Core::new(Box::new(platform), config);
        let error = core.send().unwrap_err();
        assert!(error.contains("Re-run Select Zoom Window"), "got: {error}");
    }

    #[test]
    fn send_without_the_target_monitor_connected_is_an_error() {
        let platform = MockPlatform::new();
        let candidate = platform
            .candidate_windows()
            .into_iter()
            .find(|c| c.title == "Zoom Workplace")
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
