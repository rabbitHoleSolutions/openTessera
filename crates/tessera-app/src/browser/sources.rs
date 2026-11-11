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

use tessera_core::sources::{CameraStream, Source};

use crate::auth::ServerKind;

use super::{BrowserCtx, BrowserState};
use crate::overlay::UiSettings;

/// How long a source removal stays armed before it disarms itself.
const REMOVE_ARM_SECS: f64 = 4.0;

/// The outline drawn round a required field an action found empty. Brighter
/// than the error text under the form, because a 1.5px line has to carry
/// the same message as a whole sentence.
const MISSING_RED: egui::Color32 = egui::Color32::from_rgb(230, 80, 80);

/// A required field in the add-form, so an action that refuses can say
/// *which* box is the problem, not just that one is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Field {
    Path,
    User,
    Token,
    /// The name, then the url, of the camera row at this index.
    CameraName(usize),
    CameraUrl(usize),
}

/// Which kind of source the add-form is editing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourceKind {
    #[default]
    Folder,
    Plex,
    Jellyfin,
    Cameras,
}

impl SourceKind {
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::Folder => "folder",
            SourceKind::Plex => "plex",
            SourceKind::Jellyfin => "jellyfin",
            SourceKind::Cameras => "cameras",
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
    /// Group name for a camera source.
    pub group: String,
    /// Why the last add didn't take. Shown under the form; cleared as soon
    /// as the user edits the form again.
    pub error: String,
    /// Library titles ticked in the Plex picker. Each becomes its own
    /// `Source`, so the library tab lists them separately.
    pub picked: std::collections::BTreeSet<String>,
    /// Cameras being assembled into one source, edited in place before
    /// being committed together.
    pub cameras: Vec<CameraStream>,
    /// Required fields the last action found empty. Outlined in red until
    /// they are filled — checked against the field each frame, so typing
    /// clears the outline without every text box having to remember to.
    missing: std::collections::BTreeSet<Field>,
}

impl AddForm {
    /// Everything typed, gone — after a successful add, or a kind change.
    fn clear(&mut self) {
        let kind = self.kind;
        *self = AddForm { kind, ..AddForm::default() };
    }

    fn is_blank(&self, f: Field) -> bool {
        match f {
            Field::Path => self.path.trim().is_empty(),
            Field::User => self.user.trim().is_empty(),
            Field::Token => self.token.trim().is_empty(),
            Field::CameraName(i) => self.cameras.get(i).is_some_and(|c| c.name.trim().is_empty()),
            Field::CameraUrl(i) => self.cameras.get(i).is_some_and(|c| c.url.trim().is_empty()),
        }
    }

    /// Note which of `fields` are empty, replacing whatever the last action
    /// flagged; true when none are, i.e. the action may go ahead.
    fn require(&mut self, fields: &[Field]) -> bool {
        self.missing = fields.iter().copied().filter(|&f| self.is_blank(f)).collect();
        self.missing.is_empty()
    }

    fn is_missing(&self, f: Field) -> bool {
        self.missing.contains(&f) && self.is_blank(f)
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
            // Arm-then-confirm, like "clear queue" and the transport's
            // trash: removing a source throws away a token, and the ✖ is
            // one stray click away from every row in the list.
            if state
                .remove_armed
                .is_some_and(|(_, at)| at.elapsed().as_secs_f64() > REMOVE_ARM_SECS)
            {
                state.remove_armed = None;
            }
            let mut remove = None;
            for (i, src) in settings.sources.iter().enumerate() {
                ui.horizontal(|ui| {
                    let armed = matches!(state.remove_armed, Some((a, _)) if a == i);
                    let btn = if armed {
                        egui::Button::new(
                            egui::RichText::new("remove?").small().color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(150, 50, 50))
                    } else {
                        egui::Button::new(egui::RichText::new("✖").small())
                    };
                    if ui
                        .add(btn)
                        .on_hover_text(if armed {
                            "click again to forget this source and its token"
                        } else {
                            "remove this source (the media itself is untouched)"
                        })
                        .clicked()
                    {
                        if armed {
                            remove = Some(i);
                            state.remove_armed = None;
                        } else {
                            state.remove_armed = Some((i, std::time::Instant::now()));
                        }
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
                        for kind in [
                            SourceKind::Folder,
                            SourceKind::Plex,
                            SourceKind::Jellyfin,
                            SourceKind::Cameras,
                        ] {
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
                // Cameras have no single address — the form below collects
                // one stream per camera instead.
                if state.add.kind != SourceKind::Cameras {
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
                    outline_if(ui, &path_resp, state.add.is_missing(Field::Path));
                    path_resp.on_hover_text(match state.add.kind {
                        SourceKind::Folder => "folder (or file) path on disk",
                        _ => "server url, e.g. http://nas:8096",
                    });
                } else {
                    ui.add_sized(
                        [320.0, 18.0],
                        egui::TextEdit::singleline(&mut state.add.group)
                            .hint_text("name for this group, e.g. Outside"),
                    );
                }
            });

            // Once the picker is up it *is* the library choice, so the box
            // goes away rather than sit there looking like it still counts
            // (its name, if any, was carried into the picker as a tick).
            let picking = services.auth.state.libraries.is_some();
            if !matches!(state.add.kind, SourceKind::Folder | SourceKind::Cameras) && !picking {
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

            let added: Vec<Source> = match state.add.kind {
                SourceKind::Jellyfin => jellyfin(ui, state, services),
                SourceKind::Plex => plex(ui, state, services),
                SourceKind::Folder => folder(ui, state).into_iter().collect(),
                SourceKind::Cameras => cameras(ui, state, services).into_iter().collect(),
            };

            // Every outlined box filled in: the sentence explaining the
            // outlines is now wrong, so it goes with them. (Only a refused
            // action sets `missing`, so this can't eat any other error.)
            if !state.add.missing.is_empty()
                && state.add.missing.iter().all(|&f| !state.add.is_blank(f))
            {
                state.add.missing.clear();
                state.add.error.clear();
            }

            if !services.auth.state.error.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(240, 140, 140),
                    &services.auth.state.error,
                );
            }
            if !state.add.error.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(240, 140, 140), &state.add.error);
            }

            if !added.is_empty() {
                // Several at once, because the Plex picker can tick more
                // than one library and each is its own source. Duplicates
                // are reported rather than silently skipped, but they do not
                // stop the rest of the batch.
                let mut fresh = Vec::new();
                let mut dupes = Vec::new();
                for src in added {
                    if settings.sources.contains(&src) {
                        println!("settings: source \"{}\" already configured", src.label());
                        dupes.push(src.label());
                    } else {
                        println!("settings: added source \"{}\"", src.label());
                        fresh.push(src.label());
                        settings.sources.push(src);
                    }
                }
                state.add.error = if dupes.is_empty() {
                    String::new()
                } else {
                    format!("already configured: {}", dupes.join(", "))
                };
                if !fresh.is_empty() {
                    state.flash(match fresh.len() {
                        1 => format!("added \"{}\"", fresh[0]),
                        n => format!("added {n} libraries"),
                    });
                    // Only a clean add tears the form down; leaving it up
                    // after a duplicate keeps the picker on screen so the
                    // other libraries are still one click away.
                    if dupes.is_empty() {
                        state.add.clear();
                        services.auth.reset();
                    }
                }
            }

            ui.add_space(10.0);
            super::flash_label(ui, state);
            ui.add_space(6.0);
            ui.weak("saved to ~/.config/opentessera/settings.conf, including access tokens");
        });
    });
}

/// Outline `resp` in red when `missing`. Painted over the widget after the
/// fact rather than restyled, so a field looks exactly as it always does
/// until an action has found it empty.
fn outline_if(ui: &egui::Ui, resp: &egui::Response, missing: bool) {
    if missing {
        ui.painter().rect_stroke(
            resp.rect.expand(1.0),
            ui.visuals().widgets.inactive.rounding,
            egui::Stroke::new(1.5_f32, MISSING_RED),
        );
    }
}

/// LAN discovery, then username/password, with an API key as the fallback.
///
/// Ends in the same picker as Plex: signing in used to mint a token and
/// immediately commit a source scoped by whatever was in the library text
/// box — a name the user had not been shown and, on a fresh server, could
/// not know.
fn jellyfin(ui: &mut egui::Ui, state: &mut BrowserState, services: &mut BrowserCtx) -> Vec<Source> {
    let mut added = Vec::new();
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
    if services.auth.state.libraries.is_none() {
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
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("user");
        let user = ui.add_sized([140.0, 18.0], egui::TextEdit::singleline(&mut state.add.user));
        outline_if(ui, &user, state.add.is_missing(Field::User));
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
            // No password is fine: Jellyfin lets an account have none.
            if !state.add.require(&[Field::Path, Field::User]) {
                state.add.error = match state.add.missing.len() {
                    2 => "a server url and a username are both needed",
                    _ if state.add.is_missing(Field::Path) => "pick or type a server url first",
                    _ => "enter your Jellyfin username",
                }
                .into();
            } else {
                state.add.error.clear();
                let (u, p) = (state.add.user.clone(), state.add.pass.clone());
                services.auth.jellyfin_login(&state.add.path, &u, &p, services.plex_client_id);
            }
        }
    });

    // A completed sign-in opens the picker rather than committing a
    // source: the token it just minted is exactly what is needed to ask
    // the server which libraries it has.
    if let Some((url, token)) = services.auth.take_jellyfin_token() {
        state.add.path = url.clone();
        state.add.token = token.clone();
        state.add.picked.clear();
        services.auth.list_libraries(ServerKind::Jellyfin, &url, &token);
    }

    added.extend(library_picker(ui, state, services));

    ui.horizontal(|ui| {
        ui.weak("or paste an API key:");
        let token = ui.add_sized(
            [200.0, 18.0],
            egui::TextEdit::singleline(&mut state.add.token)
                .hint_text("API key")
                .password(true),
        );
        outline_if(ui, &token, state.add.is_missing(Field::Token));
        let (url, key) = (
            state.add.path.trim().to_owned(),
            state.add.token.trim().to_owned(),
        );
        if ui
            .add_enabled(!services.auth.state.busy, egui::Button::new("browse libraries"))
            .on_hover_text("list this server's libraries and pick from them")
            .clicked()
        {
            if !state.add.require(&[Field::Path, Field::Token]) {
                state.add.error = "a server url and an API key are both needed".into();
            } else {
                state.add.error.clear();
                state.add.picked.clear();
                services.auth.list_libraries(ServerKind::Jellyfin, &url, &key);
            }
        }
        // Hidden under the picker, which has its own buttons and has
        // taken the library box's place.
        if services.auth.state.libraries.is_none()
            && ui
                .button("add")
                .on_hover_text("add with the library box above (all libraries if blank)")
                .clicked()
        {
            if !state.add.require(&[Field::Path, Field::Token]) {
                state.add.error = "a server url and an API key are both needed".into();
            } else {
                added.push(Source::Jellyfin {
                    url: url.trim_end_matches('/').to_owned(),
                    api_key: key,
                    library: state.add.library_filter(),
                });
            }
        }
    });
    added
}

/// plex.tv link-code flow, with a token as the fallback.
///
/// Picking a server no longer adds it. It lists that server's movie
/// libraries and offers them, because the alternative was typing a library
/// title into a box before you had ever been shown one — and because the
/// add used to tear the whole form down, so a second library meant walking
/// back to your phone and linking again. Each ticked library becomes its
/// own `Source`, which is what makes them browse separately in the library
/// tab.
fn plex(ui: &mut egui::Ui, state: &mut BrowserState, services: &mut BrowserCtx) -> Vec<Source> {
    let mut added = Vec::new();
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
            // One run of text with a link in it: no item spacing, or the
            // comma after the link floats free of it.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.weak("enter this at ");
                ui.hyperlink_to("plex.tv/link", "https://plex.tv/link");
                ui.weak(", then wait here");
            });
        } else if services.auth.state.busy {
            ui.weak("working…");
        }
    });
    // Server list — only while no server has been chosen yet.
    if services.auth.state.libraries.is_none() {
        let mut chosen: Option<(String, String)> = None;
        for choice in &services.auth.state.plex_servers {
            ui.horizontal(|ui| {
                // The reachable address if one answered; otherwise the first
                // candidate anyway — a firewall at add time doesn't mean the
                // server is the wrong one.
                let base = choice.reachable.clone().unwrap_or_else(|| choice.fallback.clone());
                let label = format!("{}{}", choice.name, if choice.owned { "" } else { " (shared)" });
                if ui
                    .button(label)
                    .on_hover_text(format!("{base}\nlist this server's libraries"))
                    .clicked()
                {
                    chosen = Some((base.trim_end_matches('/').to_owned(), choice.token.clone()));
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
        if let Some((base, token)) = chosen {
            state.add.path = base.clone();
            state.add.token = token.clone();
            state.add.picked.clear();
            services.auth.list_libraries(ServerKind::Plex, &base, &token);
        }
    }

    added.extend(library_picker(ui, state, services));

    ui.horizontal(|ui| {
        ui.weak("or paste a token:");
        let token = ui.add_sized(
            [200.0, 18.0],
            egui::TextEdit::singleline(&mut state.add.token)
                .hint_text("X-Plex-Token")
                .password(true),
        );
        outline_if(ui, &token, state.add.is_missing(Field::Token));
        let (url, tok) = (
            state.add.path.trim().to_owned(),
            state.add.token.trim().to_owned(),
        );
        // "browse" reaches the same picker as the plex.tv flow, so a pasted
        // token is not a second-class way in.
        if ui
            .add_enabled(!services.auth.state.busy, egui::Button::new("browse libraries"))
            .on_hover_text("list this server's libraries and pick from them")
            .clicked()
        {
            if !state.add.require(&[Field::Path, Field::Token]) {
                state.add.error = "a server url and a token are both needed".into();
            } else {
                state.add.error.clear();
                state.add.picked.clear();
                services.auth.list_libraries(ServerKind::Plex, &url, &tok);
            }
        }
        // Hidden under the picker, which has its own buttons and has
        // taken the library box's place.
        if services.auth.state.libraries.is_none()
            && ui
                .button("add")
                .on_hover_text("add with the library box above (all libraries if blank)")
                .clicked()
        {
            if !state.add.require(&[Field::Path, Field::Token]) {
                state.add.error = "a server url and a token are both needed".into();
            } else {
                added.push(Source::Plex {
                    url: url.trim_end_matches('/').to_owned(),
                    token: tok,
                    library: state.add.library_filter(),
                });
            }
        }
    });
    added
}

/// The library picker: one tick-box per library on the chosen server, and
/// the two ways to finish — a source per ticked library, or a single source
/// spanning all of them.
///
/// Shared by both server kinds, because the problem was the same in each:
/// a free-text box asking for a library name the user had never been shown.
/// Only the `Source` each tick becomes differs.
fn library_picker(
    ui: &mut egui::Ui,
    state: &mut BrowserState,
    services: &mut BrowserCtx,
) -> Vec<Source> {
    let Some(libs) = &services.auth.state.libraries else {
        return Vec::new();
    };
    let (kind, base, token) = (libs.kind, libs.base.clone(), libs.token.clone());
    let build = |library: Option<String>| match kind {
        ServerKind::Plex => Source::Plex { url: base.clone(), token: token.clone(), library },
        ServerKind::Jellyfin => {
            Source::Jellyfin { url: base.clone(), api_key: token.clone(), library }
        }
    };
    let mut added = Vec::new();
    let mut back = false;

    // A name typed into the library box before signing in becomes a tick
    // here, once — it used to be dropped without a word. Matched the way
    // listing matches it, ignoring case.
    let typed = std::mem::take(&mut state.add.library);
    let typed = typed.trim();
    if !typed.is_empty() {
        let hit = libs.found.iter().find(|l| l.name.eq_ignore_ascii_case(typed));
        match hit {
            Some(lib) => {
                state.add.picked.insert(lib.name.clone());
            }
            None => state.add.error = format!("no library named \"{typed}\" on this server — pick from the list"),
        }
    }

    ui.add_space(6.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("libraries on this server").strong());
        ui.weak(&base);
        ui.add_space(4.0);
        if libs.found.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(230, 180, 80),
                "no movie libraries on this server",
            );
        }
        for lib in &libs.found {
            let mut on = state.add.picked.contains(&lib.name);
            let label = format!(
                "{}   ({} movie{})",
                lib.name,
                lib.movies,
                if lib.movies == 1 { "" } else { "s" }
            );
            if ui.checkbox(&mut on, label).changed() {
                if on {
                    state.add.picked.insert(lib.name.clone());
                } else {
                    state.add.picked.remove(&lib.name);
                }
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let n = state.add.picked.len();
            let label = match n {
                0 => "add selected".to_owned(),
                1 => "add 1 library".to_owned(),
                n => format!("add {n} libraries"),
            };
            if ui
                .add_enabled(n > 0, egui::Button::new(label))
                .on_hover_text("one source per library, so they browse separately")
                .clicked()
            {
                for name in &state.add.picked {
                    added.push(build(Some(name.clone())));
                }
            }
            if ui
                .add_enabled(!libs.found.is_empty(), egui::Button::new("add all as one source"))
                .on_hover_text("a single source listing every library together")
                .clicked()
            {
                added.push(build(None));
            }
            if ui.button("back").on_hover_text("choose a different server").clicked() {
                back = true;
            }
        });
    });
    ui.add_space(4.0);

    if back {
        services.auth.clear_libraries();
        state.add.picked.clear();
    }
    added
}

/// Cameras: scan the LAN, then assemble a list of streams.
///
/// Deliberately not a client for any one NVR. Discovery finds the cameras
/// and names them — which is the tedious half and needs no credentials —
/// while the stream URL is supplied directly, so anything that speaks RTSP
/// works and there is no vendor API, API key or certificate bypass in the
/// way.
fn cameras(ui: &mut egui::Ui, state: &mut BrowserState, services: &mut BrowserCtx) -> Option<Source> {
    let mut added = None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!services.auth.state.busy, egui::Button::new("scan network"))
            .on_hover_text("ask Ubiquiti devices on this LAN to identify themselves")
            .clicked()
        {
            services.auth.discover_cameras();
        }
        if services.auth.state.busy {
            ui.weak("scanning…");
        }
        ui.weak("or add a stream by hand below");
    });

    if services.auth.state.cameras_scanned && services.auth.state.cameras.is_empty() {
        ui.weak("nothing answered — add streams by hand, or check they are on this subnet");
    }
    // Discovered devices: one click each to take them into the list.
    let mut take: Option<(String, String, String)> = None;
    for cam in &services.auth.state.cameras {
        let already = state.add.cameras.iter().any(|c| c.url.contains(&cam.address));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!already, egui::Button::new("add").small())
                .on_hover_text("take this camera into the list below")
                .clicked()
            {
                take = Some((cam.name.clone(), cam.address.clone(), cam.model.clone()));
            }
            ui.label(&cam.name);
            if !cam.model.is_empty() {
                ui.weak(&cam.model);
            }
            ui.weak(&cam.address);
        });
    }
    if let Some((name, address, model)) = take {
        // A Ubiquiti camera adopted by Protect does not serve RTSP at all —
        // port 554 is refused on the camera itself, because the stream comes
        // from the controller. Prefilling `rtsp://<camera>:554/` for one is
        // therefore not a helpful guess but a wrong answer that looks like a
        // right one: it is accepted, saved, queued, and only fails at the
        // moment of playback. So a Protect camera gets an empty field and
        // says what it needs instead. A "UVC" model is Ubiquiti's own
        // prefix for its cameras.
        let protect = model.starts_with("UVC");
        state.add.cameras.push(CameraStream {
            name,
            url: if protect { String::new() } else { format!("rtsp://{address}:554/") },
        });
    }

    ui.add_space(8.0);
    if !state.add.cameras.is_empty() {
        ui.label(egui::RichText::new("streams in this source").strong());
    }
    let mut drop_at = None;
    // A copy, because the rows below borrow the cameras mutably.
    let missing = state.add.missing.clone();
    for (i, cam) in state.add.cameras.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            if ui.button(egui::RichText::new("✖").small()).on_hover_text("remove").clicked() {
                drop_at = Some(i);
            }
            let name = ui.add_sized(
                [140.0, 18.0],
                egui::TextEdit::singleline(&mut cam.name).hint_text("name"),
            );
            outline_if(
                ui,
                &name,
                missing.contains(&Field::CameraName(i)) && cam.name.trim().is_empty(),
            );
            let url = ui.add_sized(
                [340.0, 18.0],
                egui::TextEdit::singleline(&mut cam.url)
                    .hint_text("rtsps://<controller>:7441/<alias>?enableSrtp"),
            );
            outline_if(
                ui,
                &url,
                missing.contains(&Field::CameraUrl(i)) && cam.url.trim().is_empty(),
            );
            if cam.url.trim().is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 180, 80),
                    "needs the controller's url",
                );
            }
        });
    }
    if let Some(i) = drop_at {
        state.add.cameras.remove(i);
        // The flags are by row index, and the rows below this one just
        // moved up — they would outline the wrong row.
        state.add.missing.clear();
    }

    ui.horizontal(|ui| {
        if ui.button("add a stream").clicked() {
            state.add.cameras.push(CameraStream::default());
        }
        // Always clickable, like folder's: a grey button said nothing about
        // which of several rows was holding it back. Every row needs both
        // halves — a half-filled one used to be dropped without a word,
        // which is how a Protect camera with no url went missing.
        if ui
            .button("add source")
            .on_hover_text("each stream becomes an item you can queue like a film")
            .clicked()
        {
            let need: Vec<Field> = (0..state.add.cameras.len())
                .flat_map(|i| [Field::CameraName(i), Field::CameraUrl(i)])
                .collect();
            if state.add.cameras.is_empty() {
                state.add.error = "add a stream first — scan the network, or \"add a stream\"".into();
                return;
            }
            if !state.add.require(&need) {
                state.add.error =
                    "every stream needs a name and a url — fill in or remove the outlined ones".into();
                return;
            }
            state.add.error.clear();
            let streams: Vec<CameraStream> = state
                .add
                .cameras
                .iter()
                .map(|c| CameraStream {
                    name: c.name.trim().to_owned(),
                    url: c.url.trim().to_owned(),
                })
                .collect();
            let label = match state.add.group.trim() {
                "" => "cameras".to_owned(),
                g => g.to_owned(),
            };
            added = Some(Source::Cameras { label, streams });
        }
    });
    ui.add_space(6.0);
    ui.colored_label(
        egui::Color32::from_rgb(230, 180, 80),
        "UniFi cameras do not serve RTSP themselves — the stream comes from the controller.",
    );
    ui.weak("In Protect: open the camera's settings, turn on RTSP for a channel, and copy the");
    ui.weak("rtsps://<controller>:7441/<alias> url it shows you into the field above.");
    added
}

fn folder(ui: &mut egui::Ui, state: &mut BrowserState) -> Option<Source> {
    // Deliberately always clickable: a permanently grey button reads as
    // "this feature doesn't work". Say what is missing instead.
    if ui.button("add source").clicked() {
        if !state.add.require(&[Field::Path]) {
            state.add.error = "enter a folder path first".into();
        } else {
            return Some(Source::Folder { path: state.add.path.trim().to_owned() });
        }
    }
    None
}
