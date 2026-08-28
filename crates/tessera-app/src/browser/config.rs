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
                    ui.checkbox(&mut settings.audio, "enabled (tiles still start muted)")
                        .on_hover_text("without this the audio pipeline is off entirely and the mute buttons do nothing");
                    ui.end_row();
                });

            ui.add_space(6.0);
            ui.weak("the mbps ceiling is the real decode-load guard; the stream cap is the blunt one");

            ui.add_space(14.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("interface").strong());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("UI scale");
                ui.weak(format!("{:.2}× — set per window in its gear panel", settings.ui_scale));
            });
            ui.weak("this is the starting scale for new windows; adjusting a window's gear changes that window and updates this default");

            ui.add_space(14.0);
            ui.separator();
            ui.add_space(6.0);
            ui.weak("changes apply immediately and are saved to");
            ui.weak("~/.config/opentessera/settings.conf");
        });
    });
}
