//! Send and Retrieve, independent of any UI.
//!
//! Every operation re-locates the target window rather than assuming the last
//! one is still valid. The handle the user picked is remembered, because Zoom's
//! main and video windows are otherwise indistinguishable, but it is checked
//! against the live window list on each use: Zoom can close and reopen the
//! video window between presses, and a dead handle can be reissued by the OS to
//! something else entirely.

use crate::config::Config;
use crate::hotkey::{Action, Hotkey, KeyChord};
use crate::identity::{matches_structurally, resolve, Resolution, WindowIdentity};
use crate::platform::{
    Bounds, MonitorInfo, Placement, Platform, PlatformError, WindowCandidate,
};

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

/// How much of its monitor a returning player must cover to count as full
/// screen again. Just short of total, because a border-to-border window can
/// report a pixel or two of slack on a mixed-DPI desktop.
const FULLSCREEN: f32 = 0.98;

/// The full-screen toggle each player answers to.
///
/// A table because there is no other source: full screen is internal state
/// each application manages for itself, keyed to whatever shortcut it chose.
/// Overridden per process by `media_keys` in the config, and extended for
/// unknown players by `media_default_key` — which is unset by default, since
/// a guessed keystroke into an unknown application is typing into it.
const FULLSCREEN_KEYS: &[(&str, &str)] = &[
    ("vlc.exe", "F"),
    ("mpv.exe", "F"),
    ("wmplayer.exe", "Alt+Enter"),
    ("mpc-hc.exe", "Alt+Enter"),
    ("mpc-hc64.exe", "Alt+Enter"),
    ("PotPlayerMini64.exe", "Enter"),
    ("chrome.exe", "F11"),
    ("msedge.exe", "F11"),
    ("firefox.exe", "F11"),
];

/// The chord that toggles full screen for this process, if any is known.
///
/// A config entry that fails to parse yields no key rather than falling back
/// to the built-in table: an override the user wrote is an instruction, and
/// quietly substituting a different key for a typo would send the wrong
/// keystroke on purpose.
fn fullscreen_key_for(config: &Config, process: &str) -> Option<KeyChord> {
    let configured = config
        .media_keys
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(process))
        .map(|(_, chord)| chord.as_str());
    let builtin = FULLSCREEN_KEYS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(process))
        .map(|(_, chord)| *chord);
    configured
        .or(builtin)
        .or(config.media_default_key.as_deref())?
        .parse()
        .ok()
}

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
    /// The application's own full-screen toggle, if one is known.
    ///
    /// Resolved from the keymap at the moment the window is noticed rather
    /// than when it is put back, because that is when the window is reliably
    /// enumerable and its process name in hand. None means no key is known,
    /// which Retrieve reports honestly instead of guessing.
    fullscreen_key: Option<KeyChord>,
}

/// How far from where it was put a window may sit before that counts as
/// drift. Two pixels of slack for the rounding a scaling change introduces,
/// which is arithmetic rather than the application changing its mind.
const SETTLE_SLACK: i32 = 2;

/// How many times a placement is re-asserted before the drift is reported
/// instead.
///
/// The first placement can cross a scaling boundary, and that is the one that
/// gets undone: the application is told its DPI changed and resizes itself
/// afterwards. A re-assertion cannot cross anything — by then the window is
/// already on the destination display — so one is normally enough and two is
/// generous. It is a cap rather than a loop that runs until it wins, because
/// past this the application is not losing an argument, it is having one, and
/// a window flickering between two sizes on camera is worse than a window
/// that is the wrong size and said so.
const SETTLE_CORRECTIONS: u32 = 2;

/// Consecutive identical looks at a wrong rectangle before it is corrected.
/// A window caught mid-move has not drifted, and correcting one spends a
/// correction on nothing.
const SETTLE_STILL: u32 = 2;

/// Whether a window is close enough to where it was put.
fn settled_at(actual: Bounds, wanted: Bounds) -> bool {
    (actual.x - wanted.x).abs() <= SETTLE_SLACK
        && (actual.y - wanted.y).abs() <= SETTLE_SLACK
        && (actual.width - wanted.width).abs() <= SETTLE_SLACK
        && (actual.height - wanted.height).abs() <= SETTLE_SLACK
}

/// What is wrong with where a window ended up, in the terms that actually
/// differ. A window that is the right size in the wrong place should not be
/// reported by its size.
fn describe_drift(wanted: Bounds, actual: Bounds) -> String {
    let resized = (actual.width, actual.height) != (wanted.width, wanted.height);
    let moved = (actual.x, actual.y) != (wanted.x, wanted.y);
    let size = format!("{}x{} rather than {}x{}", actual.width, actual.height, wanted.width, wanted.height);
    let place = format!("{},{} rather than {},{}", actual.x, actual.y, wanted.x, wanted.y);
    match (resized, moved) {
        (true, true) => format!("{size}, at {place}"),
        (true, false) => size,
        _ => format!("at {place}"),
    }
}

/// A placement whose result is still being measured.
///
/// Placing a window is not over when `place_window` returns. A window moved
/// between displays of different scaling is told so afterwards, on the
/// application's own thread, and resizes itself then — by the scaling ratio,
/// which is how a window restored to 1280x800 comes back at 1920x1200 and
/// takes over the screen. Nothing measured inside the placement call can see
/// that, because it has not happened yet. So the placement is watched for a
/// beat and re-asserted if it did not hold.
struct Settle {
    handle: u64,
    /// How the eventual complaint starts: "Sent" or "Restored".
    what: &'static str,
    /// Re-asserted verbatim. The correction is the same request again, not a
    /// cleverer one: by the time it is made the window is already on the
    /// destination display, so there is no scaling boundary left to cross and
    /// nothing to compensate for.
    placement: Placement,
    /// The last rectangle seen, to tell a window that has stopped moving from
    /// one still on its way.
    last_seen: Option<Bounds>,
    still: u32,
    corrections: u32,
}

/// The last placement and what became of it, for the diagnostics.
///
/// Kept because the interesting question after a bad restore is not what the
/// desktop looks like now but what was asked for, what arrived, and how many
/// times it had to be asked — none of which survives anywhere else.
#[derive(Debug, Clone, Copy)]
struct PlacementRecord {
    what: &'static str,
    requested: Bounds,
    landed: Bounds,
    corrections: u32,
    settled: bool,
}

/// A player put back by Retrieve whose full-screen state is still being
/// watched.
///
/// The watching has to span frames: a suspended application takes time to
/// resume, and whether it restored its own full screen can only be measured
/// once it has. Each look is a measurement — never a guess — and the key is
/// pressed at most once.
struct MediaReentry {
    handle: u64,
    /// For the status messages. A handle tells the user nothing.
    process: String,
    chord: Option<KeyChord>,
    /// The key has been pressed; what remains is seeing whether it took.
    key_sent: bool,
    /// The window has been observed windowed once already. The key is only
    /// pressed on the second consecutive look, because a player enumerable
    /// for a single frame while still mid-restore would otherwise be toggled
    /// straight back out of the full screen it was entering.
    seen_windowed: bool,
}

/// Where one look at the watched players left things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaRestoreStep {
    /// Nothing is being watched.
    Idle,
    /// At least one player has not settled; look again next frame.
    Waiting,
    /// Every player accounted for, with what happened to each.
    Done(String),
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
    /// Players put back by the last Retrieve and still being watched for
    /// full-screen re-entry. Stepped by the UI clock, emptied by completion,
    /// timeout or the next Send.
    pending_reentry: Vec<MediaReentry>,
    /// Outcomes from players that finished while others were still being
    /// watched, held so the eventual report covers all of them.
    reentry_outcomes: Vec<String>,
    /// The placement still being measured, if there is one.
    settling: Option<Settle>,
    /// What the last placement asked for and got. Diagnostics only.
    last_placement: Option<PlacementRecord>,
}

impl Core {
    pub fn new(platform: Box<dyn Platform>, config: Config) -> Self {
        Self {
            platform,
            config,
            saved_bounds: None,
            confirmed_handle: None,
            displaced: Vec::new(),
            pending_reentry: Vec::new(),
            reentry_outcomes: Vec::new(),
            settling: None,
            last_placement: None,
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
                        fullscreen_key: None,
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
                        fullscreen_key: None,
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
                    fullscreen_key: None,
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
                    fullscreen_key: None,
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
        // One snapshot for the process names. The stowed windows are
        // enumerable right up until they are put back, which makes this the
        // last easy moment to learn what they are called.
        let candidates = self.platform.candidate_windows();
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

            // A window that gave up the display on its own gets watched back
            // to full screen, when the user has not turned that off. Queued
            // rather than acted on here: whether it needs the key at all is
            // only measurable once it has finished resuming, frames from now.
            if window.refocus && self.config.restore_fullscreen {
                let process = candidates
                    .iter()
                    .find(|c| c.handle == window.handle)
                    .map(|c| c.process_name.clone())
                    .unwrap_or_else(|| "the player".to_string());
                self.pending_reentry.push(MediaReentry {
                    handle: window.handle,
                    process,
                    chord: window.fullscreen_key,
                    key_sent: false,
                    seen_windowed: false,
                });
            }
        }
    }

    /// Whether any player is still being watched for full-screen re-entry.
    pub fn media_restore_pending(&self) -> bool {
        !self.pending_reentry.is_empty()
    }

    /// One look at every player still being watched, advancing each by what
    /// was measured. Driven by the UI clock after Retrieve has finished,
    /// because the answers change frame to frame as applications resume.
    ///
    /// Per player, in order of what a look can find: gone from the window
    /// list means exclusive full screen again (or closed) and nothing to do;
    /// still minimised or cloaked means still resuming, look again; filling
    /// its monitor means full screen without needing the key; visibly
    /// windowed twice in a row means the key, once. A refused press is a
    /// not-right-now — a held modifier, focus not settled — and is retried on
    /// the next look.
    pub fn media_restore_step(&mut self) -> MediaRestoreStep {
        if self.pending_reentry.is_empty() {
            return MediaRestoreStep::Idle;
        }

        let candidates = self.platform.candidate_windows();
        let monitors = self.platform.monitors();
        let mut remaining: Vec<MediaReentry> = Vec::new();

        // `key_sent` never sets without a chord, so the fallthrough covers
        // both the player that restored itself and the defensive impossible.
        let fullscreen_again = |entry: &MediaReentry| match (entry.key_sent, entry.chord) {
            (true, Some(chord)) => format!("Sent {chord} to {}", entry.process),
            _ => format!("{} is full screen again", entry.process),
        };

        for mut entry in std::mem::take(&mut self.pending_reentry) {
            let Some(candidate) = candidates.iter().find(|c| c.handle == entry.handle) else {
                // Absent from the list is what owning the screen exclusively
                // looks like — the same signature that identified the player
                // in the first place.
                self.reentry_outcomes.push(fullscreen_again(&entry));
                continue;
            };

            if candidate.minimized || candidate.cloaked {
                remaining.push(entry);
                continue;
            }

            let fills_its_monitor = monitors
                .iter()
                .find(|m| m.id == candidate.monitor_id)
                .map(|m| candidate.bounds.coverage_of(m.bounds) >= FULLSCREEN)
                .unwrap_or(false);
            if fills_its_monitor {
                self.reentry_outcomes.push(fullscreen_again(&entry));
                continue;
            }

            // Visibly windowed. The key was already pressed: give it time to
            // take, and let the timeout say so if it never does.
            if entry.key_sent {
                remaining.push(entry);
                continue;
            }

            let Some(chord) = entry.chord else {
                self.reentry_outcomes.push(format!(
                    "{} came back windowed. No full-screen key is known for it; \
                     add one to media_keys in the config file",
                    entry.process
                ));
                continue;
            };

            if !entry.seen_windowed {
                entry.seen_windowed = true;
                remaining.push(entry);
                continue;
            }

            match self.platform.send_key(entry.handle, chord) {
                Ok(()) => {
                    entry.key_sent = true;
                    remaining.push(entry);
                }
                Err(PlatformError::Denied(_)) => remaining.push(entry),
                Err(PlatformError::WindowGone) => {
                    self.reentry_outcomes.push(format!("{} is gone", entry.process));
                }
            }
        }

        self.pending_reentry = remaining;
        if self.pending_reentry.is_empty() {
            MediaRestoreStep::Done(std::mem::take(&mut self.reentry_outcomes).join("; "))
        } else {
            MediaRestoreStep::Waiting
        }
    }

    /// Stop watching, reporting where every player got to. The timeout path,
    /// and the honest one: a player that never resumed or ignored its key is
    /// named rather than quietly forgotten.
    pub fn cancel_media_restore(&mut self) -> Option<String> {
        let mut outcomes = std::mem::take(&mut self.reentry_outcomes);
        for entry in std::mem::take(&mut self.pending_reentry) {
            outcomes.push(if entry.key_sent {
                let key = entry
                    .chord
                    .map(|chord| chord.to_string())
                    .unwrap_or_else(|| "its key".to_string());
                format!("{} did not return to full screen after {key}", entry.process)
            } else {
                format!("{} did not come back in time", entry.process)
            });
        }
        if outcomes.is_empty() {
            None
        } else {
            Some(outcomes.join("; "))
        }
    }

    /// Begin measuring a placement that has just been made.
    ///
    /// One at a time by design: the only window this ever places is the Zoom
    /// window, so a second placement is not something to queue behind the
    /// first, it is the answer to what the first should have been.
    fn watch_placement(&mut self, handle: u64, what: &'static str, placement: Placement) {
        self.settling = Some(Settle {
            handle,
            what,
            placement,
            last_seen: None,
            still: 0,
            corrections: 0,
        });
        self.last_placement = Some(PlacementRecord {
            what,
            requested: placement.bounds,
            landed: placement.bounds,
            corrections: 0,
            settled: false,
        });
    }

    pub fn placement_settling(&self) -> bool {
        self.settling.is_some()
    }

    /// One look at the window that was last placed, and a correction if it has
    /// come to rest somewhere other than where it was put.
    ///
    /// Every branch is a measurement, the same rule the full-screen watch
    /// follows. Nothing here predicts what the application will do with a
    /// scaling change — it reads what it did.
    pub fn settle_look(&mut self) {
        let Some(mut settle) = self.settling.take() else {
            return;
        };
        let Ok(actual) = self.platform.window_bounds(settle.handle) else {
            // Closed while it was being watched. There is nothing left to
            // measure and nothing worth saying about a window that has gone.
            return;
        };
        if let Some(record) = self.last_placement.as_mut() {
            record.landed = actual;
        }

        if settled_at(actual, settle.placement.bounds) {
            settle.still = 0;
            settle.last_seen = Some(actual);
            self.settling = Some(settle);
            return;
        }

        settle.still = if settle.last_seen == Some(actual) { settle.still + 1 } else { 1 };
        settle.last_seen = Some(actual);

        if settle.still >= SETTLE_STILL && settle.corrections < SETTLE_CORRECTIONS {
            settle.corrections += 1;
            settle.still = 0;
            settle.last_seen = None;
            let _ = self.platform.place_window(settle.handle, settle.placement);
            if let Some(record) = self.last_placement.as_mut() {
                record.corrections = settle.corrections;
            }
        }

        self.settling = Some(settle);
    }

    /// Stop watching, and say what to tell the user — which is nothing at all
    /// unless the window is still somewhere it was not put.
    ///
    /// Silence on success is deliberate. "Sent" already said what happened,
    /// and a second line confirming that a window is the size it was asked to
    /// be is noise on a strip that has room for three messages.
    pub fn finish_settle(&mut self) -> Option<String> {
        let settle = self.settling.take()?;
        let wanted = settle.placement.bounds;
        let actual = self.platform.window_bounds(settle.handle).ok()?;
        let settled = settled_at(actual, wanted);
        if let Some(record) = self.last_placement.as_mut() {
            record.landed = actual;
            record.settled = settled;
        }
        if settled {
            return None;
        }
        Some(format!(
            "{}, but the window is holding {}",
            settle.what,
            describe_drift(wanted, actual)
        ))
    }

    /// Abandon the watch without a word.
    ///
    /// For the moments when the placement being watched has been superseded
    /// rather than failed: a Retrieve starting cancels the Send it is undoing,
    /// and re-asserting that Send's bounds part-way through the fade would put
    /// the window back on the display it is being taken off.
    fn stop_settling(&mut self) {
        self.settling = None;
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
            let _ = writeln!(
                out,
                "  {} {} — {} dpi ({}%){}",
                monitor.id,
                monitor.label(),
                monitor.dpi,
                monitor.scaling_percent(),
                chosen
            );
        }
        if target.is_none() {
            let _ = writeln!(out, "  (no target monitor resolved)");
        }
        // Said outright rather than left to be spotted by comparing two
        // numbers in a list. A window moved between displays of different
        // scaling is resized by the application afterwards, by that ratio, and
        // that is the difference between a restore that holds and one that
        // comes back filling the screen.
        if monitors.windows(2).any(|pair| pair[0].dpi != pair[1].dpi) {
            let _ = writeln!(
                out,
                "  NOTE: the displays are scaled differently, so every Send and Retrieve"
            );
            let _ = writeln!(
                out,
                "        crosses a scaling boundary and the window is resized by the ratio"
            );
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
                "  located now: handle 0x{:X} at {},{} {}x{} at {} dpi{}",
                window.handle,
                window.bounds.x,
                window.bounds.y,
                window.bounds.width,
                window.bounds.height,
                self.platform
                    .window_dpi(window.handle)
                    .map(|dpi| dpi.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
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

        // What the last placement asked for and what became of it. The one
        // question a screenshot of the desktop cannot answer after a window
        // has come back the wrong size.
        let _ = writeln!(out, "LAST PLACEMENT");
        match &self.last_placement {
            Some(record) => {
                let _ = writeln!(
                    out,
                    "  {}: asked {},{} {}x{}",
                    record.what,
                    record.requested.x,
                    record.requested.y,
                    record.requested.width,
                    record.requested.height
                );
                let _ = writeln!(
                    out,
                    "  landed {},{} {}x{} after {} correction(s), {}",
                    record.landed.x,
                    record.landed.y,
                    record.landed.width,
                    record.landed.height,
                    record.corrections,
                    if self.settling.is_some() {
                        "still settling"
                    } else if record.settled {
                        "settled"
                    } else {
                        "NOT where it was put"
                    }
                );
            }
            None => {
                let _ = writeln!(out, "  nothing placed this session");
            }
        }
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

    /// Picker contents for the media binding.
    ///
    /// Not `identifiable`: its title filter is the exact reason the player
    /// was invisible for six attempts — a full-screen video output window
    /// typically has no title at all. Untitled windows are offered here and
    /// labelled by their process instead, because the process and class are
    /// what the binding matches on anyway.
    pub fn media_candidates(&self) -> Vec<WindowCandidate> {
        let mut candidates: Vec<WindowCandidate> = self
            .platform
            .candidate_windows()
            .into_iter()
            .filter(|c| {
                !c.own_process
                    && !c.cloaked
                    && !c.minimized
                    && c.bounds.width > 0
                    && c.bounds.height > 0
            })
            .collect();
        candidates.sort_by_key(|c| (c.process_name.to_lowercase(), c.handle));
        candidates
    }

    pub fn confirm_media_window(&mut self, candidate: &WindowCandidate) -> Result<String, Failure> {
        self.config.media_window = Some(WindowIdentity::from_candidate(candidate));
        self.config.save()?;
        let name = if candidate.title.is_empty() {
            &candidate.process_name
        } else {
            &candidate.title
        };
        Ok(format!("Media window set to \"{name}\""))
    }

    pub fn clear_media_window(&mut self) -> Result<String, Failure> {
        self.config.media_window = None;
        self.config.save()?;
        Ok("Media window cleared".to_string())
    }

    /// Bring the bound media window back and watch it to full screen.
    ///
    /// The manual half of the full-screen restore, for the player the
    /// automatic watch cannot see: one that was already stowed before Send
    /// left no before-and-after difference to notice. Deliberately ignores
    /// `restore_fullscreen` — that setting governs what Retrieve does on its
    /// own, and pressing this hotkey is the explicit request the setting
    /// exists to distinguish from.
    pub fn restore_media(&mut self) -> Result<String, Failure> {
        let Some(identity) = self.config.media_window.clone() else {
            return Err(Failure::needs_selection("No media window has been selected yet"));
        };

        // Resolution runs against every other-process window, minimised and
        // cloaked included: the bound window being stowed is the whole reason
        // this action exists. The empty-title tie-break in `resolve` is what
        // separates an untitled output window from a titled main one sharing
        // its process and class.
        let candidates: Vec<WindowCandidate> = self
            .platform
            .candidate_windows()
            .into_iter()
            .filter(|c| !c.own_process)
            .collect();
        let handle = match resolve(&identity, &candidates) {
            Resolution::Found(handle) => handle,
            Resolution::Ambiguous(_) => {
                return Err(Failure::needs_selection(format!(
                    "More than one window looks like the media window ({})",
                    identity.process_name
                )));
            }
            Resolution::NotFound => {
                return Err(Failure::plain(format!(
                    "The media window ({}) is not open",
                    identity.process_name
                )));
            }
        };

        let process = candidates
            .iter()
            .find(|c| c.handle == handle)
            .map(|c| c.process_name.clone())
            .unwrap_or_else(|| identity.process_name.clone());

        let _ = self.platform.unminimize(handle);
        let _ = self.platform.activate(handle);

        // Pressing the hotkey twice must not queue the same player twice: the
        // second watch would send a second toggle at a window the first one
        // already restored.
        self.pending_reentry.retain(|entry| entry.handle != handle);
        self.pending_reentry.push(MediaReentry {
            handle,
            process: process.clone(),
            chord: fullscreen_key_for(&self.config, &process),
            key_sent: false,
            seen_windowed: false,
        });

        Ok(format!("Restoring {process}"))
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

    pub fn set_restore_fullscreen(&mut self, restore: bool) -> Result<(), String> {
        self.config.restore_fullscreen = restore;
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

    pub fn set_fade_on_retrieve(&mut self, fade: bool) -> Result<(), String> {
        self.config.fade_on_retrieve = fade;
        self.config.save()
    }

    /// Takes effect at the next launch, since the check only ever runs once
    /// per launch and this one has already had its.
    pub fn set_check_for_updates(&mut self, check: bool) -> Result<(), String> {
        self.config.check_for_updates = check;
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
        // A new Send supersedes any re-entry still being watched from the
        // last Retrieve: pressing a player back to full screen while a window
        // is being sent over it would be working against this very request.
        self.pending_reentry.clear();
        self.reentry_outcomes.clear();

        let stowed_before = self.stowed_now();
        let _ = self.platform.activate(window.handle);

        // A full-screen window gives up the display by itself rather than being
        // pushed aside, and that is the only trace it leaves: it was not in the
        // window list at all, or not stowed, and now it is both. Without
        // noticing, Retrieve has nothing to put back and it stays in the
        // taskbar until someone clicks it.
        //
        // The full candidate is walked rather than just the handle, because
        // this is the one moment the player is reliably enumerable with its
        // process name attached — while it owned the screen it was in no list
        // at all — and the process name is what keys the full-screen toggle
        // Retrieve will need.
        for candidate in self.platform.candidate_windows() {
            let newly_stowed = (candidate.minimized || candidate.cloaked)
                && candidate.handle != window.handle
                && !stowed_before.contains(&candidate.handle);
            if !newly_stowed {
                continue;
            }
            let fullscreen_key = fullscreen_key_for(&self.config, &candidate.process_name);
            self.record_displaced(Displaced {
                handle: candidate.handle,
                was_topmost: false,
                demoted: false,
                minimized: true,
                hidden: false,
                refocus: true,
                fullscreen_key,
            });
        }

        let placement = Placement {
            bounds: destination,
            borderless: self.config.borderless,
            // The window is being put on a monitor that may already have
            // something full-screen on it.
            topmost: true,
        };
        self.platform
            .place_window(window.handle, placement)
            .map_err(|e| Failure::plain(format!("Could not move the window: {e}")))?;

        // Whether the window actually ends up filling the display is not
        // knowable yet. Sending it to a display of a different scaling makes
        // the application resize itself once it has been told, which is after
        // this returns, so the answer is measured over the next second by the
        // settle watch rather than read once here and believed.
        self.watch_placement(window.handle, "Sent", placement);

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
        Ok(format!("Sent to {label}"))
    }

    /// Retrieve, as one synchronous step. What every caller wants unless it
    /// intends to animate the gap.
    pub fn retrieve(&mut self) -> Result<String, Failure> {
        let handle = self.begin_retrieve()?;
        self.finish_retrieve(handle)
    }

    /// The half of Retrieve that happens before the window moves, ending with
    /// the handle of the window that is about to.
    ///
    /// Split out so the interface can fade the window between the two halves
    /// while `Core` itself stays synchronous and holds no notion of an
    /// animation. Everything that can fail is on this side, so a fade never
    /// starts for a Retrieve that was going to be refused anyway.
    pub fn begin_retrieve(&mut self) -> Result<u64, Failure> {
        if self.saved_bounds.is_none() {
            return Err(Failure::plain(
                "Nothing has been sent yet, so there is no position to restore.",
            ));
        }

        let window = self.locate()?;
        self.ensure_visible(&window)?;

        // The Send being undone is no longer worth measuring, and a watch that
        // outlived it would re-assert the sent bounds part-way through the
        // fade — putting the window back on the display it is being taken off.
        self.stop_settling();

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

        Ok(window.handle)
    }

    /// The half that moves the window back, for the handle `begin_retrieve`
    /// returned.
    ///
    /// Takes the handle rather than locating the window again. A fade lasts a
    /// couple of hundred milliseconds and re-enumerating every window on the
    /// desktop to confirm what was found moments ago would be work for
    /// nothing; a window that closed in the gap fails the move instead, and
    /// says so.
    pub fn finish_retrieve(&mut self, handle: u64) -> Result<String, Failure> {
        let bounds = self.saved_bounds.ok_or_else(|| {
            Failure::plain("Nothing has been sent yet, so there is no position to restore.")
        })?;

        let placement = Placement { bounds, borderless: false, topmost: false };
        self.platform
            .place_window(handle, placement)
            .map_err(|e| Failure::plain(format!("Could not restore the window: {e}")))?;

        // The half of the restore that cannot be done synchronously. Coming
        // back from a larger display usually means coming back across a
        // scaling boundary, and the application resizes itself by that ratio
        // once it has been told — after this returns, and by enough to take
        // over the screen it was restored to.
        self.watch_placement(handle, "Restored", placement);

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

/// Fixtures shared with other modules' tests.
///
/// Lives here rather than in each test module because building a `Core` that
/// is ready to send needs `confirmed_handle`, which is private for good
/// reason: it is the one thing that separates Zoom's two identical windows and
/// nothing outside `Core` should be able to assert it.
#[cfg(test)]
pub mod test_support {
    use super::Core;
    use crate::config::Config;
    use crate::identity::WindowIdentity;
    use crate::mock::MockPlatform;
    use crate::platform::Platform;

    /// The one of Zoom's two identical windows that the user picked.
    pub const VIDEO_WINDOW: u64 = 0x1002;
    /// The full-screen media player on the target display.
    pub const MEDIA_WINDOW: u64 = 0x3001;

    /// Mirrors what the picker does, without `confirm_window`'s disk write.
    /// A test that saved would overwrite the developer's own settings.
    pub fn core_with_confirmed_video_window() -> Core {
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
}

#[cfg(test)]
mod tests {
    use super::test_support::{core_with_confirmed_video_window, MEDIA_WINDOW, VIDEO_WINDOW};
    use super::*;
    use crate::mock::{Call, MockPlatform};
    use crate::platform::BASE_DPI;

    /// The two Zoom windows in the mock are identical in every respect the
    /// config records, so tests address them by handle. `VIDEO_WINDOW` and
    /// `MEDIA_WINDOW` come from `test_support`, which other modules' tests
    /// share.
    const MAIN_WINDOW: u64 = 0x1001;
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

    /// The windowed bounds a resuming player is parked at in these tests:
    /// well on the target display, nowhere near filling it.
    fn windowed_on_display_two() -> Bounds {
        Bounds::new(2600, 100, 960, 540)
    }

    /// The unsolved half of the full-screen problem, end to end: a player
    /// that resumes windowed is measured to be windowed — twice, so a single
    /// mid-restore frame cannot mislead — and then pressed back to full
    /// screen with its own shortcut, exactly once, only after the sent
    /// window has been placed back.
    #[test]
    fn a_player_that_comes_back_windowed_is_pressed_back_to_full_screen() {
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
        mock.set_returns_windowed(EXCLUSIVE_WINDOW, windowed_on_display_two());

        core.send().unwrap();
        core.retrieve().unwrap();
        assert!(core.media_restore_pending());

        let chord: KeyChord = "Alt+Enter".parse().unwrap();
        assert_eq!(core.media_restore_step(), MediaRestoreStep::Waiting);
        let mock = core.platform.as_mock().unwrap();
        assert!(
            !mock.calls().contains(&Call::KeySent(EXCLUSIVE_WINDOW, chord)),
            "the first windowed look is the debounce and presses nothing"
        );

        assert_eq!(core.media_restore_step(), MediaRestoreStep::Waiting);
        let calls = core.platform.as_mock().unwrap().calls();
        let pressed = calls
            .iter()
            .position(|c| *c == Call::KeySent(EXCLUSIVE_WINDOW, chord))
            .expect("the second windowed look presses the key");
        let placed_back = calls
            .iter()
            .rposition(|c| *c == Call::Placed(VIDEO_WINDOW))
            .expect("retrieve places the window back");
        assert!(
            pressed > placed_back,
            "the key lands after the sent window is off the player, never into the reveal"
        );

        match core.media_restore_step() {
            MediaRestoreStep::Done(message) => {
                assert!(message.contains("Sent Alt+Enter to wmplayer.exe"), "got: {message}");
            }
            other => panic!("the player is exclusive again, so the watch is done: {other:?}"),
        }
        assert_eq!(
            core.platform
                .as_mock()
                .unwrap()
                .calls()
                .iter()
                .filter(|c| matches!(c, Call::KeySent(EXCLUSIVE_WINDOW, _)))
                .count(),
            1,
            "at most one press, however many looks it took"
        );
    }

    /// A player that puts its own full screen back on being refocused must
    /// not be toggled straight back out of it.
    #[test]
    fn a_player_that_restores_its_own_full_screen_is_left_alone() {
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
        core.retrieve().unwrap();

        match core.media_restore_step() {
            MediaRestoreStep::Done(message) => {
                assert!(message.contains("full screen again"), "got: {message}");
            }
            other => panic!("absent from the list means exclusive again: {other:?}"),
        }
        assert!(
            !core
                .platform
                .as_mock()
                .unwrap()
                .calls()
                .iter()
                .any(|c| matches!(c, Call::KeySent(..))),
            "measuring is what stops a key being sent at a window already full screen"
        );
    }

    /// A suspended application that never resumes gets waited on, not typed
    /// at, and the timeout names it rather than quietly giving up.
    #[test]
    fn a_player_that_never_resumes_is_reported_by_the_timeout() {
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
        mock.set_never_resumes(EXCLUSIVE_WINDOW);

        core.send().unwrap();
        core.retrieve().unwrap();

        for _ in 0..5 {
            assert_eq!(core.media_restore_step(), MediaRestoreStep::Waiting);
        }
        assert!(
            !core
                .platform
                .as_mock()
                .unwrap()
                .calls()
                .iter()
                .any(|c| matches!(c, Call::KeySent(..))),
            "a window still cloaked cannot take input, so none is sent"
        );

        let message = core.cancel_media_restore().expect("the timeout has something to say");
        assert!(message.contains("did not come back in time"), "got: {message}");
        assert!(!core.media_restore_pending(), "cancelling is final");
    }

    /// The setting is the off switch for the whole behaviour, not a filter on
    /// part of it.
    #[test]
    fn nothing_is_watched_when_the_setting_is_off() {
        let mut core = core_with_confirmed_video_window();
        core.config.restore_fullscreen = false;
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(
            EXCLUSIVE_WINDOW,
            "wmplayer.exe",
            "WMPlayerApp",
            "",
            Bounds::new(2560, 0, 1920, 1080),
        );
        mock.set_exclusive(EXCLUSIVE_WINDOW);
        mock.set_returns_windowed(EXCLUSIVE_WINDOW, windowed_on_display_two());

        core.send().unwrap();
        core.retrieve().unwrap();

        assert!(!core.media_restore_pending());
        assert_eq!(core.media_restore_step(), MediaRestoreStep::Idle);
        assert!(
            !core.platform.as_mock().unwrap().is_minimized(EXCLUSIVE_WINDOW),
            "the player is still put back; only the key press is off"
        );
    }

    /// No key is guessed for a player the table does not know. The report
    /// says how to teach it one instead.
    #[test]
    fn an_unknown_player_gets_no_guessed_key() {
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(
            EXCLUSIVE_WINDOW,
            "obscureplayer.exe",
            "ObscureClass",
            "",
            Bounds::new(2560, 0, 1920, 1080),
        );
        mock.set_exclusive(EXCLUSIVE_WINDOW);
        mock.set_returns_windowed(EXCLUSIVE_WINDOW, windowed_on_display_two());

        core.send().unwrap();
        core.retrieve().unwrap();

        match core.media_restore_step() {
            MediaRestoreStep::Done(message) => {
                assert!(message.contains("media_keys"), "the report teaches the fix: {message}");
            }
            other => panic!("nothing to wait for without a key: {other:?}"),
        }
        assert!(
            !core
                .platform
                .as_mock()
                .unwrap()
                .calls()
                .iter()
                .any(|c| matches!(c, Call::KeySent(..)))
        );
    }

    /// An override the user wrote wins over the built-in table.
    #[test]
    fn a_configured_key_beats_the_builtin_table() {
        let mut core = core_with_confirmed_video_window();
        core.config
            .media_keys
            .insert("wmplayer.exe".to_string(), "Ctrl+F".to_string());
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(
            EXCLUSIVE_WINDOW,
            "wmplayer.exe",
            "WMPlayerApp",
            "",
            Bounds::new(2560, 0, 1920, 1080),
        );
        mock.set_exclusive(EXCLUSIVE_WINDOW);
        mock.set_returns_windowed(EXCLUSIVE_WINDOW, windowed_on_display_two());

        core.send().unwrap();
        core.retrieve().unwrap();
        core.media_restore_step();
        core.media_restore_step();

        let chord: KeyChord = "Ctrl+F".parse().unwrap();
        assert!(
            core.platform
                .as_mock()
                .unwrap()
                .calls()
                .contains(&Call::KeySent(EXCLUSIVE_WINDOW, chord)),
            "the override is an instruction, not a suggestion"
        );
    }

    /// The configured default reaches players the table has never heard of.
    #[test]
    fn the_default_key_covers_an_unknown_player() {
        let mut core = core_with_confirmed_video_window();
        core.config.media_default_key = Some("Enter".to_string());
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(
            EXCLUSIVE_WINDOW,
            "obscureplayer.exe",
            "ObscureClass",
            "",
            Bounds::new(2560, 0, 1920, 1080),
        );
        mock.set_exclusive(EXCLUSIVE_WINDOW);
        mock.set_returns_windowed(EXCLUSIVE_WINDOW, windowed_on_display_two());

        core.send().unwrap();
        core.retrieve().unwrap();
        core.media_restore_step();
        core.media_restore_step();

        let chord: KeyChord = "Enter".parse().unwrap();
        assert!(
            core.platform
                .as_mock()
                .unwrap()
                .calls()
                .contains(&Call::KeySent(EXCLUSIVE_WINDOW, chord))
        );
    }

    /// Pressing Send again while a watch is running supersedes it: pressing
    /// the player back to full screen mid-Send would fight the Send.
    /// The mixed-scaling desktop this app is actually used on: the primary at
    /// 150%, the big second display at 100%. Every Send and every Retrieve
    /// crosses that boundary.
    fn mixed_scaling(core: &Core) {
        let mock = core.platform.as_mock().expect("the tests run on the mock");
        mock.set_monitor_dpi(r"\\.\DISPLAY1", 144);
        // The window starts on the primary, which is where a Retrieve has to
        // put it back.
        mock.set_bounds(VIDEO_WINDOW, Bounds::new(120, 80, 1280, 800));
    }

    /// Drive the watch the way the interface does: one look per tick, for the
    /// length of the watch window.
    fn settle(core: &mut Core) -> Option<String> {
        for _ in 0..(SETTLE_WATCH_LOOKS) {
            core.settle_look();
        }
        core.finish_settle()
    }

    /// 1200ms of watching at one look every 50ms, matching `app.rs`.
    const SETTLE_WATCH_LOOKS: usize = 24;

    #[test]
    fn a_window_that_shrinks_itself_on_arrival_is_pushed_back_out_to_fill_the_display() {
        let mut core = core_with_confirmed_video_window();
        mixed_scaling(&core);

        assert!(core.send().is_ok());
        // Measured the moment the move returns, the window looks right. It is
        // the next look that catches the application scaling itself down to
        // two thirds, because it was moved onto a display at 100% from one at
        // 150% and told so afterwards.
        assert_eq!(
            core.platform.window_bounds(VIDEO_WINDOW),
            Ok(Bounds::new(2560, 0, 1280, 720)),
            "the mock models the rescale as landing after the placement"
        );

        assert_eq!(settle(&mut core), None, "the watch has nothing to complain about");
        assert_eq!(
            core.platform.window_bounds(VIDEO_WINDOW),
            Ok(Bounds::new(2560, 0, 1920, 1080)),
            "and the window fills the target display"
        );
    }

    #[test]
    fn a_window_that_inflates_itself_on_the_way_back_is_put_back_to_the_saved_size() {
        let mut core = core_with_confirmed_video_window();
        mixed_scaling(&core);
        let before = core.platform.window_bounds(VIDEO_WINDOW).unwrap();

        assert!(core.send().is_ok());
        assert_eq!(settle(&mut core), None);

        assert!(core.retrieve().is_ok());
        // The failure this whole watch exists for: restored to 1280x800 and a
        // beat later holding 1920x1200, because coming back onto the 150%
        // display scales it by half again and takes over the screen.
        assert_eq!(
            core.platform.window_bounds(VIDEO_WINDOW),
            Ok(Bounds::new(120, 80, 1920, 1200))
        );

        assert_eq!(settle(&mut core), None, "the watch has nothing to complain about");
        assert_eq!(
            core.platform.window_bounds(VIDEO_WINDOW),
            Ok(before),
            "the window is back at the size it was sent from"
        );
    }

    #[test]
    fn a_correction_is_not_spent_on_a_window_that_is_still_moving() {
        let mut core = core_with_confirmed_video_window();
        mixed_scaling(&core);
        assert!(core.send().is_ok());

        let placements = |core: &Core| {
            core.platform
                .as_mock()
                .unwrap()
                .calls()
                .into_iter()
                .filter(|call| *call == Call::Placed(VIDEO_WINDOW))
                .count()
        };
        let after_send = placements(&core);

        // One look at a wrong rectangle is not evidence that the window has
        // come to rest at it.
        core.settle_look();
        assert_eq!(placements(&core), after_send, "nothing corrected on the first look");

        // Seeing the same wrong rectangle twice is.
        core.settle_look();
        assert_eq!(placements(&core), after_send + 1, "corrected once it held still");
    }

    #[test]
    fn a_window_that_will_not_hold_the_size_is_reported_rather_than_fought() {
        let mut core = core_with_confirmed_video_window();
        core.platform
            .as_mock()
            .unwrap()
            .set_rescales_itself(VIDEO_WINDOW, 1.5);

        assert!(core.send().is_ok());
        let complaint = settle(&mut core).expect("the watch says the window would not take it");
        assert!(complaint.starts_with("Sent, but the window is holding"), "got: {complaint}");
        assert!(complaint.contains("2880x1620 rather than 1920x1080"), "got: {complaint}");

        let placements = core
            .platform
            .as_mock()
            .unwrap()
            .calls()
            .into_iter()
            .filter(|call| *call == Call::Placed(VIDEO_WINDOW))
            .count();
        assert_eq!(
            placements,
            1 + SETTLE_CORRECTIONS as usize,
            "the placement itself and a capped number of corrections, and then it stops"
        );
    }

    #[test]
    fn starting_a_retrieve_drops_the_watch_on_the_send_it_undoes() {
        let mut core = core_with_confirmed_video_window();
        mixed_scaling(&core);
        assert!(core.send().is_ok());
        assert!(core.placement_settling());

        // Between `begin` and `finish` the interface is fading the window out.
        // A watch still running on the Send would re-assert the sent bounds
        // mid-fade and put the window back on the display it is leaving.
        core.begin_retrieve().expect("the send left a restore point");
        assert!(!core.placement_settling());
    }

    #[test]
    fn a_window_that_closes_while_it_is_being_watched_is_not_complained_about() {
        let mut core = core_with_confirmed_video_window();
        mixed_scaling(&core);
        assert!(core.send().is_ok());

        core.platform.as_mock().unwrap().set_zoom_present(false);
        assert_eq!(settle(&mut core), None, "a window that has gone is not a drift");
    }

    #[test]
    fn the_diagnostics_name_the_scaling_of_each_display() {
        let core = core_with_confirmed_video_window();
        core.platform
            .as_mock()
            .unwrap()
            .set_monitor_dpi(r"\\.\DISPLAY1", 144);

        let report = core.diagnostics();
        assert!(report.contains("144 dpi (150%)"), "the scaling of each display:\n{report}");
        assert!(report.contains("scaled differently"), "and that they differ:\n{report}");
    }

    #[test]
    fn the_diagnostics_report_what_became_of_the_last_placement() {
        let mut core = core_with_confirmed_video_window();
        mixed_scaling(&core);
        assert!(core.send().is_ok());
        let _ = settle(&mut core);

        let report = core.diagnostics();
        assert!(report.contains("LAST PLACEMENT"), "{report}");
        assert!(
            report.contains("Sent: asked 2560,0 1920x1080"),
            "what was asked for:\n{report}"
        );
        assert!(
            report.contains("landed 2560,0 1920x1080 after 1 correction(s), settled"),
            "and what became of it:\n{report}"
        );
    }

    #[test]
    fn a_new_send_stops_the_watch() {
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
        mock.set_returns_windowed(EXCLUSIVE_WINDOW, windowed_on_display_two());

        core.send().unwrap();
        core.retrieve().unwrap();
        assert!(core.media_restore_pending());

        core.send().unwrap();
        assert!(!core.media_restore_pending(), "the new Send owns the display now");
        assert_eq!(core.media_restore_step(), MediaRestoreStep::Idle);
    }

    /// The identity of the fixture's untitled VLC output window, as binding
    /// it through the media picker would record.
    fn vlc_identity() -> WindowIdentity {
        WindowIdentity {
            process_name: "vlc.exe".to_string(),
            class_name: "Qt5152QWindowIcon".to_string(),
            title: String::new(),
        }
    }

    /// The media picker exists because the ordinary one cannot show the
    /// player: an untitled window has nothing to match a saved identity by
    /// title, but process and class are the identity anyway.
    #[test]
    fn the_media_picker_offers_the_untitled_player() {
        let core = core_with_confirmed_video_window();

        assert!(
            core.media_candidates().iter().any(|c| c.handle == MEDIA_WINDOW),
            "the untitled player is the one window this picker is for"
        );
        assert!(
            core.candidates().iter().all(|c| c.handle != MEDIA_WINDOW),
            "and the Zoom picker still keeps it out"
        );
        assert!(
            core.media_candidates().iter().all(|c| !c.own_process && !c.minimized),
            "our own windows and iconic ones are still nothing to offer"
        );
    }

    /// Without a binding there is nothing to restore, and the failure routes
    /// to the media picker rather than to an error to decode.
    #[test]
    fn restore_media_without_a_binding_asks_for_selection() {
        let mut core = core_with_confirmed_video_window();

        let failure = core.restore_media().unwrap_err();
        assert!(failure.needs_selection);
    }

    /// The manual path, measured like the automatic one: a player that comes
    /// back already full screen gets focus and nothing else.
    #[test]
    fn restore_media_brings_back_a_stowed_player_without_typing_at_it() {
        let mut core = core_with_confirmed_video_window();
        core.config.media_window = Some(vlc_identity());
        core.platform.minimize(MEDIA_WINDOW).unwrap();

        let message = core.restore_media().unwrap();
        assert!(message.contains("vlc.exe"), "got: {message}");
        let mock = core.platform.as_mock().unwrap();
        assert!(!mock.is_minimized(MEDIA_WINDOW));
        assert!(mock.was_activated(MEDIA_WINDOW), "focus is what brings a player back");

        match core.media_restore_step() {
            MediaRestoreStep::Done(message) => {
                assert!(message.contains("full screen again"), "got: {message}");
            }
            other => panic!("unminimising restored its full-screen bounds: {other:?}"),
        }
        assert!(
            !core
                .platform
                .as_mock()
                .unwrap()
                .calls()
                .iter()
                .any(|c| matches!(c, Call::KeySent(..)))
        );
    }

    /// The manual path pressing the key: bound player in the taskbar, resumes
    /// windowed, gets its own shortcut and returns to full screen.
    #[test]
    fn restore_media_presses_a_windowed_player_back_to_full_screen() {
        const BOUND_PLAYER: u64 = 0x6001;
        let mut core = core_with_confirmed_video_window();
        let mock = core.platform.as_mock().unwrap();
        mock.add_window(
            BOUND_PLAYER,
            "mpv.exe",
            "MpvOutput",
            "",
            Bounds::new(2560, 0, 1920, 1080),
        );
        mock.set_exclusive(BOUND_PLAYER);
        mock.set_returns_windowed(BOUND_PLAYER, windowed_on_display_two());
        core.config.media_window = Some(WindowIdentity {
            process_name: "mpv.exe".to_string(),
            class_name: "MpvOutput".to_string(),
            title: String::new(),
        });
        core.platform.minimize(BOUND_PLAYER).unwrap();

        core.restore_media().unwrap();
        assert_eq!(core.media_restore_step(), MediaRestoreStep::Waiting);
        assert_eq!(core.media_restore_step(), MediaRestoreStep::Waiting);

        let chord: KeyChord = "F".parse().unwrap();
        assert!(
            core.platform
                .as_mock()
                .unwrap()
                .calls()
                .contains(&Call::KeySent(BOUND_PLAYER, chord))
        );
        match core.media_restore_step() {
            MediaRestoreStep::Done(message) => {
                assert!(message.contains("Sent F to mpv.exe"), "got: {message}");
            }
            other => panic!("the toggle made it exclusive again: {other:?}"),
        }
    }

    /// Pressing the hotkey twice while a watch is running must not queue a
    /// second toggle at the same window.
    #[test]
    fn restore_media_pressed_twice_watches_once() {
        let mut core = core_with_confirmed_video_window();
        core.config.media_window = Some(vlc_identity());
        core.platform.minimize(MEDIA_WINDOW).unwrap();

        core.restore_media().unwrap();
        core.restore_media().unwrap();

        assert_eq!(
            core.pending_reentry.len(),
            1,
            "one player, one watch, however many presses"
        );
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
            dpi: BASE_DPI,
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
            dpi: BASE_DPI,
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
