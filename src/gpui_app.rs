//! GPUI front end. Presentation only: every decision lives in `Core`.
//!
//! Built to be looked at next to the eframe surface rather than to replace it
//! yet. What it changes is the information architecture, not the renderer:
//!
//! - The state of the world is the largest thing on screen, because the
//!   question asked mid-broadcast is "is the window out there right now" and
//!   the answer used to be inferable only from whether Retrieve was greyed.
//! - The accent follows the state. Whichever action the operator is most
//!   likely to reach for next is the filled one, and it is only ever one.
//! - Send is gated on being able to succeed. A primary action that is always
//!   live, and reports its own impossibility afterwards in small text, spends
//!   the operator's attention at the worst moment.
//! - What is missing is fixed where it is named. A blocked surface offers the
//!   monitor list and the window confirmation inline, rather than sending the
//!   operator into a settings screen to find them.
//! - The bound hotkeys are on the live surface. They are how this is actually
//!   driven, and they used to be visible only inside the configuration.

use gpui::{
    Context, FontWeight, IntoElement, Render, SharedString, Window, div, prelude::*, px, rgb, size,
};

use crate::core::{Core, Failure};
use crate::hotkey::Action;
use crate::platform::MonitorInfo;

pub const WIDTH: f32 = 360.0;

/// The height with nothing to set up: the shape that sits on screen during a
/// broadcast, and the one worth keeping small.
pub const HEIGHT: f32 = 344.0;

/// What the setup blocks add when they are showing.
///
/// The window grows to fit them and shrinks back, which is the same bargain
/// the eframe surface strikes with its configuration disclosure: a live
/// surface sized for a first-run state it will not be in again is a surface
/// that is too tall every day after the first.
const SECTION_HEADING: f32 = 21.0;
const SETUP_ROW: f32 = 34.0;
const ROW_GAP: f32 = 8.0;
const BLOCK_GAP: f32 = 12.0;

const BG: u32 = 0x141414;
const RAISED: u32 = 0x1e1e1e;
const BORDER: u32 = 0x2e2e2e;
const TEXT: u32 = 0xf0f0f0;
const SUBDUED: u32 = 0x8a8a8a;
const FAINT: u32 = 0x5c5c5c;
const ACCENT: u32 = 0x4e8ef0;
const OK: u32 = 0x66bb7a;
const WARN: u32 = 0xe0a458;
const ERR: u32 = 0xe26a6a;

/// How many recent messages the status strip keeps.
///
/// The same count the eframe surface settles on, for the same reason: enough
/// that a message cannot be pushed out before it has been read, few enough
/// that what is on screen is still status rather than history.
const STATUS_HISTORY: usize = 3;

/// What the surface is currently able to do.
///
/// Derived on every render rather than stored, because every input to it —
/// the confirmed window, the target monitor, the restore point — already lives
/// in `Core` and a second copy would be a second thing to keep true.
enum State {
    /// Send cannot succeed yet, and this is what is missing.
    Blocked(Vec<Missing>),
    /// Everything Send needs is in place, and nothing is currently out.
    Ready,
    /// A window is on the target display right now.
    Sent,
}

#[derive(PartialEq)]
enum Missing {
    TargetMonitor,
    ZoomWindow,
}

struct Message {
    text: SharedString,
    failed: bool,
}

pub struct WinSendGpui {
    core: Core,
    status: Vec<Message>,
    /// The height last asked for, so the window is resized when the state
    /// changes shape rather than on every frame.
    applied_height: f32,
}

impl WinSendGpui {
    pub fn new(core: Core) -> Self {
        Self { core, status: Vec::new(), applied_height: HEIGHT }
    }

    fn state(&self) -> State {
        if self.core.can_retrieve() {
            return State::Sent;
        }
        let mut missing = Vec::new();
        if self.core.config.resolve_monitor(&self.core.monitors()).is_none() {
            missing.push(Missing::TargetMonitor);
        }
        if self.core.config.zoom_window.is_none() {
            missing.push(Missing::ZoomWindow);
        }
        if missing.is_empty() { State::Ready } else { State::Blocked(missing) }
    }

    fn target(&self) -> Option<MonitorInfo> {
        self.core.config.resolve_monitor(&self.core.monitors()).cloned()
    }

    fn report(&mut self, outcome: Result<String, Failure>) {
        let (text, failed) = match outcome {
            Ok(message) => (message, false),
            Err(failure) => (failure.to_string(), true),
        };
        self.status.insert(0, Message { text: text.into(), failed });
        self.status.truncate(STATUS_HISTORY);
    }

    fn perform(&mut self, action: Action) {
        let outcome = match action {
            Action::Send => self.core.send(),
            Action::Retrieve => self.core.retrieve(),
            Action::RestoreMedia => self.core.restore_media(),
        };
        self.report(outcome);
    }
}

/// How tall the window has to be to show everything the state puts in it.
fn wanted_height(state: &State, monitors: usize) -> f32 {
    let State::Blocked(missing) = state else {
        return HEIGHT;
    };
    let mut height = HEIGHT;
    if missing.contains(&Missing::TargetMonitor) {
        let rows = monitors as f32;
        height += BLOCK_GAP + SECTION_HEADING + rows * SETUP_ROW + (rows - 1.0).max(0.0) * ROW_GAP;
    }
    if missing.contains(&Missing::ZoomWindow) {
        height += BLOCK_GAP + SECTION_HEADING + SETUP_ROW;
    }
    height
}

/// "Display 2" out of `\\.\DISPLAY2`.
///
/// The geometry is the detail, not the name. `MonitorInfo::label` reads as a
/// diagnostic — which is right where it is used, in the picker and the
/// diagnostics dump, and wrong as the text on a thing you click under
/// pressure.
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

/// The state banner: the largest thing on the surface, and the only thing that
/// answers the question the operator actually has.
///
/// Tinted with the state's own colour rather than carrying a lone dot, so it
/// reads from across a room at a glance rather than on inspection.
fn banner(state: &State, target: Option<&MonitorInfo>) -> impl IntoElement {
    let (tone, heading, detail) = match state {
        State::Sent => (
            ACCENT,
            "ON TARGET",
            target
                .map(|m| format!("{} · {}", display_name(m), display_detail(m)))
                .unwrap_or_else(|| "the target display".to_string()),
        ),
        State::Ready => (
            OK,
            "READY",
            target
                .map(|m| format!("{} · {}", display_name(m), display_detail(m)))
                .unwrap_or_else(|| "no target display".to_string()),
        ),
        State::Blocked(_) => (WARN, "NOT READY", "finish the setup below".to_string()),
    };

    div()
        .flex()
        .items_center()
        .gap_3()
        .w_full()
        .p_3()
        .rounded_md()
        .bg(rgb(tone).opacity(0.10))
        .border_1()
        .border_color(rgb(tone).opacity(0.35))
        .child(div().w(px(12.)).h(px(12.)).rounded_full().bg(rgb(tone)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(19.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(TEXT))
                        .child(heading),
                )
                .child(div().text_size(px(11.5)).text_color(rgb(SUBDUED)).child(detail)),
        )
}

/// A full-width action button.
///
/// `accent` is what makes it the one to reach for, and the surface only ever
/// gives it to one control at a time. A disabled button recedes rather than
/// greys: it is still there, so pressing it is not a surprise later, but it
/// does not compete with whatever is actually live.
fn action_button(
    id: &'static str,
    label: &'static str,
    accent: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let (fill, text, border) = match (enabled, accent) {
        (false, _) => (BG, FAINT, BORDER),
        (true, true) => (ACCENT, 0xffffff, ACCENT),
        (true, false) => (RAISED, TEXT, BORDER),
    };

    div()
        .id(id)
        .flex()
        .justify_center()
        .items_center()
        .w_full()
        .h(px(42.))
        .rounded_md()
        .bg(rgb(fill))
        .border_1()
        .border_color(rgb(border))
        .text_size(px(14.))
        .font_weight(if accent { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
        .text_color(rgb(text))
        // Only when it can actually be pressed. A hover that lights up a
        // control which will not respond is a promise the surface cannot keep.
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|style| style.border_color(rgb(ACCENT)))
        })
        .child(label)
}

/// A small heading over a block of controls.
fn section(label: &'static str) -> impl IntoElement {
    div()
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(FAINT))
        .child(label)
}

impl Render for WinSendGpui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let target = self.target();
        let can_send = !matches!(state, State::Blocked(_));
        let can_retrieve = self.core.can_retrieve();
        let monitors = self.core.monitors();
        let selected = target.as_ref().map(|m| m.id.clone());
        let needs_monitor = matches!(&state, State::Blocked(m) if m.contains(&Missing::TargetMonitor));
        let needs_window = matches!(&state, State::Blocked(m) if m.contains(&Missing::ZoomWindow));
        // Exactly one control is the accent, and which one it is follows the
        // state: Send while there is nothing out, Retrieve once there is.
        // Pressing Send twice is already a no-op, so nothing is lost by
        // demoting it, and the operator's eye lands on the half that matters.
        let accent_on_retrieve = matches!(state, State::Sent);

        // Only when the shape actually changed. Resizing every frame would
        // fight the user's own drag on the window edge.
        let wanted = wanted_height(&state, monitors.len());
        if (wanted - self.applied_height).abs() > 0.5 {
            window.resize(size(px(WIDTH), px(wanted)));
            self.applied_height = wanted;
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_3()
            .p_4()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(banner(&state, target.as_ref()))
            // Fix what is missing where it is named, rather than in a screen
            // the operator has to go and find.
            .when(needs_monitor, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(section("TARGET DISPLAY"))
                        .children(monitors.into_iter().map(|monitor| {
                            let chosen = selected.as_deref() == Some(monitor.id.as_str());
                            monitor_row(monitor, chosen, cx)
                        })),
                )
            })
            .when(needs_window, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(section("ZOOM VIDEO WINDOW"))
                        .child(
                            div()
                                .id("confirm-zoom")
                                .flex()
                                .justify_center()
                                .items_center()
                                .w_full()
                                .h(px(34.))
                                .rounded_md()
                                .bg(rgb(RAISED))
                                .border_1()
                                .border_color(rgb(ACCENT).opacity(0.5))
                                .cursor_pointer()
                                .text_size(px(12.))
                                .hover(|style| style.border_color(rgb(ACCENT)))
                                .child("Confirm the pinned video window")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    // The picker is not ported yet. This takes
                                    // the same path it would, on the candidate
                                    // Core already considers most likely Zoom.
                                    let candidate = this
                                        .core
                                        .candidates()
                                        .into_iter()
                                        .find(|candidate| candidate.likely_zoom);
                                    match candidate {
                                        Some(candidate) => {
                                            let outcome = this.core.confirm_window(&candidate);
                                            this.report(outcome);
                                        }
                                        None => this.status.insert(
                                            0,
                                            Message {
                                                text: "No Zoom video window is open".into(),
                                                failed: true,
                                            },
                                        ),
                                    }
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        action_button("send", "Send to Monitor", !accent_on_retrieve && can_send, can_send)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.perform(Action::Send);
                                cx.notify();
                            })),
                    )
                    .child(
                        action_button("retrieve", "Retrieve", accent_on_retrieve, can_retrieve)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.perform(Action::Retrieve);
                                cx.notify();
                            })),
                    ),
            )
            .child(hotkey_strip(&self.core))
            .child(status_strip(&self.status))
    }
}

/// One selectable display, named rather than described.
fn monitor_row(
    monitor: MonitorInfo,
    chosen: bool,
    cx: &mut Context<WinSendGpui>,
) -> impl IntoElement {
    let name = display_name(&monitor);
    let detail = display_detail(&monitor);
    let id = SharedString::from(format!("monitor-{}", monitor.id));

    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(34.))
        .px_3()
        .rounded_md()
        .bg(rgb(RAISED))
        .border_1()
        .border_color(rgb(if chosen { ACCENT } else { BORDER }))
        .cursor_pointer()
        .hover(|style| style.border_color(rgb(ACCENT)))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .child(name),
        )
        .child(div().text_size(px(10.5)).text_color(rgb(SUBDUED)).child(detail))
        .on_click(cx.listener(move |this, _, _, cx| {
            let outcome = this.core.set_target_monitor(&monitor);
            this.report(outcome);
            cx.notify();
        }))
}

/// What is bound, on the live surface rather than inside the configuration.
///
/// These are how the application is driven during a broadcast — from inside
/// Zoom, without focusing this window — so what they are is live state, not a
/// setting. An unbound action says so rather than showing nothing.
fn hotkey_strip(core: &Core) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .child(section("HOTKEYS"))
        .children(Action::ALL.into_iter().map(|action| {
            let bound = core.config.hotkeys.binding(action);
            let (text, colour) = match bound {
                Some(hotkey) => (hotkey.to_string(), TEXT),
                None => ("not set".to_string(), FAINT),
            };
            div()
                .flex()
                .justify_between()
                .w_full()
                .text_size(px(11.))
                .child(div().text_color(rgb(SUBDUED)).child(action.label()))
                .child(div().text_color(rgb(colour)).child(text))
        }))
}

/// The most recent messages, newest first and brightest.
///
/// Older ones recede rather than disappear, which says which one just arrived
/// without a clock the window would have to keep repainting to keep honest.
/// The strip keeps its height whether or not it has anything to say, so a
/// message arriving never moves the controls above it.
fn status_strip(status: &[Message]) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .justify_end()
        .gap_1()
        .w_full()
        .h(px(46.))
        .pt_2()
        .border_t_1()
        .border_color(rgb(BORDER))
        .children(status.iter().enumerate().map(|(age, message)| {
            div()
                .text_size(px(11.5))
                .text_color(rgb(if message.failed { ERR } else { OK }))
                .when(age > 0, |this| this.opacity(0.55))
                .child(message.text.clone())
        }))
}
