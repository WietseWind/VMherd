//! Cluster bookmarks: list, add / edit / delete, connect.

use std::time::{Duration, Instant};

use egui::{Align, Color32, Frame, Label, Layout, Margin, RichText, ScrollArea, Stroke, TextEdit, Ui, vec2};
use uuid::Uuid;

use crate::config::{Bookmark, Config, SecretSource};
use crate::secrets;
use crate::theme;
use crate::widgets::{self, PlateStyle};

pub enum ClusterAction {
    Connect(Uuid),
    /// Open the built-in demo cluster.
    Demo,
    /// `secret`: a new secret to save (None = keep the saved one). `forget_pin`: drop the pinned
    /// certificate. The app merges the edited fields into the bookmark and calls `finish_save`.
    Save {
        bookmark: Box<Bookmark>,
        secret: Option<String>,
        forget_pin: bool,
    },
    Delete(Uuid),
}

/// Connection feedback shown on the cards.
#[derive(Default)]
pub struct Status {
    pub connecting: Option<Uuid>,
    pub error: Option<(Uuid, String)>,
}

#[derive(Default)]
pub struct ClustersUi {
    form: Option<Form>,
    delete_armed: Option<(Uuid, Instant)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Keyring,
    MacKeychain,
    Command,
    File,
}

impl Source {
    /// The default for a new bookmark: the credential store, else (Linux without a Secret
    /// Service) the private file.
    fn default_here() -> Self {
        if secrets::store_status().is_ok() { Source::Keyring } else { Source::File }
    }

    fn saves_secret(self) -> bool {
        matches!(self, Source::Keyring | Source::File)
    }
}

struct Form {
    id: Option<Uuid>,
    name: String,
    url: String,
    token_id: String,
    source: Source,
    secret: String,
    mac_service: String,
    mac_account: String,
    command: String,
    /// shown only; the app keeps the current pin unless `forget_pin`
    pinned: Option<String>,
    forget_pin: bool,
    error: Option<String>,
}

impl Form {
    fn new() -> Self {
        Self {
            id: None,
            name: String::new(),
            url: "https://".into(),
            token_id: String::new(),
            source: Source::default_here(),
            secret: String::new(),
            mac_service: String::new(),
            mac_account: String::new(),
            command: String::new(),
            pinned: None,
            forget_pin: false,
            error: None,
        }
    }

    fn edit(b: &Bookmark) -> Self {
        let (mut mac_service, mut mac_account, mut command) = (String::new(), String::new(), String::new());
        let source = match &b.secret {
            SecretSource::Keyring => Source::Keyring,
            SecretSource::File => Source::File,
            SecretSource::MacKeychain { service, account } => {
                mac_service.clone_from(service);
                mac_account = account.clone().unwrap_or_default();
                Source::MacKeychain
            }
            SecretSource::Command { command: c } => {
                command.clone_from(c);
                Source::Command
            }
        };
        // Mac App Store build: those sources are gone; offer the Keychain instead and say why
        let (source, error) = match source {
            Source::MacKeychain | Source::Command if !secrets::EXTERNAL_SOURCES => (
                Source::Keyring,
                Some(format!(
                    "Reading the token secret from a Keychain item or a command is not available in the Mac App \
                     Store version. Enter the token secret to save it in the {}.",
                    secrets::STORE_NAME
                )),
            ),
            s => (s, None),
        };
        Self {
            id: Some(b.id),
            name: b.name.clone(),
            url: b.url.clone(),
            token_id: b.token_id.clone(),
            source,
            secret: String::new(),
            mac_service,
            mac_account,
            command,
            pinned: b.pinned_sha256.clone(),
            forget_pin: false,
            error,
        }
    }

    fn validate(&self, existing_secret: bool) -> Result<ClusterAction, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Give the cluster a name.".into());
        }
        let url = url::Url::parse(self.url.trim()).map_err(|e| format!("Invalid API URL: {e}."))?;
        if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
            return Err("The API URL must look like https://host:8006.".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("Leave user names and passwords out of the API URL; use the token fields.".into());
        }
        if url.scheme() == "http" && !is_loopback(&url) {
            return Err("Use https: plain http would send the token and every keystroke unencrypted.".into());
        }
        let token = self.token_id.trim();
        pve::check_token_id(token).map_err(|e| widgets::sentence(&e) + ".")?;
        let (secret_source, secret) = match self.source {
            Source::Keyring | Source::File => {
                if self.source == Source::Keyring {
                    secrets::store_status().clone()?;
                }
                let secret = self.secret.trim();
                if secret.is_empty() && !existing_secret {
                    return Err("Enter the token secret.".into());
                }
                let source = if self.source == Source::Keyring { SecretSource::Keyring } else { SecretSource::File };
                (source, (!secret.is_empty()).then(|| secret.to_owned()))
            }
            Source::MacKeychain | Source::Command if !secrets::EXTERNAL_SOURCES => {
                return Err(format!("Save the token secret in the {}.", secrets::STORE_NAME));
            }
            Source::Command => {
                let command = self.command.trim();
                if command.is_empty() {
                    return Err("Enter the command that prints the token secret.".into());
                }
                (SecretSource::Command { command: command.to_owned() }, None)
            }
            Source::MacKeychain => {
                if self.mac_service.trim().is_empty() {
                    return Err("Enter the service name of the Keychain item.".into());
                }
                let account = self.mac_account.trim();
                (
                    SecretSource::MacKeychain {
                        service: self.mac_service.trim().to_owned(),
                        account: (!account.is_empty()).then(|| account.to_owned()),
                    },
                    None,
                )
            }
        };
        let mut base = url.clone();
        base.set_path("");
        base.set_query(None);
        base.set_fragment(None);
        let bookmark = Bookmark {
            id: self.id.unwrap_or_else(Uuid::new_v4),
            name: name.to_owned(),
            url: base.as_str().trim_end_matches('/').to_owned(),
            token_id: token.to_owned(),
            secret: secret_source,
            pinned_sha256: None,
            grid: Vec::new(),
            nosync: Vec::new(),
        };
        Ok(ClusterAction::Save { bookmark: Box::new(bookmark), secret, forget_pin: self.forget_pin })
    }
}

/// http is only accepted for this machine (a local mock / tunnel).
fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// `AB:CD:…` shortened for a card, or a warning when the stored pin is not a fingerprint.
fn pin_label(pin: &str) -> String {
    if pve::parse_fingerprint(pin).is_some() {
        format!("Certificate pinned {}…", pin.chars().take(17).collect::<String>())
    } else {
        "Invalid certificate pin: edit the cluster and forget it".to_owned()
    }
}

fn card() -> Frame {
    Frame::new()
        .fill(theme::TILE_TOP)
        .stroke(Stroke::new(1.0, theme::LINE))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(16, 14))
        .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(100) })
}

/// First thing on an empty clusters screen: a way in without a Proxmox server. True = clicked.
fn demo_card(ui: &mut Ui) -> bool {
    let mut go = false;
    card().stroke(Stroke::new(1.0, theme::GREEN_DARK)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_max_width((ui.available_width() - 170.0).max(200.0));
                ui.label(widgets::spaced("NO PROXMOX AT HAND?", theme::bold(11.0), theme::GREEN, 2.0));
                ui.label(RichText::new("Try the demo cluster").font(theme::bold(18.0)).color(theme::TEXT));
                ui.add(
                    Label::new(
                        RichText::new(
                            "13 simulated VMs with live consoles, power buttons and a shell to type into. \
                             It all runs inside VMherd: no server, no network, nothing saved.",
                        )
                        .font(theme::mono(12.0))
                        .color(theme::MUTED),
                    )
                    .wrap(),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                go = widgets::plate_button(
                    ui,
                    widgets::spaced("TRY THE DEMO", theme::bold(12.0), theme::INK, 1.6),
                    vec2(140.0, 34.0),
                    PlateStyle::GO,
                    true,
                )
                .clicked();
            });
        });
    });
    go
}

fn field(ui: &mut Ui, label: &str, edit: TextEdit<'_>) -> egui::Response {
    ui.label(widgets::spaced(label, theme::label(11.0), theme::MUTED, 1.4));
    ui.add(edit.font(theme::mono(13.0)).desired_width(f32::INFINITY).margin(vec2(8.0, 6.0)))
}

impl ClustersUi {
    pub fn close_form(&mut self) {
        self.form = None;
    }

    /// Result of a Save: close the form, or keep it open with the error.
    pub fn finish_save(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.form = None,
            Err(e) => {
                if let Some(f) = &mut self.form {
                    f.error = Some(widgets::sentence(&e));
                }
            }
        }
    }

    /// The app icon, big and centred, with the wordmark under it.
    fn logo_ui(&mut self, ui: &mut Ui) {
        widgets::logo(ui, 184.0);
        ui.add_space(6.0);
        ui.label(widgets::wordmark(34.0));
        ui.label(RichText::new("Many Proxmox VM consoles. One keyboard.").color(theme::MUTED));
    }

    /// `saves`: false in a `--demo` session, where nothing is saved.
    pub fn ui(&mut self, ui: &mut Ui, cfg: &Config, status: &Status, saves: bool) -> Option<ClusterAction> {
        let mut action = None;
        let width = ui.available_width().min(760.0);
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.set_max_width(width);
                ui.add_space(28.0);
                self.logo_ui(ui);
                ui.add_space(26.0);
                ui.label(widgets::spaced("CLUSTERS", theme::bold(14.0), theme::MUTED, 3.0));
                ui.label(
                    RichText::new("Bookmark Proxmox clusters, then connect to one to pick VMs.").color(theme::DIM),
                );
                ui.add_space(14.0);
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    ui.set_max_width(width);
                    for b in &cfg.bookmarks {
                        if let Some(a) = self.bookmark_card(ui, b, status) {
                            action = Some(a);
                        }
                        ui.add_space(10.0);
                    }
                    if cfg.bookmarks.is_empty() && self.form.is_none() {
                        if demo_card(ui) {
                            action = Some(ClusterAction::Demo);
                        }
                        ui.add_space(10.0);
                    }
                    if let Some(a) = self.form_ui(ui, cfg) {
                        action = Some(a);
                    } else if self.form.is_none() {
                        ui.horizontal(|ui| {
                            let add = widgets::plate_button(
                                ui,
                                widgets::spaced("+ ADD CLUSTER", theme::bold(12.0), theme::TEXT, 1.6),
                                vec2(150.0, 34.0),
                                PlateStyle::DEFAULT,
                                true,
                            );
                            if add.clicked() {
                                self.form = Some(Form::new());
                            }
                            if !cfg.bookmarks.is_empty() {
                                let demo = widgets::plate_button(
                                    ui,
                                    widgets::spaced("Try the demo", theme::label(12.0), theme::MUTED, 0.6),
                                    vec2(0.0, 34.0),
                                    PlateStyle::DEFAULT,
                                    true,
                                );
                                if demo.on_hover_text("13 simulated VMs, no server or network needed").clicked() {
                                    action = Some(ClusterAction::Demo);
                                }
                            }
                        });
                    }
                    ui.add_space(24.0);
                    let saved_where = if !saves {
                        Some("Started with --demo: nothing is saved in this session.".to_owned())
                    } else {
                        Config::path().map(|path| {
                            format!(
                                "Bookmarks are saved in {}. Token secrets never go into that file.",
                                crate::config::tilde(&path),
                            )
                        })
                    };
                    if let Some(text) = saved_where {
                        ui.label(RichText::new(text).font(theme::mono(11.0)).color(theme::DIM));
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "VMherd {}  ·  {}  ·",
                                env!("CARGO_PKG_VERSION"),
                                crate::app::AUTHOR
                            ))
                            .font(theme::mono(11.0))
                            .color(theme::DIM),
                        );
                        ui.hyperlink_to(RichText::new("GitHub").font(theme::mono(11.0)), crate::app::REPO_URL);
                        ui.label(RichText::new("·").font(theme::mono(11.0)).color(theme::DIM));
                        ui.hyperlink_to(RichText::new("README").font(theme::mono(11.0)), crate::app::README_URL);
                    });
                    widgets::link_row(ui, crate::app::SITE_LINKS, 11.0, false);
                });
            });
        });
        action
    }

    fn bookmark_card(&mut self, ui: &mut Ui, b: &Bookmark, status: &Status) -> Option<ClusterAction> {
        let mut action = None;
        let editing = self.form.as_ref().is_some_and(|f| f.id == Some(b.id));
        if editing {
            return None; // the form replaces the card
        }
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&b.name).font(theme::bold(18.0)).color(theme::TEXT));
                    let secret = match &b.secret {
                        SecretSource::Keyring => format!("Secret saved in the {}", secrets::STORE_NAME),
                        SecretSource::MacKeychain { service, .. } => format!("Secret from Keychain item \"{service}\""),
                        SecretSource::Command { command } => {
                            let short: String = command.chars().take(40).collect();
                            let more = if command.chars().count() > 40 { "…" } else { "" };
                            format!("Secret from the command \"{short}{more}\"")
                        }
                        SecretSource::File => "Secret in a private file (not encrypted)".to_owned(),
                    };
                    let pin = if b.url.starts_with("http://") {
                        "UNENCRYPTED (no TLS)".to_owned()
                    } else {
                        b.pinned_sha256.as_deref().map_or_else(|| "Certificate not pinned yet".to_owned(), pin_label)
                    };
                    ui.label(
                        RichText::new(format!("{}  ·  {}", b.url, b.token_id))
                            .font(theme::mono(12.0))
                            .color(theme::MUTED),
                    );
                    ui.label(RichText::new(format!("{secret}  ·  {pin}")).font(theme::mono(11.0)).color(theme::DIM));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let armed =
                        self.delete_armed.is_some_and(|(id, t)| id == b.id && t.elapsed() < Duration::from_secs(3));
                    let (label, style) =
                        if armed { ("DELETE?", PlateStyle::DANGER) } else { ("✕", PlateStyle::DEFAULT) };
                    let del = widgets::plate_button(
                        ui,
                        widgets::spaced(label, theme::bold(11.0), theme::TEXT, 1.0),
                        vec2(34.0, 32.0),
                        style,
                        true,
                    );
                    if del.on_hover_text("Delete bookmark (click twice)").clicked() {
                        if armed {
                            self.delete_armed = None;
                            action = Some(ClusterAction::Delete(b.id));
                        } else {
                            self.delete_armed = Some((b.id, Instant::now()));
                        }
                    }
                    if armed {
                        ui.ctx().request_repaint_after(Duration::from_millis(200));
                    }
                    if widgets::plate_button(
                        ui,
                        widgets::spaced("EDIT", theme::bold(11.0), theme::TEXT, 1.4),
                        vec2(60.0, 32.0),
                        PlateStyle::DEFAULT,
                        true,
                    )
                    .clicked()
                    {
                        self.form = Some(Form::edit(b));
                    }
                    let connecting = status.connecting == Some(b.id);
                    let label = if connecting { "CONNECTING…" } else { "CONNECT" };
                    let go = widgets::plate_button(
                        ui,
                        widgets::spaced(label, theme::bold(12.0), theme::INK, 1.6),
                        vec2(110.0, 32.0),
                        PlateStyle::GO,
                        !connecting,
                    );
                    if go.clicked() {
                        action = Some(ClusterAction::Connect(b.id));
                    }
                });
            });
            if let Some((id, err)) = &status.error
                && *id == b.id
            {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(widgets::sentence(err))
                        .font(theme::mono(12.0))
                        .color(Color32::from_rgb(0xff, 0x9c, 0x9c)),
                );
            }
        });
        action
    }

    fn form_ui(&mut self, ui: &mut Ui, cfg: &Config) -> Option<ClusterAction> {
        let form = self.form.as_mut()?;
        let mut action = None;
        let mut close = false;
        card().stroke(Stroke::new(1.0, theme::STEEL)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let title = if form.id.is_some() { "EDIT CLUSTER" } else { "ADD CLUSTER" };
            ui.label(widgets::spaced(title, theme::bold(13.0), theme::STEEL, 2.0));
            ui.add_space(8.0);
            field(ui, "NAME", TextEdit::singleline(&mut form.name).hint_text("Lab"));
            field(ui, "API URL", TextEdit::singleline(&mut form.url).hint_text("https://pve.example.com:8006"));
            field(ui, "API TOKEN ID", TextEdit::singleline(&mut form.token_id).hint_text("user@pve!vmherd"));
            ui.add_space(4.0);
            ui.label(widgets::spaced("TOKEN SECRET", theme::label(11.0), theme::MUTED, 1.4));
            let store = secrets::store_status();
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(store.is_ok(), |ui| {
                    ui.radio_value(
                        &mut form.source,
                        Source::Keyring,
                        format!("Save it in the {}", secrets::STORE_NAME),
                    )
                    .on_disabled_hover_text(store.as_ref().err().cloned().unwrap_or_default());
                });
                if cfg!(target_os = "macos") && secrets::EXTERNAL_SOURCES {
                    ui.radio_value(&mut form.source, Source::MacKeychain, "Use an existing Keychain item");
                }
                if secrets::EXTERNAL_SOURCES {
                    ui.radio_value(&mut form.source, Source::Command, "Get it from a command")
                        .on_hover_text("For password managers: pass, secret-tool, op, bw, keepassxc-cli, ...");
                }
                if cfg!(all(unix, not(target_os = "macos"))) {
                    ui.radio_value(&mut form.source, Source::File, "Save it in a private file (not encrypted)")
                        .on_hover_text("A file only your user can read, in VMherd's config folder");
                }
            });
            if let (Err(why), true) = (store, cfg!(all(unix, not(target_os = "macos")))) {
                ui.label(
                    RichText::new(format!(
                        "{why}. It needs a keyring daemon such as GNOME Keyring, KWallet or KeePassXC; \
                         otherwise use a command or the private file."
                    ))
                    .font(theme::mono(11.0))
                    .color(theme::DIM),
                );
            }
            match form.source {
                Source::Keyring | Source::File => {
                    let hint = if form.id.is_some() {
                        "Leave empty to keep the saved secret"
                    } else {
                        "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
                    };
                    ui.add(
                        TextEdit::singleline(&mut form.secret)
                            .password(true)
                            .hint_text(hint)
                            .font(theme::mono(13.0))
                            .desired_width(f32::INFINITY)
                            .margin(vec2(8.0, 6.0)),
                    );
                }
                Source::MacKeychain => {
                    field(
                        ui,
                        "KEYCHAIN SERVICE",
                        TextEdit::singleline(&mut form.mac_service).hint_text("vmherd-token"),
                    );
                    field(ui, "KEYCHAIN ACCOUNT (OPTIONAL)", TextEdit::singleline(&mut form.mac_account));
                }
                Source::Command => {
                    field(
                        ui,
                        "COMMAND (THE FIRST LINE IT PRINTS IS THE SECRET)",
                        TextEdit::singleline(&mut form.command).hint_text("pass show pve/token"),
                    );
                }
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let pin = match form.pinned.as_deref() {
                    Some(_) if form.forget_pin => "The pinned certificate will be forgotten on save.".to_owned(),
                    Some(p) => format!("Certificate pinned: {p}"),
                    None => {
                        "Certificate not pinned yet. You will be asked to trust it on the first connect.".to_owned()
                    }
                };
                ui.label(RichText::new(pin).font(theme::mono(11.0)).color(theme::DIM));
                if form.pinned.is_some()
                    && !form.forget_pin
                    && ui.small_button("Forget").on_hover_text("Ask again on the next connect").clicked()
                {
                    form.forget_pin = true;
                }
            });
            if let Some(err) = &form.error {
                ui.label(RichText::new(err).color(Color32::from_rgb(0xff, 0x9c, 0x9c)));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let existing = form.source.saves_secret()
                    && form.id.and_then(|id| cfg.bookmark(id)).is_some_and(|b| {
                        let same = matches!(
                            (&b.secret, form.source),
                            (SecretSource::Keyring, Source::Keyring) | (SecretSource::File, Source::File)
                        );
                        same && secrets::has_saved(b.id, &b.secret)
                    });
                if widgets::plate_button(
                    ui,
                    widgets::spaced("SAVE", theme::bold(12.0), theme::INK, 1.6),
                    vec2(80.0, 32.0),
                    PlateStyle::GO,
                    true,
                )
                .clicked()
                {
                    match form.validate(existing) {
                        Ok(a) => {
                            form.error = None;
                            action = Some(a);
                        }
                        Err(e) => form.error = Some(e),
                    }
                }
                if widgets::plate_button(
                    ui,
                    widgets::spaced("CANCEL", theme::bold(12.0), theme::TEXT, 1.6),
                    vec2(80.0, 32.0),
                    PlateStyle::DEFAULT,
                    true,
                )
                .clicked()
                {
                    close = true;
                }
            });
        });
        if close {
            self.form = None;
        }
        action
    }
}
