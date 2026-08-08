//! egui front end. Presentation only: every decision lives in `Core`.

use std::collections::HashMap;

use eframe::egui;

use crate::core::{Core, Failure};
use crate::hotkey::{Action, Hotkey, Key};
use crate::platform::WindowCandidate;
use crate::shell::{self, HotkeyReport, Shell, ShellEvent, TrayState};

/// The utility sits on screen during a broadcast, so it stays small — except
/// while picking a window, where showing only two entries would force scrolling
/// through a list the user is trying to compare visually.
const COMPACT_SIZE: egui::Vec2 = egui::vec2(340.0, 260.0);
/// Tall enough to compare several candidates without scrolling. The user can
/// still resize from here; this is only the starting size.
const PICKER_SIZE: egui::Vec2 = egui::vec2(460.0, 640.0);
/// Settings outgrew the compact height once hotkeys were added. It is a
/// transient screen rather than the one that sits over a broadcast, so it can
/// afford the room.
const SETTINGS_SIZE: egui::Vec2 = egui::vec2(360.0, 470.0);

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    Settings,
    SelectWindow,
}

impl Screen {
    fn size(self) -> egui::Vec2 {
        match self {
            Screen::Main => COMPACT_SIZE,
            Screen::Settings => SETTINGS_SIZE,
            Screen::SelectWindow => PICKER_SIZE,
        }
    }
}

enum Status {
    Idle,
    Ok(String),
    Err(String),
}

pub struct WinSendApp {
    core: Core,
    shell: Box<dyn Shell>,
    screen: Screen,
    status: Status,
    /// Picker contents, snapshotted when the screen opens so the list does not
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
    pub fn new(cc: &eframe::CreationContext<'_>, core: Core) -> Self {
        apply_style(&cc.egui_ctx);

        // The waker is a repaint request against a cloned context. Without it
        // a hotkey press would sit in the queue until something else woke the
        // window, which defeats the point of not having to touch the window.
        let ctx = cc.egui_ctx.clone();
        let shell = shell::create(Box::new(move || ctx.request_repaint()));
        shell.apply_hotkeys(core.config.hotkeys);

        // Only the debug-only screen override below mutates this.
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut app = Self {
            core,
            shell,
            screen: Screen::Main,
            status: Status::Idle,
            candidates: Vec::new(),
            thumbnails: HashMap::new(),
            capturing: None,
            hotkey_report: HotkeyReport::default(),
            hidden: false,
            quitting: false,
        };

        // Debug builds only: open straight onto a screen so it can be inspected
        // without clicking through. Compiled out of release entirely.
        #[cfg(debug_assertions)]
        match std::env::var("WINSEND_SCREEN").as_deref() {
            Ok("settings") => app.go_to(&cc.egui_ctx, Screen::Settings),
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
            Ok(message) => self.status = Status::Ok(message),
            Err(failure) => {
                let needs_selection = failure.needs_selection;
                self.status = Status::Err(failure.message);
                if needs_selection {
                    self.open_picker(ctx);
                }
            }
        }
    }

    /// Run an action, however it was asked for. A hotkey press and a button
    /// click are the same thing by the time they reach here.
    fn perform(&mut self, ctx: &egui::Context, action: Action) {
        let outcome = match action {
            Action::Send => self.core.send(),
            Action::Retrieve => self.core.retrieve(),
        };
        self.report(ctx, outcome);
    }

    fn handle(&mut self, ctx: &egui::Context, event: ShellEvent) {
        match event {
            ShellEvent::Trigger(action) => self.perform(ctx, action),
            // A refusal is shown the moment it is known. A binding the user
            // believes is live but which never registered is the one failure
            // this feature cannot afford.
            ShellEvent::HotkeysApplied(report) => {
                if let Some(summary) = report.summary() {
                    self.status = Status::Err(summary);
                }
                self.hotkey_report = report;
            }
            // A left click on the icon toggles, which is what makes the icon a
            // way to get the window back rather than only a way to lose it.
            ShellEvent::ShowWindow => self.set_hidden(ctx, !self.hidden),
            ShellEvent::ShowSettings => {
                self.go_to(ctx, Screen::Settings);
                self.set_hidden(ctx, false);
            }
            // The only path that actually exits. Everything else, including
            // the window's own close button, hides instead.
            ShellEvent::Quit => {
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
    fn start_capture(&mut self, action: Action) {
        self.capturing = Some(action);
        self.shell.apply_hotkeys(Default::default());
        self.status = Status::Idle;
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
            self.status = Status::Err(why);
            return;
        }
        match self.core.set_hotkey(action, Some(hotkey)) {
            Ok(message) => {
                self.status = Status::Ok(message);
                self.end_capture();
            }
            Err(failure) => self.status = Status::Err(failure.message),
        }
    }

    fn open_picker(&mut self, ctx: &egui::Context) {
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
        self.go_to(ctx, Screen::SelectWindow);
    }

    fn leave_picker(&mut self, ctx: &egui::Context, to: Screen) {
        self.thumbnails.clear();
        self.candidates.clear();
        self.go_to(ctx, to);
    }

    /// Switch screens and resize to suit. Also ends any capture in progress,
    /// so a half-finished binding cannot keep swallowing keys from a screen
    /// that has no way to finish it.
    fn go_to(&mut self, ctx: &egui::Context, screen: Screen) {
        self.capturing = None;
        self.screen = screen;
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(screen.size()));
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let (text, colour) = match &self.status {
            Status::Idle => (String::new(), egui::Color32::GRAY),
            Status::Ok(message) => (message.clone(), OK),
            Status::Err(message) => (message.clone(), ERR),
        };
        if text.is_empty() {
            return;
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new(text).color(colour).size(11.5));
    }

    fn main_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // No in-app title: the OS title bar already carries the app name, and
        // vertical space is scarce in a window this small.
        let target = self
            .core
            .config
            .resolve_monitor(&self.core.monitors())
            .map(|m| m.label())
            .unwrap_or_else(|| "no target monitor selected".to_string());

        let mut opening_settings = false;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("Target: {target}"))
                    .size(11.0)
                    .color(egui::Color32::from_gray(150)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(egui::RichText::new("Settings").size(11.0))
                    .clicked()
                {
                    opening_settings = true;
                }
            });
        });
        if opening_settings {
            self.go_to(ctx, Screen::Settings);
            return;
        }

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

        self.status_bar(ui);

        // Mock-only: exercise the "window vanished" path without needing Zoom,
        // and the hotkey path without a real key registration.
        #[cfg(not(windows))]
        {
            ui.add_space(10.0);
            ui.separator();
            if let Some(mock) = self.core.platform.as_mock() {
                let mut present = mock.zoom_present();
                if ui.checkbox(&mut present, "mock: Zoom running").changed() {
                    mock.set_zoom_present(present);
                }
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
        }
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut going_back = false;
        ui.horizontal(|ui| {
            if ui.button(egui::RichText::new("Back").size(11.0)).clicked() {
                going_back = true;
            }
            ui.label(egui::RichText::new("Settings").size(14.0).strong());
        });
        if going_back {
            self.go_to(ctx, Screen::Main);
            return;
        }
        ui.add_space(8.0);

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

        ui.add_space(10.0);

        let mut borderless = self.core.config.borderless;
        if ui
            .checkbox(&mut borderless, "Strip window frame when sending")
            .on_hover_text("Removes the title bar and border so the window fills the monitor edge to edge")
            .changed()
        {
            if let Err(message) = self.core.set_borderless(borderless) {
                self.status = Status::Err(message);
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

        self.status_bar(ui);
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

    fn select_window_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut going_back = false;
        ui.horizontal(|ui| {
            if ui.button(egui::RichText::new("Back").size(11.0)).clicked() {
                going_back = true;
            }
            ui.label(egui::RichText::new("Select Zoom Window").size(14.0).strong());
        });
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
            self.leave_picker(ctx, Screen::Main);
        } else if going_back {
            self.leave_picker(ctx, Screen::Settings);
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

        egui::CentralPanel::default().show(ctx, |ui| match self.screen {
            Screen::Main => self.main_screen(ui, ctx),
            // Scrollable as a safety net: the window is user-resizable, and
            // settings content clipped with no way to reach it would be worse
            // than a scrollbar that is usually not needed.
            Screen::Settings => {
                egui::ScrollArea::vertical().show(ui, |ui| self.settings_screen(ui, ctx));
            }
            Screen::SelectWindow => self.select_window_screen(ui, ctx),
        });
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
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    ctx.set_style(style);
}
