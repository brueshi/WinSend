//! Persisted settings: which monitor to send to, and which window to send.
//!
//! A corrupt or missing config is never an error worth surfacing — it just
//! means "not configured yet". Refusing to start because of a bad JSON file
//! would be the wrong failure mode for a tool used live.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hotkey::{Action, Hotkeys};
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
    /// Minimise everything else on the target monitor when sending, and put it
    /// back on Retrieve.
    ///
    /// Off by default because minimising can pause or throttle a media player,
    /// which the automatic behaviour goes out of its way to avoid. As a
    /// deliberate choice it is the blunt instrument that always works.
    pub clear_target: bool,
    #[serde(with = "hotkeys_as_text")]
    pub hotkeys: Hotkeys,
}

/// Hotkeys persist as the text they display as — `"Ctrl+Alt+F9"` — rather than
/// as a record of flags and a virtual-key code. The file is meant to be
/// readable, and `0x78` tells nobody anything.
///
/// Reading is deliberately lenient. A binding that no longer parses, or one
/// hand-edited to collide with the other action, drops just that binding;
/// returning an error would fail the whole file, and `Config::load` turns a
/// failed parse into a reset of every other setting.
mod hotkeys_as_text {
    use super::{Action, Deserialize, Hotkeys, Serialize};
    use serde::{Deserializer, Serializer};

    #[derive(Serialize, Deserialize, Default)]
    #[serde(default)]
    struct Stored {
        send: Option<String>,
        retrieve: Option<String>,
    }

    pub fn serialize<S: Serializer>(hotkeys: &Hotkeys, serializer: S) -> Result<S::Ok, S::Error> {
        Stored {
            send: hotkeys.send.map(|h| h.to_string()),
            retrieve: hotkeys.retrieve.map(|h| h.to_string()),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Hotkeys, D::Error> {
        let stored = Stored::deserialize(deserializer)?;

        // Routed through `set` rather than assigned, so the no-duplicates rule
        // holds however the file came to be written.
        let mut hotkeys = Hotkeys::default();
        let _ = hotkeys.set(Action::Send, stored.send.and_then(|t| t.parse().ok()));
        let _ = hotkeys.set(Action::Retrieve, stored.retrieve.and_then(|t| t.parse().ok()));
        Ok(hotkeys)
    }
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

/// Beside the config, so one folder holds everything worth looking at.
pub fn diagnostics_path() -> Option<PathBuf> {
    Some(config_path()?.with_file_name("diagnostics.txt"))
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
    fn hotkeys_persist_as_readable_text() {
        let mut config = Config::default();
        config
            .hotkeys
            .set(Action::Send, Some("Ctrl+Alt+F9".parse().unwrap()))
            .unwrap();

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains(r#""send":"Ctrl+Alt+F9""#), "got: {json}");

        let loaded: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.hotkeys, config.hotkeys);
    }

    /// A hand-edited typo must cost the user their hotkey, not their monitor.
    #[test]
    fn an_unparseable_hotkey_does_not_take_the_rest_of_the_config_with_it() {
        let loaded: Config = serde_json::from_str(
            r#"{"borderless": true, "hotkeys": {"send": "Ctrl+Alt+F99"}}"#,
        )
        .expect("the file must still load");

        assert_eq!(loaded.hotkeys.send, None, "the bad binding is dropped");
        assert!(loaded.borderless, "everything else survives");
    }

    #[test]
    fn a_hand_edited_collision_keeps_only_the_first_binding() {
        let loaded: Config = serde_json::from_str(
            r#"{"hotkeys": {"send": "Ctrl+Alt+F9", "retrieve": "Ctrl+Alt+F9"}}"#,
        )
        .unwrap();

        assert_eq!(loaded.hotkeys.send, Some("Ctrl+Alt+F9".parse().unwrap()));
        assert_eq!(loaded.hotkeys.retrieve, None);
    }

    #[test]
    fn clearing_the_target_is_off_unless_asked_for() {
        assert!(!Config::default().clear_target);
        let loaded: Config = serde_json::from_str(r#"{"borderless": true}"#).unwrap();
        assert!(!loaded.clear_target, "an existing config must not gain it");
    }

    #[test]
    fn a_config_written_before_hotkeys_existed_still_loads() {
        let loaded: Config = serde_json::from_str(r#"{"borderless": true}"#).unwrap();
        assert_eq!(loaded.hotkeys, Hotkeys::default());
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
