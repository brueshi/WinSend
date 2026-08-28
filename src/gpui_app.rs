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
    AssetSource, Context, FocusHandle, FontWeight, Hsla, IntoElement, KeyDownEvent, MouseButton,
    RenderImage, SharedString, Window, div, img, prelude::*, px, rgb, size, svg,
};

use crate::core::{Core, Failure};
use crate::hotkey::{Action, Hotkey, Key};
use crate::platform::{MonitorInfo, WindowCandidate};

pub const WIDTH: f32 = 340.0;

/// The height with nothing expanded: the shape that sits on screen during a
/// broadcast, and the one worth keeping small.
pub const HEIGHT: f32 = 416.0;

/// The sub-surfaces, each sized for its own content.
/// A row plus the gap under it, measured rather than guessed.
const CANDIDATE_ROW: f32 = 79.0;
/// Beyond this the list scrolls rather than the window growing off the screen.
const PICKER_MAX_HEIGHT: f32 = 620.0;
const SETTINGS_HEIGHT: f32 = 454.0;
const HOTKEYS_HEIGHT: f32 = 260.0;

/// What an expanded block adds.
const PANEL_HEADING: f32 = 21.0;
const PICKER_ROW: f32 = 36.0;
const ROW_HEIGHT: f32 = 56.0;
const ROW_GAP: f32 = 10.0;
const BLOCK_GAP: f32 = 12.0;

const BG: u32 = 0x141414;
/// The row fill. Loom's rows are a light grey against white; this is the same
/// one-step lift against the panel.
const ROW: u32 = 0x232323;
const ROW_HOVER: u32 = 0x2e2e2e;
const BORDER: u32 = 0x303030;
const TEXT: u32 = 0xf2f2f2;
const SUBDUED: u32 = 0x8f8f8f;
const FAINT: u32 = 0x5e5e5e;
const ACCENT: u32 = 0x4e8ef0;
const ACCENT_HOVER: u32 = 0x6ba2f5;
const OK: u32 = 0x66bb7a;
const WARN: u32 = 0xe0a458;
const ERR: u32 = 0xe26a6a;

/// How long a toast stays up.
///
/// Long enough to read one line, short enough that it is gone before the
/// operator needs the controls under it. A failure holds longer, because it is
/// the one worth reading twice.
const TOAST_LIFE: std::time::Duration = std::time::Duration::from_secs(3);
const TOAST_LIFE_FAILED: std::time::Duration = std::time::Duration::from_secs(6);

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
    Likely,
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
    Picker(PickerFor),
    Settings,
    Hotkeys,
}

pub struct WinSendGpui {
    core: Core,
    /// The message currently showing, if any. One at a time and transient:
    /// a strip reserved for messages costs the live surface its height every
    /// day for something that is on screen for three seconds.
    toast: Option<Message>,
    /// Which toast the pending dismissal belongs to, so a newer message is not
    /// cleared by the timer armed for the one it replaced.
    toast_seq: u64,
    screen: Screen,
    /// Open only while the target display is being chosen, so the row reads as
    /// a value most of the time and a picker briefly.
    picking_display: bool,
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
    /// The height last asked for, so the window is resized when the layout
    /// changes shape rather than on every frame.
    applied_height: f32,
}

impl WinSendGpui {
    pub fn new(core: Core, cx: &mut Context<Self>) -> Self {
        let mut app = Self {
            core,
            toast: None,
            toast_seq: 0,
            screen: Screen::Main,
            picking_display: false,
            candidates: Vec::new(),
            thumbnails: HashMap::new(),
            capturing: None,
            focus_handle: cx.focus_handle(),
            applied_height: HEIGHT,
        };
        app.open_requested_screen();
        app
    }

    /// Open straight onto a surface, the same way the eframe binary does.
    ///
    /// Debug only: a release build has no business reading this, and the
    /// surfaces are all a press away in any case.
    fn open_requested_screen(&mut self) {
        #[cfg(debug_assertions)]
        match std::env::var("WINSEND_SCREEN").as_deref() {
            Ok("select") => self.open_picker(PickerFor::Zoom),
            Ok("media") => self.open_picker(PickerFor::Media),
            Ok("settings") => self.screen = Screen::Settings,
            Ok("hotkeys") => self.screen = Screen::Hotkeys,
            _ => {}
        }
    }



    fn open_picker(&mut self, picker: PickerFor) {
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
        self.screen = Screen::Picker(picker);
    }

    fn confirm(&mut self, picker: PickerFor, candidate: &WindowCandidate, cx: &mut Context<Self>) {
        let outcome = match picker {
            PickerFor::Zoom => self.core.confirm_window(candidate),
            PickerFor::Media => self.core.confirm_media_window(candidate),
        };
        self.report(outcome, cx);
        self.screen = Screen::Main;
        self.candidates.clear();
        self.thumbnails.clear();
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
        self.capturing = None;
        let outcome = self.core.set_hotkey(action, Some(hotkey));
        self.report(outcome, cx);
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

        let life = if failed { TOAST_LIFE_FAILED } else { TOAST_LIFE };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(life).await;
            this.update(cx, |this, cx| {
                // Only if nothing has been said since. A newer message owns
                // the toast and its own timer.
                if this.toast_seq == seq {
                    this.toast = None;
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
        self.toast_seq += 1;
        self.toast = None;
        cx.notify();
    }

    fn perform(&mut self, action: Action, cx: &mut Context<Self>) {
        let outcome = match action {
            Action::Send => self.core.send(),
            Action::Retrieve => self.core.retrieve(),
            Action::RestoreMedia => self.core.restore_media(),
        };
        self.report(outcome, cx);
    }
    fn height(&self) -> f32 {
        match self.screen {
            Screen::Picker(_) => {
                let rows = self.candidates.len() as f32;
                let content = HEADER_HEIGHT + 48.0 + rows * CANDIDATE_ROW + 7.0;
                content.min(PICKER_MAX_HEIGHT).max(240.0)
            }
            Screen::Settings => SETTINGS_HEIGHT,
            Screen::Hotkeys => HOTKEYS_HEIGHT,
            Screen::Main if self.picking_display => {
                let rows = self.core.monitors().len() as f32;
                HEIGHT
                    + BLOCK_GAP
                    + PANEL_HEADING
                    + rows * PICKER_ROW
                    + (rows - 1.0).max(0.0) * ROW_GAP
            }
            Screen::Main => HEIGHT,
        }
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
fn chip(kind: Chip, group: &'static str) -> impl IntoElement {
    let (label, colour) = match kind {
        Chip::Set => ("SET", OK),
        Chip::Needed => ("NEEDED", WARN),
        Chip::Optional => ("OPTIONAL", FAINT),
        Chip::Likely => ("LIKELY", ACCENT),
    };
    div()
        .flex_none()
        .whitespace_nowrap()
        .px_2p5()
        .py(px(4.))
        .rounded_full()
        .bg(tint(colour, 0.14))
        .group_hover(group, |style| style.bg(tint(colour, 0.28)))
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
    right: gpui::AnyElement,
    muted: bool,
) -> gpui::Stateful<gpui::Div> {
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
        // A pill, like the actions under it. One radius for everything that
        // is a control, rather than rows in one geometry and buttons in
        // another, which is what made the surface read as assembled from
        // parts rather than designed.
        .rounded_full()
        .bg(rgb(ROW))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(ROW_HOVER)))
        .active(|style| style.opacity(0.7))
        .child(icon(glyph, 19.0, if muted { SUBDUED } else { TEXT }))
        // One line. The value is the label, the way Loom's row says "No
        // Camera" rather than saying "Camera" and putting the answer
        // underneath it.
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(13.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(if muted { SUBDUED } else { TEXT }))
                .child(label),
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
                // Presses register. Without it the only feedback that a click
                // landed is whatever the action itself does, which for Send is
                // a window moving on another display.
                .active(|style| style.opacity(0.82))
        })
        .child(label)
}

/// One of Loom's circular footer buttons.
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
        .gap_1()
        .cursor_pointer()
        .active(|style| style.opacity(0.7))
        .child(
            div()
                .flex()
                .justify_center()
                .items_center()
                .w(px(38.))
                .h(px(38.))
                .rounded_full()
                .bg(rgb(ROW))
                .group_hover(id, |style| style.bg(rgb(ROW_HOVER)))
                .child(icon(glyph, 17.0, SUBDUED)),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(FAINT))
                .group_hover(id, |style| style.text_color(rgb(SUBDUED)))
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
        let wanted = self.height();
        if (wanted - self.applied_height).abs() > 0.5 {
            window.resize(size(px(WIDTH), px(wanted)));
            self.applied_height = wanted;
        }

        let screen = self.screen;
        let capturing = self.capturing;

        div()
            .track_focus(&self.focus_handle)
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
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
                Screen::Picker(PickerFor::Zoom) => {
                    self.sub_header("Select Zoom window", cx).into_any_element()
                }
                Screen::Picker(PickerFor::Media) => {
                    self.sub_header("Select media window", cx).into_any_element()
                }
                Screen::Settings => self.sub_header("Settings", cx).into_any_element(),
                Screen::Hotkeys => self.sub_header("Hotkeys", cx).into_any_element(),
            })
            .child(match screen {
                Screen::Main => self.main(cx).into_any_element(),
                Screen::Picker(picker) => self.picker(picker, cx).into_any_element(),
                Screen::Settings => self.settings(cx).into_any_element(),
                Screen::Hotkeys => self.hotkeys(cx).into_any_element(),
            })
            .when_some(self.toast.clone(), |this, message| this.child(toast(&message, cx)))
    }
}

impl WinSendGpui {
    /// The live surface: the three rows, the two actions, the footer.
    fn main(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let target = self.target();
        let monitors = self.core.monitors();
        let can_retrieve = self.core.can_retrieve();
        let can_send = state != State::Blocked;
        let sent = state == State::Sent;
        let zoom_set = self.core.config.zoom_window.is_some();
        let media_set = self.core.config.media_window.is_some();
        let picking = self.picking_display;

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
                                chip(Chip::Set, "row-display").into_any_element()
                            } else {
                                chip(Chip::Needed, "row-display").into_any_element()
                            },
                            target.is_none(),
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
                            if zoom_set {
                                chip(Chip::Set, "row-zoom").into_any_element()
                            } else {
                                chip(Chip::Needed, "row-zoom").into_any_element()
                            },
                            !zoom_set,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_picker(PickerFor::Zoom);
                            cx.notify();
                        })),
                    )
                    .child(
                        row(
                            "row-media",
                            "media",
                            media_label,
                            if media_set {
                                chip(Chip::Set, "row-media").into_any_element()
                            } else {
                                chip(Chip::Optional, "row-media").into_any_element()
                            },
                            !media_set,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_picker(PickerFor::Media);
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
                        cta("send", "Send to Monitor", !sent && can_send, can_send).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.perform(Action::Send, cx);
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        cta("retrieve", "Retrieve", sent, can_retrieve).on_click(cx.listener(
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

        header_shell()
            .child(div().when(sent, |this| {
                this.flex()
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
            }))
            .child(header_controls(cx))
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
                        .hover(|style| style.bg(rgb(ROW_HOVER)))
                        .active(|style| style.opacity(0.6))
                        .child(icon("back", 15.0, SUBDUED))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.screen = Screen::Main;
                            this.capturing = None;
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

    /// The window selector.
    ///
    /// The thumbnail is the point rather than decoration: Zoom's main and
    /// video windows carry the same process, class and title, so the list can
    /// only be told apart by looking at it.
    fn picker(&mut self, picker: PickerFor, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = match picker {
            PickerFor::Zoom => "Pin someone in Zoom first, then pick the window showing only that video feed.",
            PickerFor::Media => "Pick the media player to restore. A full-screen video window often has no title, so go by the process.",
        };
        let candidates = self.candidates.clone();
        let thumbnails: HashMap<u64, Arc<RenderImage>> = self.thumbnails.clone();

        div()
            .id("picker")
            .flex()
            .flex_col()
            .flex_1()
            .gap_2()
            .px_4()
            .pb_4()
            .overflow_y_scroll()
            .child(div().text_size(px(11.)).text_color(rgb(SUBDUED)).pb_1().child(hint))
            .when(candidates.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .justify_center()
                        .py_8()
                        .text_size(px(12.))
                        .text_color(rgb(FAINT))
                        .child("Nothing to choose from"),
                )
            })
            .children(candidates.into_iter().map(move |candidate| {
                candidate_row(candidate, thumbnails.clone(), picker, cx)
            }))
    }

    /// The settings, as toggles that write straight through `Core`.
    fn settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .bg(rgb(ROW))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(ROW_HOVER)))
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
    }

    /// The bindings, and the capture that sets them.
    fn hotkeys(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let capturing = self.capturing;
        let bindings: Vec<(Action, Option<Hotkey>)> = Action::ALL
            .into_iter()
            .map(|action| (action, self.core.config.hotkeys.binding(action)))
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
                    .text_color(rgb(SUBDUED))
                    .pb_1()
                    .child("Work from inside Zoom, without focusing this window."),
            )
            .children(bindings.into_iter().map(|(action, bound)| {
                binding_row(action, bound, capturing == Some(action), cx)
            }))
    }
}

/// Which setting a toggle writes. Named rather than passed as a closure so the
/// list above stays a table of what exists rather than a list of callbacks.
#[derive(Clone, Copy)]
enum Setting {
    ClearTarget,
    Borderless,
    Fade,
    RestoreFullscreen,
    CheckUpdates,
}

impl Setting {
    fn apply(self, core: &mut Core, on: bool) -> Result<(), String> {
        match self {
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
        .bg(rgb(ROW))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(ROW_HOVER)))
        .active(|style| style.opacity(0.7))
        .child(match thumbnail {
            Some(image) => img(image)
                .w(px(96.))
                .h(px(54.))
                .rounded_lg()
                .into_any_element(),
            // A window that will not be captured is still a window that can be
            // chosen. GPU-composited ones routinely refuse.
            None => div()
                .flex()
                .justify_center()
                .items_center()
                .w(px(96.))
                .h(px(54.))
                .rounded_lg()
                .bg(rgb(BG))
                .text_size(px(9.))
                .text_color(rgb(FAINT))
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
                        .text_color(rgb(SUBDUED))
                        .truncate()
                        .child(detail),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(FAINT))
                        .truncate()
                        .child(where_to),
                ),
        )
        .when(candidate.likely_zoom && picker == PickerFor::Zoom, |this| {
            this.child(chip(Chip::Likely, "candidate"))
        })
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
        .bg(rgb(ROW))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(ROW_HOVER)))
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
                .child(div().text_size(px(10.)).text_color(rgb(FAINT)).child(detail)),
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
        .bg(if on { rgb(ACCENT).into() } else { tint(TEXT, 0.12) })
        .when(!on, |this| this.justify_start())
        .when(on, |this| this.justify_end())
        .child(div().w(px(16.)).h(px(16.)).rounded_full().bg(rgb(0xffffff)))
}

/// One action, its binding, and the controls that change it.
fn binding_row(
    action: Action,
    bound: Option<Hotkey>,
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
        .bg(rgb(ROW))
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
                } else {
                    match &bound {
                        Some(hotkey) => div()
                            .text_size(px(10.5))
                            .text_color(rgb(SUBDUED))
                            .child(hotkey.to_string())
                            .into_any_element(),
                        None => div()
                            .text_size(px(10.5))
                            .text_color(rgb(FAINT))
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
                    cx.notify();
                })),
            )
        })
        .child(
            small_button(set_id, if capturing { "Cancel" } else if bound.is_some() { "Change" } else { "Set" })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.capturing = if capturing { None } else { Some(action) };
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
        .bg(rgb(BG))
        .border_1()
        .border_color(rgb(BORDER))
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
        .hover(|style| style.bg(rgb(ROW_HOVER)))
        .active(|style| style.opacity(0.6))
        .child(icon(glyph, 15.0, SUBDUED))
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
        .bg(rgb(ROW))
        .border_1()
        .border_color(rgb(if chosen { ACCENT } else { ROW }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(ROW_HOVER)))
        .active(|style| style.opacity(0.7))
        .child(div().text_size(px(12.5)).child(name))
        .child(div().text_size(px(10.)).text_color(rgb(FAINT)).child(detail))
        .on_click(cx.listener(move |this, _, _, cx| {
            let outcome = this.core.set_target_monitor(&monitor);
            this.report(outcome, cx);
            this.picking_display = false;
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
fn toast(message: &Message, cx: &mut Context<WinSendGpui>) -> impl IntoElement {
    let (tone, glyph) = if message.failed { (ERR, "alert") } else { (OK, "check") };

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
        .bg(rgb(ROW_HOVER))
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
                .text_color(rgb(TEXT))
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
                .hover(|style| style.bg(tint(TEXT, 0.12)))
                .child(icon("close", 11.0, SUBDUED)),
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
        .border_color(rgb(BORDER))
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
