//! SMART window: a disk, its health summary ([`smart_summary`] rows with coloured markers)
//! and the raw attribute table; "not available" when the drive reports no SMART data.

use super::{error_text, tool_frame, Task, ToolEnv, ToolWindow, WindowKind, WindowOutcome};
use eframe::egui::{self, vec2, Color32, Sense};
use pm_analyze::{smart_summary, Health};
use pm_platform::SmartData;
use std::time::Duration;

pub fn health_color(h: Health) -> Color32 {
    match h {
        Health::Ok => Color32::from_rgb(0x4C, 0xAF, 0x50),
        Health::Warn => Color32::from_rgb(0xE0, 0x80, 0x10),
        Health::Bad => crate::theme::DANGER,
    }
}

/// What the window shows for disk `disk`.
enum Shown {
    Loading,
    Data(SmartData),
    NotAvailable,
    Error(String),
}

pub struct SmartWindow {
    pub disk: Option<u32>,
    task: Option<Task<Option<SmartData>>>,
    shown: Option<Shown>,
}

impl SmartWindow {
    pub fn new(disk: Option<u32>) -> SmartWindow {
        SmartWindow { disk, task: None, shown: None }
    }

    fn load(&mut self, env: &ToolEnv) {
        let Some(d) = self.disk else { return };
        let p = env.platform.clone();
        self.shown = Some(Shown::Loading);
        self.task = Some(Task::spawn("SMART", move |_progress, cancel| {
            cancel.check()?;
            p.smart(d)
        }));
    }
}

impl ToolWindow for SmartWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Smart
    }

    fn show(&mut self, ctx: &egui::Context, env: &ToolEnv) -> WindowOutcome {
        if self.shown.is_none() && self.disk.is_some() {
            self.load(env);
        }
        if let Some(t) = self.task.as_mut() {
            ctx.request_repaint_after(Duration::from_millis(150));
            if let Some(r) = t.poll() {
                self.shown = Some(match r {
                    Ok(Some(d)) => Shown::Data(d),
                    Ok(None) => Shown::NotAvailable,
                    Err(e) => Shown::Error(error_text(&e)),
                });
                self.task = None;
            }
        }
        if tool_frame(ctx, "SMART", [620.0, 520.0], |ui| self.ui(ui, env)) {
            WindowOutcome::Open
        } else {
            WindowOutcome::Close
        }
    }
}

impl SmartWindow {
    fn ui(&mut self, ui: &mut egui::Ui, env: &ToolEnv) {
        ui.horizontal(|ui| {
            ui.label("Disk:");
            let name = |i: u32| env.live.disk(i).map_or(format!("Disk {i}"), |d| d.display_name());
            let mut pick = self.disk;
            egui::ComboBox::from_id_salt("smart_disk")
                .width(380.0)
                .selected_text(pick.map_or("Choose a disk…".into(), name))
                .show_ui(ui, |ui| {
                    for d in &env.live.disks {
                        ui.selectable_value(&mut pick, Some(d.index), d.display_name());
                    }
                });
            if pick != self.disk {
                self.disk = pick;
                self.task = None;
                self.load(env);
            }
            if ui.add_enabled(self.task.is_none() && self.disk.is_some(), egui::Button::new("Reload")).clicked() {
                self.load(env);
            }
        });
        ui.separator();
        match &self.shown {
            None => {
                ui.weak("Choose a disk.");
            }
            Some(Shown::Loading) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading SMART data…");
                });
            }
            Some(Shown::NotAvailable) => {
                ui.label("SMART data is not available for this disk.");
            }
            Some(Shown::Error(e)) => super::danger(ui, e.as_str()),
            Some(Shown::Data(d)) => {
                egui::Grid::new("smart_summary").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                    for (name, value, h) in smart_summary(d) {
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 5.0, health_color(h));
                            ui.label(name);
                        });
                        ui.label(value);
                        ui.end_row();
                    }
                });
                if !d.attrs.is_empty() {
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        egui::Grid::new("smart_attrs").num_columns(6).striped(true).show(ui, |ui| {
                            for h in ["ID", "Attribute", "Value", "Worst", "Threshold", "Raw"] {
                                ui.strong(h);
                            }
                            ui.end_row();
                            for a in &d.attrs {
                                ui.label(format!("{:3}", a.id));
                                ui.label(&a.name);
                                ui.label(a.value.to_string());
                                ui.label(a.worst.to_string());
                                ui.label(a.threshold.to_string());
                                ui.label(a.raw.to_string());
                                ui.end_row();
                            }
                        });
                    });
                }
            }
        }
    }
}
