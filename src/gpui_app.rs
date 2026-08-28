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

use gpui::{
    AssetSource, Context, FontWeight, Hsla, IntoElement, MouseButton, Render, SharedString, Window,
    div, prelude::*, px, rgb, size, svg,
};

use crate::core::{Core, Failure};
use crate::hotkey::Action;
use crate::platform::MonitorInfo;

pub const WIDTH: f32 = 340.0;

/// The height with nothing expanded: the shape that sits on screen during a
/// broadcast, and the one worth keeping small.
pub const HEIGHT: f32 = 430.0;

/// What an expanded block adds.
const PANEL_HEADING: f32 = 21.0;
const PICKER_ROW: f32 = 34.0;
const ROW_GAP: f32 = 8.0;
const BLOCK_GAP: f32 = 12.0;

const BG: u32 = 0x141414;
/// The row fill. Loom's rows are a light grey against white; this is the same
/// one-step lift against the panel.
const ROW: u32 = 0x212121;
const ROW_HOVER: u32 = 0x2a2a2a;
const BORDER: u32 = 0x303030;
const TEXT: u32 = 0xf2f2f2;
const SUBDUED: u32 = 0x8f8f8f;
const FAINT: u32 = 0x5e5e5e;
const ACCENT: u32 = 0x4e8ef0;
const ACCENT_HOVER: u32 = 0x6ba2f5;
const OK: u32 = 0x66bb7a;
const WARN: u32 = 0xe0a458;
const ERR: u32 = 0xe26a6a;

/// How many recent messages the status strip keeps.
const STATUS_HISTORY: usize = 2;

const HEADER_HEIGHT: f32 = 52.0;

/// Where the drawn header's own content starts.
///
/// With the system titlebar transparent, macOS still draws its close, minimise
/// and zoom buttons over whatever is there, so the header's content has to
/// begin to the right of them. Windows draws none and starts at the normal
/// margin. That is also why the window carries its own minimise and close: on
/// Windows they are the only ones there are.
#[cfg(target_os = "macos")]
const TITLE_INSET: f32 = 78.0;
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
    "alert",
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

struct Message {
    text: SharedString,
    failed: bool,
}

/// Which footer panel is open, if any. Only one at a time: this is a small
/// window and two open panels would be a screen.
#[derive(PartialEq, Clone, Copy)]
enum Panel {
    Hotkeys,
    Settings,
}

pub struct WinSendGpui {
    core: Core,
    status: Vec<Message>,
    /// Open only while the target display is being chosen, so the row reads as
    /// a value most of the time and a picker briefly.
    picking_display: bool,
    panel: Option<Panel>,
    /// The height last asked for, so the window is resized when the layout
    /// changes shape rather than on every frame.
    applied_height: f32,
}

impl WinSendGpui {
    pub fn new(core: Core) -> Self {
        Self {
            core,
            status: Vec::new(),
            picking_display: false,
            panel: None,
            applied_height: HEIGHT,
        }
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

    fn report(&mut self, outcome: Result<String, Failure>) {
        let (text, failed) = match outcome {
            Ok(message) => (message, false),
            Err(failure) => (failure.to_string(), true),
        };
        self.status.insert(0, Message { text: text.into(), failed });
        self.status.truncate(STATUS_HISTORY);
    }

    fn note(&mut self, text: &str, failed: bool) {
        self.status.insert(0, Message { text: text.to_string().into(), failed });
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

    /// The picker is not ported yet. This takes the same path it would, on the
    /// candidate `Core` already considers most likely to be Zoom.
    fn confirm_zoom(&mut self) {
        match self.core.candidates().into_iter().find(|c| c.likely_zoom) {
            Some(candidate) => {
                let outcome = self.core.confirm_window(&candidate);
                self.report(outcome);
            }
            None => self.note("No Zoom video window is open", true),
        }
    }

    fn height(&self) -> f32 {
        let mut height = HEIGHT;
        if self.picking_display {
            let rows = self.core.monitors().len() as f32;
            height += BLOCK_GAP + PANEL_HEADING + rows * PICKER_ROW + (rows - 1.0).max(0.0) * ROW_GAP;
        }
        height += match self.panel {
            Some(Panel::Hotkeys) => BLOCK_GAP + PANEL_HEADING + 3.0 * 18.0,
            Some(Panel::Settings) => BLOCK_GAP + PANEL_HEADING + 4.0 * 24.0,
            None => 0.0,
        };
        height
    }
}

fn tint(colour: u32, alpha: f32) -> Hsla {
    Hsla::from(rgb(colour)).opacity(alpha)
}

fn icon(name: &'static str, size: f32, colour: u32) -> impl IntoElement {
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
fn chip(kind: Chip) -> impl IntoElement {
    let (label, colour) = match kind {
        Chip::Set => ("SET", OK),
        Chip::Needed => ("NEEDED", WARN),
        Chip::Optional => ("OPTIONAL", FAINT),
    };
    div()
        .px_2()
        .py(px(3.))
        .rounded_full()
        .bg(tint(colour, 0.16))
        .text_size(px(9.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(colour))
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
    detail: Option<String>,
    right: gpui::AnyElement,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .w_full()
        .h(px(52.))
        .px_3()
        .rounded_lg()
        .bg(rgb(ROW))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(ROW_HOVER)))
        .child(icon(glyph, 18.0, SUBDUED))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(TEXT))
                        .child(label),
                )
                .when_some(detail, |this, detail| {
                    this.child(div().text_size(px(10.5)).text_color(rgb(FAINT)).child(detail))
                }),
        )
        .child(right)
}

/// The one saturated control, and the only filled thing on the surface.
fn cta(
    id: &'static str,
    label: &'static str,
    filled: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let (fill, text) = match (enabled, filled) {
        (false, _) => (BG, FAINT),
        (true, true) => (ACCENT, 0xffffff),
        (true, false) => (ROW, TEXT),
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
        .when(!enabled, |this| this.border_1().border_color(rgb(BORDER)))
        .text_size(px(14.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(text))
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(rgb(if filled { ACCENT_HOVER } else { ROW_HOVER })))
        })
        .child(label)
}

/// One of Loom's circular footer buttons.
fn footer_button(
    id: &'static str,
    glyph: &'static str,
    label: &'static str,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .cursor_pointer()
        .child(
            div()
                .flex()
                .justify_center()
                .items_center()
                .w(px(38.))
                .h(px(38.))
                .rounded_full()
                .bg(rgb(if active { ROW_HOVER } else { ROW }))
                .child(icon(glyph, 17.0, if active { ACCENT } else { SUBDUED })),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(if active { TEXT } else { FAINT }))
                .child(label),
        )
}

fn section(label: &'static str) -> impl IntoElement {
    div()
        .text_size(px(9.5))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(FAINT))
        .child(label)
}

impl Render for WinSendGpui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let target = self.target();
        let monitors = self.core.monitors();
        let can_retrieve = self.core.can_retrieve();
        let can_send = state != State::Blocked;
        // Exactly one control is filled, and which one follows the state: Send
        // while nothing is out, Retrieve once something is. Pressing Send twice
        // is already a no-op, so nothing is lost by demoting it.
        let sent = state == State::Sent;

        // Only when the shape actually changed. Resizing every frame would
        // fight the user's own drag on the window edge.
        let wanted = self.height();
        if (wanted - self.applied_height).abs() > 0.5 {
            window.resize(size(px(WIDTH), px(wanted)));
            self.applied_height = wanted;
        }

        let zoom_label = self
            .core
            .config
            .zoom_window
            .as_ref()
            .map(|w| w.title.clone())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Zoom video window".to_string());
        let media_label = self
            .core
            .config
            .media_window
            .as_ref()
            .map(|w| if w.title.is_empty() { w.process_name.clone() } else { w.title.clone() })
            .unwrap_or_else(|| "Media player".to_string());
        let zoom_set = self.core.config.zoom_window.is_some();
        let media_set = self.core.config.media_window.is_some();
        let picking = self.picking_display;
        let panel = self.panel;

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(header(sent, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_4()
                    .pb_3()
                    // The row is the value, and opens the picker in place
                    // rather than navigating anywhere.
                    .child(
                        row(
                            "row-display",
                            "display",
                            target
                                .as_ref()
                                .map(display_name)
                                .unwrap_or_else(|| "No target display".to_string()),
                            target.as_ref().map(display_detail),
                            if target.is_some() {
                                chip(Chip::Set).into_any_element()
                            } else {
                                chip(Chip::Needed).into_any_element()
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.picking_display = !this.picking_display;
                            cx.notify();
                        })),
                    )
                    .when(picking, |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .pt_1()
                                .child(section("CHOOSE A DISPLAY"))
                                .children(monitors.into_iter().map(|monitor| {
                                    let chosen =
                                        target.as_ref().map(|t| t.id.as_str()) == Some(&monitor.id);
                                    picker_row(monitor, chosen, cx)
                                })),
                        )
                    })
                    .child(
                        row(
                            "row-zoom",
                            "video",
                            zoom_label,
                            Some(if zoom_set {
                                "confirmed".to_string()
                            } else {
                                "pin someone in Zoom, then confirm".to_string()
                            }),
                            if zoom_set {
                                chip(Chip::Set).into_any_element()
                            } else {
                                chip(Chip::Needed).into_any_element()
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_zoom();
                            cx.notify();
                        })),
                    )
                    .child(
                        row(
                            "row-media",
                            "media",
                            media_label,
                            Some(if media_set {
                                "brought back by Restore Media".to_string()
                            } else {
                                "none selected".to_string()
                            }),
                            if media_set {
                                chip(Chip::Set).into_any_element()
                            } else {
                                chip(Chip::Optional).into_any_element()
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.note("The media window picker is not ported yet", false);
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
                    .child(
                        cta("send", "Send to Monitor", !sent && can_send, can_send).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.perform(Action::Send);
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        cta("retrieve", "Retrieve", sent, can_retrieve).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.perform(Action::Retrieve);
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(status_strip(&self.status))
            .when_some(panel, |this, panel| {
                this.child(div().px_4().pb_2().child(match panel {
                    Panel::Hotkeys => hotkeys_panel(&self.core).into_any_element(),
                    Panel::Settings => settings_panel(&self.core).into_any_element(),
                }))
            })
            .child(footer(panel, cx))
    }
}

/// The drawn header: name and live state on the left, close on the right.
///
/// Where Loom puts its logo. The state lives here rather than in a banner of
/// its own, because with the rows carrying their own chips the only thing left
/// to say at the top is whether a window is out right now.
fn header(sent: bool, cx: &mut Context<WinSendGpui>) -> impl IntoElement {
    let (tone, label) = if sent { (ACCENT, "ON TARGET") } else { (FAINT, "IDLE") };

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
        .child(
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .px_2()
                .py(px(4.))
                .rounded_full()
                .bg(tint(tone, 0.16))
                .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(rgb(tone)))
                .child(
                    div()
                        .text_size(px(9.5))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(tone))
                        .child(label),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(chrome_button("minimize", "minimize").on_click(cx.listener(
                    |_, _, window, _| window.minimize_window(),
                )))
                .child(chrome_button("close", "close").on_click(cx.listener(
                    |_, _, window, _| window.remove_window(),
                ))),
        )
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
        .hover(|style| style.bg(rgb(ROW_HOVER)))
        .child(icon(glyph, 15.0, SUBDUED))
        // Otherwise a press here starts dragging the window instead of
        // arming the button, and the click never lands.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// One display in the in-place picker.
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
        .px_3()
        .rounded_lg()
        .bg(rgb(ROW))
        .border_1()
        .border_color(rgb(if chosen { ACCENT } else { ROW }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(ROW_HOVER)))
        .child(div().text_size(px(12.5)).child(name))
        .child(div().text_size(px(10.)).text_color(rgb(FAINT)).child(detail))
        .on_click(cx.listener(move |this, _, _, cx| {
            let outcome = this.core.set_target_monitor(&monitor);
            this.report(outcome);
            this.picking_display = false;
            cx.notify();
        }))
}

/// The most recent messages, newest first and brightest.
///
/// Keeps its height whether or not it has anything to say, so a message
/// arriving never moves the controls above it.
fn status_strip(status: &[Message]) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .justify_center()
        .gap_1()
        .w_full()
        .h(px(42.))
        .px_4()
        .children(status.iter().enumerate().map(|(age, message)| {
            div()
                .text_size(px(11.))
                .text_color(rgb(if message.failed { ERR } else { OK }))
                .when(age > 0, |this| this.opacity(0.5))
                .child(message.text.clone())
        }))
}

/// What is bound. Behind a footer button rather than on the surface: with the
/// rows carrying the setup, the live shape has room for what is urgent only.
fn hotkeys_panel(core: &Core) -> impl IntoElement {
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

/// Read-only for now: the toggles write through `Core` in the eframe surface,
/// and duplicating that here before the front end is chosen would be two
/// places to keep one setting true.
fn settings_panel(core: &Core) -> impl IntoElement {
    let settings = [
        ("Minimize others on target", core.config.clear_target),
        ("Strip window frame", core.config.borderless),
        ("Fade out on Retrieve", core.config.fade_on_retrieve),
        ("Restore full-screen video", core.config.restore_fullscreen),
    ];

    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .child(section("SETTINGS"))
        .children(settings.into_iter().map(|(label, on)| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .w_full()
                .h(px(22.))
                .text_size(px(11.))
                .child(div().text_color(rgb(SUBDUED)).child(label))
                .child(if on {
                    icon("check", 13.0, OK).into_any_element()
                } else {
                    div().text_size(px(10.)).text_color(rgb(FAINT)).child("off").into_any_element()
                })
        }))
}

/// Loom's row of circular buttons, for what is occasionally needed and never
/// urgent.
fn footer(open: Option<Panel>, cx: &mut Context<WinSendGpui>) -> impl IntoElement {
    div()
        .flex()
        .justify_around()
        .items_center()
        .w_full()
        .pt_3()
        .pb_3()
        .px_4()
        .border_t_1()
        .border_color(rgb(BORDER))
        .child(
            footer_button("f-hotkeys", "keyboard", "Hotkeys", open == Some(Panel::Hotkeys))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.panel = (this.panel != Some(Panel::Hotkeys)).then_some(Panel::Hotkeys);
                    cx.notify();
                })),
        )
        .child(
            footer_button("f-settings", "settings", "Settings", open == Some(Panel::Settings))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.panel = (this.panel != Some(Panel::Settings)).then_some(Panel::Settings);
                    cx.notify();
                })),
        )
        .child(
            footer_button("f-diagnostics", "info", "Diagnostics", false).on_click(cx.listener(
                |this, _, _, cx| {
                    // The clipboard first, because the file lands in AppData,
                    // which Explorer hides by default.
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(this.core.diagnostics()));
                    let saved = this.core.save_diagnostics();
                    this.report(saved.map(|where_to| format!("Copied. {where_to}")));
                    cx.notify();
                },
            )),
        )
}
