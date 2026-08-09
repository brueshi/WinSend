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

/// Window classes belonging to the shell rather than to any application.
///
/// These span whole displays without being anything a person would call a
/// window, and they became visible to this code the moment untitled windows
/// started being enumerated. Minimising one is pointless; hiding one takes the
/// desktop apart and makes Windows reshuffle everything else, which is not a
/// thing to do while a broadcast is running.
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "SysShadow",
    "Windows.UI.Core.CoreWindow",
    "ForegroundStaging",
    "MultitaskingViewFrame",
    "XamlExplorerHostIslandWindow",
    "Static",
    "Button",
];

/// Whether a window is something an application owns and a person could
/// reasonably expect to be moved out of the way.
fn is_movable(window: &WindowCandidate) -> bool {
    !window.own_process
        && !window.minimized
        && !window.cloaked
        && window.bounds.width > 0
        && window.bounds.height > 0
        && !SHELL_CLASSES
            .iter()
            .any(|shell| window.class_name.eq_ignore_ascii_case(shell))
}

/// How much of the target monitor a window must cover before clearing the
/// monitor will minimise it. Only used for the explicit setting, where the
/// question really is about the display rather than about one window.
const COVERING: f32 = 0.7;

/// How much of the sent window something must obscure before it counts as
/// being in the way.
///
/// Measured against the sent window rather than the monitor. Covering most of
/// a display was only ever a proxy, and a poor one: a window taking up half the
/// screen in front of the video feed is in the way whatever fraction of the
/// monitor it happens to occupy.
const OBSCURING: f32 = 0.15;

/// What was done to a window that was blocking the target monitor, so it can be
/// put back exactly as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Displaced {
    handle: u64,
    /// Which band it came from, so it goes back into that one and not the
    /// always-on-top band it never belonged to.
    was_topmost: bool,
    /// Dropped to the back of the stacking order.
    demoted: bool,
    /// Minimised, because demoting alone did not move it.
    minimized: bool,
    /// Taken off screen, because it would not minimise either.
    hidden: bool,
    /// Give it the foreground again when putting it back. A window that
    /// minimised itself on losing focus needs focus to come back, which is
    /// what clicking it in the taskbar does by hand.
    refocus: bool,
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
    /// Windows moved aside by the last Send, in the order they were moved.
    /// Session-scoped for the same reason as `saved_bounds`: putting back a
    /// window from a previous run would be restoring a layout that is gone.
    displaced: Vec<Displaced>,
}

impl Core {
    pub fn new(platform: Box<dyn Platform>, config: Config) -> Self {
        Self {
            platform,
            config,
            saved_bounds: None,
            confirmed_handle: None,
            displaced: Vec::new(),
        }
    }

    /// Every window covering `monitor`, front-first, whatever its depth.
    ///
    /// The blunt list. Used only when the user has asked for the monitor to be
    /// cleared, where "is it actually in front" stops being the question.
    fn covering(&self, monitor: Bounds, sent: u64) -> Vec<(u64, bool)> {
        let mut windows: Vec<WindowCandidate> = self
            .platform
            .candidate_windows()
            .into_iter()
            .filter(|window| {
                window.handle != sent
                    && is_movable(window)
                    && window.bounds.coverage_of(monitor) >= COVERING
            })
            .collect();
        windows.sort_by_key(|window| window.z_order);
        windows
            .into_iter()
            .map(|window| (window.handle, window.topmost))
            .collect()
    }

    /// Windows actually in front of the sent window and obscuring it.
    ///
    /// Both halves are measured rather than inferred. Earlier attempts guessed
    /// at the mechanism — that raising the sent window would be enough, then
    /// that anything in the way would be marked always-on-top — and both were
    /// wrong about how a full-screen player behaves. Depth comes from the
    /// stacking order and overlap comes from the two rectangles, so neither
    /// depends on a theory about the other application.
    ///
    /// Returns them front-first, so the one most in the way is dealt with
    /// first and the order can be reversed to put them back.
    fn blocking(&self, sent: u64) -> Vec<(u64, bool)> {
        let windows = self.platform.candidate_windows();

        let Some(target) = windows.iter().find(|window| window.handle == sent) else {
            return Vec::new();
        };
        let (depth, area) = (target.z_order, target.bounds);

        let mut blocking: Vec<&WindowCandidate> = windows
            .iter()
            .filter(|window| {
                window.handle != sent
                    && is_movable(window)
                    && window.z_order < depth
                    && window.bounds.coverage_of(area) >= OBSCURING
            })
            .collect();
        blocking.sort_by_key(|window| window.z_order);
        blocking
            .into_iter()
            .map(|window| (window.handle, window.topmost))
            .collect()
    }

    /// Move whatever is covering the target monitor out of the way.
    ///
    /// Demoting is tried first because it leaves the other application running
    /// and unaware. A full-screen media player that re-asserts always-on-top
    /// the moment it is demoted will still be in the way on the second look,
    /// and only minimising will shift it.
    /// Returns how many windows are still in front once it has finished, which
    /// is zero unless something is refusing to move.
    fn clear_the_way(&mut self, monitor: Bounds, sent: u64) -> usize {
        let mut displaced: Vec<Displaced> = Vec::new();

        // Asked for explicitly, so it is blunt on purpose: everything covering
        // the monitor is minimised whether or not it is currently in front.
        // Deciding what is in the way is the part that keeps being wrong, and
        // this setting exists to not have to decide.
        if self.config.clear_target {
            for (handle, was_topmost) in self.covering(monitor, sent) {
                if self.platform.minimize(handle).is_ok() {
                    displaced.push(Displaced {
                        handle,
                        was_topmost,
                        demoted: false,
                        minimized: true,
                        hidden: false,
                        refocus: false,
                    });
                }
            }

            // A borderless full-screen popup often has no minimise behaviour,
            // so the call above can report success and change nothing. Whatever
            // is still covering the monitor is taken off screen instead.
            for (handle, was_topmost) in self.covering(monitor, sent) {
                if self.platform.hide(handle).is_err() {
                    continue;
                }
                match displaced.iter_mut().find(|d| d.handle == handle) {
                    Some(already) => already.hidden = true,
                    None => displaced.push(Displaced {
                        handle,
                        was_topmost,
                        demoted: false,
                        minimized: false,
                        hidden: true,
                        refocus: false,
                    }),
                }
            }

            for entry in displaced {
                self.record_displaced(entry);
            }
            return self.covering(monitor, sent).len();
        }

        for (handle, was_topmost) in self.blocking(sent) {
            if self.platform.demote(handle).is_ok() {
                displaced.push(Displaced {
                    handle,
                    was_topmost,
                    demoted: true,
                    minimized: false,
                    hidden: false,
                    refocus: false,
                });
            }
        }

        // Anything still in front after being pushed to the back is holding
        // itself there, and only minimising will move it.
        for (handle, was_topmost) in self.blocking(sent) {
            if self.platform.minimize(handle).is_err() {
                continue;
            }
            match displaced.iter_mut().find(|d| d.handle == handle) {
                Some(already) => already.minimized = true,
                None => displaced.push(Displaced {
                    handle,
                    was_topmost,
                    demoted: false,
                    minimized: true,
                    hidden: false,
                    refocus: false,
                }),
            }
        }

        for entry in displaced {
            self.record_displaced(entry);
        }
        self.blocking(sent).len()
    }

    /// Record a window as moved aside, unless it already is.
    ///
    /// Pressing Send twice must not lose track of what the first press moved,
    /// which would leave it stranded with nothing to put it back.
    fn record_displaced(&mut self, entry: Displaced) {
        if self.displaced.iter().any(|held| held.handle == entry.handle) {
            return;
        }
        self.displaced.push(entry);
    }

    /// Handles of everything currently out of the way.
    ///
    /// Minimised or cloaked, because applications leave the screen in more than
    /// one way. A packaged application suspends rather than minimises when it
    /// loses the display, and its window goes cloaked with the minimised flag
    /// never being set — so looking only for minimised windows misses exactly
    /// the full-screen players this exists to notice.
    fn stowed_now(&self) -> Vec<u64> {
        self.platform
            .candidate_windows()
            .into_iter()
            .filter(|window| window.minimized || window.cloaked)
            .map(|window| window.handle)
            .collect()
    }

    /// Put back everything moved aside, in reverse order so the window that was
    /// on top ends up on top again.
    fn put_back_displaced(&mut self) {
        for window in std::mem::take(&mut self.displaced).into_iter().rev() {
            if window.hidden {
                let _ = self.platform.show(window.handle);
            }
            if window.minimized {
                let _ = self.platform.unminimize(window.handle);
            }
            if window.demoted {
                let _ = self.platform.raise(window.handle, window.was_topmost);
            }
            if window.refocus {
                let _ = self.platform.activate(window.handle);
            }
        }
    }

    /// Everything this code can see, in the terms it reasons about.
    ///
    /// Exists because five attempts at getting the sent window in front of
    /// full-screen media were each built on a guess about what was there. A
    /// separate diagnostic tool answers a slightly different question; this one
    /// reports the same list, the same filters and the same verdicts that Send
    /// actually acts on, so a disagreement can be settled instead of theorised
    /// about.
    pub fn diagnostics(&self) -> String {
        use std::fmt::Write;

        let mut out = String::new();
        let monitors = self.platform.monitors();
        let target = self.config.resolve_monitor(&monitors).map(|m| m.bounds);

        let _ = writeln!(out, "WinSend diagnostics");
        let _ = writeln!(out, "clear_target setting : {}", self.config.clear_target);
        let _ = writeln!(out, "borderless setting   : {}", self.config.borderless);
        for note in self.platform.diagnostic_notes() {
            let _ = writeln!(out, "{note}");
        }
        let _ = writeln!(out);

        let _ = writeln!(out, "MONITORS");
        for monitor in &monitors {
            let chosen = if Some(monitor.bounds) == target { " <- TARGET" } else { "" };
            let _ = writeln!(out, "  {} {}{}", monitor.id, monitor.label(), chosen);
        }
        if target.is_none() {
            let _ = writeln!(out, "  (no target monitor resolved)");
        }
        let _ = writeln!(out);

        let _ = writeln!(out, "CONFIRMED ZOOM WINDOW");
        match &self.config.zoom_window {
            Some(identity) => {
                let _ = writeln!(
                    out,
                    "  {} / {} / {:?}",
                    identity.process_name, identity.class_name, identity.title
                );
            }
            None => {
                let _ = writeln!(out, "  none");
            }
        }
        let located = self.locate();
        let _ = match &located {
            Ok(window) => writeln!(
                out,
                "  located now: handle 0x{:X} at {},{} {}x{}{}",
                window.handle,
                window.bounds.x,
                window.bounds.y,
                window.bounds.width,
                window.bounds.height,
                match target {
                    Some(bounds) if window.bounds == bounds => "  (filling the target)",
                    Some(_) => "  (NOT on the target)",
                    None => "",
                }
            ),
            Err(failure) => writeln!(out, "  located now: NO ({})", failure.message),
        };
        let _ = writeln!(out, "  restore point held: {}", self.can_retrieve());
        let _ = writeln!(out);

        // The verdicts, computed exactly as Send computes them.
        let sent = located.as_ref().map(|w| w.handle).unwrap_or(0);
        let covering: Vec<u64> = target
            .map(|bounds| self.covering(bounds, sent).into_iter().map(|(h, _)| h).collect())
            .unwrap_or_default();
        let blocking: Vec<u64> = self.blocking(sent).into_iter().map(|(h, _)| h).collect();

        let _ = writeln!(
            out,
            "VERDICT: {} window(s) obscure the Zoom window from in front (>= {:.0}% of it)",
            blocking.len(),
            OBSCURING * 100.0
        );
        let _ = writeln!(
            out,
            "         {} window(s) cover the target monitor (>= {:.0}% of it), which is what",
            covering.len(),
            COVERING * 100.0
        );
        let _ = writeln!(out, "         the clear-the-monitor setting would minimise");
        let _ = writeln!(out);

        let _ = writeln!(out, "WINDOWS, FRONT TO BACK");
        let _ = writeln!(
            out,
            "  {:>3} {:<5} {:<5} {:<5} {:<5} {:<5} {:>7} {:>7}  {:<18} {:<26} {:<24} {}",
            "Z", "TOP", "MIN", "CLOAK", "OWN", "MOVE", "OF-WIN", "OF-MON", "PROCESS", "CLASS",
            "BOUNDS", "TITLE"
        );
        let zoom_area = located.as_ref().ok().map(|window| window.bounds);
        for window in self.platform.candidate_windows() {
            let of_monitor = target.map(|b| window.bounds.coverage_of(b)).unwrap_or(0.0);
            let of_window = zoom_area.map(|b| window.bounds.coverage_of(b)).unwrap_or(0.0);
            let mark = if blocking.contains(&window.handle) {
                "BLOCKING"
            } else if covering.contains(&window.handle) {
                "covering"
            } else if window.handle == sent {
                "<- THE ZOOM WINDOW"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "  {:>3} {:<5} {:<5} {:<5} {:<5} {:<5} {:>6.0}% {:>6.0}%  {:<18} {:<26} {:<24} {:?} {}",
                window.z_order,
                if window.topmost { "yes" } else { "-" },
                if window.minimized { "yes" } else { "-" },
                if window.cloaked { "yes" } else { "-" },
                if window.own_process { "yes" } else { "-" },
                if is_movable(&window) { "yes" } else { "-" },
                of_window * 100.0,
                of_monitor * 100.0,
                window.process_name,
                window.class_name,
                format!(
                    "{},{} {}x{}",
                    window.bounds.x, window.bounds.y, window.bounds.width, window.bounds.height
                ),
                window.title,
                mark,
            );
        }
        out
    }

    pub fn can_retrieve(&self) -> bool {
        self.saved_bounds.is_some()
    }

    pub fn monitors(&self) -> Vec<MonitorInfo> {
        self.platform.monitors()
    }

    /// Windows a person could actually recognise and confirm.
    ///
    /// Untitled windows are excluded here rather than during enumeration.
    /// They matter enormously for working out what is covering a monitor — a
    /// full-screen video output window typically has no title — but they
    /// cannot be picked from a list or matched against a saved identity.
    fn identifiable(&self) -> Vec<WindowCandidate> {
        self.platform
            .candidate_windows()
            .into_iter()
            .filter(|window| {
                !window.title.is_empty() && !window.own_process && !window.cloaked
            })
            .collect()
    }

    /// Picker contents, with the Zoom-ish windows first so the likely target is
    /// near the top without anything being hidden. Minimised windows are left
    /// out: they cannot be identified visually and their bounds are nonsense.
    pub fn candidates(&self) -> Vec<WindowCandidate> {
        let mut candidates: Vec<WindowCandidate> = self
            .identifiable()
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

    /// Write the report next to the config and say where it went.
    pub fn save_diagnostics(&self) -> Result<String, Failure> {
        let path = crate::config::diagnostics_path()
            .ok_or_else(|| Failure::plain("Could not work out where to write the report."))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Failure::plain(format!("Could not create the folder: {e}")))?;
        }
        std::fs::write(&path, self.diagnostics())
            .map_err(|e| Failure::plain(format!("Could not write the report: {e}")))?;
        Ok(format!("Diagnostics written to {}", path.display()))
    }

    pub fn set_clear_target(&mut self, clear_target: bool) -> Result<(), String> {
        self.config.clear_target = clear_target;
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

        let candidates = self.identifiable();

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

        // Focus first, geometry last. Taking the foreground is the one lever
        // that reaches a full-screen exclusive window — it is managed outside
        // the stacking order, never appears in the window list, and gives way
        // only when something else is activated, which is exactly what
        // clicking another window does by hand. Doing it after positioning
        // meant anything the application re-arranged on being focused happened
        // after the size had been set, and undid it.
        let stowed_before = self.stowed_now();
        let _ = self.platform.activate(window.handle);

        // A full-screen window gives up the display by itself rather than being
        // pushed aside, and that is the only trace it leaves: it was not in the
        // window list at all, or not stowed, and now it is both. Without
        // noticing, Retrieve has nothing to put back and it stays in the
        // taskbar until someone clicks it.
        for handle in self.stowed_now() {
            if handle != window.handle && !stowed_before.contains(&handle) {
                self.record_displaced(Displaced {
                    handle,
                    was_topmost: false,
                    demoted: false,
                    minimized: true,
                    hidden: false,
                    refocus: true,
                });
            }
        }

        self.platform
            .place_window(
                window.handle,
                Placement {
                    bounds: destination,
                    borderless: self.config.borderless,
                    // The window is being put on a monitor that may already
                    // have something full-screen on it.
                    topmost: true,
                },
            )
            .map_err(|e| Failure::plain(format!("Could not move the window: {e}")))?;

        // Whether the window actually ended up filling the display. An
        // application that resizes itself afterwards, or coordinates scaled on
        // a display the process was told the wrong DPI for, both show up here,
        // and neither should be discovered by squinting at the screen.
        let landed = self.platform.window_bounds(window.handle).ok();

        // Only after the move has succeeded. Pushing another application's
        // window aside for a Send that then failed would be interference with
        // nothing to show for it.
        let still_in_front = self.clear_the_way(destination, window.handle);

        // Commit the restore point only once the move has actually succeeded,
        // so a failed Send does not leave Retrieve pointing somewhere wrong.
        if let Some(bounds) = captured {
            self.saved_bounds = Some(bounds);
        }

        // Said plainly rather than silently tolerated. A Send that appears to
        // do nothing, with no explanation, is what took three attempts to get
        // to the bottom of.
        if still_in_front > 0 {
            return Ok(format!(
                "Sent to {label}, but {still_in_front} window(s) will not move out of the way"
            ));
        }
        if let Some(landed) = landed.filter(|landed| *landed != destination) {
            return Ok(format!(
                "Sent to {label}, but it settled at {}x{} rather than {}x{}",
                landed.width, landed.height, destination.width, destination.height
            ));
        }
        Ok(format!("Sent to {label}"))
    }

    pub fn retrieve(&mut self) -> Result<String, Failure> {
        let bounds = self.saved_bounds.ok_or_else(|| {
            Failure::plain("Nothing has been sent yet, so there is no position to restore.")
        })?;

        let window = self.locate()?;
        self.ensure_visible(&window)?;

        // Before the sent window moves, not after.
        //
        // Both orders leave the same desktop a moment later, so this looks
        // arbitrary and is not. Moving the sent window first uncovers a bare
        // target monitor, and the player then snaps back on top of it: two
        // transitions where the user asked for one. Restoring first puts the
        // player back underneath a window that is still covering it, so the
        // move reveals the thing that is meant to be there.
        //
        // It also stays correct without the sent window being on top, since
        // everything here is restored into the band it came from — an ordinary
        // window goes to the front of the ordinary band, which is still behind
        // the sent window while that one is held topmost.
        self.put_back_displaced();

        self.platform
            .place_window(window.handle, Placement { bounds, borderless: false, topmost: false })
            .map_err(|e| Failure::plain(format!("Could not restore the window: {e}")))?;

        // Consumed: the next Send captures a fresh restore point rather than
        // reusing a position that may no longer mean anything.
        self.saved_bounds = None;

        Ok("Restored to its original position".to_string())
    }
}

impl Drop for Core {
    /// Put back anything moved aside, even if the app is quitting.
    ///
    /// Mirrors `Win32Platform::drop`: a media player left minimised, or knocked
    /// out of always-on-top, is a change to someone else's application that
    /// would otherwise outlive this process.
    fn drop(&mut self) {
        self.put_back_displaced();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{Call, MockPlatform};

    /// The two Zoom windows in the mock are identical in every respect the
    /// config records, so tests address them by handle.
    const MAIN_WINDOW: u64 = 0x1001;
    const VIDEO_WINDOW: u64 = 0x1002;
    /// The full-screen media player on the target display.
    const MEDIA_WINDOW: u64 = 0x3001;
    const SHELL_WINDOW: u64 = 0x4001;
    const PARTIAL_WINDOW: u64 = 0x4003;
    /// A media player owning the second display exclusively.
    const EXCLUSIVE_WINDOW: u64 = 0x5001;
    const OWN_WINDOW: u64 = 0x4002;

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

    /// The regression for sending onto a monitor that already has something
    /// full-screen on it: without raising the window, it lands behind the
    /// media and looks like nothing happened.
    #[test]
    fn send_raises_the_window_above_whatever_is_already_there() {
        let mut core = core_with_confirmed_video_window();
        core.send().unwrap();

        assert!(
            core.platform.as_mock().unwrap().is_topmost(VIDEO_WINDOW),
            "a window sent behind full-screen media is a window that did not move"
        );
    }

    /// Raising it is only acceptable because it is undone. Otherwise Zoom
    /// stays pinned over everything long after it was retrieved.
    #[test]
    fn retrieve_puts_the_window_back_in_the_ordinary_order() {
        let mut core = core_with_confirmed_video_window();
        core.send().unwrap();
        core.retrieve().unwrap();

        assert!(!core.platform.as_mock().unwrap().is_topmost(VIDEO_WINDOW));
    }

    /// The regression this whole mechanism exists for: something already
    /// filling the target display and sitting in front of what gets sent
    /// there, which looks exactly like Send doing nothing.
    #[test]
    fn whatever_is_in_front_on_the_target_is_pushed_behind() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.set_topmost(MEDIA_WINDOW, true).unwrap();
        mock.bring_to_front(MEDIA_WINDOW);

        let message = core.send().unwrap();

        assert!(
            !message.contains("will not move"),
            "nothing should be left in front: {message}"
        );
        let mock = core.platform.as_mock().unwrap();
        assert!(!mock.is_topmost(MEDIA_WINDOW), "it must lose always-on-top");
        assert!(
            !mock.is_minimized(MEDIA_WINDOW),
            "pushing it behind was enough, so it must not be minimised as well"
        );
    }

    /// Demoting leaves the player running, which minimising cannot promise, so
    /// minimising is only for a window that will not stay put.
    #[test]
    fn a_window_that_refuses_to_move_is_minimised_instead() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.bring_to_front(MEDIA_WINDOW);
        mock.set_sticky(MEDIA_WINDOW);

        let message = core.send().unwrap();

        assert!(!message.contains("will not move"), "got: {message}");
        assert!(core.platform.as_mock().unwrap().is_minimized(MEDIA_WINDOW));
    }

    #[test]
    fn retrieve_puts_the_player_back_in_front() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.set_topmost(MEDIA_WINDOW, true).unwrap();
        mock.bring_to_front(MEDIA_WINDOW);

        core.send().unwrap();
        core.retrieve().unwrap();

        assert!(
            core.platform.as_mock().unwrap().is_topmost(MEDIA_WINDOW),
            "it was always-on-top before, so it must be again"
        );
    }

    /// Both orders end with the same desktop, so nothing about the final state
    /// can tell them apart. The sequence is the behaviour here: restoring after
    /// the move uncovers a bare monitor and then snaps the player onto it.
    #[test]
    fn retrieve_puts_the_player_back_before_it_uncovers_the_monitor() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.set_topmost(MEDIA_WINDOW, true).unwrap();
        mock.bring_to_front(MEDIA_WINDOW);

        core.send().unwrap();
        core.retrieve().unwrap();

        let calls = core.platform.as_mock().unwrap().calls();
        let restored = calls
            .iter()
            .rposition(|call| *call == Call::Raised(MEDIA_WINDOW))
            .expect("the player must be put back");
        let uncovered = calls
            .iter()
            .rposition(|call| *call == Call::Placed(VIDEO_WINDOW))
            .expect("the sent window must be moved back");

        assert!(
            restored < uncovered,
            "the player has to be there before the window moves off it, got: {calls:?}"
        );
    }

    /// The band it came from, not the band that happens to be convenient.
    /// Restoring an ordinary window as always-on-top would pin it over
    /// everything the user owns.
    #[test]
    fn an_ordinary_window_is_not_restored_as_always_on_top() {
        let mut core = core_with_confirmed_video_window();
        core.platform.as_mock().unwrap().bring_to_front(MEDIA_WINDOW);

        core.send().unwrap();
        core.retrieve().unwrap();

        assert!(!core.platform.as_mock().unwrap().is_topmost(MEDIA_WINDOW));
    }

    /// Hiding the desktop host takes the desktop apart and makes Windows
    /// reshuffle every other window, which is not something to discover during
    /// a broadcast. These became reachable the moment untitled windows started
    /// being enumerated.
    #[test]
    fn shell_windows_are_never_moved_however_much_they_cover() {
        let mut core = core_with_confirmed_video_window();
        core.config.clear_target = true;
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(SHELL_WINDOW, "explorer.exe", "WorkerW", "", Bounds::new(2560, 0, 1920, 1080));
        mock.bring_to_front(SHELL_WINDOW);

        core.send().unwrap();

        let mock = core.platform.as_mock().unwrap();
        assert!(!mock.is_minimized(SHELL_WINDOW), "the desktop host must be left alone");
        assert!(!mock.is_hidden(SHELL_WINDOW));
    }

    #[test]
    fn our_own_window_is_never_moved() {
        let mut core = core_with_confirmed_video_window();
        core.config.clear_target = true;
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(OWN_WINDOW, "winsend.exe", "Winit", "WinSend", Bounds::new(2560, 0, 1920, 1080));
        mock.set_own_process(OWN_WINDOW);
        mock.bring_to_front(OWN_WINDOW);

        core.send().unwrap();

        assert!(!core.platform.as_mock().unwrap().is_minimized(OWN_WINDOW));
    }

    /// The report is what settles a disagreement about what is on screen, so
    /// it has to name the things the disagreement is about.
    #[test]
    fn the_diagnostics_report_names_what_it_acts_on() {
        let core = core_with_confirmed_video_window();
        core.platform.as_mock().unwrap().bring_to_front(MEDIA_WINDOW);

        let report = core.diagnostics();

        assert!(report.contains("TARGET"), "the chosen monitor must be marked");
        assert!(report.contains("VERDICT"), "the counts it acted on must be stated");
        assert!(report.contains("vlc.exe"), "every window must be listed");
        assert!(report.contains("WINDOWS, FRONT TO BACK"));
    }

    /// The regression that made four attempts at the z-order problem all fail
    /// the same silent way: the window that needed moving has no title, so
    /// anything filtering on having one never saw it.
    #[test]
    fn an_untitled_full_screen_window_is_still_found_and_moved() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        assert_eq!(
            mock.title_of(MEDIA_WINDOW),
            "",
            "the fixture must model the untitled output window"
        );
        mock.bring_to_front(MEDIA_WINDOW);

        core.send().unwrap();

        assert!(
            !core.platform.as_mock().unwrap().is_in_front_of(MEDIA_WINDOW, VIDEO_WINDOW),
            "an untitled window is still a window that is in the way"
        );
    }

    /// It has to be enumerated, and it still cannot be offered to a person who
    /// would have nothing to recognise it by.
    #[test]
    fn an_untitled_window_stays_out_of_the_picker() {
        let core = core_with_confirmed_video_window();
        assert!(core.candidates().iter().all(|c| c.handle != MEDIA_WINDOW));
    }

    /// A borderless popup can report a successful minimise and not move.
    #[test]
    fn clearing_the_target_hides_what_will_not_minimise() {
        let mut core = core_with_confirmed_video_window();
        core.config.clear_target = true;
        core.platform.as_mock().unwrap().set_unminimisable(MEDIA_WINDOW);

        let message = core.send().unwrap();

        assert!(!message.contains("will not move"), "got: {message}");
        assert!(core.platform.as_mock().unwrap().is_hidden(MEDIA_WINDOW));
    }

    #[test]
    fn a_hidden_window_comes_back_on_retrieve() {
        let mut core = core_with_confirmed_video_window();
        core.config.clear_target = true;
        core.platform.as_mock().unwrap().set_unminimisable(MEDIA_WINDOW);

        core.send().unwrap();
        core.retrieve().unwrap();

        assert!(!core.platform.as_mock().unwrap().is_hidden(MEDIA_WINDOW));
    }

    /// The escape hatch, for when working out what is in the way keeps being
    /// wrong: clear the monitor and stop deciding.
    #[test]
    fn clearing_the_target_minimises_even_what_is_already_behind() {
        let mut core = core_with_confirmed_video_window();
        core.config.clear_target = true;

        core.send().unwrap();

        assert!(
            core.platform.as_mock().unwrap().is_minimized(MEDIA_WINDOW),
            "the setting exists precisely so depth stops mattering"
        );
    }

    #[test]
    fn clearing_the_target_puts_everything_back_on_retrieve() {
        let mut core = core_with_confirmed_video_window();
        core.config.clear_target = true;

        core.send().unwrap();
        core.retrieve().unwrap();

        assert!(!core.platform.as_mock().unwrap().is_minimized(MEDIA_WINDOW));
    }

    #[test]
    fn clearing_the_target_leaves_other_monitors_alone() {
        let mut core = core_with_confirmed_video_window();
        core.config.clear_target = true;

        core.send().unwrap();

        // The main Zoom window fills part of DISPLAY1, not the target.
        assert!(!core.platform.as_mock().unwrap().is_minimized(MAIN_WINDOW));
    }

    /// Already behind the sent window, so moving it would be interfering with
    /// the desktop for nothing.
    #[test]
    fn a_window_already_behind_is_left_alone() {
        let mut core = core_with_confirmed_video_window();

        core.send().unwrap();

        let mock = core.platform.as_mock().unwrap();
        assert!(!mock.is_minimized(MEDIA_WINDOW));
    }

    /// The whole shape of the real problem, end to end: a window that owns the
    /// display exclusively, is absent from the window list entirely, minimises
    /// itself when something else takes focus, and has to be given focus back.
    #[test]
    fn a_full_screen_exclusive_window_is_noticed_and_put_back() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(
            EXCLUSIVE_WINDOW,
            "wmplayer.exe",
            "WMPlayerApp",
            "",
            Bounds::new(2560, 0, 1920, 1080),
        );
        mock.set_exclusive(EXCLUSIVE_WINDOW);

        // Nothing can push aside a window that is not there to be found. This
        // is why six attempts at rearranging the stacking order changed
        // nothing at all.
        assert!(
            core.candidates().iter().all(|c| c.handle != EXCLUSIVE_WINDOW),
            "it is invisible to enumeration while it owns the screen"
        );

        core.send().unwrap();
        assert!(
            core.platform.as_mock().unwrap().is_minimized(EXCLUSIVE_WINDOW),
            "taking the foreground is what moves it"
        );

        core.retrieve().unwrap();
        let mock = core.platform.as_mock().unwrap();
        assert!(!mock.is_minimized(EXCLUSIVE_WINDOW), "it must come back");
        assert!(
            mock.was_activated(EXCLUSIVE_WINDOW),
            "it needs the foreground back, which is what clicking the taskbar does"
        );
    }

    /// A packaged media player suspends rather than minimising, so its window
    /// goes cloaked and the minimised flag is never set. Looking only for
    /// newly minimised windows leaves it stranded in the taskbar.
    #[test]
    fn a_player_that_suspends_rather_than_minimising_is_still_put_back() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(
            EXCLUSIVE_WINDOW,
            "explorer.exe",
            "ApplicationFrameWindow",
            "",
            Bounds::new(2560, 0, 1920, 1080),
        );
        mock.set_exclusive(EXCLUSIVE_WINDOW);
        mock.set_suspends(EXCLUSIVE_WINDOW);

        core.send().unwrap();
        assert!(
            core.platform.as_mock().unwrap().is_cloaked(EXCLUSIVE_WINDOW),
            "it suspends rather than minimising"
        );
        assert!(
            !core.platform.as_mock().unwrap().is_minimized(EXCLUSIVE_WINDOW),
            "and the minimised flag is never set, which is the trap"
        );

        core.retrieve().unwrap();

        let mock = core.platform.as_mock().unwrap();
        assert!(!mock.is_cloaked(EXCLUSIVE_WINDOW), "it must come back");
        assert!(mock.was_activated(EXCLUSIVE_WINDOW));
    }

    /// Pressing Send twice must not lose what the first press moved aside.
    #[test]
    fn a_second_send_does_not_forget_the_first() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(
            EXCLUSIVE_WINDOW,
            "wmplayer.exe",
            "WMPlayerApp",
            "",
            Bounds::new(2560, 0, 1920, 1080),
        );
        mock.set_exclusive(EXCLUSIVE_WINDOW);

        core.send().unwrap();
        core.send().unwrap();
        core.retrieve().unwrap();

        assert!(!core.platform.as_mock().unwrap().is_minimized(EXCLUSIVE_WINDOW));
    }

    /// The only lever that reaches a full-screen exclusive window, which never
    /// appears in the window list at all and so cannot be pushed aside.
    #[test]
    fn send_takes_the_foreground() {
        let mut core = core_with_confirmed_video_window();

        core.send().unwrap();

        assert!(
            core.platform.as_mock().unwrap().was_activated(VIDEO_WINDOW),
            "an exclusive full-screen window gives way to focus and nothing else"
        );
    }

    /// A window covering a third of the video feed is in the way, even though
    /// it is nowhere near filling the monitor. Measuring against the display
    /// was a proxy, and it let exactly this case through.
    #[test]
    fn something_obscuring_part_of_the_sent_window_still_counts() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        // Half the target display, so nowhere near the covering threshold.
        mock.add_window(
            PARTIAL_WINDOW,
            "chrome.exe",
            "Chrome_WidgetWin_1",
            "Something in the way",
            Bounds::new(2560, 0, 960, 1080),
        );
        mock.bring_to_front(PARTIAL_WINDOW);

        core.send().unwrap();

        assert!(
            !core.platform.as_mock().unwrap().is_in_front_of(PARTIAL_WINDOW, VIDEO_WINDOW),
            "half the video feed is obscured, so it has to move"
        );
    }

    /// Something in front but barely touching it is not worth disturbing the
    /// desktop over.
    #[test]
    fn a_window_barely_touching_the_sent_one_is_left_alone() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(
            PARTIAL_WINDOW,
            "chrome.exe",
            "Chrome_WidgetWin_1",
            "Just a corner",
            Bounds::new(2560, 0, 160, 120),
        );
        mock.bring_to_front(PARTIAL_WINDOW);

        core.send().unwrap();

        assert!(core
            .platform
            .as_mock()
            .unwrap()
            .is_in_front_of(PARTIAL_WINDOW, VIDEO_WINDOW));
    }

    /// Windows on other displays are none of our business, however big.
    #[test]
    fn a_fullscreen_player_on_a_different_monitor_is_left_alone() {
        let mut core = core_with_confirmed_video_window();
        core.platform
            .place_window(
                MEDIA_WINDOW,
                Placement {
                    bounds: Bounds::new(0, 0, 2560, 1440),
                    borderless: false,
                    topmost: true,
                },
            )
            .unwrap();
        core.platform.as_mock().unwrap().bring_to_front(MEDIA_WINDOW);

        core.send().unwrap();

        let mock = core.platform.as_mock().unwrap();
        assert!(mock.is_topmost(MEDIA_WINDOW), "it is not on the target monitor");
        assert!(!mock.is_minimized(MEDIA_WINDOW));
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
            .place_window(VIDEO_WINDOW, Placement { bounds: moved_by_hand, borderless: false, topmost: false })
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

        core.platform.minimize(VIDEO_WINDOW).unwrap();
        core.send().unwrap();
        core.retrieve().unwrap();

        // Without un-minimising first, the captured bounds would be the
        // off-screen coordinates Windows reports for iconic windows.
        assert_eq!(window(&core, VIDEO_WINDOW).bounds, original);
    }

    #[test]
    fn minimized_windows_are_kept_out_of_the_picker() {
        let core = core_with_confirmed_video_window();
        core.platform.minimize(VIDEO_WINDOW).unwrap();

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
