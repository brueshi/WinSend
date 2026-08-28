//! The three things that keep happening after an action has reported.
//!
//! A Send or a Retrieve returns the moment its calls return, but none of them
//! is over at that point: the window is still fading, the placement has not
//! been argued out yet, and a displaced player has not decided whether it is
//! coming back to full screen. Each of those needs a clock.
//!
//! The clock is the only thing here. `Core` owns every measurement and every
//! decision; these types own when to look, when to stop, and what to say when
//! they do. That split is what keeps the state machines testable against the
//! mock without waiting out a real 200ms fade — and it is what lets two front
//! ends drive one copy of them.
//!
//! Nothing in this module knows what a frame is. Every entry point takes the
//! current instant rather than reading the clock, and answers with what to do
//! rather than by asking a toolkit to wake up.

use crate::core::{Core, Failure, MediaRestoreStep};

/// How long the video window takes to fade out on Retrieve.
///
/// Long enough to read as deliberate, short enough that nobody waits for it.
/// Deliberately not configurable until someone asks: another setting to get
/// wrong, for a quantity with one right answer.
pub const FADE: std::time::Duration = std::time::Duration::from_millis(200);

/// How often the last placement is looked at, and for how long.
///
/// Fifty milliseconds is faster than anyone can see a window move and slower
/// than the frame rate, so the watch costs a couple of dozen cheap reads
/// rather than one per frame. A second and a bit covers an application being
/// told its scaling changed and resizing itself in response, which is the
/// slowest thing this is waiting for, without leaving a watch running into
/// whatever the operator does next.
pub const SETTLE_LOOK: std::time::Duration = std::time::Duration::from_millis(50);
pub const SETTLE_WATCH: std::time::Duration = std::time::Duration::from_millis(1200);

/// How long a put-back player gets to settle before the watch reports what
/// happened instead. Generous against a suspended application resuming, and
/// short enough that the report still lands while the Retrieve is the thing
/// the user just did.
pub const MEDIA_RESTORE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// What a watch has to say when it ends.
///
/// `Ok` is what happened and `Err` is why it did not, which is the whole of
/// the distinction a front end needs to colour it. Deliberately not a
/// [`Failure`]: nothing a watch reports can be answered by picking a window,
/// so carrying the flag that opens the picker would be carrying a lie.
pub type Report = Result<String, String>;

/// Where a watch has got to.
pub enum Tick {
    /// Still running. Look again after the watch's interval.
    Watching,
    /// Over, with anything it had to say about how it ended.
    Done(Option<Report>),
}

/// A Retrieve part-way through, with the window fading and not yet moved.
///
/// Takes `now` rather than reading the clock, so the ramp, the move at the end
/// and every failure path landing opaque can all be exercised against the mock
/// without a window and without waiting 200ms per test.
pub struct Fade {
    /// The window being faded. Held rather than located again each frame: the
    /// whole thing lasts 200ms, and re-enumerating the desktop per frame to
    /// re-confirm what was found moments ago would be work for nothing.
    handle: u64,
    started: std::time::Instant,
}

/// What starting a Retrieve turned into.
pub enum Started {
    /// The fade is running. Drive it with [`Fade::advance`].
    Fading(Fade),
    /// Nothing was animated and the Retrieve is already over, either because
    /// it was refused or because the window would not go translucent.
    Cut(Result<String, Failure>),
}

impl Fade {
    /// Begin, or fall back to the hard cut this replaced.
    pub fn begin(core: &mut Core, now: std::time::Instant) -> Started {
        // Everything that can fail about the Retrieve is on this side, so a
        // fade never starts for one that was going to be refused. It also puts
        // the displaced windows back while the sent window is still opaque and
        // still covering them, which is what the fade then reveals.
        let handle = match core.begin_retrieve() {
            Ok(handle) => handle,
            Err(failure) => return Started::Cut(Err(failure)),
        };

        // The first step is full opacity, so a window that will not join the
        // layered band at all says so here — before anything on screen has
        // changed, and while a plain cut is still the whole of the fallback.
        if core.platform.set_window_opacity(handle, 1.0).is_err() {
            let fade = Self { handle, started: now };
            return Started::Cut(fade.finish(core));
        }

        Started::Fading(Self { handle, started: now })
    }

    /// How opaque the window should be, or `None` once the ramp is over.
    fn alpha(&self, now: std::time::Instant) -> Option<f32> {
        // Saturating, so a clock that steps backwards reads as no time passed
        // rather than as a negative alpha.
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= FADE {
            return None;
        }
        Some(1.0 - elapsed.as_secs_f32() / FADE.as_secs_f32())
    }

    /// Advance one step. `None` while still fading, otherwise the outcome of
    /// the completed Retrieve.
    pub fn advance(
        &self,
        core: &mut Core,
        now: std::time::Instant,
    ) -> Option<Result<String, Failure>> {
        match self.alpha(now) {
            Some(alpha) if core.platform.set_window_opacity(self.handle, alpha).is_ok() => None,
            // Either the ramp is over, or the window stopped accepting an
            // opacity at all — having closed mid-animation being the likely
            // reason. Both end the same way, because a fade that stops part
            // way through is the one outcome this must never leave behind.
            _ => Some(self.finish(core)),
        }
    }

    /// The one place a fade ends.
    ///
    /// Every path out of the animation comes through here — finished, failed,
    /// interrupted by another press, or the app going down — so there is a
    /// single place to be sure the window lands opaque and un-layered. A
    /// window stuck at forty percent on camera is a visible fault, where the
    /// hard cut this replaced was merely unremarkable.
    pub fn finish(&self, core: &mut Core) -> Result<String, Failure> {
        let outcome = core.finish_retrieve(self.handle);
        // After the move, and whether or not it worked. Opacity is not
        // conditional on anything.
        let _ = core.platform.clear_window_opacity(self.handle);
        outcome
    }
}

/// The clock half of the placement watch: when to look at the window that was
/// last placed, and when to give up looking.
#[derive(Debug, Clone, Copy)]
pub struct SettleClock {
    next_look: std::time::Instant,
    until: std::time::Instant,
}

impl SettleClock {
    /// Start the clock, if the action placed anything at all.
    pub fn begin(core: &Core, now: std::time::Instant) -> Option<Self> {
        core.placement_settling().then(|| Self {
            next_look: now + SETTLE_LOOK,
            until: now + SETTLE_WATCH,
        })
    }

    /// One look at the last placement.
    ///
    /// Runs to the end of its window rather than stopping at the first
    /// agreeable measurement. A window that has crossed a scaling boundary is
    /// the right size until the application is told, and stopping early would
    /// mean stopping in exactly that gap — measuring the one moment the bug is
    /// invisible and calling it settled.
    pub fn tick(&mut self, core: &mut Core, now: std::time::Instant) -> Tick {
        // Superseded: a Retrieve starting drops the Send's watch, and there is
        // nothing left for this clock to drive.
        if !core.placement_settling() {
            return Tick::Done(None);
        }

        if now >= self.until {
            return Tick::Done(core.finish_settle().map(Err));
        }
        if now >= self.next_look {
            core.settle_look();
            self.next_look = now + SETTLE_LOOK;
        }
        Tick::Watching
    }
}

/// The clock half of the full-screen watch: how long a put-back player has to
/// come back before the watch reports where it got to instead.
#[derive(Debug, Clone, Copy)]
pub struct MediaRestore {
    deadline: std::time::Instant,
}

impl MediaRestore {
    /// Start the clock, if the Retrieve queued a watch at all.
    ///
    /// Called wherever an action reports its outcome, which is the moment the
    /// sent window has finished moving and the players are free to resume.
    /// Answers `None` when nothing was displaced or the setting is off, which
    /// is every Retrieve that never covered a full-screen player.
    pub fn begin(core: &Core, now: std::time::Instant) -> Option<Self> {
        core.media_restore_pending()
            .then(|| Self { deadline: now + MEDIA_RESTORE_TIMEOUT })
    }

    /// One look at the watched players.
    ///
    /// `fade_running` is a parameter rather than the caller's business because
    /// forgetting it would be silent: the key must land after the sent window
    /// has moved off the player, not into the middle of the reveal, and that
    /// ordering is what makes "was it restored" measurable at all. A fade in
    /// progress holds the watch where it is rather than ending it.
    pub fn tick(&mut self, core: &mut Core, now: std::time::Instant, fade_running: bool) -> Tick {
        if fade_running {
            return Tick::Watching;
        }

        if now >= self.deadline {
            return Tick::Done(core.cancel_media_restore().map(Err));
        }

        match core.media_restore_step() {
            MediaRestoreStep::Idle => Tick::Done(None),
            MediaRestoreStep::Waiting => Tick::Watching,
            MediaRestoreStep::Done(message) => Tick::Done(Some(Ok(message))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod fading {
        use super::*;
        use crate::core::test_support::{
            core_with_confirmed_video_window, MEDIA_WINDOW, VIDEO_WINDOW,
        };
        use crate::mock::Call;
        use std::time::Instant;

        /// A Core with something to retrieve, and a player on the target
        /// display that Send will have pushed out of the way.
        fn sent() -> (Core, Instant) {
            let mut core = core_with_confirmed_video_window();
            let mock = core.platform.as_mock().unwrap();
            mock.set_topmost(MEDIA_WINDOW, true).unwrap();
            mock.bring_to_front(MEDIA_WINDOW);
            core.send().expect("the send must succeed");
            (core, Instant::now())
        }

        fn begin(core: &mut Core, now: Instant) -> Fade {
            match Fade::begin(core, now) {
                Started::Fading(fade) => fade,
                Started::Cut(_) => panic!("the mock window accepts opacity, so it must fade"),
            }
        }

        #[test]
        fn the_ramp_runs_from_opaque_to_gone() {
            let fade = Fade { handle: VIDEO_WINDOW, started: Instant::now() };
            let at = |ms| fade.alpha(fade.started + std::time::Duration::from_millis(ms));

            assert_eq!(at(0), Some(1.0), "it starts where the window already is");
            assert!(
                at(100).is_some_and(|alpha| (alpha - 0.5).abs() < 0.01),
                "halfway through is halfway down, got {:?}",
                at(100)
            );
            assert_eq!(at(200), None, "the ramp is over rather than at zero");
            assert_eq!(at(5_000), None, "and stays over");
        }

        /// A clock that steps backwards must read as no time passed, not as a
        /// negative alpha that would clamp to invisible.
        #[test]
        fn a_clock_that_goes_backwards_does_not_make_the_window_vanish() {
            let started = Instant::now() + std::time::Duration::from_secs(1);
            let fade = Fade { handle: VIDEO_WINDOW, started };
            assert_eq!(fade.alpha(Instant::now()), Some(1.0));
        }

        #[test]
        fn the_window_goes_translucent_while_it_fades() {
            let (mut core, now) = sent();
            let fade = begin(&mut core, now);

            let still_going =
                fade.advance(&mut core, now + std::time::Duration::from_millis(100));

            assert!(still_going.is_none(), "it is not finished halfway through");
            let opacity = core.platform.as_mock().unwrap().opacity(VIDEO_WINDOW);
            assert!(
                opacity.is_some_and(|alpha| (100..=155).contains(&alpha)),
                "about half opaque, got {opacity:?}"
            );
        }

        /// The end state that matters most. Whatever happened on the way, the
        /// window is opaque and carries nothing that was making it otherwise.
        #[test]
        fn the_fade_ends_opaque_and_carrying_nothing() {
            let (mut core, now) = sent();
            let fade = begin(&mut core, now);

            let outcome = fade.advance(&mut core, now + FADE);

            assert!(outcome.is_some_and(|result| result.is_ok()), "it completes the retrieve");
            assert_eq!(
                core.platform.as_mock().unwrap().opacity(VIDEO_WINDOW),
                None,
                "nothing may be left holding the window translucent"
            );
        }

        /// A second press mid-fade finishes it rather than queueing behind it,
        /// and must land on the same end state as running to completion.
        #[test]
        fn finishing_early_still_lands_opaque() {
            let (mut core, now) = sent();
            let fade = begin(&mut core, now);
            fade.advance(&mut core, now + std::time::Duration::from_millis(40));

            let outcome = fade.finish(&mut core);

            assert!(outcome.is_ok());
            assert_eq!(core.platform.as_mock().unwrap().opacity(VIDEO_WINDOW), None);
        }

        /// The failure the whole design is arranged around: a window that goes
        /// away part-way through must not leave anything half-applied.
        #[test]
        fn a_window_that_closes_mid_fade_does_not_stay_translucent() {
            let (mut core, now) = sent();
            let fade = begin(&mut core, now);
            fade.advance(&mut core, now + std::time::Duration::from_millis(60));

            core.platform.as_mock().unwrap().set_zoom_present(false);
            let outcome = fade.advance(&mut core, now + std::time::Duration::from_millis(120));

            assert!(outcome.is_some(), "it gives up rather than ramping against nothing");
            assert_eq!(
                core.platform.as_mock().unwrap().opacity(VIDEO_WINDOW),
                None,
                "the opacity is cleared even though the window went away"
            );
        }

        /// The reordering the fade exists to make use of: the player is back
        /// underneath the still-opaque window before any of it fades, so what
        /// the fade reveals is the thing that belongs there.
        #[test]
        fn the_player_is_back_before_any_of_the_fade_happens() {
            let (mut core, now) = sent();
            let fade = begin(&mut core, now);
            fade.advance(&mut core, now + FADE);

            let calls = core.platform.as_mock().unwrap().calls();
            let restored = calls
                .iter()
                .rposition(|call| *call == Call::Raised(MEDIA_WINDOW))
                .expect("the player must be put back");
            let first_fade = calls
                .iter()
                .position(|call| matches!(call, Call::Opacity(handle, _) if *handle == VIDEO_WINDOW))
                .expect("the window must be faded");

            assert!(restored < first_fade, "got: {calls:?}");
        }

        /// And the move happens after the fade rather than during it, so the
        /// window is invisible by the time it jumps to the other display.
        #[test]
        fn the_window_moves_only_once_it_has_faded_out() {
            let (mut core, now) = sent();
            let fade = begin(&mut core, now);
            fade.advance(&mut core, now + FADE);

            let calls = core.platform.as_mock().unwrap().calls();
            let last_fade = calls
                .iter()
                .rposition(|call| matches!(call, Call::Opacity(handle, _) if *handle == VIDEO_WINDOW))
                .expect("the window must be faded");
            let moved = calls
                .iter()
                .rposition(|call| *call == Call::Placed(VIDEO_WINDOW))
                .expect("the window must move back");
            let cleared = calls
                .iter()
                .rposition(|call| *call == Call::OpacityCleared(VIDEO_WINDOW))
                .expect("the opacity must be put back");

            assert!(last_fade < moved, "the fade finishes before the move: {calls:?}");
            assert!(moved < cleared, "and it is opaque again only once home: {calls:?}");
        }

        /// A Retrieve with nothing to restore must be refused before anything
        /// is made translucent, rather than fading a window and then failing.
        #[test]
        fn nothing_fades_when_there_is_nothing_to_retrieve() {
            let mut core = core_with_confirmed_video_window();

            let started = Fade::begin(&mut core, Instant::now());

            assert!(matches!(started, Started::Cut(Err(_))));
            assert_eq!(core.platform.as_mock().unwrap().opacity(VIDEO_WINDOW), None);
            assert!(
                !core
                    .platform
                    .as_mock()
                    .unwrap()
                    .calls()
                    .iter()
                    .any(|call| matches!(call, Call::Opacity(..))),
                "the window must never have been touched"
            );
        }
    }

    /// The two watches share a shape, so what is worth pinning is the part
    /// each owns on its own: when it starts, when it gives up, and that a
    /// front end cannot accidentally drive one it was never handed.
    mod watches {
        use super::*;
        use crate::core::test_support::{core_with_confirmed_video_window, MEDIA_WINDOW};
        use std::time::Instant;

        #[test]
        fn no_placement_means_no_settle_watch() {
            let core = core_with_confirmed_video_window();
            assert!(SettleClock::begin(&core, Instant::now()).is_none());
        }

        #[test]
        fn a_send_arms_the_settle_watch() {
            let mut core = core_with_confirmed_video_window();
            core.send().expect("the send must succeed");
            assert!(SettleClock::begin(&core, Instant::now()).is_some());
        }

        /// The watch runs its full window rather than stopping at the first
        /// agreeable measurement, which is the whole of the fix in
        /// `docs/placement-and-scaling.md`.
        #[test]
        fn the_settle_watch_keeps_looking_until_its_window_is_up() {
            let mut core = core_with_confirmed_video_window();
            core.send().expect("the send must succeed");
            let now = Instant::now();
            let mut clock = SettleClock::begin(&core, now).expect("the send placed a window");

            assert!(matches!(clock.tick(&mut core, now + SETTLE_LOOK), Tick::Watching));
            assert!(matches!(
                clock.tick(&mut core, now + SETTLE_WATCH - SETTLE_LOOK),
                Tick::Watching
            ));
            assert!(matches!(clock.tick(&mut core, now + SETTLE_WATCH), Tick::Done(_)));
        }

        /// A Retrieve starting cancels the Send it is undoing, so the Send's
        /// clock — still armed through the fade, since nothing re-arms until
        /// the fade reports — must end quietly rather than re-assert bounds
        /// that would put the window back on the display it is leaving.
        #[test]
        fn a_retrieve_starting_ends_the_send_watch_quietly() {
            let mut core = core_with_confirmed_video_window();
            core.send().expect("the send must succeed");
            let now = Instant::now();
            let mut clock = SettleClock::begin(&core, now).expect("the send placed a window");

            core.begin_retrieve().expect("the retrieve must start");

            assert!(matches!(clock.tick(&mut core, now + SETTLE_LOOK), Tick::Done(None)));
        }

        #[test]
        fn nothing_displaced_means_no_full_screen_watch() {
            let mut core = core_with_confirmed_video_window();
            core.send().expect("the send must succeed");
            core.retrieve().expect("the retrieve must succeed");
            assert!(MediaRestore::begin(&core, Instant::now()).is_none());
        }

        /// The ordering `docs/media-restore.md` turns on: the key must land
        /// after the sent window has moved off the player, so a fade still
        /// running holds the watch where it is rather than advancing it.
        #[test]
        fn a_running_fade_holds_the_full_screen_watch() {
            let mut core = core_with_confirmed_video_window();
            let mock = core.platform.as_mock().unwrap();
            mock.set_exclusive(MEDIA_WINDOW);
            mock.set_suspends(MEDIA_WINDOW);
            core.send().expect("the send must succeed");
            core.retrieve().expect("the retrieve must succeed");

            let now = Instant::now();
            let Some(mut watch) = MediaRestore::begin(&core, now) else {
                return;
            };

            assert!(matches!(
                watch.tick(&mut core, now + MEDIA_RESTORE_TIMEOUT, true),
                Tick::Watching
            ));
            assert!(
                core.media_restore_pending(),
                "the watch must still be there once the fade is over"
            );
        }

        /// A player that never comes back is named rather than quietly
        /// forgotten, which is what the deadline is for.
        #[test]
        fn the_full_screen_watch_reports_a_player_that_never_came_back() {
            let mut core = core_with_confirmed_video_window();
            let mock = core.platform.as_mock().unwrap();
            mock.set_exclusive(MEDIA_WINDOW);
            mock.set_suspends(MEDIA_WINDOW);
            mock.set_never_resumes(MEDIA_WINDOW);
            core.send().expect("the send must succeed");
            core.retrieve().expect("the retrieve must succeed");

            let now = Instant::now();
            let Some(mut watch) = MediaRestore::begin(&core, now) else {
                return;
            };

            let ended = watch.tick(&mut core, now + MEDIA_RESTORE_TIMEOUT, false);
            assert!(matches!(ended, Tick::Done(Some(Err(_)))), "it says where the player got to");
        }
    }
}
