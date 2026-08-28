//! GPUI front end. Presentation only: every decision lives in `Core`.
//!
//! Laid out after Loom's capture panel, which is the closest thing to what
//! this is: a small always-there utility window with two or three pieces of
//! state and one action that matters. What that borrows is the structure, not
//! the palette —
//!
//! - **The rows are the state and the control.** Loom's "No Camera" row both
//!   reports the camera and changes it. Here the target display, the Zoom
//!   window and the media window do the same, so there is no separate settings
//!   screen for the three things that decide whether Send can work.
//! - **One saturated colour, used once.** Loom spends its orange only on Start
//!   recording. The accent here goes to whichever of Send or Retrieve is the
//!   one to reach for, and to nothing else.
//! - **A drawn header rather than the system titlebar**, so the window reads
//!   as one designed object.
//! - **A footer of icon buttons** for what is occasionally needed and never
//!   urgent.
//!
//! It stays dark where Loom is light. `src/app.rs` records why, and the reason
//! is specific to this application rather than a matter of taste: it sits on
//! screen during a live broadcast, where a white panel spills light onto the
//! operator and clashes with the rest of the production kit.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    Animation, AnimationExt, AssetSource, Context, FocusHandle, FontWeight, Hsla, IntoElement,
    KeyDownEvent, MouseButton, RenderImage, SharedString, Window, div, ease_out_quint, img,
    prelude::*, px, rgb, size, svg,
};

use crate::core::{Core, Failure};
use crate::hotkey::{Action, Hotkey, Key};
use crate::platform::{MonitorInfo, WindowCandidate};
use crate::shell::{self, HotkeyReport, Shell, ShellEvent, TrayState};
use crate::update::{self, Release, UpdateEvent, Updater};
use crate::watch::{Fade, MediaRestore, SettleClock, Started, Tick, SETTLE_LOOK};

pub const WIDTH: f32 = 340.0;

/// The height with nothing expanded: the shape that sits on screen during a
/// broadcast, and the one worth keeping small.
pub const HEIGHT: f32 = 416.0;

/// The chooser beside the surface. Flush against it, divided by a rule rather
/// than a gap, so the two read as one window with two columns.
const PANEL_WIDTH: f32 = 340.0;

/// Deliberately the same with the mock controls compiled in as without.
///
/// `app.rs` buys room for its own, because they sit on the live surface, which
/// does not scroll. These sit at the foot of a panel that already does, and a
/// window grows downward from where it opened — so paying for them in height
/// would push the foot of Settings under the dock to save a scroll that costs
/// nothing.
const SETTINGS_HEIGHT: f32 = 512.0;
const HOTKEYS_HEIGHT: f32 = 260.0;

const PICKER_ROW: f32 = 36.0;
const ROW_HEIGHT: f32 = 56.0;

/// The colours the surface is drawn from.
///
/// Read through functions rather than named as constants, because the whole
/// point is that they change: a parameter threaded through forty layout
/// helpers would be forty signatures describing a thing none of them decide.
/// GPUI's own theme is a global for the same reason.
struct Palette {
    bg: u32,
    /// The row fill, one step off the panel.
    row: u32,
    row_hover: u32,
    border: u32,
    text: u32,
    subdued: u32,
    faint: u32,
    /// What a chip's own colour is read against.
    chip_text: u32,
    ok: u32,
    warn: u32,
    err: u32,
}

/// Dark is the default, and the reason is specific to this application rather
/// than a matter of taste: it sits on screen during a live broadcast, where a
/// white panel spills light onto the operator and clashes with the rest of the
/// production kit. Light is offered for everywhere else.
const DARK: Palette = Palette {
    bg: 0x141414,
    row: 0x232323,
    row_hover: 0x2e2e2e,
    border: 0x303030,
    text: 0xf2f2f2,
    subdued: 0x8f8f8f,
    faint: 0x5e5e5e,
    chip_text: 0x141414,
    ok: 0x66bb7a,
    warn: 0xe0a458,
    err: 0xe26a6a,
};

/// Loom's own panel, near enough: a white surface with rows a step darker
/// rather than a step lighter. The status colours are darkened, because the
/// dark ones were chosen to carry against near-black and wash out on white.
const LIGHT: Palette = Palette {
    bg: 0xffffff,
    row: 0xf1f2f4,
    row_hover: 0xe6e8ec,
    border: 0xdfe1e6,
    text: 0x1a1c1f,
    subdued: 0x6b7280,
    faint: 0x9aa1ac,
    chip_text: 0xffffff,
    ok: 0x2e8b4f,
    warn: 0xb26a12,
    err: 0xc4322a,
};

/// Which palette is live. A `u8` rather than a lock: it is written when the
/// setting is toggled and read while laying out, both on the one thread GPUI
/// renders from.
static PALETTE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn set_palette(light: bool) {
    PALETTE.store(u8::from(light), std::sync::atomic::Ordering::Relaxed);
}

fn palette() -> &'static Palette {
    if PALETTE.load(std::sync::atomic::Ordering::Relaxed) == 1 { &LIGHT } else { &DARK }
}

fn bg() -> u32 {
    palette().bg
}
fn row_fill() -> u32 {
    palette().row
}
fn row_hover() -> u32 {
    palette().row_hover
}
fn border() -> u32 {
    palette().border
}
fn text() -> u32 {
    palette().text
}
fn subdued() -> u32 {
    palette().subdued
}
fn faint() -> u32 {
    palette().faint
}
fn chip_text() -> u32 {
    palette().chip_text
}
fn ok() -> u32 {
    palette().ok
}
fn warn() -> u32 {
    palette().warn
}
fn err() -> u32 {
    palette().err
}

/// Selection, and the action that puts something out. The same in both
/// palettes: it is the one thing that should read identically wherever this
/// is running.
const ACCENT: u32 = 0x4e8ef0;
const ACCENT_HOVER: u32 = 0x6ba2f5;
/// Retrieve, once there is something to retrieve.
///
/// Warm because of what it is for. Send is the considered half — nothing is on
/// air yet and there is time. Retrieve is the half pressed when something is
/// already out and needs to come back, which is the closest thing this has to
/// an emergency, and it is worth being the one warm thing on the surface.
const WARM: u32 = 0xe8593f;
const WARM_HOVER: u32 = 0xf06a52;
/// What sits on top of an accent or warm fill, in either palette.
const ON_ACCENT: u32 = 0xffffff;

/// How long the toast takes to arrive and to leave.
///
/// One thing animates, and it is the only thing that can be animated honestly.
/// GPUI fires an animation when an element appears and offers no hook for one
/// being removed, so anything whose disappearance it does not control fades in
/// and then vanishes — and a one-sided animation draws the eye to exactly the
/// half that could not be animated. The toast is the exception because its
/// whole life is already on a timer here, which is what makes an exit possible
/// at all.
///
/// The panel is not animated for a different reason: the window jumps to its
/// new width in a single step, and a fade drawn over that leaves an empty
/// column visible until it catches up. The window growing is the motion.
///
/// Out is quicker than in, which is the usual ratio: an exit that takes as
/// long as an entrance feels like the interface is reluctant.
const TOAST_IN: std::time::Duration = std::time::Duration::from_millis(200);
const TOAST_OUT: std::time::Duration = std::time::Duration::from_millis(160);

/// How long a toast stays up.
///
/// Long enough to read one line, short enough that it is gone before the
/// operator needs the controls under it. A failure holds longer, because it is
/// the one worth reading twice.
const TOAST_LIFE: std::time::Duration = std::time::Duration::from_secs(3);
const TOAST_LIFE_FAILED: std::time::Duration = std::time::Duration::from_secs(6);

/// How often the shell is drained.
///
/// eframe's waker is `ctx.request_repaint()`, which any thread may call. GPUI
/// has no equivalent: `AsyncApp` is `!Send`, so nothing the hotkey thread holds
/// can reach the main loop. The waker it is handed is therefore a no-op and
/// this timer is the wake — a task that drains the queue and only calls
/// `notify` when it found something, so an idle window stays idle.
///
/// Fifty milliseconds is a third of the rate eframe polls at, and well under
/// the time it takes to notice a keypress did nothing. `Shell::poll` drains a
/// mutex-guarded `Vec` and costs nothing when it is empty.
const SHELL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// How often a running fade is advanced.
///
/// The ramp is 200ms, so this is a dozen steps — enough that the opacity moves
/// smoothly and few enough that the desktop is not being written to sixty
/// times a second for a fifth of a second. `app.rs` steps its own per frame,
/// which on a 60Hz display is this number.
const FADE_STEP: std::time::Duration = std::time::Duration::from_millis(16);

/// How often a put-back player is looked at.
///
/// `app.rs` looks once per frame, which on a 60Hz display is this. The watch
/// runs for at most two seconds and each look is one enumeration of the
/// desktop, so the cost is bounded by the watch rather than by the frame rate.
const MEDIA_RESTORE_LOOK: std::time::Duration = std::time::Duration::from_millis(16);

const HEADER_HEIGHT: f32 = 44.0;

/// Where the drawn header's own content starts.
///
/// With the system titlebar transparent, macOS still draws its close, minimise
/// and zoom buttons over whatever is there, so the header's content has to
/// begin to the right of them. Windows draws none and starts at the normal
/// margin. That is also why the window carries its own minimise and close: on
/// Windows they are the only ones there are.
#[cfg(target_os = "macos")]
const TITLE_INSET: f32 = 88.0;
#[cfg(not(target_os = "macos"))]
const TITLE_INSET: f32 = 16.0;

/// The icons, compiled in rather than read from disk.
///
/// The same bargain the tray and window icons already strike: a few kilobytes
/// in the executable, against a utility that cannot draw its own interface if
/// someone moves the folder it shipped in.
///
/// GPUI paints an SVG as a mask tinted by the element's `text_color`, so
/// whatever colour is written in the file is discarded. Icons are single solid
/// shapes on a 24x24 viewBox for that reason.
pub struct Icons;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        impl AssetSource for Icons {
            fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
                Ok(match path {
                    $(concat!("icons/", $name, ".svg") => Some(Cow::Borrowed(
                        include_bytes!(concat!("../assets/icons/", $name, ".svg")).as_slice(),
                    )),)*
                    _ => None,
                })
            }

            fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
                Ok(vec![$(concat!("icons/", $name, ".svg").into()),*])
            }
        }
    };
}

icons![
    "display", "video", "media", "keyboard", "settings", "info", "minimize", "close", "check",
    "alert", "back", "chevron",
];

/// What the surface is currently able to do.
///
/// Derived on every render rather than stored, because every input to it —
/// the confirmed window, the target monitor, the restore point — already lives
/// in `Core` and a second copy would be a second thing to keep true.
#[derive(PartialEq)]
enum State {
    /// Send cannot succeed yet: a display, a window, or both are missing.
    Blocked,
    /// Everything Send needs is in place, and nothing is currently out.
    Ready,
    /// A window is on the target display right now.
    Sent,
}

/// What a row reports about itself, in Loom's "Off"/"On" chip position.
enum Chip {
    Set,
    Needed,
    Optional,
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

#[derive(Clone)]
struct Message {
    text: SharedString,
    failed: bool,
}

/// Which window the picker is choosing.
#[derive(PartialEq, Clone, Copy)]
pub enum PickerFor {
    Zoom,
    Media,
}

/// What the window is showing.
///
/// The live controls are never a screen away — `Main` is where the window
/// opens and where it returns — but the picker, the settings and the hotkeys
/// each take the whole surface when asked for. `src/app.rs` records why the
/// eframe version refused to navigate: a window that resized on every
/// navigation jumped size when nobody had asked it to. Here the only way into
/// one of these is a press, so the resize is always something the user asked
/// for.
#[derive(PartialEq, Clone, Copy)]
enum Screen {
    Main,
    Settings,
    Hotkeys,
}

/// A chooser, open beside the surface rather than over it.
///
/// Loom opens its camera and screen pickers as a panel to the side, with the
/// row that opened it still filled and still readable. That is worth copying:
/// a picker that replaces the surface takes away the context you are picking
/// for. GPUI's native anchored popup is rejected on both macOS and Windows, so
/// this is one window that grows sideways rather than a second window placed
/// by hand — which in an application about multi-monitor placement is the
/// hand-placing worth avoiding.
#[derive(PartialEq, Clone, Copy)]
enum Side {
    Display,
    Window(PickerFor),
}

pub struct WinSendGpui {
    core: Core,
    /// Global hotkeys and the tray icon. Polled rather than pushed; see
    /// [`SHELL_POLL`].
    shell: Box<dyn Shell>,
    /// Which bindings the shell refused, so the offending row can say so
    /// rather than the reason living only in a toast that has since gone.
    hotkey_report: HotkeyReport,
    /// A Retrieve fading the window out, if one is running.
    fading: Option<Fade>,
    /// The placement watch on the window last placed, if one is running.
    settling: Option<SettleClock>,
    settle_seq: u64,
    /// The full-screen watch on put-back players, if one is running.
    media_restore: Option<MediaRestore>,
    media_seq: u64,
    updater: Box<dyn Updater>,
    update: UpdateState,
    /// Set when an update has been installed, so `main` can start the new
    /// executable after this one has finished putting the desktop back.
    relaunch: Arc<std::sync::atomic::AtomicBool>,
    /// Which fade the running stepper belongs to, so a fade ended early
    /// cannot be advanced by the task armed for the one before it. The same
    /// guard the toast uses, for the same reason.
    fade_seq: u64,
    /// The message currently showing, if any. One at a time and transient:
    /// a strip reserved for messages costs the live surface its height every
    /// day for something that is on screen for three seconds.
    toast: Option<Message>,
    /// Which toast the pending dismissal belongs to, so a newer message is not
    /// cleared by the timer armed for the one it replaced.
    toast_seq: u64,
    /// Set while the toast is fading out. It is still mounted through this —
    /// an element GPUI has already removed cannot be animated.
    toast_leaving: bool,
    screen: Screen,
    /// The chooser open beside the surface, if any.
    side: Option<Side>,
    /// The candidates the picker is showing, held rather than re-read each
    /// frame: enumerating every window on the desktop is not a thing to do
    /// sixty times a second, and a list that reordered under the pointer
    /// would be worse than a slightly stale one.
    candidates: Vec<WindowCandidate>,
    /// Captured once when the picker opens. Zoom's main and video windows are
    /// identical in process, class and title, so the picture is the only thing
    /// that tells them apart — which is what makes this worth the trouble.
    thumbnails: HashMap<u64, Arc<RenderImage>>,
    /// The action whose binding is being captured, if any.
    capturing: Option<Action>,
    focus_handle: FocusHandle,
    /// The size last asked for, so the window is resized when the layout
    /// changes shape rather than on every frame.
    applied: (f32, f32),
}

impl WinSendGpui {
    pub fn new(
        core: Core,
        relaunch: Arc<std::sync::atomic::AtomicBool>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Once, here, because the window exists by the time this runs and
        // neither of the two things below is something that needs
        // re-asserting per frame.
        if let Some(handle) = own_window_handle(window) {
            core.platform.apply_window_chrome(handle);
            // Always-on-top. eframe asks for it as a window flag; GPUI has no
            // equivalent in WindowOptions, so it goes on through the handle —
            // and `raise` is already exactly that call, so this needs no Win32
            // of its own.
            let _ = core.platform.raise(handle, true);
        }

        // The waker is a no-op on purpose: nothing GPUI exposes can be called
        // from the shell's thread to wake this one, so `poll_shell` below is
        // what the queue is drained by. See [`SHELL_POLL`].
        let shell = shell::create(Arc::new(|| {}));
        shell.apply_hotkeys(core.config.hotkeys);

        // The same no-op waker, for the same reason: the updater answers from
        // a thread of its own and `poll_shell` drains it on the same timer.
        let updater = update::create(Arc::new(|| {}));
        // Once per launch, and only if the user has not turned it off. Never
        // blocking: this returns immediately and the answer arrives later or
        // not at all.
        if core.config.check_for_updates {
            updater.check();
        }

        let mut app = Self {
            core,
            shell,
            hotkey_report: HotkeyReport::default(),
            fading: None,
            fade_seq: 0,
            settling: None,
            settle_seq: 0,
            media_restore: None,
            media_seq: 0,
            updater,
            update: UpdateState::Quiet,
            relaunch,
            toast: None,
            toast_seq: 0,
            toast_leaving: false,
            screen: Screen::Main,
            side: None,
            candidates: Vec::new(),
            thumbnails: HashMap::new(),
            capturing: None,
            focus_handle: cx.focus_handle(),
            applied: (WIDTH, HEIGHT),
        };
        set_palette(app.core.config.light_theme);
        app.open_requested_screen();
        app.poll_shell(cx);
        app
    }

    /// Drain the shell for as long as this window exists.
    ///
    /// One task rather than one per source, and in the same order
    /// `app.rs`'s frame runs them in: what arrived from outside is taken in
    /// first, then the tray is told what to say about the state that left it.
    fn poll_shell(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SHELL_POLL).await;
                let updated = this
                    .update_in(cx, |this, window, cx| {
                        let events = this.shell.poll();
                        let found = this.updater.poll();
                        let anything = !events.is_empty() || !found.is_empty();
                        for event in events {
                            this.handle(event, window, cx);
                        }
                        for event in found {
                            this.handle_update(event, cx);
                        }
                        // Cheap every time: the shell drops a state that has
                        // not moved rather than talking to the OS about it.
                        this.refresh_tray();
                        if anything {
                            cx.notify();
                        }
                    })
                    .is_ok();
                // A failed update is not on its own a reason to stop: the app
                // can be mid-borrow, or on its way down. The view having gone
                // is, and it is the only thing that ends this loop.
                if !updated && this.upgrade().is_none() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Something the user asked for from outside the window.
    fn handle(&mut self, event: ShellEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            ShellEvent::Trigger(action) => self.perform(action, cx),
            // A refusal is shown the moment it is known. A binding the user
            // believes is live but which never registered is the one failure
            // this feature cannot afford.
            ShellEvent::HotkeysApplied(report) => {
                if let Some(summary) = report.summary() {
                    self.note(&summary, true, cx);
                }
                self.hotkey_report = report;
            }
            ShellEvent::ShowWindow => window.activate_window(),
            ShellEvent::ShowSettings => {
                self.screen = Screen::Settings;
                self.close_side();
                window.activate_window();
            }
            // The only path that actually exits. The fade goes first: one
            // abandoned here would leave Zoom's window translucent after this
            // process has gone, with nothing left running that could put it
            // back.
            ShellEvent::Quit => {
                self.finish_fade(cx);
                cx.quit();
            }
        }
    }

    /// Take in whatever the updater has found.
    fn handle_update(&mut self, event: UpdateEvent, cx: &mut Context<Self>) {
        match event {
            UpdateEvent::Available(release) => self.update = UpdateState::Available(release),
            // The executable on disk is the new one now, so this process has
            // to give way to it. `main` starts the replacement once this one
            // has finished putting the desktop back.
            UpdateEvent::Installed => {
                self.relaunch.store(true, std::sync::atomic::Ordering::SeqCst);
                cx.quit();
            }
            // The old executable is still the one on disk, so the offer goes
            // back up rather than disappearing with the explanation.
            UpdateEvent::Failed(why) => {
                self.note(&why, true, cx);
                if let UpdateState::Installing(release) =
                    std::mem::replace(&mut self.update, UpdateState::Quiet)
                {
                    self.update = UpdateState::Available(release);
                }
            }
        }
    }

    /// Ask for the update, or warn first when warning is the point.
    ///
    /// `Core` holds the restore point in memory and it is session-scoped by
    /// design, so restarting while a window is still sent leaves Zoom on the
    /// wrong monitor with nothing left able to put it back.
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

    /// Keep the tray icon in step with the window.
    fn refresh_tray(&self) {
        self.shell.set_tray_state(tray_state(&self.core));
    }

    /// Begin capturing a combination for `action`.
    ///
    /// The registered bindings are dropped first. A registered hotkey is
    /// swallowed by the OS and never reaches this window, so without this,
    /// rebinding a key to itself — or to the other action's key — would look
    /// like the capture had simply stopped working.
    fn start_capture(&mut self, action: Action) {
        self.capturing = Some(action);
        self.shell.apply_hotkeys(Default::default());
    }

    /// Stop capturing, however it ended, and put the bindings back.
    ///
    /// The one way out, so a capture abandoned by pressing Cancel or by
    /// leaving the screen cannot leave the hotkeys unregistered.
    fn end_capture(&mut self) {
        self.capturing = None;
        self.shell.apply_hotkeys(self.core.config.hotkeys);
    }

    /// Open straight onto a surface, the same way the eframe binary does.
    ///
    /// Debug only: a release build has no business reading this, and the
    /// surfaces are all a press away in any case.
    fn open_requested_screen(&mut self) {
        #[cfg(debug_assertions)]
        match std::env::var("WINSEND_SCREEN").as_deref() {
            Ok("select") => self.open_side(Side::Window(PickerFor::Zoom)),
            Ok("media") => self.open_side(Side::Window(PickerFor::Media)),
            Ok("display") => self.open_side(Side::Display),
            // Goes through Core against the mock desktop, so what is on
            // screen afterwards is the real sent state rather than a flag.
            // The window has to be confirmed first: the config remembers what
            // it was, but a handle does not survive a restart, and Send
            // re-validates one on every use.
            Ok("sent") => {
                if let Some(candidate) =
                    self.core.candidates().into_iter().find(|c| c.likely_zoom)
                {
                    drop(self.core.confirm_window(&candidate));
                    drop(self.core.send());
                }
            }
            Ok("settings") => self.screen = Screen::Settings,
            Ok("hotkeys") => self.screen = Screen::Hotkeys,
            _ => {}
        }
    }



    fn open_side(&mut self, side: Side) {
        // A second press on the row that opened it closes it, the way a menu
        // button works.
        if self.side == Some(side) {
            self.close_side();
            return;
        }
        if let Side::Window(picker) = side {
            self.load_candidates(picker);
        }
        self.side = Some(side);
    }

    fn close_side(&mut self) {
        self.side = None;
        self.candidates.clear();
        self.thumbnails.clear();
    }

    fn load_candidates(&mut self, picker: PickerFor) {
        self.candidates = match picker {
            PickerFor::Zoom => self.core.candidates(),
            PickerFor::Media => self.core.media_candidates(),
        };
        self.thumbnails.clear();
        for candidate in &self.candidates {
            if let Some(shot) = self.core.platform.thumbnail(candidate.handle) {
                if let Some(image) = render_image(&shot) {
                    self.thumbnails.insert(candidate.handle, image);
                }
            }
        }
    }

    fn confirm(&mut self, picker: PickerFor, candidate: &WindowCandidate, cx: &mut Context<Self>) {
        let outcome = match picker {
            PickerFor::Zoom => self.core.confirm_window(candidate),
            PickerFor::Media => self.core.confirm_media_window(candidate),
        };
        self.report(outcome, cx);
        self.close_side();
    }

    /// Turn a captured keystroke into a binding, or say why it cannot be one.
    ///
    /// The rules live in `Hotkey` and `Core`, not here: a bare key is refused
    /// because a global binding swallows it everywhere, and a duplicate is
    /// refused because two actions cannot share one combination. This only
    /// translates GPUI's keystroke into the shape those rules are written in.
    fn capture(&mut self, action: Action, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        // A press that is only a modifier is the operator on their way to the
        // combination, not the combination.
        let Some(key) = key_from_keystroke(&event.keystroke.key) else {
            return;
        };
        let hotkey = Hotkey {
            ctrl: modifiers.control,
            alt: modifiers.alt,
            shift: modifiers.shift,
            win: modifiers.platform,
            key,
        };
        let outcome = self.core.set_hotkey(action, Some(hotkey));
        self.report(outcome, cx);
        // After the config has the new binding, so what is registered is what
        // was just set rather than what it replaced.
        self.end_capture();
    }

    fn state(&self) -> State {
        if self.core.can_retrieve() {
            State::Sent
        } else if self.target().is_none() || self.core.config.zoom_window.is_none() {
            State::Blocked
        } else {
            State::Ready
        }
    }

    fn target(&self) -> Option<MonitorInfo> {
        self.core.config.resolve_monitor(&self.core.monitors()).cloned()
    }

    fn report(&mut self, outcome: Result<String, Failure>, cx: &mut Context<Self>) {
        match outcome {
            Ok(message) => self.note(&message, false, cx),
            Err(failure) => self.note(&failure.to_string(), true, cx),
        }
    }

    /// Put a message up, and take it down again on its own.
    ///
    /// The dismissal is a task rather than a clock checked each frame, so an
    /// idle window stays idle instead of repainting to find out whether three
    /// seconds have passed.
    fn note(&mut self, text: &str, failed: bool, cx: &mut Context<Self>) {
        self.toast_seq += 1;
        let seq = self.toast_seq;
        self.toast = Some(Message { text: text.to_string().into(), failed });
        self.toast_leaving = false;

        let life = if failed { TOAST_LIFE_FAILED } else { TOAST_LIFE };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(life).await;
            // Two steps, because the fade has to happen while the toast is
            // still mounted. The first marks it leaving and the second takes
            // it away once the fade has run.
            this.update(cx, |this, cx| {
                // Only if nothing has been said since. A newer message owns
                // the toast and its own timer.
                if this.toast_seq == seq {
                    this.toast_leaving = true;
                    cx.notify();
                }
            })
            .ok();
            cx.background_executor().timer(TOAST_OUT).await;
            this.update(cx, |this, cx| {
                if this.toast_seq == seq {
                    this.toast = None;
                    this.toast_leaving = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Take the toast down before its timer does.
    ///
    /// Bumps the sequence too, so the task still waiting on the dismissed
    /// message cannot clear whatever is put up next.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        // A second press while it is already going does nothing, rather than
        // restarting the fade from wherever it had got to.
        if self.toast.is_none() || self.toast_leaving {
            return;
        }
        self.toast_seq += 1;
        let seq = self.toast_seq;
        self.toast_leaving = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_OUT).await;
            this.update(cx, |this, cx| {
                if this.toast_seq == seq {
                    this.toast = None;
                    this.toast_leaving = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Run an action, however it was asked for. A hotkey press and a button
    /// click are the same thing by the time they reach here.
    fn perform(&mut self, action: Action, cx: &mut Context<Self>) {
        // A press arriving mid-fade finishes it at once rather than queueing
        // behind it. The operator pressing a key twice means they want it
        // done, not animated twice.
        let interrupted_a_fade = self.finish_fade(cx);

        match action {
            // Send is the urgent half and still has to happen. It just acts on
            // a window that has finished moving rather than on one caught part
            // way through being moved.
            Action::Send => {
                let outcome = self.core.send();
                self.report(outcome, cx);
                self.arm_watches(cx);
            }
            // The fade that was just cut short *was* the Retrieve. Starting
            // another would only produce "nothing has been sent yet".
            Action::Retrieve if interrupted_a_fade => {}
            Action::Retrieve => self.start_retrieve(cx),
            Action::RestoreMedia => {
                let outcome = self.core.restore_media();
                self.report(outcome, cx);
                self.arm_watches(cx);
            }
        }
    }

    /// Retrieve, with the fade when it is wanted and a hard cut when it is not.
    fn start_retrieve(&mut self, cx: &mut Context<Self>) {
        if !self.core.config.fade_on_retrieve {
            let outcome = self.core.retrieve();
            self.report(outcome, cx);
            self.arm_watches(cx);
            return;
        }

        match Fade::begin(&mut self.core, std::time::Instant::now()) {
            Started::Fading(fade) => {
                self.fading = Some(fade);
                self.step_fade(cx);
            }
            Started::Cut(outcome) => {
                self.report(outcome, cx);
                self.arm_watches(cx);
            }
        }
    }

    /// Drive the running fade to its end.
    ///
    /// A task of its own rather than a branch of the shell poll: 200ms wants
    /// a dozen steps, and pulling the whole poll loop up to that rate for the
    /// one fifth of a second a fade lasts would be paying for it always.
    fn step_fade(&mut self, cx: &mut Context<Self>) {
        self.fade_seq += 1;
        let seq = self.fade_seq;

        Self::every(FADE_STEP, cx, move |this, cx| {
            // Superseded: another press ended this fade and either started a
            // new one or finished the Retrieve outright.
            if this.fade_seq != seq {
                return false;
            }
            let Some(fade) = this.fading.take() else {
                return false;
            };
            match fade.advance(&mut this.core, std::time::Instant::now()) {
                None => {
                    this.fading = Some(fade);
                    true
                }
                Some(outcome) => {
                    this.report(outcome, cx);
                    this.arm_watches(cx);
                    cx.notify();
                    false
                }
            }
        });
    }

    /// End a fade now, wherever it had got to. Says whether there was one.
    ///
    /// The interruption half of the guarantee in `watch.rs`: every path out of
    /// the animation runs through `Fade::finish`, so the window lands opaque
    /// whether the ramp completed, another press cut it short, or the
    /// application is going down.
    fn finish_fade(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(fade) = self.fading.take() else {
            return false;
        };
        // Supersede the stepper: its next tick finds a bumped sequence and
        // stops rather than advancing a fade that is already over.
        self.fade_seq += 1;
        let outcome = fade.finish(&mut self.core);
        self.report(outcome, cx);
        self.arm_watches(cx);
        true
    }

    /// Start the clocks on whatever the action just queued.
    ///
    /// Both watches begin at the same moment and for the same reason: the
    /// action has reported, so the window has finished moving and everything
    /// that happens next is the desktop reacting rather than us acting. They
    /// are separate watches because they measure different things — where the
    /// sent window ended up, and whether a displaced player came back to full
    /// screen — and either can be running without the other.
    fn arm_watches(&mut self, cx: &mut Context<Self>) {
        self.arm_media_restore(cx);
        self.arm_settle(cx);
    }

    /// Watch where the window that was just placed comes to rest.
    fn arm_settle(&mut self, cx: &mut Context<Self>) {
        self.settling = SettleClock::begin(&self.core, std::time::Instant::now());
        if self.settling.is_none() {
            return;
        }
        // Supersedes any watch already running, the same way a new placement
        // supersedes the one before it.
        self.settle_seq += 1;
        let seq = self.settle_seq;

        Self::every(SETTLE_LOOK, cx, move |this, cx| {
            if this.settle_seq != seq {
                return false;
            }
            let Some(mut clock) = this.settling else {
                return false;
            };
            match clock.tick(&mut this.core, std::time::Instant::now()) {
                Tick::Watching => {
                    this.settling = Some(clock);
                    true
                }
                Tick::Done(report) => {
                    this.settling = None;
                    this.say(report, cx);
                    false
                }
            }
        });
    }

    /// Watch whether a player Retrieve gave the display back to takes it.
    fn arm_media_restore(&mut self, cx: &mut Context<Self>) {
        self.media_restore = MediaRestore::begin(&self.core, std::time::Instant::now());
        if self.media_restore.is_none() {
            return;
        }
        self.media_seq += 1;
        let seq = self.media_seq;

        Self::every(MEDIA_RESTORE_LOOK, cx, move |this, cx| {
            if this.media_seq != seq {
                return false;
            }
            let Some(mut watch) = this.media_restore else {
                return false;
            };
            let fading = this.fading.is_some();
            match watch.tick(&mut this.core, std::time::Instant::now(), fading) {
                Tick::Watching => {
                    this.media_restore = Some(watch);
                    true
                }
                Tick::Done(report) => {
                    this.media_restore = None;
                    this.say(report, cx);
                    false
                }
            }
        });
    }

    /// Put what a watch had to say up, if it had anything.
    ///
    /// Silence is the common answer and the deliberate one: a placement that
    /// held and a player that came back on its own both already had their
    /// outcome reported by the action that started them.
    fn say(&mut self, report: Option<crate::watch::Report>, cx: &mut Context<Self>) {
        let Some(report) = report else {
            return;
        };
        match report {
            Ok(message) => self.note(&message, false, cx),
            Err(message) => self.note(&message, true, cx),
        }
        // These arrive on a timer rather than out of a press, so there is no
        // click on its way to redraw the window for them.
        cx.notify();
    }

    /// Run `step` on a timer for as long as it says to carry on.
    ///
    /// The shape both watches share. A task rather than a clock read while
    /// rendering, because a window with nothing to draw should not be drawing
    /// frames to find out what time it is.
    fn every(
        interval: std::time::Duration,
        cx: &mut Context<Self>,
        step: impl Fn(&mut Self, &mut Context<Self>) -> bool + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                // A released view answers `false`, which ends the watch along
                // with the window it was watching for.
                if !this.update(cx, |this, cx| step(this, cx)).unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }
    fn width(&self) -> f32 {
        if self.side.is_some() { WIDTH + PANEL_WIDTH } else { WIDTH }
    }

    /// The surface decides how tall the window is, and always.
    ///
    /// A chooser scrolls inside that rather than stretching the window to fit
    /// its list. There is no bound on how many windows are open on a desktop,
    /// and a window that grew to hold all of them would be a window taller
    /// than the screen at the worst moment.
    fn height(&self) -> f32 {
        match self.screen {
            Screen::Settings => SETTINGS_HEIGHT,
            Screen::Hotkeys => HOTKEYS_HEIGHT,
            Screen::Main => HEIGHT,
        }
    }
}

/// Our own window's OS handle, in the same opaque form `Platform` speaks.
///
/// Kept free of `cfg` attributes: `RawWindowHandle` names every platform's
/// variant on every platform, so this compiles as written on macOS and simply
/// answers `None` there. Returning `None` is a normal answer and means only
/// that there is no native frame to ask anything of.
fn own_window_handle(window: &Window) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // Called through the trait: `Window` has an inherent `window_handle` of
    // its own, returning GPUI's `AnyWindowHandle`, which shadows this one.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as u64),
        _ => None,
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

/// The backstop under every exit path out of a fade.
///
/// Quit finishes one explicitly, and so does a press that interrupts it, but
/// neither covers the window simply going away — which on macOS is what
/// closing it means. The view is dropped either way, and the one thing that
/// must not outlive this process is a Zoom window left at forty percent on
/// camera. `Fade::finish` is idempotent about the opacity, so running here
/// after it has already run costs a call that clears nothing.
impl Drop for WinSendGpui {
    fn drop(&mut self) {
        if let Some(fade) = self.fading.take() {
            let _ = fade.finish(&mut self.core);
        }
    }
}

fn tint(colour: u32, alpha: f32) -> Hsla {
    Hsla::from(rgb(colour)).opacity(alpha)
}

fn icon(name: &'static str, size: f32, colour: u32) -> gpui::Svg {
    svg()
        .path(format!("icons/{name}.svg"))
        .w(px(size))
        .h(px(size))
        .text_color(rgb(colour))
}

/// "Display 2" out of `\\.\DISPLAY2`.
///
/// The geometry is the detail, not the name. `MonitorInfo::label` reads as a
/// diagnostic — right in the picker and the diagnostics dump, wrong on a thing
/// clicked under pressure.
fn display_name(monitor: &MonitorInfo) -> String {
    let digits: String = monitor.id.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() { "Display".to_string() } else { format!("Display {digits}") }
}

fn display_detail(monitor: &MonitorInfo) -> String {
    format!(
        "{}x{}{}",
        monitor.bounds.width,
        monitor.bounds.height,
        if monitor.is_primary { " · primary" } else { "" }
    )
}

/// The chip on the right of a row: Loom's "Off"/"On" pill.
fn chip(kind: Chip, on_accent: bool) -> impl IntoElement {
    let (label, colour) = match kind {
        Chip::Set => ("SET", ok()),
        Chip::Needed => ("NEEDED", warn()),
        Chip::Optional => ("OPTIONAL", faint()),
    };
    // Solid rather than tinted, the way Loom's "Off" is solid red: a chip that
    // is a wash of its own colour reads as decoration. On an accent row it
    // inverts to white, since the fill it was carrying is now the row.
    let (fill, label_tone) =
        if on_accent { (ON_ACCENT, ACCENT) } else { (colour, chip_text()) };

    div()
        .flex_none()
        .whitespace_nowrap()
        .px_2()
        .py(px(3.))
        .rounded_md()
        .bg(rgb(fill))
        .text_size(px(9.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(label_tone))
        .child(label)
}

/// One of Loom's grey pill rows: an icon, a label, and the state on the right.
///
/// Clicking it is how the thing it reports gets changed, which is the whole
/// reason there is no separate settings screen for these three.
fn row(
    id: &'static str,
    glyph: &'static str,
    label: String,
    right: gpui::AnyElement,
    muted: bool,
    open: bool,
) -> gpui::Stateful<gpui::Div> {
    // The row it is standing in becomes the selection, the way Loom's camera
    // row fills solid while its picker is showing. It is a much louder signal
    // than a border, and it ties the panel that opened to the thing that
    // opened it without an arrow drawn between them.
    let (fill, hover_fill, glyph_tone, label_tone) = match (open, muted) {
        (true, _) => (ACCENT, ACCENT, ON_ACCENT, ON_ACCENT),
        (false, true) => (row_fill(), row_hover(), subdued(), subdued()),
        (false, false) => (row_fill(), row_hover(), text(), text()),
    };

    div()
        .id(id)
        .group(id)
        .flex()
        .items_center()
        .gap_3()
        .w_full()
        .h(px(ROW_HEIGHT))
        .pl_4()
        .pr_3()
        // A rounded rectangle, not a pill. Only the action underneath is a
        // pill, which is what makes it read as the one thing to press.
        .rounded_xl()
        .bg(rgb(fill))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(hover_fill)))
        .active(|style| style.opacity(0.8))
        .child(icon(glyph, 19.0, glyph_tone))
        // One line. The value is the label, the way Loom's row says "No
        // Camera" rather than "Camera" with the answer underneath.
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(13.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(label_tone))
                .child(label),
        )
        .child(right)
}

/// The one saturated control, and the only filled thing on the surface.
fn cta(
    id: &'static str,
    label: &'static str,
    filled: Option<u32>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let (fill, text) = match (enabled, filled) {
        (false, _) => (bg(), faint()),
        (true, Some(accent)) => (accent, ON_ACCENT),
        (true, None) => (row_fill(), text()),
    };
    let hover = match filled {
        Some(ACCENT) => ACCENT_HOVER,
        Some(_) => WARM_HOVER,
        None => row_hover(),
    };
    div()
        .id(id)
        .flex()
        .justify_center()
        .items_center()
        .w_full()
        .h(px(46.))
        .rounded_full()
        .bg(rgb(fill))
        .when(!enabled, |this| this.border_1().border_color(rgb(border())))
        .text_size(px(14.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(text))
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(rgb(hover)))
                // Presses register. Without it the only feedback that a click
                // landed is whatever the action itself does, which for Send is
                // a window moving on another display.
                .active(|style| style.opacity(0.82))
        })
        .child(label)
}

/// A footer button: an icon over a label, and nothing drawn around it.
///
/// Loom's Effects, Notes and More are bare glyphs. A circle around each one
/// gave three more filled shapes to a surface that already has rows and a
/// button, and made the least important controls the heaviest.
fn footer_button(
    id: &'static str,
    glyph: &'static str,
    label: &'static str,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .group(id)
        .flex()
        .flex_col()
        .items_center()
        .gap_1p5()
        .px_3()
        .py_1()
        .cursor_pointer()
        .active(|style| style.opacity(0.6))
        .child(
            icon(glyph, 19.0, subdued())
                .group_hover(id, |style| style.text_color(rgb(text()))),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(subdued()))
                .group_hover(id, |style| style.text_color(rgb(text())))
                .child(label),
        )
}

/// A window thumbnail, converted for GPUI.
///
/// `RenderImage` is BGRA and `Thumbnail` is RGBA, so the red and blue channels
/// are swapped on the way in. Without it every preview comes out looking like
/// a colour-negative, which reads as a broken capture rather than a wrong
/// channel order.
fn render_image(shot: &crate::platform::Thumbnail) -> Option<Arc<RenderImage>> {
    let mut bgra = shot.rgba.clone();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer = image::ImageBuffer::from_raw(shot.width, shot.height, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// GPUI names keys the way they are printed; `Key` parses the same names with
/// a capital. Everything that is not a single character or a function key is
/// spelled differently at the two ends and is mapped by hand.
fn key_from_keystroke(key: &str) -> Option<Key> {
    let name = match key {
        "escape" => "Escape",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "enter" => "Enter",
        "space" => "Space",
        "insert" => "Insert",
        "delete" => "Delete",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        // A modifier on its own is the operator part-way to a combination.
        "ctrl" | "alt" | "shift" | "cmd" | "win" | "fn" => return None,
        // Letters, digits and function keys name themselves.
        other => return other.to_uppercase().parse().ok(),
    };
    name.parse().ok()
}

impl Render for WinSendGpui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Only when the shape actually changed. Resizing every frame would
        // fight the user's own drag on the window edge.
        let wanted = (self.width(), self.height());
        if (wanted.0 - self.applied.0).abs() > 0.5 || (wanted.1 - self.applied.1).abs() > 0.5 {
            window.resize(size(px(wanted.0), px(wanted.1)));
            self.applied = wanted;
        }

        let screen = self.screen;
        let capturing = self.capturing;

        let panel = self.side.map(|side| self.side_panel(side, cx).into_any_element());
        // Over the surface, so the decision it asks for cannot be missed.
        let confirm = self.confirm_restart(cx);

        div()
            .track_focus(&self.focus_handle)
            .flex()
            .size_full()
            .bg(rgb(bg()))
            .text_color(rgb(text()))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .w(px(WIDTH))
                    .h_full()
                    .flex_none()
            // Armed only while a binding is being captured, so ordinary typing
            // in the window is never swallowed.
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if let Some(action) = capturing {
                    this.capture(action, event, cx);
                    cx.notify();
                }
            }))
                    .child(match screen {
                        Screen::Main => self.main_header(cx).into_any_element(),
                        Screen::Settings => {
                            self.sub_header("Settings", cx).into_any_element()
                        }
                        Screen::Hotkeys => self.sub_header("Hotkeys", cx).into_any_element(),
                    })
            .child(match screen {
                Screen::Main => self.main(cx).into_any_element(),
                Screen::Settings => self.settings(cx).into_any_element(),
                Screen::Hotkeys => self.hotkeys(cx).into_any_element(),
            })
                    .when_some(self.toast.clone(), |this, message| {
                        this.child(toast(&message, self.toast_leaving, cx))
                    })
                    .children(confirm),
            )
            .children(panel)
    }
}

impl WinSendGpui {
    /// The live surface: the three rows, the two actions, the footer.
    fn main(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let target = self.target();
        let can_retrieve = self.core.can_retrieve();
        let can_send = state != State::Blocked;
        let sent = state == State::Sent;
        let zoom_set = self.core.config.zoom_window.is_some();
        let media_set = self.core.config.media_window.is_some();
        let side = self.side;

        let zoom_label = self
            .core
            .config
            .zoom_window
            .as_ref()
            .map(|w| w.title.clone())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Choose the Zoom video window".to_string());
        let media_label = self
            .core
            .config
            .media_window
            .as_ref()
            .map(|w| if w.title.is_empty() { w.process_name.clone() } else { w.title.clone() })
            .unwrap_or_else(|| "No media player".to_string());

        div()
            .flex()
            .flex_col()
            .flex_1()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_4()
                    .pb_3()
                    .child(
                        row(
                            "row-display",
                            "display",
                            target
                                .as_ref()
                                .map(display_name)
                                .unwrap_or_else(|| "Choose a display".to_string()),
                            if target.is_some() {
                                chip(Chip::Set, side == Some(Side::Display)).into_any_element()
                            } else {
                                chip(Chip::Needed, side == Some(Side::Display)).into_any_element()
                            },
                            target.is_none(),
                            side == Some(Side::Display),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_side(Side::Display);
                            cx.notify();
                        })),
                    )
                    .child(
                        row(
                            "row-zoom",
                            "video",
                            zoom_label,
                            if zoom_set {
                                chip(Chip::Set, side == Some(Side::Window(PickerFor::Zoom))).into_any_element()
                            } else {
                                chip(Chip::Needed, side == Some(Side::Window(PickerFor::Zoom))).into_any_element()
                            },
                            !zoom_set,
                            side == Some(Side::Window(PickerFor::Zoom)),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_side(Side::Window(PickerFor::Zoom));
                            cx.notify();
                        })),
                    )
                    .child(
                        row(
                            "row-media",
                            "media",
                            media_label,
                            if media_set {
                                chip(Chip::Set, side == Some(Side::Window(PickerFor::Media))).into_any_element()
                            } else {
                                chip(Chip::Optional, side == Some(Side::Window(PickerFor::Media))).into_any_element()
                            },
                            !media_set,
                            side == Some(Side::Window(PickerFor::Media)),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_side(Side::Window(PickerFor::Media));
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_4()
                    .pb_4()
                    .child(
                        cta(
                            "send",
                            "Send to Monitor",
                            // Quiet while a picker is open: nothing is sent
                            // mid-configuration, and the open row already owns
                            // the accent.
                            (!sent && can_send && side.is_none()).then_some(ACCENT),
                            can_send,
                        )
                        .on_click(
                            cx.listener(|this, _, _, cx| {
                                this.perform(Action::Send, cx);
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        cta("retrieve", "Retrieve", sent.then_some(WARM), can_retrieve).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.perform(Action::Retrieve, cx);
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(footer(cx))
    }
}

impl WinSendGpui {
    /// The live surface's header: the state, when there is one, and the window
    /// controls.
    ///
    /// Nothing is shown while nothing is out. An indicator that is always lit
    /// is one the eye stops reading, and "idle" is not a thing the operator
    /// needs telling — the two buttons below already say it. What is worth
    /// interrupting for is a window being on another display right now, so
    /// that is the only thing this ever says.
    fn main_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sent = self.state() == State::Sent;

        // A child rather than a `when`, because the animation wrapper is a
        // different element type and both arms of a `when` have to agree.
        let chip = sent.then(|| {
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .px_2()
                .py(px(4.))
                .rounded_full()
                .bg(tint(ACCENT, 0.16))
                .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(rgb(ACCENT)))
                .child(
                    div()
                        .text_size(px(9.5))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(ACCENT))
                        .child("ON TARGET"),
                )
        });

        header_shell()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .children(chip)
                    .children(self.update_pill(cx)),
            )
            .child(header_controls(cx))
    }

    /// The update indicator, and every state after it.
    ///
    /// In the header rather than on the surface, because it is the only thing
    /// here that appears without being asked for and the header is already
    /// where this window puts what it has to volunteer. Nothing is shown until
    /// a check has found something, and the click is the whole of what it
    /// does: downloading and restarting never happen on their own.
    fn update_pill(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let pill = |tone: u32, label: String| {
            div()
                .flex()
                .items_center()
                .px_2()
                .py(px(4.))
                .rounded_full()
                .bg(tint(tone, 0.16))
                .text_size(px(9.5))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(tone))
                .child(label)
        };

        match &self.update {
            UpdateState::Quiet | UpdateState::Confirming(_) => None,
            UpdateState::Installing(_) => {
                Some(pill(ACCENT, "UPDATING".to_string()).into_any_element())
            }
            UpdateState::Available(release) => {
                let release = release.clone();
                Some(
                    pill(WARM, format!("UPDATE {}", release.version))
                        .id("update")
                        .cursor_pointer()
                        .hover(|style| style.bg(tint(WARM, 0.28)))
                        .active(|style| style.opacity(0.7))
                        // Otherwise the press drags the window from the header
                        // instead of arming the button.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.ask_to_install(release.clone());
                            cx.notify();
                        }))
                        .into_any_element(),
                )
            }
        }
    }

    /// The warning, over the surface, because it is a decision and not a
    /// setting.
    ///
    /// `Core` holds the restore point in memory and it is session-scoped by
    /// design, so restarting while a window is still sent leaves Zoom on the
    /// target display with nothing left able to put it back.
    fn confirm_restart(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let UpdateState::Confirming(release) = &self.update else {
            return None;
        };
        let release = release.clone();
        let (retrieving, anyway) = (release.clone(), release.clone());

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .justify_center()
                .px_4()
                // A scrim rather than a bare card: what is underneath is still
                // the live surface, and a decision this one must not be
                // answerable by clicking past it.
                .bg(tint(0x000000, 0.62))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .w_full()
                        .p_4()
                        .rounded_2xl()
                        .bg(rgb(row_fill()))
                        .border_1()
                        .border_color(rgb(border()))
                        .shadow_lg()
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("Update to {}", release.version)),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(subdued()))
                                .pb_1()
                                .child(
                                    "A window is still sent. Where it came from is only \
                                     remembered for as long as this is running, so restarting \
                                     now leaves it on the target display with nothing able to \
                                     put it back.",
                                ),
                        )
                        .child(
                            cta("update-retrieve", "Retrieve, then update", Some(ACCENT), true)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    // Straight through Core rather than the
                                    // fade: the point is to be finished before
                                    // anything restarts, and an animation would
                                    // only put 200ms between the decision and
                                    // the thing it was guarding.
                                    let outcome = this.core.retrieve();
                                    let restored = outcome.is_ok();
                                    this.report(outcome, cx);
                                    this.arm_watches(cx);
                                    if restored {
                                        this.install(retrieving.clone());
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(cta("update-anyway", "Update anyway", None, true).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.install(anyway.clone());
                                cx.notify();
                            }),
                        ))
                        .child(cta("update-later", "Not now", None, true).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.update = UpdateState::Available(release.clone());
                                cx.notify();
                            },
                        ))),
                )
                .into_any_element(),
        )
    }

    /// A sub-surface's header: a way back, and what this is.
    fn sub_header(&self, title: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        header_shell()
            .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .id("back")
                        .flex()
                        .justify_center()
                        .items_center()
                        .w(px(26.))
                        .h(px(26.))
                        .rounded_full()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(row_hover())))
                        .active(|style| style.opacity(0.6))
                        .child(icon("back", 15.0, subdued()))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.screen = Screen::Main;
                            this.end_capture();
                            this.candidates.clear();
                            this.thumbnails.clear();
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                ),
            )
            .child(header_controls(cx))
    }

    /// The chooser, beside the surface.
    ///
    /// The thumbnail is the point rather than decoration: Zoom's main and
    /// video windows carry the same process, class and title, so the list can
    /// only be told apart by looking at it.
    fn side_panel(&mut self, side: Side, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, hint) = match side {
            Side::Display => ("Target display", "Where the video window is sent."),
            Side::Window(PickerFor::Zoom) => (
                "Zoom video window",
                "Pin someone in Zoom first, then pick the window showing only that feed.",
            ),
            Side::Window(PickerFor::Media) => (
                "Media window",
                "A full-screen video window often has no title, so go by the process.",
            ),
        };

        let monitors = self.core.monitors();
        let selected = self.target().map(|m| m.id);
        let candidates = self.candidates.clone();
        let thumbnails = self.thumbnails.clone();
        let count = match side {
            Side::Display => monitors.len(),
            Side::Window(_) => candidates.len(),
        };

        div()
            .flex()
            .flex_col()
            .w(px(PANEL_WIDTH))
            .h_full()
            .flex_none()
            // Flush against the surface and divided by a rule, so the window
            // reads as one object with two columns rather than as a card
            // floating beside a panel.
            .border_l_1()
            .border_color(rgb(border()))
            // The same height as the surface's own header, so the title sits
            // on the line the window controls sit on rather than a few pixels
            // under it.
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(px(HEADER_HEIGHT))
                    .px_4()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            // Says how many there are when only some of them
                            // fit, which is what tells you the list scrolls.
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(rgb(faint()))
                                    .child(format!("{count}")),
                            ),
                    )
                    .child(
                        div()
                            .id("close-panel")
                            .flex()
                            .justify_center()
                            .items_center()
                            .w(px(28.))
                            .h(px(28.))
                            .rounded_full()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(row_hover())))
                            .active(|style| style.opacity(0.6))
                            .child(icon("close", 15.0, subdued()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_side();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .px_4()
                    .pb_3()
                    .text_size(px(10.5))
                    .text_color(rgb(faint()))
                    .child(hint),
            )
            // Only the list scrolls, so the title and the way out of the panel
            // stay put however long it is.
            .child(
                div()
                    .id("panel-list")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .flex_1()
                    .min_h_0()
                    .px_4()
                    .pb_4()
                    .overflow_y_scroll()
                    .map(|this| match side {
                Side::Display => this.children(monitors.into_iter().map(|monitor| {
                    let chosen = selected.as_deref() == Some(monitor.id.as_str());
                    picker_row(monitor, chosen, cx)
                })),
                Side::Window(picker) => this
                    .when(candidates.is_empty(), |this| {
                        this.child(
                            div()
                                .flex()
                                .justify_center()
                                .py_6()
                                .text_size(px(11.))
                                .text_color(rgb(faint()))
                                .child("Nothing to choose from"),
                        )
                    })
                    .children(candidates.into_iter().map(move |candidate| {
                        candidate_row(candidate, thumbnails.clone(), picker, cx)
                    })),
            }),
            )
    }

    /// The settings, as toggles that write straight through `Core`.
    fn settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mock = self.mock_controls(cx);
        let config = &self.core.config;
        let rows = [
            (
                "set-clear",
                "Minimize others on target",
                "Some players pause while minimized",
                config.clear_target,
                Setting::ClearTarget,
            ),
            (
                "set-borderless",
                "Strip window frame when sending",
                "Fills the target display edge to edge",
                config.borderless,
                Setting::Borderless,
            ),
            (
                "set-fade",
                "Fade out when retrieving",
                "Uncovers the display smoothly instead of cutting",
                config.fade_on_retrieve,
                Setting::Fade,
            ),
            (
                "set-restore",
                "Return full-screen video after Retrieve",
                "Uses the player's own shortcut, at most once",
                config.restore_fullscreen,
                Setting::RestoreFullscreen,
            ),
            (
                "set-theme",
                "Light theme",
                "Dark avoids spilling light in a broadcast",
                config.light_theme,
                Setting::LightTheme,
            ),
            (
                "set-updates",
                "Check for updates on startup",
                "Nothing downloads until you click it",
                config.check_for_updates,
                Setting::CheckUpdates,
            ),
        ];

        div()
            .id("settings")
            .flex()
            .flex_col()
            .flex_1()
            .gap_2()
            .px_4()
            .pb_4()
            .overflow_y_scroll()
            .children(rows.into_iter().map(|(id, label, detail, on, setting)| {
                setting_row(id, label, detail, on, setting, cx)
            }))
            .child(
                div()
                    .id("diagnostics")
                    .flex()
                    .justify_center()
                    .items_center()
                    .w_full()
                    .h(px(38.))
                    .mt_2()
                    .rounded_full()
                    .bg(rgb(row_fill()))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(row_hover())))
                    .active(|style| style.opacity(0.7))
                    .text_size(px(12.))
                    .child("Copy diagnostics")
                    .on_click(cx.listener(|this, _, _, cx| {
                        // The clipboard first, because the file lands in
                        // AppData, which Explorer hides by default.
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            this.core.diagnostics(),
                        ));
                        let saved = this.core.save_diagnostics();
                        // The path is real information and belongs in the
                        // file, not in a line that is gone in three seconds.
                        this.report(
                            saved.map(|_| "Copied, and saved beside your config".to_string()),
                            cx,
                        );
                        cx.notify();
                    })),
            )
            .children(mock)
    }

    /// Mock-only: inject what only a real desktop could otherwise produce.
    ///
    /// The mock platform and shell exist so the surface can be driven on a
    /// machine that is not Windows. Without these, everything the shell
    /// delivers — a hotkey press, a tray choice — is unreachable here, and the
    /// wiring behind it could only be exercised by shipping it.
    ///
    /// At the foot of Settings rather than on the live surface. `app.rs` puts
    /// its own on the surface and pays for them in window height; this one is
    /// a press away in a panel that already scrolls, and the shape that sits
    /// on screen during a broadcast is left alone.
    #[cfg(not(windows))]
    fn mock_controls(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        // Both or neither: the mock platform and the mock shell are chosen
        // together, so a surface offering half of these would be lying.
        self.core.platform.as_mock()?;
        let tray = self.shell.as_mock()?.tray();
        let zoom_present = self.core.platform.as_mock()?.zoom_present();

        let heading = |text: &'static str| {
            div()
                .text_size(px(10.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(faint()))
                .child(text)
        };
        let strip = || div().flex().items_center().gap_1p5().w_full();

        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .mt_4()
                .pt_3()
                .border_t_1()
                .border_color(rgb(border()))
                .child(heading("MOCK DESKTOP"))
                .child(
                    strip()
                        .child(
                            small_button(
                                "mock-zoom".into(),
                                if zoom_present { "Zoom: running" } else { "Zoom: gone" },
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(mock) = this.core.platform.as_mock() {
                                    mock.set_zoom_present(!zoom_present);
                                }
                                cx.notify();
                            })),
                        )
                        // The picker is the only way to confirm a window, so
                        // every state that needs one is otherwise out of reach
                        // of anything driving this from outside. Same shortcut
                        // the picker takes, without the clicking.
                        .child(small_button("mock-confirm".into(), "Confirm Zoom").on_click(
                            cx.listener(|this, _, _, cx| {
                                let found = this
                                    .core
                                    .candidates()
                                    .into_iter()
                                    .find(|candidate| candidate.likely_zoom);
                                if let Some(candidate) = found {
                                    let outcome = this.core.confirm_window(&candidate);
                                    this.report(outcome, cx);
                                }
                                cx.notify();
                            }),
                        )),
                )
                .child(heading("MOCK HOTKEY"))
                .child(strip().children(Action::ALL.map(|action| {
                    // Injected rather than performed directly, so the press
                    // travels the same queue-and-drain path a real one takes.
                    small_button(format!("mock-key-{}", action.label()).into(), action.label())
                        .on_click(cx.listener(move |this, _, _, _| {
                            if let Some(mock) = this.shell.as_mock() {
                                mock.trigger(action);
                            }
                        }))
                })))
                .child(heading("MOCK TRAY"))
                .child(strip().children(
                    [
                        ("mock-tray-show", "Show", ShellEvent::ShowWindow),
                        ("mock-tray-settings", "Settings", ShellEvent::ShowSettings),
                        ("mock-tray-quit", "Quit", ShellEvent::Quit),
                    ]
                    .map(|(id, label, event)| {
                        small_button(id.into(), label).on_click(cx.listener(
                            move |this, _, _, _| {
                                if let Some(mock) = this.shell.as_mock() {
                                    mock.choose(event.clone());
                                }
                            },
                        ))
                    }),
                ))
                .child(heading("MOCK UPDATE"))
                // Walks the whole update flow without the network: the canned
                // response goes through the same parsing and comparison the
                // real updater uses, so what is exercised here is the real
                // decision.
                .child(strip().children(
                    [("mock-offer", "Offer", false), ("mock-offer-fails", "Offer, fails", true)]
                        .map(|(id, label, fails)| {
                            small_button(id.into(), label).on_click(cx.listener(
                                move |this, _, _, _| {
                                    if let Some(mock) = this.updater.as_mock() {
                                        if fails {
                                            mock.set_install_failure(
                                                "the download did not match its checksum",
                                            );
                                        }
                                        mock.set_response(
                                            &crate::mock::MockUpdater::release_list("99.0.0"),
                                        );
                                        mock.check();
                                    }
                                },
                            ))
                        }),
                ))
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(faint()))
                        .child(format!(
                            "{}  —  Retrieve {}",
                            tray.tooltip,
                            if tray.can_retrieve { "offered" } else { "greyed" }
                        )),
                )
                .into_any_element(),
        )
    }

    /// Nothing to inject on a real desktop, which has the real thing.
    #[cfg(windows)]
    fn mock_controls(&mut self, _cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        None
    }

    /// The bindings, and the capture that sets them.
    fn hotkeys(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let capturing = self.capturing;
        // The refusal travels with the row rather than only through a toast:
        // a combination another application already owns is discovered by
        // looking at the binding, which is where the user will look.
        let bindings: Vec<(Action, Option<Hotkey>, Option<SharedString>)> = Action::ALL
            .into_iter()
            .map(|action| {
                (
                    action,
                    self.core.config.hotkeys.binding(action),
                    self.hotkey_report.reason(action).map(SharedString::from),
                )
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .gap_2()
            .px_4()
            .pb_4()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(subdued()))
                    .pb_1()
                    .child("Work from inside Zoom, without focusing this window."),
            )
            .children(bindings.into_iter().map(|(action, bound, refused)| {
                binding_row(action, bound, refused, capturing == Some(action), cx)
            }))
    }
}

/// Which setting a toggle writes. Named rather than passed as a closure so the
/// list above stays a table of what exists rather than a list of callbacks.
#[derive(Clone, Copy)]
enum Setting {
    LightTheme,
    ClearTarget,
    Borderless,
    Fade,
    RestoreFullscreen,
    CheckUpdates,
}

impl Setting {
    fn apply(self, core: &mut Core, on: bool) -> Result<(), String> {
        match self {
            Setting::LightTheme => {
                // The palette is read while laying out, so it has to change
                // before the frame that follows this press.
                set_palette(on);
                core.set_light_theme(on)
            }
            Setting::ClearTarget => core.set_clear_target(on),
            Setting::Borderless => core.set_borderless(on),
            Setting::Fade => core.set_fade_on_retrieve(on),
            Setting::RestoreFullscreen => core.set_restore_fullscreen(on),
            Setting::CheckUpdates => core.set_check_for_updates(on),
        }
    }
}

/// The header both surfaces share: content on the left, window controls right.
fn header_shell() -> gpui::Stateful<gpui::Div> {
    div()
        .id("header")
        .flex()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(HEADER_HEIGHT))
        .pr_2()
        .pl(px(TITLE_INSET))
        // The system titlebar is transparent, which takes the window's drag
        // handle with it. This puts it back on the header, where it was.
        .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
}

/// The window controls, added after a header's own content so that
/// `justify_between` puts them on the right.
fn header_controls(cx: &mut Context<WinSendGpui>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(chrome_button("minimize", "minimize").on_click(cx.listener(
            |_, _, window, _| window.minimize_window(),
        )))
        .child(chrome_button("close", "close").on_click(cx.listener(
            |_, _, window, _| window.remove_window(),
        )))
}

/// A window in the selector: a picture, what it is, and where.
fn candidate_row(
    candidate: WindowCandidate,
    thumbnails: HashMap<u64, Arc<RenderImage>>,
    picker: PickerFor,
    cx: &mut Context<WinSendGpui>,
) -> impl IntoElement {
    // An untitled window is included on purpose in the media list; a blank
    // heading would read as a bug.
    let heading = if candidate.title.is_empty() {
        "(no title)".to_string()
    } else {
        candidate.title.clone()
    };
    let detail = format!("{} · {}", candidate.process_name, candidate.class_name);
    let where_to = format!(
        "{}x{} on {}",
        candidate.bounds.width, candidate.bounds.height, candidate.monitor_id
    );
    let thumbnail = thumbnails.get(&candidate.handle).cloned();
    let id = SharedString::from(format!("cand-{}", candidate.handle));

    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .w_full()
        .p_2()
        .rounded_2xl()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(row_hover())))
        .active(|style| style.opacity(0.7))
        .child(match thumbnail {
            Some(image) => img(image)
                .w(px(88.))
                .h(px(50.))
                .rounded_lg()
                .into_any_element(),
            // A window that will not be captured is still a window that can be
            // chosen. GPU-composited ones routinely refuse.
            None => div()
                .flex()
                .justify_center()
                .items_center()
                .w(px(88.))
                .h(px(50.))
                .rounded_lg()
                .bg(rgb(row_fill()))
                .text_size(px(9.))
                .text_color(rgb(faint()))
                .child("no preview")
                .into_any_element(),
        })
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child(heading),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(subdued()))
                        .truncate()
                        .child(detail),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(faint()))
                        .truncate()
                        .child(where_to),
                ),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.confirm(picker, &candidate, cx);
            cx.notify();
        }))
}

/// A setting: what it does, why, and a switch.
fn setting_row(
    id: &'static str,
    label: &'static str,
    detail: &'static str,
    on: bool,
    setting: Setting,
    cx: &mut Context<WinSendGpui>,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .w_full()
        .p_3()
        .rounded_2xl()
        .bg(rgb(row_fill()))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(row_hover())))
        .active(|style| style.opacity(0.7))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(label),
                )
                .child(div().text_size(px(10.)).text_color(rgb(faint())).child(detail)),
        )
        .child(switch(on))
        .on_click(cx.listener(move |this, _, _, cx| {
            if let Err(message) = setting.apply(&mut this.core, !on) {
                this.note(&message, true, cx);
            }
            cx.notify();
        }))
}

/// A drawn switch. GPUI ships no controls, so this is a track and a knob.
fn switch(on: bool) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .w(px(36.))
        .h(px(20.))
        .p(px(2.))
        .rounded_full()
        .bg(if on { rgb(ACCENT).into() } else { tint(text(), 0.12) })
        .when(!on, |this| this.justify_start())
        .when(on, |this| this.justify_end())
        .child(div().w(px(16.)).h(px(16.)).rounded_full().bg(rgb(ON_ACCENT)))
}

/// One action, its binding, and the controls that change it.
fn binding_row(
    action: Action,
    bound: Option<Hotkey>,
    refused: Option<SharedString>,
    capturing: bool,
    cx: &mut Context<WinSendGpui>,
) -> impl IntoElement {
    let row_id = SharedString::from(format!("bind-{}", action.label()));
    let set_id = SharedString::from(format!("set-{}", action.label()));
    let clear_id = SharedString::from(format!("clear-{}", action.label()));

    div()
        .id(row_id)
        .flex()
        .items_center()
        .gap_2()
        .w_full()
        .h(px(52.))
        .px_4()
        .rounded_2xl()
        .bg(rgb(row_fill()))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap_0p5()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(action.label()),
                )
                .child(if capturing {
                    div()
                        .text_size(px(10.5))
                        .text_color(rgb(ACCENT))
                        .child("press a combination")
                        .into_any_element()
                } else if let Some(why) = refused {
                    // Ahead of the binding it replaces, because a binding
                    // shown as set when the OS refused it is the lie this
                    // exists to prevent.
                    div()
                        .text_size(px(10.5))
                        .text_color(rgb(err()))
                        .child(why)
                        .into_any_element()
                } else {
                    match &bound {
                        Some(hotkey) => div()
                            .text_size(px(10.5))
                            .text_color(rgb(subdued()))
                            .child(hotkey.to_string())
                            .into_any_element(),
                        None => div()
                            .text_size(px(10.5))
                            .text_color(rgb(faint()))
                            .child("not set")
                            .into_any_element(),
                    }
                }),
        )
        .when(bound.is_some() && !capturing, |this| {
            this.child(
                small_button(clear_id, "Clear").on_click(cx.listener(move |this, _, _, cx| {
                    let outcome = this.core.set_hotkey(action, None);
                    this.report(outcome, cx);
                    // The binding is gone from the config; this is what takes
                    // it off the OS as well.
                    this.shell.apply_hotkeys(this.core.config.hotkeys);
                    cx.notify();
                })),
            )
        })
        .child(
            small_button(set_id, if capturing { "Cancel" } else if bound.is_some() { "Change" } else { "Set" })
                .on_click(cx.listener(move |this, _, window, cx| {
                    if capturing {
                        this.end_capture();
                    } else {
                        this.start_capture(action);
                    }
                    // The key listener sits on the root and only fires while
                    // the window holds focus, so capture has to take it.
                    window.focus(&this.focus_handle, cx);
                    cx.notify();
                })),
        )
}

fn small_button(id: SharedString, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .justify_center()
        .items_center()
        .h(px(26.))
        .px_3()
        .rounded_full()
        .bg(rgb(bg()))
        .border_1()
        .border_color(rgb(border()))
        .cursor_pointer()
        .hover(|style| style.border_color(rgb(ACCENT)))
        .active(|style| style.opacity(0.7))
        .text_size(px(11.))
        .child(label)
}

/// A window control in the drawn header.
///
/// Present on every platform because on Windows they are the only close and
/// minimise there are; macOS shows its own beside them, which is a cost paid
/// only on the machine this is developed on.
fn chrome_button(id: &'static str, glyph: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .justify_center()
        .items_center()
        .w(px(28.))
        .h(px(28.))
        .rounded_full()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(row_hover())))
        .active(|style| style.opacity(0.6))
        .child(icon(glyph, 15.0, subdued()))
        // Otherwise a press here starts dragging the window instead of
        // arming the button, and the click never lands.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// One display in the in-place picker on the live surface.
fn picker_row(
    monitor: MonitorInfo,
    chosen: bool,
    cx: &mut Context<WinSendGpui>,
) -> impl IntoElement {
    let name = display_name(&monitor);
    let detail = display_detail(&monitor);
    let id = SharedString::from(format!("pick-{}", monitor.id));

    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(PICKER_ROW))
        .px_4()
        .rounded_full()
        .when(chosen, |this| this.bg(tint(ACCENT, 0.14)))
        .border_1()
        .border_color(rgb(if chosen { ACCENT } else { bg() }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(row_hover())))
        .active(|style| style.opacity(0.7))
        .child(div().text_size(px(12.5)).child(name))
        .child(div().text_size(px(10.)).text_color(rgb(faint())).child(detail))
        .on_click(cx.listener(move |this, _, _, cx| {
            let outcome = this.core.set_target_monitor(&monitor);
            this.report(outcome, cx);
            this.close_side();
            cx.notify();
        }))
}

/// A message, over the surface rather than inside it.
///
/// Absolutely positioned, so nothing moves when one arrives and nothing is
/// reserved for it when there is none. One line, truncated: anything needing
/// more room than that is a report, and reports go to the file.
///
/// It sits over the footer rather than over the actions. Something has to be
/// covered for three seconds, and Retrieve is the control someone reaches for
/// while a window is on air — the three buttons it hides instead are the ones
/// that were chosen for the footer precisely because they are never urgent.
fn toast(
    message: &Message,
    leaving: bool,
    cx: &mut Context<WinSendGpui>,
) -> impl IntoElement {
    let (tone, glyph) = if message.failed { (err(), "alert") } else { (ok(), "check") };

    div()
        .id("toast")
        .absolute()
        .bottom(px(20.))
        .left_3()
        .right_3()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_2xl()
        .bg(rgb(row_hover()))
        .border_1()
        .border_color(tint(tone, 0.4))
        .shadow_lg()
        .cursor_pointer()
        // The whole thing dismisses, which also gives it a hitbox covering the
        // footer buttons underneath. Without one a press meant for the toast
        // would land on whatever it is sitting over.
        .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx)))
        .child(icon(glyph, 13.0, tone))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.5))
                .text_color(rgb(text()))
                .child(message.text.clone()),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .justify_center()
                .items_center()
                .w(px(18.))
                .h(px(18.))
                .rounded_full()
                .hover(|style| style.bg(tint(text(), 0.12)))
                .child(icon("close", 11.0, subdued())),
        )
        // The id differs between the two, which is what makes the second one
        // run: GPUI keys an animation to its element, so a changed id is a new
        // element and a fresh start rather than a continuation.
        .with_animation(
            if leaving { "toast-out" } else { "toast-in" },
            Animation::new(if leaving { TOAST_OUT } else { TOAST_IN })
                .with_easing(ease_out_quint()),
            move |this, delta| {
                // Rises the last eight pixels on the way in and settles back
                // on the way out, which it can do because it is absolutely
                // positioned and nothing reflows around it.
                let shown = if leaving { 1.0 - delta } else { delta };
                this.opacity(shown).bottom(px(12. + 8. * shown))
            },
        )
}

/// Loom's row of circular buttons, for what is occasionally needed and never
/// urgent. Each opens a surface of its own rather than expanding in place:
/// they are reached by a press, so the resize is always asked for.
fn footer(cx: &mut Context<WinSendGpui>) -> impl IntoElement {
    div()
        .flex()
        .justify_around()
        .items_center()
        .w_full()
        .pt_3()
        .pb_3()
        .px_4()
        .border_t_1()
        .border_color(rgb(border()))
        .child(
            footer_button("f-hotkeys", "keyboard", "Hotkeys").on_click(cx.listener(
                |this, _, _, cx| {
                    this.screen = Screen::Hotkeys;
                    cx.notify();
                },
            )),
        )
        .child(
            footer_button("f-settings", "settings", "Settings").on_click(cx.listener(
                |this, _, _, cx| {
                    this.screen = Screen::Settings;
                    cx.notify();
                },
            )),
        )
        .child(
            footer_button("f-diagnostics", "info", "Diagnostics").on_click(cx.listener(
                |this, _, _, cx| {
                    // The clipboard first, because the file lands in AppData,
                    // which Explorer hides by default.
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(this.core.diagnostics()));
                    let saved = this.core.save_diagnostics();
                    // The path is real information and belongs in the file,
                    // not in a line that is gone in three seconds.
                    this.report(
                        saved.map(|_| "Copied, and saved beside your config".to_string()),
                        cx,
                    );
                    cx.notify();
                },
            )),
        )
}
