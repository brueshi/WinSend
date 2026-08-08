//! egui front end. Presentation only: every decision lives in `Core`.

use std::collections::HashMap;

use eframe::egui;

use crate::core::{Core, Failure};
use crate::platform::WindowCandidate;

/// The utility sits on screen during a broadcast, so it stays small — except
/// while picking a window, where showing only two entries would force scrolling
/// through a list the user is trying to compare visually.
const COMPACT_SIZE: egui::Vec2 = egui::vec2(340.0, 260.0);
/// Tall enough to compare several candidates without scrolling. The user can
/// still resize from here; this is only the starting size.
const PICKER_SIZE: egui::Vec2 = egui::vec2(460.0, 640.0);

const ACCENT: egui::Color32 = egui::Color32::from_rgb(78, 142, 240);
const OK: egui::Color32 = egui::Color32::from_rgb(102, 187, 122);
const ERR: egui::Color32 = egui::Color32::from_rgb(226, 106, 106);

#[derive(PartialEq, Eq)]
enum Screen {
    Main,
    Settings,
    SelectWindow,
}

enum Status {
    Idle,
    Ok(String),
    Err(String),
}

pub struct WinSendApp {
    core: Core,
    screen: Screen,
    status: Status,
    /// Picker contents, snapshotted when the screen opens so the list does not
    /// reshuffle under the cursor while the user is reading it.
    candidates: Vec<WindowCandidate>,
    thumbnails: HashMap<u64, egui::TextureHandle>,
}

impl WinSendApp {
    pub fn new(cc: &eframe::CreationContext<'_>, core: Core) -> Self {
        apply_style(&cc.egui_ctx);
        // Only the debug-only screen override below mutates this.
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut app = Self {
            core,
            screen: Screen::Main,
            status: Status::Idle,
            candidates: Vec::new(),
            thumbnails: HashMap::new(),
        };

        // Debug builds only: open straight onto a screen so it can be inspected
        // without clicking through. Compiled out of release entirely.
        #[cfg(debug_assertions)]
        match std::env::var("WINSEND_SCREEN").as_deref() {
            Ok("settings") => app.screen = Screen::Settings,
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
        self.screen = Screen::SelectWindow;
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(PICKER_SIZE));
    }

    fn leave_picker(&mut self, ctx: &egui::Context, to: Screen) {
        self.screen = to;
        self.thumbnails.clear();
        self.candidates.clear();
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(COMPACT_SIZE));
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
                    self.screen = Screen::Settings;
                }
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

        ui.vertical_centered_justified(|ui| {
            if ui.add(primary).clicked() {
                let outcome = self.core.send();
                self.report(ctx, outcome);
            }
            ui.add_space(6.0);

            let can_retrieve = self.core.can_retrieve();
            let response = ui.add_enabled(can_retrieve, button("Retrieve"));
            if response.clicked() {
                let outcome = self.core.retrieve();
                self.report(ctx, outcome);
            }
            if !can_retrieve {
                response.on_hover_text("Nothing has been sent yet this session");
            }
        });

        self.status_bar(ui);

        // Mock-only: exercise the "window vanished" path without needing Zoom.
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
        }
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if ui.button(egui::RichText::new("Back").size(11.0)).clicked() {
                self.screen = Screen::Main;
            }
            ui.label(egui::RichText::new("Settings").size(14.0).strong());
        });
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

        self.status_bar(ui);
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
        egui::CentralPanel::default().show(ctx, |ui| match self.screen {
            Screen::Main => self.main_screen(ui, ctx),
            Screen::Settings => self.settings_screen(ui, ctx),
            Screen::SelectWindow => self.select_window_screen(ui, ctx),
        });
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
