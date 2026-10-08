//! Config tab: the preferences that apply to the whole app.
//!
//! Everything here is global by nature — one decode budget across every
//! screen, one mpv option set applied to every tile. Anything that is
//! per-window belongs in that window's gear panel instead, not here.

use crate::overlay::UiSettings;

pub fn draw(ctx: &egui::Context, settings: &mut UiSettings) {
    egui::CentralPanel::default().show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("playback").strong());
            ui.weak("the decode budget is shared by every window on every monitor");
            ui.add_space(6.0);

            egui::Grid::new("config-playback")
                .num_columns(2)
                .spacing(egui::vec2(16.0, 8.0))
                .show(ui, |ui| {
                    ui.label("max concurrent streams");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut settings.max_streams).clamp_range(1..=128));
                        ui.weak("hard cap on playing tiles");
                    });
                    ui.end_row();

                    ui.label("cache per stream");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut settings.cache_mb)
                                .clamp_range(64..=8192)
                                .suffix(" MiB"),
                        );
                        ui.weak("mpv demuxer cache — bigger rides out network stalls");
                    });
                    ui.end_row();

                    ui.label("audio");
                    ui.checkbox(&mut settings.audio, "enabled (tiles after the first start muted)")
                        .on_hover_text("without this the audio pipeline is off entirely and the mute buttons do nothing");
                    ui.end_row();
                });

            ui.add_space(6.0);
            ui.weak("the decode-load ceiling is the real guard; this stream cap is the blunt one");

            ui.add_space(14.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("interface").strong());
            ui.add_space(4.0);
            ui.weak("scale is per window, set in each window's gear and remembered per monitor");
            ui.weak("it is a target: a tile's controls shrink below it when the tile is too small to hold them");
            ui.add_space(6.0);
            egui::Grid::new("config-scale")
                .num_columns(2)
                .spacing(egui::vec2(16.0, 6.0))
                .show(ui, |ui| {
                    ui.label("fallback target scale");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut settings.ui_scale)
                                .clamp_range(0.75..=3.0)
                                .speed(0.05)
                                .suffix("×"),
                        );
                        ui.weak("for a monitor with nothing remembered");
                    });
                    ui.end_row();
                });

            if settings.monitor_scale.is_empty() {
                ui.add_space(4.0);
                ui.weak("no monitor has its own scale yet");
            } else {
                ui.add_space(8.0);
                ui.label("remembered per monitor");
                // Listed here because otherwise a scale saved months ago on
                // a display you rarely use is invisible and unclearable.
                let mut forget = None;
                for (name, scale) in settings.monitor_scale.iter() {
                    ui.horizontal(|ui| {
                        if ui
                            .button(egui::RichText::new("✖").small())
                            .on_hover_text("forget this monitor's scale (back to the fallback)")
                            .clicked()
                        {
                            forget = Some(name.clone());
                        }
                        ui.label(format!("{name}  —  {scale:.2}×"));
                    });
                }
                if let Some(name) = forget {
                    settings.monitor_scale.remove(&name);
                    println!("settings: forgot UI scale for {name}");
                }
                ui.weak("windows already open keep their current scale until you change it");
            }

            ui.add_space(14.0);
            ui.separator();
            ui.add_space(6.0);
            ui.weak("changes apply immediately and are saved to");
            ui.weak("~/.config/opentessera/settings.conf");
        });
    });
}
