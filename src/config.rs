//! Persisted settings: which monitor to send to, and which window to send.
//!
//! A corrupt or missing config is never an error worth surfacing — it just
//! means "not configured yet". Refusing to start because of a bad JSON file
//! would be the wrong failure mode for a tool used live.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::identity::WindowIdentity;
use crate::platform::{Bounds, MonitorInfo};

/// Device names can shuffle when displays are replugged, so the geometry is
/// stored alongside as a fallback discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetMonitor {
    pub id: String,
    pub bounds: Bounds,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub target_monitor: Option<TargetMonitor>,
    pub zoom_window: Option<WindowIdentity>,
    /// Strip the window frame so it fills the monitor edge to edge. Whether
    /// this is needed depends on how Zoom frames its video window, which we
    /// cannot know until it is tested against a real session.
    pub borderless: bool,
}

impl Config {
    /// Pick the configured monitor out of what is currently connected.
    ///
    /// Falls back to geometry when the device name has changed, then gives up
    /// rather than silently retargeting a different display.
    pub fn resolve_monitor<'a>(&self, monitors: &'a [MonitorInfo]) -> Option<&'a MonitorInfo> {
        let target = self.target_monitor.as_ref()?;
        monitors
            .iter()
            .find(|m| m.id == target.id)
            .or_else(|| monitors.iter().find(|m| m.bounds == target.bounds))
    }

    pub fn set_target(&mut self, monitor: &MonitorInfo) {
        self.target_monitor = Some(TargetMonitor {
            id: monitor.id.clone(),
            bounds: monitor.bounds,
        });
    }

    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path().ok_or("could not determine a config directory")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| e.to_string())
    }
}

pub fn config_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var("APPDATA").ok()?)
    } else {
        // macOS, for developing against the mock platform.
        PathBuf::from(std::env::var("HOME").ok()?).join("Library/Application Support")
    };
    Some(base.join("WinSend").join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, x: i32, primary: bool) -> MonitorInfo {
        MonitorInfo {
            id: id.to_string(),
            bounds: Bounds::new(x, 0, 1920, 1080),
            work_area: Bounds::new(x, 0, 1920, 1040),
            is_primary: primary,
        }
    }

    fn configured_for(m: &MonitorInfo) -> Config {
        let mut config = Config::default();
        config.set_target(m);
        config
    }

    #[test]
    fn resolves_by_device_name() {
        let monitors = vec![monitor(r"\\.\DISPLAY1", 0, true), monitor(r"\\.\DISPLAY2", 1920, false)];
        let config = configured_for(&monitors[1]);
        assert_eq!(config.resolve_monitor(&monitors).unwrap().id, r"\\.\DISPLAY2");
    }

    #[test]
    fn falls_back_to_geometry_when_the_name_changed() {
        let original = monitor(r"\\.\DISPLAY2", 1920, false);
        let config = configured_for(&original);
        // Same physical layout, renamed after a replug.
        let monitors = vec![monitor(r"\\.\DISPLAY1", 0, true), monitor(r"\\.\DISPLAY3", 1920, false)];
        assert_eq!(config.resolve_monitor(&monitors).unwrap().id, r"\\.\DISPLAY3");
    }

    #[test]
    fn returns_none_when_the_monitor_is_gone() {
        let config = configured_for(&monitor(r"\\.\DISPLAY2", 1920, false));
        let monitors = vec![monitor(r"\\.\DISPLAY1", 0, true)];
        assert!(config.resolve_monitor(&monitors).is_none());
    }

    #[test]
    fn unconfigured_resolves_to_none() {
        let monitors = vec![monitor(r"\\.\DISPLAY1", 0, true)];
        assert!(Config::default().resolve_monitor(&monitors).is_none());
    }

    #[test]
    fn corrupt_config_deserialises_to_default() {
        assert!(serde_json::from_str::<Config>("{ not json").is_err());
        // Partial configs must still load, which is what serde(default) buys.
        let partial: Config = serde_json::from_str(r#"{"borderless": true}"#).unwrap();
        assert!(partial.borderless);
        assert!(partial.target_monitor.is_none());
    }
}
