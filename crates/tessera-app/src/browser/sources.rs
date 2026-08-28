//! Sources tab: which servers and folders the wall draws from.
//!
//! This is data, not preference — server addresses with credentials
//! attached — which is why it is its own tab rather than a section of
//! config. It used to live in the wall's gear panel, floating over video
//! in a 420px column; the sign-in flows (LAN discovery, username and
//! password, plex.tv linking) are forms, and forms want a window.
//!
//! Credentials still land in `settings.conf` in the clear. See
//! `docs/ROADMAP.md` §9 — proper key management is tracked, not forgotten.

use tessera_core::sources::Source;

use super::{BrowserCtx, BrowserState};
use crate::overlay::UiSettings;

/// Which kind of source the add-form is editing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourceKind {
    #[default]
    Folder,
    Plex,
    Jellyfin,
}

impl SourceKind {
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::Folder => "folder",
            SourceKind::Plex => "plex",
            SourceKind::Jellyfin => "jellyfin",
        }
    }
}

/// The add-source form. Per browser window, and never persisted: the
/// password is used once to mint a token and then dropped.
#[derive(Default)]
pub struct AddForm {
    pub kind: SourceKind,
    pub path: String,
    pub token: String,
    pub library: String,
    pub user: String,
    pub pass: String,
    /// Why the last add didn't take. Shown under the form; cleared as soon
    /// as the user edits the form again.
    pub error: String,
}

impl AddForm {
    /// Everything typed, gone — after a successful add, or a kind change.
    fn clear(&mut self) {
        let kind = self.kind;
        *self = AddForm { kind, ..AddForm::default() };
    }

    fn library_filter(&self) -> Option<String> {
        let l = self.library.trim();
        (!l.is_empty()).then(|| l.to_owned())
    }
}

pub fn draw(
    ctx: &egui::Context,
    state: &mut BrowserState,
    settings: &mut UiSettings,
    services: &mut BrowserCtx,
) {
    services.auth.poll();

    egui::CentralPanel::default().show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("configured sources").strong());
            ui.weak("where the wall gets its media — available in the library tab as soon as you add one");
            ui.add_space(6.0);

            if settings.sources.is_empty() {
                ui.weak("none yet — add one below, or pass paths on the command line");
            }
            let mut remove = None;
            for (i, src) in settings.sources.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("✖").small())
                        .on_hover_text("remove this source (the media itself is untouched)")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                    ui.label(src.label());
                });
            }
            if let Some(i) = remove {
                let src = settings.sources.remove(i);
                println!("settings: removed source \"{}\"", src.label());
                // Selection is an index into this list; drop it rather than
                // let it point at whatever slid into the gap.
                state.source = None;
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("add a source").strong());
            ui.add_space(4.0);

            // Folders take a path; servers get a sign-in flow (discovery /
            // login / plex.tv link) with the raw token as the fallback,
            // since that is the only path that works with no internet or a
            // locked-down server.
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_source("src-kind")
                    .selected_text(state.add.kind.label())
                    .show_ui(ui, |ui| {
                        for kind in [SourceKind::Folder, SourceKind::Plex, SourceKind::Jellyfin] {
                            if ui
                                .selectable_value(&mut state.add.kind, kind, kind.label())
                                .clicked()
                            {
                                // Flows don't carry across kinds.
                                services.auth.reset();
                                state.add.clear();
                            }
                        }
                    });
                let path_hint = match state.add.kind {
                    SourceKind::Folder => "folder path",
                    _ => "server url",
                };
                let path_resp = ui.add_sized(
                    [320.0, 18.0],
                    egui::TextEdit::singleline(&mut state.add.path).hint_text(path_hint),
                );
                if path_resp.changed() {
                    state.add.error.clear();
                }
                path_resp.on_hover_text(match state.add.kind {
                    SourceKind::Folder => "folder (or file) path on disk",
                    _ => "server url, e.g. http://nas:8096",
                });
            });

            if state.add.kind != SourceKind::Folder {
                ui.horizontal(|ui| {
                    ui.label("library");
                    let lib = ui.add_sized(
                        [200.0, 18.0],
                        egui::TextEdit::singleline(&mut state.add.library)
                            .hint_text("optional — all libraries if blank"),
                    );
                    if lib.changed() {
                        state.add.error.clear();
                    }
                });
            }

            let added = match state.add.kind {
                SourceKind::Jellyfin => jellyfin(ui, state, services),
                SourceKind::Plex => plex(ui, state, services),
                SourceKind::Folder => folder(ui, state),
            };

            if !services.auth.state.error.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(240, 140, 140),
                    &services.auth.state.error,
                );
            }
            if !state.add.error.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(240, 140, 140), &state.add.error);
            }

            if let Some(src) = added {
                if settings.sources.contains(&src) {
                    println!("settings: source \"{}\" already configured", src.label());
                    state.add.error = format!("already configured: {}", src.label());
                } else {
                    println!("settings: added source \"{}\"", src.label());
                    let label = src.label();
                    settings.sources.push(src);
                    state.add.clear();
                    services.auth.reset();
                    state.flash(format!("added \"{label}\""));
                }
            }

            ui.add_space(10.0);
            super::flash_label(ui, state);
            ui.add_space(6.0);
            ui.weak("saved to ~/.config/opentessera/settings.conf, including access tokens");
        });
    });
}

/// LAN discovery, then username/password, with an API key as the fallback.
fn jellyfin(ui: &mut egui::Ui, state: &mut BrowserState, services: &mut BrowserCtx) -> Option<Source> {
    let mut added = None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!services.auth.state.busy, egui::Button::new("scan network"))
            .on_hover_text("ask Jellyfin servers on this LAN to identify themselves")
            .clicked()
        {
            services.auth.discover();
        }
        if services.auth.state.busy && services.auth.state.plex_code.is_none() {
            ui.weak("working…");
        }
    });
    if services.auth.state.scanned && services.auth.state.discovered.is_empty() {
        ui.weak("no servers answered on this network — type the url above instead");
    }
    let mut pick = None;
    for srv in &services.auth.state.discovered {
        if ui
            .selectable_label(state.add.path == srv.address, &srv.name)
            .on_hover_text(&srv.address)
            .clicked()
        {
            pick = Some(srv.address.clone());
        }
    }
    if let Some(address) = pick {
        state.add.path = address;
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("user");
        ui.add_sized([140.0, 18.0], egui::TextEdit::singleline(&mut state.add.user));
        ui.label("password");
        ui.add_sized(
            [140.0, 18.0],
            egui::TextEdit::singleline(&mut state.add.pass).password(true),
        );
        if ui
            .add_enabled(!services.auth.state.busy, egui::Button::new("sign in & add"))
            .on_hover_text("exchanges your password for an access token — the password is not stored")
            .clicked()
        {
            if state.add.path.trim().is_empty() {
                state.add.error = "pick or type a server url first".into();
            } else if state.add.user.trim().is_empty() {
                state.add.error = "enter your Jellyfin username".into();
            } else {
                state.add.error.clear();
                let (u, p) = (state.add.user.clone(), state.add.pass.clone());
                services.auth.jellyfin_login(&state.add.path, &u, &p);
            }
        }
    });

    ui.horizontal(|ui| {
        ui.weak("or paste an API key:");
        ui.add_sized(
            [200.0, 18.0],
            egui::TextEdit::singleline(&mut state.add.token)
                .hint_text("API key")
                .password(true),
        );
        if ui.button("add").clicked() {
            let (url, key) = (state.add.path.trim(), state.add.token.trim());
            if url.is_empty() || key.is_empty() {
                state.add.error = "a server url and an API key are both needed".into();
            } else {
                added = Some(Source::Jellyfin {
                    url: url.trim_end_matches('/').to_owned(),
                    api_key: key.to_owned(),
                    library: state.add.library_filter(),
                });
            }
        }
    });

    // A completed sign-in becomes a source here, where the library filter
    // from the form is in scope.
    if let Some((url, token)) = services.auth.take_jellyfin_token() {
        added = Some(Source::Jellyfin {
            url: url.trim_end_matches('/').to_owned(),
            api_key: token,
            library: state.add.library_filter(),
        });
    }
    added
}

/// plex.tv link-code flow, with a token as the fallback.
fn plex(ui: &mut egui::Ui, state: &mut BrowserState, services: &mut BrowserCtx) -> Option<Source> {
    let mut added = None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!services.auth.state.busy, egui::Button::new("link with plex.tv"))
            .on_hover_text("no token typing: enter a short code at plex.tv/link")
            .clicked()
        {
            services.auth.plex_link(services.plex_client_id);
        }
        if let Some(code) = &services.auth.state.plex_code {
            ui.label(
                egui::RichText::new(code)
                    .strong()
                    .size(24.0)
                    .color(egui::Color32::from_rgb(230, 180, 80)),
            );
            ui.weak("enter this at plex.tv/link, then wait here");
        } else if services.auth.state.busy {
            ui.weak("working…");
        }
    });
    for choice in &services.auth.state.plex_servers {
        ui.horizontal(|ui| {
            // The reachable address if one answered; otherwise the first
            // candidate anyway — a firewall at add time doesn't mean the
            // server is the wrong one.
            let base = choice.reachable.clone().unwrap_or_else(|| choice.fallback.clone());
            let label = format!("{}{}", choice.name, if choice.owned { "" } else { " (shared)" });
            if ui.button(label).on_hover_text(&base).clicked() {
                added = Some(Source::Plex {
                    url: base.trim_end_matches('/').to_owned(),
                    token: choice.token.clone(),
                    library: state.add.library_filter(),
                });
            }
            match &choice.reachable {
                Some(u) => ui.weak(u.clone()),
                None => ui.colored_label(
                    egui::Color32::from_rgb(230, 180, 80),
                    "didn't answer — try anyway",
                ),
            };
        });
    }

    ui.horizontal(|ui| {
        ui.weak("or paste a token:");
        ui.add_sized(
            [200.0, 18.0],
            egui::TextEdit::singleline(&mut state.add.token)
                .hint_text("X-Plex-Token")
                .password(true),
        );
        if ui.button("add").clicked() {
            let (url, tok) = (state.add.path.trim(), state.add.token.trim());
            if url.is_empty() || tok.is_empty() {
                state.add.error = "a server url and a token are both needed".into();
            } else {
                added = Some(Source::Plex {
                    url: url.trim_end_matches('/').to_owned(),
                    token: tok.to_owned(),
                    library: state.add.library_filter(),
                });
            }
        }
    });
    added
}

fn folder(ui: &mut egui::Ui, state: &mut BrowserState) -> Option<Source> {
    // Deliberately always clickable: a permanently grey button reads as
    // "this feature doesn't work". Say what is missing instead.
    if ui.button("add source").clicked() {
        let path = state.add.path.trim();
        if path.is_empty() {
            state.add.error = "enter a folder path first".into();
        } else {
            return Some(Source::Folder { path: path.to_owned() });
        }
    }
    None
}
