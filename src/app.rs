//! egui front end. Presentation only: every decision lives in `Core`.

use std::collections::HashMap;

use eframe::egui;

use crate::core::{Core, Failure};
use crate::hotkey::{Action, Hotkey, Key};
use crate::platform::WindowCandidate;
use crate::shell::{self, HotkeyReport, Shell, ShellEvent, TrayState};
use crate::update::{self, Release, UpdateEvent, Updater};

/// The height of the surface with the configuration folded away.
///
/// This is the shape that matters: it sits on screen during a broadcast, so it
/// stays as small as two large buttons and the status strip allow.
///
/// The strip costs the live window its full height, where the single label it
/// replaced cost nothing until it had something to say. That is what a message
/// which cannot be overwritten before it is read is worth, and it is paid in
/// window height rather than out of the controls, which are pressed under
/// pressure and do not shrink to make room for anything.
pub const COMPACT_HEIGHT: f32 = 260.0 + STATUS_HEIGHT + MOCK_CONTROLS_HEIGHT;

/// Room for the mock-only debug controls, where they are compiled in.
///
/// They are real content and need real room, but adding it unconditionally
/// would make the window that sits over a live broadcast taller for the sake of
/// controls that never ship. Without it the disclosure falls below the fold on
/// macOS, which is the one machine the interface is developed on.
#[cfg(windows)]
const MOCK_CONTROLS_HEIGHT: f32 = 0.0;
#[cfg(not(windows))]
const MOCK_CONTROLS_HEIGHT: f32 = 56.0;
/// The height with the configuration disclosed. The window grows to this and
/// shrinks back, and only ever because the user asked it to — which is the
/// whole difference between a response and a surprise.
const EXPANDED_HEIGHT: f32 = 724.0 + STATUS_HEIGHT + MOCK_CONTROLS_HEIGHT;
/// The width the window opens at, and what a resize falls back to when the
/// current width cannot be read.
pub const DEFAULT_WIDTH: f32 = 340.0;

/// The picker's own window.
///
/// It needs room to compare thumbnails, and the surface it opens from is a
/// quarter of that tall, so it cannot be a panel inside it: an `egui::Window`
/// would be clamped to a viewport far smaller than the content. A viewport of
/// its own is sized on its own terms and leaves the surface behind it alone.
const PICKER_SIZE: egui::Vec2 = egui::vec2(460.0, 640.0);
const PICKER_VIEWPORT: &str = "winsend-picker";

/// Closing hides to the tray only where there is a tray to hide to. On macOS
/// the notification area does not exist and the mock has no icon to click, so
/// a hidden window would be unreachable; there, closing still quits.
const CLOSE_HIDES_TO_TRAY: bool = cfg!(windows);

/// How often to wake while hidden.
///
/// The waker already asks for a repaint on every shell event, but a window that
/// is not visible is not guaranteed to be told to redraw, and a hotkey that
/// only works while the window is on screen would defeat the point of both
/// features. This is the floor under that: a trivial frame ten times a second,
/// only while hidden, in exchange for the guarantee.
const HIDDEN_POLL: std::time::Duration = std::time::Duration::from_millis(100);

const ACCENT: egui::Color32 = egui::Color32::from_rgb(78, 142, 240);
const OK: egui::Color32 = egui::Color32::from_rgb(102, 187, 122);
const ERR: egui::Color32 = egui::Color32::from_rgb(226, 106, 106);
const SUBDUED: egui::Color32 = egui::Color32::from_gray(150);

/// Half the height of the tallest control, which is what makes a rectangle a
/// pill. epaint clamps this to half of whatever it is actually painting, so the
/// same value is correct for every control regardless of its height.
const PILL: egui::CornerRadius = egui::CornerRadius::same(19);

/// How many recent messages the status strip keeps.
///
/// Enough that a message cannot be pushed out before it has been read, which
/// is the whole complaint against a single label, and few enough that what is
/// on screen is still status rather than history.
const STATUS_HISTORY: usize = 3;

/// The strip's height, reserved whether or not there is anything to say.
///
/// Fixed on purpose. A strip that grew with each message would move the
/// disclosure underneath it, and a control that shifts while the operator is
/// reaching for it is a worse failure than a blank strip.
const STATUS_HEIGHT: f32 = 52.0;

/// How long the video window takes to fade out on Retrieve.
///
/// Long enough to read as deliberate, short enough that nobody waits for it.
/// Deliberately not configurable until someone asks: another setting to get
/// wrong, for a quantity with one right answer.
const FADE: std::time::Duration = std::time::Duration::from_millis(200);

/// A Retrieve part-way through, with the window fading and not yet moved.
///
/// Takes `now` rather than reading the clock, so the ramp, the move at the end
/// and every failure path landing opaque can all be exercised against the mock
/// without a window and without waiting 200ms per test.
struct Fade {
    /// The window being faded. Held rather than located again each frame: the
    /// whole thing lasts 200ms, and re-enumerating the desktop per frame to
    /// re-confirm what was found moments ago would be work for nothing.
    handle: u64,
    started: std::time::Instant,
}

/// What starting a Retrieve turned into.
enum Started {
    /// The fade is running. Drive it with [`Fade::advance`].
    Fading(Fade),
    /// Nothing was animated and the Retrieve is already over, either because
    /// it was refused or because the window would not go translucent.
    Cut(Result<String, Failure>),
}

impl Fade {
    /// Begin, or fall back to the hard cut this replaced.
    fn begin(core: &mut Core, now: std::time::Instant) -> Started {
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

    /// Advance one frame. `None` while still fading, otherwise the outcome of
    /// the completed Retrieve.
    fn advance(&self, core: &mut Core, now: std::time::Instant) -> Option<Result<String, Failure>> {
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
    fn finish(&self, core: &mut Core) -> Result<String, Failure> {
        let outcome = core.finish_retrieve(self.handle);
        // After the move, and whether or not it worked. Opacity is not
        // conditional on anything.
        let _ = core.platform.clear_window_opacity(self.handle);
        outcome
    }
}

/// What the updater has found, and how far the user has got with it.
///
/// Nothing here happens on its own. The check makes an indicator appear and
/// that is the whole of its effect; every step after it is a click, because
/// the one thing this feature must never do is restart the application in the
/// middle of a broadcast.
enum UpdateState {
    /// Nothing to say. Either the check found nothing newer, or it never
    /// answered at all — which look the same on purpose.
    Quiet,
    /// A newer release is waiting to be asked for.
    Available(Release),
    /// Asked for while a window was still sent, so the warning is up.
    Confirming(Release),
    /// Downloading and swapping. The release is kept so a failure can put the
    /// offer back rather than losing it.
    Installing(Release),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ok,
    Err,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Message {
    outcome: Outcome,
    text: String,
}

/// The last few things that happened, newest first.
///
/// A list rather than one label because the old one was overwritten by
/// whatever came next: a Send followed immediately by a hotkey report left no
/// trace of the first, and a message that vanished before it was read was
/// never delivered.
#[derive(Debug, Default)]
struct StatusLog {
    messages: std::collections::VecDeque<Message>,
}

impl StatusLog {
    fn push(&mut self, outcome: Outcome, text: impl Into<String>) {
        let message = Message { outcome, text: text.into() };

        // Pressing Send twice is a normal thing to do here and says the same
        // thing twice. Repeating it would push the two messages that give it
        // context out of a three-deep list for no information at all.
        if self.messages.front() == Some(&message) {
            return;
        }

        self.messages.push_front(message);
        self.messages.truncate(STATUS_HISTORY);
    }

    /// Newest first, which is where the eye goes and where the message that
    /// just arrived belongs.
    fn iter(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter()
    }
}

pub struct WinSendApp {
    core: Core,
    shell: Box<dyn Shell>,
    /// Whether the configuration section is disclosed. The one thing that
    /// changes the window's size, and only when clicked.
    configuring: bool,
    /// Whether the picker's viewport is open.
    picking: bool,
    /// A Retrieve fading the window out, if one is running.
    fading: Option<Fade>,
    updater: Box<dyn Updater>,
    update: UpdateState,
    /// Set when an update has been installed, so `main` can start the new
    /// executable after this one has finished putting the desktop back.
    relaunch: std::sync::Arc<std::sync::atomic::AtomicBool>,
    status: StatusLog,
    /// Picker contents, snapshotted when it opens so the list does not
    /// reshuffle under the cursor while the user is reading it.
    candidates: Vec<WindowCandidate>,
    thumbnails: HashMap<u64, egui::TextureHandle>,
    /// The action whose binding is being captured, if any. While this is set,
    /// every key press belongs to the capture and nothing else.
    capturing: Option<Action>,
    /// Which bindings the shell refused, so the offending row can say so
    /// rather than the reason living only in the status bar.
    hotkey_report: HotkeyReport,
    /// Hidden to the tray. The process is still running and still listening.
    hidden: bool,
    /// Set only by Quit, and the only thing that lets a close request through.
    quitting: bool,
}

impl WinSendApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        core: Core,
        relaunch: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        apply_style(&cc.egui_ctx);

        // Once, here, because the window exists by the time this runs and the
        // frame it asks for is not something that needs re-asserting per frame.
        if let Some(handle) = own_window_handle(cc) {
            core.platform.apply_window_chrome(handle);
        }

        // The waker is a repaint request against a cloned context. Without it
        // a hotkey press would sit in the queue until something else woke the
        // window, which defeats the point of not having to touch the window.
        let ctx = cc.egui_ctx.clone();
        let waker: crate::shell::Waker = std::sync::Arc::new(move || ctx.request_repaint());
        let shell = shell::create(std::sync::Arc::clone(&waker));
        shell.apply_hotkeys(core.config.hotkeys);

        // The same waker: the updater answers from a thread of its own too, and
        // an indicator that only appeared once the mouse moved over the window
        // would be no better than no indicator.
        let updater = update::create(waker);
        // Once per launch, and only if the user has not turned it off. Never
        // blocking: this returns immediately and the answer arrives later or
        // not at all.
        if core.config.check_for_updates {
            updater.check();
        }

        // Only the debug-only screen override below mutates this.
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut app = Self {
            core,
            shell,
            configuring: false,
            picking: false,
            fading: None,
            updater,
            update: UpdateState::Quiet,
            relaunch,
            status: StatusLog::default(),
            candidates: Vec::new(),
            thumbnails: HashMap::new(),
            capturing: None,
            hotkey_report: HotkeyReport::default(),
            hidden: false,
            quitting: false,
        };

        // Debug builds only: open straight onto a section so it can be
        // inspected without clicking through. Compiled out of release entirely.
        #[cfg(debug_assertions)]
        match std::env::var("WINSEND_SCREEN").as_deref() {
            Ok("settings") => app.set_configuring(&cc.egui_ctx, true),
            Ok("select") => app.open_picker(&cc.egui_ctx),
            _ => {}
        }

        app
    }

    /// Show the outcome, and when the window needs picking again, go straight
    /// to the picker instead of leaving the user to decode an error and find
    /// their own way to Settings.
    fn report(&mut self, ctx: &egui::Context, outcome: Result<String, Failure>) {
        match outcome {
            Ok(message) => self.status.push(Outcome::Ok, message),
            Err(failure) => {
                let needs_selection = failure.needs_selection;
                self.status.push(Outcome::Err, failure.message);
                if needs_selection {
                    self.open_picker(ctx);
                }
            }
        }
    }

    /// Run an action, however it was asked for. A hotkey press and a button
    /// click are the same thing by the time they reach here.
    fn perform(&mut self, ctx: &egui::Context, action: Action) {
        // A press arriving mid-fade finishes it at once rather than queueing
        // behind it. The operator pressing a key twice means they want it
        // done, not animated twice.
        let interrupted_a_fade = self.finish_fade(ctx);

        match action {
            // Send is the urgent half and still has to happen. It just acts on
            // a window that has finished moving rather than on one caught part
            // way through being moved.
            Action::Send => {
                let outcome = self.core.send();
                self.report(ctx, outcome);
            }
            // The fade that was just cut short *was* the Retrieve. Starting
            // another would only produce "nothing has been sent yet".
            Action::Retrieve if interrupted_a_fade => {}
            Action::Retrieve => self.start_retrieve(ctx),
        }
    }

    /// Retrieve, with the fade when it is wanted and a hard cut when it is not.
    fn start_retrieve(&mut self, ctx: &egui::Context) {
        if !self.core.config.fade_on_retrieve {
            let outcome = self.core.retrieve();
            self.report(ctx, outcome);
            return;
        }

        match Fade::begin(&mut self.core, std::time::Instant::now()) {
            Started::Fading(fade) => {
                self.fading = Some(fade);
                ctx.request_repaint();
            }
            Started::Cut(outcome) => self.report(ctx, outcome),
        }
    }

    /// Advance a fade by one frame, if one is running.
    fn fade_step(&mut self, ctx: &egui::Context) {
        // Taken out and put back rather than borrowed, so the fade can be
        // handed `&mut self.core` without the two borrows overlapping.
        let Some(fade) = self.fading.take() else {
            return;
        };

        match fade.advance(&mut self.core, std::time::Instant::now()) {
            None => {
                self.fading = Some(fade);
                // Unconditional rather than `request_repaint_after`: hidden to
                // the tray the loop is already on a 100ms timer, which would
                // step a 200ms fade about twice.
                ctx.request_repaint();
            }
            Some(outcome) => self.report(ctx, outcome),
        }
    }

    /// End a fade now, wherever it had got to. Says whether there was one.
    fn finish_fade(&mut self, ctx: &egui::Context) -> bool {
        let Some(fade) = self.fading.take() else {
            return false;
        };
        let outcome = fade.finish(&mut self.core);
        self.report(ctx, outcome);
        true
    }

    fn handle(&mut self, ctx: &egui::Context, event: ShellEvent) {
        match event {
            ShellEvent::Trigger(action) => self.perform(ctx, action),
            // A refusal is shown the moment it is known. A binding the user
            // believes is live but which never registered is the one failure
            // this feature cannot afford.
            ShellEvent::HotkeysApplied(report) => {
                if let Some(summary) = report.summary() {
                    self.status.push(Outcome::Err, summary);
                }
                self.hotkey_report = report;
            }
            // A left click on the icon toggles, which is what makes the icon a
            // way to get the window back rather than only a way to lose it.
            ShellEvent::ShowWindow => self.set_hidden(ctx, !self.hidden),
            ShellEvent::ShowSettings => {
                self.set_configuring(ctx, true);
                self.set_hidden(ctx, false);
            }
            // The only path that actually exits. Everything else, including
            // the window's own close button, hides instead.
            ShellEvent::Quit => {
                // Before the window goes. A fade abandoned here would leave
                // Zoom's window translucent after this process has gone, with
                // nothing left running that could put it back.
                self.finish_fade(ctx);
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn set_hidden(&mut self, ctx: &egui::Context, hidden: bool) {
        self.hidden = hidden;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(!hidden));
        if !hidden {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    /// Turn the window's close button into hide-to-tray.
    ///
    /// The request has to be cancelled rather than ignored: eframe treats an
    /// unanswered close as a close, and the process would go with it.
    fn intercept_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        if self.quitting || !CLOSE_HIDES_TO_TRAY {
            // Going down for real, so the same guarantee as Quit applies.
            self.finish_fade(ctx);
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        self.set_hidden(ctx, true);
    }

    /// Keep the tray icon in step with the window. Cheap to call every frame:
    /// the shell drops it when nothing has changed.
    fn refresh_tray(&mut self) {
        self.shell.set_tray_state(tray_state(&self.core));
    }

    /// Begin capturing a combination for `action`.
    ///
    /// The registered bindings are dropped first. A registered hotkey is
    /// swallowed by the OS and never reaches this window, so without this,
    /// rebinding a key to itself — or to the other action's key — would look
    /// like the capture had simply stopped working.
    /// The log is deliberately not cleared here. It used to be, because one
    /// label showing a stale error while the user pressed keys was confusing;
    /// with a list the new message simply arrives on top, and throwing away
    /// what came before would be the very thing this replaced.
    fn start_capture(&mut self, action: Action) {
        self.capturing = Some(action);
        self.shell.apply_hotkeys(Default::default());
    }

    fn end_capture(&mut self) {
        self.capturing = None;
        self.shell.apply_hotkeys(self.core.config.hotkeys);
    }

    /// Consume this frame's keyboard input on behalf of a capture in progress.
    ///
    /// Runs before anything is drawn and swallows every key event, which is
    /// what lets Escape and Tab be bound like any other key instead of being
    /// acted on by the widgets underneath.
    fn capture_step(&mut self, ctx: &egui::Context) {
        let Some(action) = self.capturing else {
            return;
        };

        let (modifiers, pressed) = ctx.input_mut(|input| {
            let pressed = input.events.iter().find_map(|event| match event {
                egui::Event::Key { key, pressed: true, .. } => key_from_egui(*key),
                _ => None,
            });
            input.events.clear();
            (input.modifiers, pressed)
        });

        // Modifiers alone are not a binding; keep waiting for the real key.
        let Some(key) = pressed else {
            return;
        };

        let hotkey = Hotkey {
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
            // egui does not report the Windows key as a modifier, so it cannot
            // be captured here. Bindings that use it still parse from a
            // hand-edited config; Windows reserves most of them anyway.
            win: false,
            key,
        };

        // A rejected combination leaves the capture running, so the user can
        // correct it by pressing another rather than starting over.
        if let Err(why) = hotkey.validate() {
            self.status.push(Outcome::Err, why);
            return;
        }
        match self.core.set_hotkey(action, Some(hotkey)) {
            Ok(message) => {
                self.status.push(Outcome::Ok, message);
                self.end_capture();
            }
            Err(failure) => self.status.push(Outcome::Err, failure.message),
        }
    }

    /// Disclose or fold away the configuration, growing and shrinking the
    /// window to suit.
    ///
    /// Only the height moves. The width is whatever the window currently is,
    /// so a user who has widened it keeps that; clobbering it here would be the
    /// same unasked-for resize this replaced.
    fn set_configuring(&mut self, ctx: &egui::Context, configuring: bool) {
        if self.configuring == configuring {
            return;
        }
        self.configuring = configuring;

        // Folding the configuration away takes the hotkey rows with it, so any
        // capture in progress has to end properly rather than just stop. Ending
        // it is what re-registers the bindings that `start_capture` dropped —
        // without that, walking away mid-capture leaves every hotkey dead until
        // something else happens to re-apply them.
        if !configuring {
            self.end_capture();
        }

        let width = ctx
            .input(|input| input.viewport().inner_rect.map(|rect| rect.width()))
            .unwrap_or(DEFAULT_WIDTH);
        let height = if configuring { EXPANDED_HEIGHT } else { COMPACT_HEIGHT };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(width, height)));
    }

    fn open_picker(&mut self, ctx: &egui::Context) {
        self.picking = true;
        self.candidates = self.core.candidates();
        self.thumbnails.clear();
        for candidate in &self.candidates {
            if let Some(thumb) = self.core.platform.thumbnail(candidate.handle) {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [thumb.width as usize, thumb.height as usize],
                    &thumb.rgba,
                );
                let texture = ctx.load_texture(
                    format!("thumb-{}", candidate.handle),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.thumbnails.insert(candidate.handle, texture);
            }
        }
    }

    fn close_picker(&mut self) {
        self.picking = false;
        self.thumbnails.clear();
        self.candidates.clear();
    }

    /// Take in whatever the updater has found.
    fn handle_update(&mut self, ctx: &egui::Context, event: UpdateEvent) {
        match event {
            UpdateEvent::Available(release) => self.update = UpdateState::Available(release),
            // The executable on disk is the new one now, so this process has
            // to give way to it. `main` starts the replacement once this one
            // has finished putting the desktop back.
            UpdateEvent::Installed => {
                self.relaunch.store(true, std::sync::atomic::Ordering::SeqCst);
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            // The old executable is still the one on disk, so the offer goes
            // back up rather than disappearing with the explanation.
            UpdateEvent::Failed(why) => {
                self.status.push(Outcome::Err, why);
                if let UpdateState::Installing(release) = std::mem::replace(
                    &mut self.update,
                    UpdateState::Quiet,
                ) {
                    self.update = UpdateState::Available(release);
                }
            }
        }
    }

    /// Ask for the update, or warn first when warning is the point.
    ///
    /// `Core` holds the restore point in memory and it is session-scoped by
    /// design, so restarting while a window is still sent leaves Zoom on the
    /// wrong monitor with nothing left able to put it back. That is the sort
    /// of thing that is obvious in hindsight at three in the morning.
    fn ask_to_install(&mut self, release: Release) {
        if self.core.can_retrieve() {
            self.update = UpdateState::Confirming(release);
            return;
        }
        self.install(release);
    }

    fn install(&mut self, release: Release) {
        self.updater.install(release.clone());
        self.update = UpdateState::Installing(release);
    }

    /// The warning, as a modal, because it is a decision and not a setting.
    ///
    /// A modal rather than another row on the surface: the surface has a fixed
    /// height and two extra lines would push the controls under it down, which
    /// is exactly what the rework removed.
    fn confirm_restart(&mut self, ctx: &egui::Context) {
        let UpdateState::Confirming(release) = &self.update else {
            return;
        };
        let release = release.clone();

        let mut chosen = None;
        egui::Modal::new(egui::Id::new("update-confirm")).show(ctx, |ui| {
            ui.set_max_width(260.0);
            ui.label(
                egui::RichText::new(format!("Update to {}", release.version))
                    .size(13.0)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "A window is still sent. Where it came from is only remembered \
                     for as long as this is running, so restarting now leaves it on \
                     the target display with nothing able to put it back.",
                )
                .size(11.0)
                .color(SUBDUED),
            );
            ui.add_space(10.0);

            ui.vertical_centered_justified(|ui| {
                if ui.button("Retrieve, then update").clicked() {
                    chosen = Some(true);
                }
                if ui.button("Update anyway").clicked() {
                    chosen = Some(false);
                }
                if ui.button("Not now").clicked() {
                    chosen = None;
                    self.update = UpdateState::Available(release.clone());
                }
            });
        });

        match chosen {
            // Straight through `Core`, not the fade: the point is to be
            // finished before anything restarts, and an animation would only
            // put 200ms between the decision and the thing it was guarding.
            Some(true) => {
                let outcome = self.core.retrieve();
                let restored = outcome.is_ok();
                self.report(ctx, outcome);
                if restored {
                    self.install(release);
                }
            }
            Some(false) => self.install(release),
            None => {}
        }
    }

    /// The indicator, and every state after it, in one line beside the target.
    fn update_row(&mut self, ui: &mut egui::Ui) {
        let mut asked_for = None;

        match &self.update {
            UpdateState::Quiet | UpdateState::Confirming(_) => {}
            UpdateState::Available(release) => {
                let version = release.version;
                if ui
                    .small_button(egui::RichText::new(format!("Update to {version}")).size(11.0))
                    .on_hover_text(
                        "Downloads the new version, checks it against the checksum published \
                         with the release, and restarts. Nothing happens until you click.",
                    )
                    .clicked()
                {
                    asked_for = Some(release.clone());
                }
            }
            UpdateState::Installing(_) => {
                ui.label(egui::RichText::new("Updating...").size(11.0).color(ACCENT));
            }
        }

        if let Some(release) = asked_for {
            self.ask_to_install(release);
        }
    }

    /// The status strip, in its own panel at the foot of the window.
    ///
    /// A panel rather than the last thing in the surface so that it is in the
    /// same place whether the configuration is disclosed or not, and so that
    /// nothing above it moves as messages arrive.
    fn status_strip(&self, ui: &mut egui::Ui) {
        // Scrolls rather than truncates. A message worth showing is worth
        // showing whole, and the ones that overflow are the long ones that
        // explain a failure.
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                for (age, message) in self.status.iter().enumerate() {
                    let colour = match message.outcome {
                        Outcome::Ok => OK,
                        Outcome::Err => ERR,
                    };
                    // Older messages recede rather than disappear, which says
                    // which one just arrived without a clock the window would
                    // have to keep repainting to keep honest.
                    let colour = if age == 0 { colour } else { colour.gamma_multiply(0.55) };
                    ui.label(egui::RichText::new(&message.text).color(colour).size(11.5));
                }
            });
    }

    /// The whole interface: the live controls, then the configuration when it
    /// has been asked for. One surface, so nothing is ever a screen away and
    /// nothing resizes unbidden.
    fn surface(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // No in-app title: the OS title bar already carries the app name, and
        // vertical space is scarce in a window this small.
        let target = self
            .core
            .config
            .resolve_monitor(&self.core.monitors())
            .map(|m| m.label())
            .unwrap_or_else(|| "no target monitor selected".to_string());

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("Target: {target}"))
                    .size(11.0)
                    .color(egui::Color32::from_gray(150)),
            );
            // Right-aligned and small: an update is worth noticing and never
            // worth competing with the two controls below it.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.update_row(ui);
            });
        });

        ui.add_space(12.0);

        let button = |text: &str| {
            egui::Button::new(egui::RichText::new(text).size(14.0)).min_size(egui::vec2(0.0, 38.0))
        };
        // Explicit white: the default foreground is tuned for the panel
        // background, not for a saturated accent fill, and reads as low
        // contrast on top of it.
        let primary = egui::Button::new(
            egui::RichText::new("Send to Monitor")
                .size(14.0)
                .color(egui::Color32::WHITE),
        )
        .min_size(egui::vec2(0.0, 38.0))
        .fill(ACCENT);

        let mut triggered = None;
        ui.vertical_centered_justified(|ui| {
            if ui.add(primary).clicked() {
                triggered = Some(Action::Send);
            }
            ui.add_space(6.0);

            let can_retrieve = self.core.can_retrieve();
            let response = ui.add_enabled(can_retrieve, button("Retrieve"));
            if response.clicked() {
                triggered = Some(Action::Retrieve);
            }
            if !can_retrieve {
                response.on_hover_text("Nothing has been sent yet this session");
            }
        });
        if let Some(action) = triggered {
            self.perform(ctx, action);
        }


        // Mock-only: exercise the "window vanished" path without needing Zoom,
        // and the hotkey path without a real key registration.
        #[cfg(not(windows))]
        {
            ui.add_space(10.0);
            ui.separator();
            if let Some(mock) = self.core.platform.as_mock() {
                let mut present = mock.zoom_present();
                if checkbox(ui, &mut present, "mock: Zoom running").changed() {
                    mock.set_zoom_present(present);
                }
            }
            // The picker is the only way to confirm a window, and it is a
            // viewport of its own now, which puts every state that needs a
            // confirmed window out of reach of anything driving this from
            // outside. This is the same shortcut the picker takes, without the
            // clicking.
            let confirming = ui
                .small_button(egui::RichText::new("mock: confirm Zoom window").size(10.0))
                .clicked()
                .then(|| {
                    self.core
                        .candidates()
                        .into_iter()
                        .find(|candidate| candidate.likely_zoom)
                });
            if let Some(Some(candidate)) = confirming {
                let outcome = self.core.confirm_window(&candidate);
                self.report(ctx, outcome);
            }
            // Injected rather than performed directly, so the press travels the
            // same queue-and-wake path a real hotkey would.
            let mut pressed = None;
            let mut chosen = None;
            if let Some(mock) = self.shell.as_mock() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("mock: hotkey").size(10.0));
                    for action in Action::ALL {
                        if ui
                            .small_button(egui::RichText::new(action.label()).size(10.0))
                            .clicked()
                        {
                            pressed = Some(action);
                        }
                    }
                });
                let tray = mock.tray();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("mock: tray").size(10.0));
                    for (label, event) in [
                        ("Show", ShellEvent::ShowWindow),
                        ("Settings", ShellEvent::ShowSettings),
                        ("Quit", ShellEvent::Quit),
                    ] {
                        if ui
                            .small_button(egui::RichText::new(label).size(10.0))
                            .clicked()
                        {
                            chosen = Some(event.clone());
                        }
                    }
                    if !tray.can_retrieve {
                        ui.label(
                            egui::RichText::new("Retrieve greyed").size(9.0).color(SUBDUED),
                        );
                    }
                });
                ui.label(egui::RichText::new(tray.tooltip).size(9.0).color(SUBDUED));

                if let Some(action) = pressed {
                    mock.trigger(action);
                }
                if let Some(event) = chosen {
                    mock.choose(event);
                }
            }

            // Walks the whole update flow without the network: the canned
            // response goes through the same parsing and comparison the real
            // updater uses, so what is exercised here is the real decision.
            let mut offer = false;
            let mut fail = false;
            if let Some(mock) = self.updater.as_mock() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("mock: update").size(10.0));
                    if ui.small_button(egui::RichText::new("offer").size(10.0)).clicked() {
                        offer = true;
                    }
                    if ui.small_button(egui::RichText::new("offer, fails").size(10.0)).clicked() {
                        offer = true;
                        fail = true;
                    }
                });
                if offer {
                    if fail {
                        mock.set_install_failure("the download did not match its checksum");
                    }
                    mock.set_response(&crate::mock::MockUpdater::release_list("99.0.0"));
                    mock.check();
                }
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(2.0);

        // egui's own header rather than a button with a caret in its label:
        // it paints the triangle with the painter, where a glyph like U+25BE
        // falls outside the bundled fonts and comes out as a missing-glyph box.
        //
        // Driven from `configuring` rather than from egui's remembered state,
        // because the same flag has to be settable from the tray's Settings
        // item and is what decides the window's height.
        let disclosure = egui::CollapsingHeader::new(
            egui::RichText::new("Settings").size(12.0).strong(),
        )
        .open(Some(self.configuring))
        .show(ui, |ui| {
            // Scrollable as a safety net rather than as the plan. The window
            // grows to fit this, but it is user-resizable, and configuration
            // clipped with no way to reach it would be worse than a scrollbar
            // nobody needs.
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| self.configuration(ui, ctx));
        });

        if disclosure.header_response.clicked() {
            self.set_configuring(ctx, !self.configuring);
        }
    }

    /// Everything that used to be the Settings screen, minus its title and its
    /// way back: it is part of the surface now, and the disclosure it sits in
    /// is the way back.
    fn configuration(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(egui::RichText::new("Target monitor").size(12.0).strong());
        ui.add_space(4.0);

        let monitors = self.core.monitors();
        let selected = self
            .core
            .config
            .resolve_monitor(&monitors)
            .map(|m| m.id.clone());

        let mut chosen = None;
        for monitor in &monitors {
            let is_selected = selected.as_deref() == Some(monitor.id.as_str());
            if ui
                .selectable_label(is_selected, format!("{}\n{}", monitor.id, monitor.label()))
                .clicked()
            {
                chosen = Some(monitor.clone());
            }
        }
        if let Some(monitor) = chosen {
            let outcome = self.core.set_target_monitor(&monitor);
            self.report(ctx, outcome);
        }

        ui.add_space(8.0);

        let mut clear_target = self.core.config.clear_target;
        if checkbox(ui, &mut clear_target, "Minimise other windows on this monitor")
            .on_hover_text(
                "Minimises anything filling the target monitor when sending, and restores it on Retrieve. \
                 Use this when something full screen refuses to give up the display. \
                 Some players pause while minimised.",
            )
            .changed()
        {
            if let Err(message) = self.core.set_clear_target(clear_target) {
                self.status.push(Outcome::Err, message);
            }
        }

        ui.add_space(10.0);

        let mut borderless = self.core.config.borderless;
        if checkbox(ui, &mut borderless, "Strip window frame when sending")
            .on_hover_text("Removes the title bar and border so the window fills the monitor edge to edge")
            .changed()
        {
            if let Err(message) = self.core.set_borderless(borderless) {
                self.status.push(Outcome::Err, message);
            }
        }

        ui.add_space(10.0);

        let mut fade = self.core.config.fade_on_retrieve;
        if checkbox(ui, &mut fade, "Fade out when retrieving")
            .on_hover_text(
                "Fades the video window out over a fifth of a second instead of cutting it, \
                 so the display it leaves is uncovered smoothly. Turn this off if the window \
                 flickers or the fade stutters: it needs the window to be composed differently \
                 while it runs, which not every application takes kindly to.",
            )
            .changed()
        {
            if let Err(message) = self.core.set_fade_on_retrieve(fade) {
                self.status.push(Outcome::Err, message);
            }
        }

        ui.add_space(10.0);

        let mut checking = self.core.config.check_for_updates;
        if checkbox(ui, &mut checking, "Check for updates on startup")
            .on_hover_text(
                "Asks GitHub once per launch whether there is a newer release, and shows a \
                 button if there is. Nothing downloads or restarts unless you click it. \
                 Takes effect at the next launch.",
            )
            .changed()
        {
            if let Err(message) = self.core.set_check_for_updates(checking) {
                self.status.push(Outcome::Err, message);
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);

        ui.label(egui::RichText::new("Zoom window").size(12.0).strong());
        let confirmed = self
            .core
            .config
            .zoom_window
            .as_ref()
            .map(|w| format!("{} ({})", w.title, w.class_name))
            .unwrap_or_else(|| "none confirmed".to_string());
        ui.label(
            egui::RichText::new(confirmed)
                .size(11.0)
                .color(egui::Color32::from_gray(150)),
        );
        ui.add_space(4.0);
        if ui.button("Select Zoom Window").clicked() {
            self.open_picker(ctx);
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        self.hotkey_settings(ui, ctx);

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        if ui
            .button(egui::RichText::new("Copy diagnostics").size(11.0))
            .on_hover_text(
                "Copies what WinSend can currently see — every window, which monitor it covers, \
                 and which ones it considers to be in the way — ready to paste. Also saved next \
                 to the config file.",
            )
            .clicked()
        {
            // The clipboard first, because the file lands in AppData, which
            // Explorer hides by default. A report nobody can find is a report
            // that may as well not exist.
            ctx.copy_text(self.core.diagnostics());
            let saved = self.core.save_diagnostics();
            self.report(
                ctx,
                saved.map(|where_to| format!("Copied to the clipboard. {where_to}")),
            );
        }

    }

    fn hotkey_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(egui::RichText::new("Hotkeys").size(12.0).strong());
        ui.label(
            egui::RichText::new("Work from inside Zoom, without focusing this window.")
                .size(11.0)
                .color(SUBDUED),
        );
        ui.add_space(4.0);

        // Collected rather than applied inline: every branch below borrows
        // `self` through the closure, and acting on it there would conflict.
        let mut start = None;
        let mut cancel = false;
        let mut cleared = None;

        for action in Action::ALL {
            let capturing = self.capturing == Some(action);
            let bound = self.core.config.hotkeys.binding(action);

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(action.label()).size(11.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if capturing {
                        if ui.small_button("Cancel").clicked() {
                            cancel = true;
                        }
                        ui.label(
                            egui::RichText::new("press a combination")
                                .size(11.0)
                                .color(ACCENT),
                        );
                        return;
                    }
                    if bound.is_some() && ui.small_button("Clear").clicked() {
                        cleared = Some(action);
                    }
                    if ui
                        .small_button(if bound.is_some() { "Change" } else { "Set" })
                        .clicked()
                    {
                        start = Some(action);
                    }
                    let (text, colour) = match bound {
                        Some(hotkey) => (hotkey.to_string(), egui::Color32::from_gray(210)),
                        None => ("not set".to_string(), SUBDUED),
                    };
                    ui.label(egui::RichText::new(text).size(11.0).color(colour));
                });
            });

            // The row that failed says so, rather than the reason being one
            // status message the user has already scrolled past.
            if let Some(why) = self.hotkey_report.reason(action) {
                ui.label(egui::RichText::new(why).size(10.0).color(ERR));
            }
        }

        if let Some(action) = start {
            self.start_capture(action);
        } else if cancel {
            self.end_capture();
        } else if let Some(action) = cleared {
            let outcome = self.core.set_hotkey(action, None);
            self.report(ctx, outcome);
            self.shell.apply_hotkeys(self.core.config.hotkeys);
        }
    }

    /// The picker, in a window of its own over the surface.
    ///
    /// An immediate viewport rather than a deferred one: it needs `&mut self`
    /// for the candidate list and the textures, and a deferred viewport's
    /// closure has to be `Send + Sync + 'static`, which that is not.
    fn picker_viewport(&mut self, ctx: &egui::Context) {
        if !self.picking {
            return;
        }

        let builder = egui::ViewportBuilder::default()
            .with_title("Select Zoom Window")
            .with_inner_size(PICKER_SIZE)
            .with_min_inner_size([360.0, 320.0])
            // The surface it opens from is always on top, and a picker behind
            // it would be a window asking for an answer from out of sight.
            .with_always_on_top();

        let mut closing = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(PICKER_VIEWPORT),
            builder,
            |ctx, _class| {
                egui::CentralPanel::default().show(ctx, |ui| self.picker(ui, ctx));
                if ctx.input(|input| input.viewport().close_requested()) {
                    closing = true;
                }
            },
        );

        if closing {
            self.close_picker();
        }
    }

    fn picker(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(
            egui::RichText::new(
                "Pin someone in Zoom first, then pick the window showing only that video feed.",
            )
            .size(11.0)
            .color(egui::Color32::from_gray(150)),
        );
        ui.add_space(8.0);

        let mut confirmed = None;
        // auto_shrink false: without it the list sizes to its content and the
        // window's spare height goes unused, which is what made this feel
        // cramped at anything short of full screen.
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
            for candidate in &self.candidates {
                let response = ui
                    .push_id(candidate.handle, |ui| {
                        egui::Frame::group(ui.style())
                            .fill(egui::Color32::from_gray(28))
                            .show(ui, |ui| {
                                // Uniform full-width rows; without this each
                                // row sizes to its own content and the right
                                // edges come out ragged.
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    if let Some(texture) = self.thumbnails.get(&candidate.handle) {
                                        ui.add(
                                            egui::Image::new(texture)
                                                .fit_to_exact_size(egui::vec2(96.0, 54.0)),
                                        );
                                    } else {
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(96.0, 54.0),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(
                                            rect,
                                            2.0,
                                            egui::Color32::from_gray(40),
                                        );
                                        ui.painter().text(
                                            rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            "no preview",
                                            egui::FontId::proportional(9.0),
                                            egui::Color32::from_gray(110),
                                        );
                                    }
                                    // Truncate rather than wrap: a long title
                                    // would otherwise widen every row and push
                                    // the window past the screen.
                                    ui.vertical(|ui| {
                                        let detail = |text: String, size: f32, gray: u8| {
                                            egui::Label::new(
                                                egui::RichText::new(text)
                                                    .size(size)
                                                    .color(egui::Color32::from_gray(gray)),
                                            )
                                            .truncate()
                                        };
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&candidate.title)
                                                    .size(12.0)
                                                    .strong(),
                                            )
                                            .truncate(),
                                        );
                                        ui.add(detail(
                                            format!(
                                                "{} · {}",
                                                candidate.process_name, candidate.class_name
                                            ),
                                            10.0,
                                            140,
                                        ));
                                        ui.add(detail(
                                            format!(
                                                "{}x{} on {}",
                                                candidate.bounds.width,
                                                candidate.bounds.height,
                                                candidate.monitor_id
                                            ),
                                            10.0,
                                            120,
                                        ));
                                    });
                                });
                            });
                    })
                    .response;

                if response.interact(egui::Sense::click()).clicked() {
                    confirmed = Some(candidate.clone());
                }
                ui.add_space(4.0);
            }
        });

        if let Some(candidate) = confirmed {
            let outcome = self.core.confirm_window(&candidate);
            self.report(ctx, outcome);
            self.close_picker();
        }
    }
}

impl eframe::App for WinSendApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drained before anything is drawn, so an action triggered from
        // outside the window is reflected in this frame rather than the next.
        for event in self.shell.poll() {
            self.handle(ctx, event);
        }
        for event in self.updater.poll() {
            self.handle_update(ctx, event);
        }
        // Before the hidden check below, since a Retrieve can be triggered by
        // a hotkey while the window is in the tray and its fade still has to
        // run to completion.
        self.fade_step(ctx);
        // Before any widget sees the keyboard, so a capture in progress takes
        // every press for itself.
        self.capture_step(ctx);
        self.refresh_tray();
        self.intercept_close(ctx);

        // Nothing below is worth drawing for a window nobody can see, but the
        // frame still has to happen so the next shell event is picked up.
        if self.hidden {
            ctx.request_repaint_after(HIDDEN_POLL);
            return;
        }

        // Before the central panel, as egui requires, and at the foot of the
        // window so the strip is in one place whether the configuration is
        // disclosed or not.
        egui::TopBottomPanel::bottom("status")
            .exact_height(STATUS_HEIGHT)
            .show(ctx, |ui| self.status_strip(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.surface(ui, ctx));
        // Over the surface, so the decision it asks for cannot be missed.
        self.confirm_restart(ctx);
        // After the surface, so a Send that could not identify the window has
        // already asked for the picker and it opens in the same frame.
        self.picker_viewport(ctx);
    }
}

/// What the tray icon should show for the current state.
///
/// The tooltip carries the target monitor because that is the one setting worth
/// confirming without opening the window, which is the whole point of the icon.
fn tray_state(core: &Core) -> TrayState {
    let tooltip = match core.config.resolve_monitor(&core.monitors()) {
        Some(monitor) => format!("WinSend — sends to {}", monitor.label()),
        None => "WinSend — no target monitor selected".to_string(),
    };
    TrayState { can_retrieve: core.can_retrieve(), tooltip }
}

/// Our own window's OS handle, in the same opaque form `Platform` speaks.
///
/// Kept free of `cfg` attributes: `RawWindowHandle` names every platform's
/// variant on every platform, so this compiles as written on macOS and simply
/// answers `None` there. Returning `None` is a normal answer and means only
/// that there is no native frame to ask anything of.
fn own_window_handle(cc: &eframe::CreationContext<'_>) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(window) => Some(window.hwnd.get() as u64),
        _ => None,
    }
}

/// A checkbox, kept square while everything around it is a pill.
///
/// The pill radius turns egui's checkbox into a circle, and a circle means
/// "one of these" in every other piece of desktop software. These settings are
/// independent of each other, so a round box would be claiming something that
/// is not true of them.
fn checkbox(ui: &mut egui::Ui, checked: &mut bool, label: &str) -> egui::Response {
    ui.scope(|ui| {
        let square = egui::CornerRadius::same(4);
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.corner_radius = square;
        widgets.hovered.corner_radius = square;
        widgets.active.corner_radius = square;
        ui.checkbox(checked, label)
    })
    .inner
}

/// Translate an egui key into one that can be registered with Windows.
///
/// Returning `None` is the normal answer for anything without a virtual-key
/// code, such as egui's F25 upward, and simply means the capture keeps waiting.
fn key_from_egui(key: egui::Key) -> Option<Key> {
    use egui::Key as E;

    let name = match key {
        E::ArrowLeft => "Left",
        E::ArrowRight => "Right",
        E::ArrowUp => "Up",
        E::ArrowDown => "Down",
        E::Escape => "Escape",
        E::Tab => "Tab",
        E::Backspace => "Backspace",
        E::Enter => "Enter",
        E::Space => "Space",
        E::Insert => "Insert",
        E::Delete => "Delete",
        E::Home => "Home",
        E::End => "End",
        E::PageUp => "PageUp",
        E::PageDown => "PageDown",
        // Letters, digits and function keys name themselves, give or take the
        // prefix egui puts on digits.
        other => return other.name().strip_prefix("Num").unwrap_or(other.name()).parse().ok(),
    };
    name.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mock::MockPlatform;
    use crate::platform::Platform;

    fn key(name: &str) -> Option<Key> {
        name.parse().ok()
    }

    fn core_targeting_second_monitor() -> Core {
        let platform = MockPlatform::new();
        let monitors = platform.monitors();
        let mut config = Config::default();
        config.set_target(&monitors[1]);
        Core::new(Box::new(platform), config)
    }

    #[test]
    fn the_tray_tooltip_names_the_target_monitor() {
        let tooltip = tray_state(&core_targeting_second_monitor()).tooltip;
        assert!(tooltip.contains("1920x1080"), "got: {tooltip}");
    }

    #[test]
    fn the_tray_tooltip_says_when_no_monitor_is_chosen() {
        let core = Core::new(Box::new(MockPlatform::new()), Config::default());
        let tooltip = tray_state(&core).tooltip;
        assert!(tooltip.contains("no target monitor"), "got: {tooltip}");
    }

    /// Mirrors the Retrieve button. The menu must not offer a restore that
    /// could only produce an error.
    #[test]
    fn the_tray_offers_retrieve_only_once_there_is_something_to_restore() {
        let core = core_targeting_second_monitor();
        assert!(!tray_state(&core).can_retrieve);
    }

    #[test]
    fn capturable_keys_map_onto_bindable_ones() {
        for (pressed, expected) in [
            (egui::Key::A, "A"),
            (egui::Key::Z, "Z"),
            (egui::Key::Num0, "0"),
            (egui::Key::F9, "F9"),
            (egui::Key::F13, "F13"),
            (egui::Key::F24, "F24"),
            (egui::Key::Escape, "Escape"),
            (egui::Key::Tab, "Tab"),
            (egui::Key::Space, "Space"),
            (egui::Key::ArrowLeft, "Left"),
            (egui::Key::PageDown, "PageDown"),
        ] {
            assert_eq!(key_from_egui(pressed), key(expected), "{pressed:?}");
        }
    }

    /// egui models keys Windows has no virtual code for. Capture must ignore
    /// them and keep waiting rather than bind something unregisterable.
    #[test]
    fn keys_windows_cannot_register_are_ignored() {
        assert_eq!(key_from_egui(egui::Key::F35), None);
        assert_eq!(key_from_egui(egui::Key::Plus), None);
    }

    fn texts(log: &StatusLog) -> Vec<&str> {
        log.iter().map(|message| message.text.as_str()).collect()
    }

    /// The whole point of a list. A Send followed by a hotkey report used to
    /// leave no trace of the Send at all.
    #[test]
    fn a_message_is_not_lost_to_the_one_after_it() {
        let mut log = StatusLog::default();
        log.push(Outcome::Ok, "Sent to 1920x1080");
        log.push(Outcome::Err, "Send hotkey unavailable");

        assert_eq!(
            texts(&log),
            vec!["Send hotkey unavailable", "Sent to 1920x1080"],
            "newest first, and the earlier one survives"
        );
    }

    #[test]
    fn only_the_most_recent_few_are_kept() {
        let mut log = StatusLog::default();
        for n in 0..STATUS_HISTORY + 2 {
            log.push(Outcome::Ok, format!("message {n}"));
        }

        assert_eq!(log.iter().count(), STATUS_HISTORY);
        assert_eq!(texts(&log)[0], "message 4", "the newest is still on top");
    }

    /// Pressing Send twice is normal here and says the same thing twice.
    /// Repeating it would push out the messages that give it context.
    #[test]
    fn saying_the_same_thing_twice_running_does_not_repeat_it() {
        let mut log = StatusLog::default();
        log.push(Outcome::Ok, "Sent to 1920x1080");
        log.push(Outcome::Ok, "Sent to 1920x1080");

        assert_eq!(texts(&log), vec!["Sent to 1920x1080"]);
    }

    /// Only when they are consecutive. The same message either side of a
    /// failure is two separate events and reads as one if the second is
    /// swallowed.
    #[test]
    fn the_same_message_after_something_else_is_still_shown() {
        let mut log = StatusLog::default();
        log.push(Outcome::Ok, "Sent to 1920x1080");
        log.push(Outcome::Err, "Could not move the window");
        log.push(Outcome::Ok, "Sent to 1920x1080");

        assert_eq!(
            texts(&log),
            vec!["Sent to 1920x1080", "Could not move the window", "Sent to 1920x1080"]
        );
    }

    /// The same words are not the same message when one is a success and the
    /// other a failure, so the outcome has to be part of the comparison.
    #[test]
    fn the_same_words_with_a_different_outcome_are_two_messages() {
        let mut log = StatusLog::default();
        log.push(Outcome::Ok, "Copied to the clipboard");
        log.push(Outcome::Err, "Copied to the clipboard");

        assert_eq!(log.iter().count(), 2);
    }

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
}

fn apply_style(ctx: &egui::Context) {
    // Without pinning the preference, eframe re-applies the system theme every
    // frame and the custom visuals below are silently discarded. Dark is not a
    // preference here: this sits on screen during a live broadcast, where a
    // white panel spills light and clashes with the rest of the production kit.
    ctx.options_mut(|opt| opt.theme_preference = egui::ThemePreference::Dark);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_gray(20);
    visuals.window_fill = egui::Color32::from_gray(20);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_gray(48);
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.5);

    // Every interactive state, not just the resting one. A radius set on
    // `inactive` alone gives a button that changes shape the moment the
    // pointer touches it, which reads as a rendering fault rather than as a
    // style.
    //
    // One value serves every control because epaint clamps the radius to half
    // of whatever rectangle it is painting, so this is a full pill on the
    // 38px main buttons and on a checkbox alike, without a table of sizes to
    // keep in step with the layout.
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = PILL;
    }

    // Deliberately not a pill. `noninteractive` is what frames the grouped
    // rows in the picker and the panel separators, and a 19px radius on a
    // full-width card makes it a lozenge rather than a container.
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(8);
    visuals.window_corner_radius = egui::CornerRadius::same(10);
    visuals.menu_corner_radius = egui::CornerRadius::same(8);

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    // Wider than it was: a pill curves away from its text at both ends, and
    // the old padding left short labels touching the curve.
    style.spacing.button_padding = egui::vec2(14.0, 6.0);
    ctx.set_style(style);
}
