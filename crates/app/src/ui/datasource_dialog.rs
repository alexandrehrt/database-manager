//! The New / Edit connection dialog.

use std::path::PathBuf;

use dbm_core::config::{ConnColor, DataSourceConfig, DataSourceKind, SshAuth, SshConfig, SslMode, TlsFiles};
use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText, Stroke};

use crate::ui::theme::{self, color, icon};

const SAVE_AND_CONNECT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
const WIDTH: f32 = 560.0;
/// Width of the dialog body between its side margins.
const CONTENT: f32 = WIDTH - 44.0;

#[derive(Clone, Copy, PartialEq)]
enum Engine {
    Postgres,
    Sqlite,
    Oracle,
}

#[derive(Clone, Copy, PartialEq)]
enum SshMethod {
    Password,
    Key,
    Agent,
}

pub enum TestState {
    Running(u64),
    Done(Result<String, String>),
}

pub struct DataSourceDialog {
    /// `Some(id)` when editing an existing source.
    editing: Option<String>,
    name: String,
    color: Option<ConnColor>,
    read_only: bool,
    engine: Engine,
    host: String,
    port: String,
    database: String,
    user: String,
    password: String,
    show_password: bool,
    save_password: bool,
    ssl_mode: SslMode,
    root_cert: String,
    client_cert_on: bool,
    client_cert: String,
    client_key: String,
    path: String,
    service: String,
    /// Oracle Instant Client folder; empty means the system library path.
    client_dir: String,
    ssh: bool,
    ssh_host: String,
    ssh_port: String,
    ssh_user: String,
    ssh_method: SshMethod,
    ssh_key: String,
    /// SSH password or key passphrase.
    ssh_secret: String,
    show_ssh_secret: bool,
    ssh_verify: bool,
    pub test: Option<TestState>,
    error: Option<String>,
}

pub enum DialogAction {
    /// `connect` also selects the source and connects to it.
    Save {
        config: DataSourceConfig,
        password: Option<String>,
        /// SSH password or key passphrase.
        ssh_secret: Option<String>,
        save_password: bool,
        connect: bool,
    },
    Test {
        config: DataSourceConfig,
        password: Option<String>,
        ssh_secret: Option<String>,
    },
    Delete(String),
    Close,
}

impl DataSourceDialog {
    pub fn new() -> Self {
        Self {
            editing: None,
            name: String::new(),
            color: None,
            read_only: false,
            engine: Engine::Postgres,
            host: "localhost".into(),
            port: "5432".into(),
            database: "postgres".into(),
            user: "postgres".into(),
            password: String::new(),
            show_password: false,
            save_password: true,
            ssl_mode: SslMode::Prefer,
            root_cert: String::new(),
            client_cert_on: false,
            client_cert: String::new(),
            client_key: String::new(),
            path: String::new(),
            service: "FREEPDB1".into(),
            client_dir: String::new(),
            ssh: false,
            ssh_host: String::new(),
            ssh_port: "22".into(),
            ssh_user: String::new(),
            ssh_method: SshMethod::Key,
            ssh_key: "~/.ssh/id_ed25519".into(),
            ssh_secret: String::new(),
            show_ssh_secret: false,
            ssh_verify: true,
            test: None,
            error: None,
        }
    }

    pub fn edit(config: &DataSourceConfig) -> Self {
        let mut d = Self::new();
        d.editing = Some(config.id.clone());
        d.name = config.name.clone();
        d.color = config.color;
        d.read_only = config.read_only;
        if let Some(ssh) = &config.ssh {
            d.ssh = true;
            d.ssh_host = ssh.host.clone();
            d.ssh_port = ssh.port.to_string();
            d.ssh_user = ssh.user.clone();
            d.ssh_verify = ssh.verify_host_key;
            d.ssh_method = match &ssh.auth {
                SshAuth::Password => SshMethod::Password,
                SshAuth::Key { path } => {
                    d.ssh_key = path.display().to_string();
                    SshMethod::Key
                }
                SshAuth::Agent => SshMethod::Agent,
            };
        }
        match &config.kind {
            DataSourceKind::Postgres { host, port, database, user, ssl_mode, tls } => {
                let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                d.root_cert = path(&tls.root_cert);
                d.client_cert = path(&tls.client_cert);
                d.client_key = path(&tls.client_key);
                d.client_cert_on = tls.client_cert.is_some() || tls.client_key.is_some();
                d.engine = Engine::Postgres;
                d.host = host.clone();
                d.port = port.to_string();
                d.database = database.clone();
                d.user = user.clone();
                d.ssl_mode = *ssl_mode;
            }
            DataSourceKind::Sqlite { path } => {
                d.engine = Engine::Sqlite;
                d.path = path.display().to_string();
            }
            DataSourceKind::Oracle { host, port, service, user, client_dir } => {
                d.engine = Engine::Oracle;
                d.host = host.clone();
                d.port = port.to_string();
                d.service = service.clone();
                d.user = user.clone();
                d.client_dir = client_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
            }
        }
        d
    }

    fn password(&self) -> Option<String> {
        (!self.password.is_empty()).then(|| self.password.clone())
    }

    fn tls_files(&self) -> Result<TlsFiles, String> {
        let path = |s: &str| Some(s.trim()).filter(|s| !s.is_empty()).map(PathBuf::from);
        let mut files = TlsFiles::default();
        if self.ssl_mode == SslMode::Disable {
            return Ok(files);
        }
        files.root_cert = path(&self.root_cert);
        if self.client_cert_on {
            files.client_cert = path(&self.client_cert);
            files.client_key = path(&self.client_key);
            if files.client_cert.is_none() || files.client_key.is_none() {
                return Err("A client certificate needs both the certificate and the key file".into());
            }
        }
        Ok(files)
    }

    fn ssh_secret(&self) -> Option<String> {
        let used = self.ssh && self.engine != Engine::Sqlite && self.ssh_method != SshMethod::Agent;
        (used && !self.ssh_secret.is_empty()).then(|| self.ssh_secret.clone())
    }

    fn ssh_config(&self) -> Result<Option<SshConfig>, String> {
        if !self.ssh || self.engine == Engine::Sqlite {
            return Ok(None);
        }
        if self.ssh_host.trim().is_empty() || self.ssh_user.trim().is_empty() {
            return Err("SSH host and user are required".into());
        }
        let port = self.ssh_port.trim().parse().map_err(|_| "SSH port must be a number between 1 and 65535")?;
        let auth = match self.ssh_method {
            SshMethod::Password => SshAuth::Password,
            SshMethod::Agent => SshAuth::Agent,
            SshMethod::Key if self.ssh_key.trim().is_empty() => return Err("Choose an SSH private key file".into()),
            SshMethod::Key => SshAuth::Key { path: PathBuf::from(self.ssh_key.trim()) },
        };
        Ok(Some(SshConfig {
            host: self.ssh_host.trim().to_string(),
            port,
            user: self.ssh_user.trim().to_string(),
            auth,
            verify_host_key: self.ssh_verify,
        }))
    }

    fn kind(&self) -> Result<DataSourceKind, String> {
        let port = || self.port.trim().parse().map_err(|_| "Port must be a number between 1 and 65535".to_string());
        Ok(match self.engine {
            Engine::Postgres => {
                if self.host.trim().is_empty() {
                    return Err("Host is required".into());
                }
                DataSourceKind::Postgres {
                    host: self.host.trim().to_string(),
                    port: port()?,
                    database: self.database.trim().to_string(),
                    user: self.user.trim().to_string(),
                    ssl_mode: self.ssl_mode,
                    tls: self.tls_files()?,
                }
            }
            Engine::Sqlite => {
                if self.path.trim().is_empty() {
                    return Err("Choose a database file".into());
                }
                DataSourceKind::Sqlite { path: PathBuf::from(self.path.trim()) }
            }
            Engine::Oracle => {
                if self.host.trim().is_empty() || self.service.trim().is_empty() {
                    return Err("Host and service name are required".into());
                }
                DataSourceKind::Oracle {
                    host: self.host.trim().to_string(),
                    port: port()?,
                    service: self.service.trim().to_string(),
                    user: self.user.trim().to_string(),
                    client_dir: Some(self.client_dir.trim()).filter(|d| !d.is_empty()).map(PathBuf::from),
                }
            }
        })
    }

    fn to_config(&self) -> Result<DataSourceConfig, String> {
        let kind = self.kind()?;
        let name = match self.name.trim() {
            "" => default_name(&kind),
            n => n.to_string(),
        };
        let id = self.editing.clone().unwrap_or_else(new_id);
        let ssh = self.ssh_config()?;
        Ok(DataSourceConfig { id, name, kind, color: self.color, read_only: self.read_only, ssh })
    }

    fn save(&mut self, connect: bool) -> Option<DialogAction> {
        match self.to_config() {
            Ok(config) => Some(DialogAction::Save {
                config,
                password: self.password(),
                ssh_secret: self.ssh_secret(),
                save_password: self.save_password,
                connect,
            }),
            Err(e) => {
                self.error = Some(e);
                None
            }
        }
    }

    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<DialogAction> {
        let mut action = None;
        if ctx.input_mut(|i| i.consume_shortcut(&SAVE_AND_CONNECT)) {
            action = self.save(true);
        }
        let frame = egui::Frame::new().fill(color::BG).corner_radius(12).stroke(Stroke::new(1.0, color::BORDER));
        let modal = egui::Modal::new(egui::Id::new("datasource_dialog")).frame(frame).show(ctx, |ui| {
            ui.set_width(WIDTH);
            ui.spacing_mut().item_spacing.y = 6.0;
            if self.header(ui) {
                action = Some(DialogAction::Close);
            }
            // The right margin leaves room for the scroll bar the scroll area reserves.
            egui::Frame::new().inner_margin(egui::Margin { left: 22, right: 2, top: 4, bottom: 16 }).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(560.0).auto_shrink([false, true]).show(ui, |ui| {
                    // Fixed width, so right-aligned rows can't stretch the dialog.
                    ui.set_width(CONTENT);
                    self.engine_cards(ui);
                    divider(ui);
                    match self.engine {
                        Engine::Sqlite => self.sqlite_fields(ui),
                        Engine::Postgres | Engine::Oracle => {
                            self.server_fields(ui);
                            divider(ui);
                            self.ssh_fields(ui);
                        }
                    }
                    divider(ui);
                    self.identification(ui);
                    self.banners(ui);
                });
            });
            if let Some(a) = self.footer(ui) {
                action = Some(a);
            }
        });
        if action.is_none() && modal.should_close() {
            action = Some(DialogAction::Close);
        }
        action
    }

    /// Title, subtitle and the close button; returns whether × was clicked.
    fn header(&mut self, ui: &mut egui::Ui) -> bool {
        let mut close = false;
        egui::Frame::new().inner_margin(egui::Margin { left: 22, right: 16, top: 18, bottom: 10 }).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let title = if self.editing.is_some() { "Edit connection" } else { "New connection" };
                    ui.label(RichText::new(title).font(theme::font(18.0, theme::semibold())));
                    ui.label(RichText::new("Fill in the fields to connect.").color(color::TEXT_WEAK));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if ui.add(theme::flat_button(RichText::new(icon::X).size(16.0))).on_hover_text("Close").clicked() {
                        close = true;
                    }
                });
            });
        });
        close
    }

    fn engine_cards(&mut self, ui: &mut egui::Ui) {
        let cards = [
            (Engine::Postgres, icon::DATABASE, "PostgreSQL", "Network server"),
            (Engine::Sqlite, icon::FILE_TEXT, "SQLite", "Local file"),
            (Engine::Oracle, icon::HARD_DRIVES, "Oracle", "Needs Instant Client"),
        ];
        let gap = 10.0;
        let width = (CONTENT - gap * (cards.len() as f32 - 1.0)) / cards.len() as f32;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (engine, glyph, title, subtitle) in cards {
                let selected = self.engine == engine;
                let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 56.0), egui::Sense::click());
                let (fill, stroke) = if selected {
                    (Color32::from_rgb(240, 244, 255), Stroke::new(1.5, Color32::from_rgb(52, 92, 214)))
                } else if response.hovered() {
                    (color::BG_SUBTLE, Stroke::new(1.0, Color32::from_rgb(205, 205, 214)))
                } else {
                    (color::BG, Stroke::new(1.0, color::BORDER))
                };
                ui.painter().rect(rect, 8.0, fill, stroke, egui::StrokeKind::Inside);
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect.shrink2(egui::vec2(14.0, 0.0)))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                child.spacing_mut().item_spacing.x = 10.0;
                child.label(RichText::new(glyph).size(20.0).color(color::TEXT_WEAK));
                child.vertical(|ui| {
                    ui.add_space(9.0);
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(RichText::new(title).font(theme::font(13.5, theme::semibold())));
                    ui.label(RichText::new(subtitle).small().color(color::TEXT_WEAK));
                });
                if response.clicked() && !selected {
                    let before = self.engine;
                    self.engine = engine;
                    self.switch_defaults(before);
                    self.test = None;
                    self.error = None;
                }
            }
        });
    }

    /// Swaps the port and user defaults when they still hold the previous engine's.
    fn switch_defaults(&mut self, from: Engine) {
        let defaults = |e: Engine| match e {
            Engine::Postgres => Some(("5432", "postgres")),
            Engine::Oracle => Some(("1521", "")),
            Engine::Sqlite => None,
        };
        let (Some((old_port, old_user)), Some((new_port, new_user))) = (defaults(from), defaults(self.engine)) else {
            return;
        };
        if self.port == old_port {
            self.port = new_port.into();
        }
        if self.user == old_user {
            self.user = new_user.into();
        }
    }

    fn server_fields(&mut self, ui: &mut egui::Ui) {
        section(ui, "Server");
        let gap = 12.0;
        let full = CONTENT;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            labeled(ui, "Host", full - 96.0 - gap, |ui| input(ui, &mut self.host, ""));
            labeled(ui, "Port", 96.0, |ui| input(ui, &mut self.port, ""));
        });
        match self.engine {
            Engine::Oracle => {
                labeled(ui, "Service name", full, |ui| input(ui, &mut self.service, "e.g. FREEPDB1, ORCLPDB1"));
            }
            _ => {
                labeled(ui, "Database", full, |ui| input(ui, &mut self.database, ""));
            }
        }
        let half = (full - gap) / 2.0;
        let hint = if self.editing.is_some() { "unchanged" } else { "" };
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            labeled(ui, "User", half, |ui| input(ui, &mut self.user, ""));
            labeled(ui, "Password", half, |ui| {
                password_input(ui, &mut self.password, &mut self.show_password, hint);
            });
        });
        ui.add_space(2.0);
        let store = if cfg!(target_os = "macos") { "Keychain" } else { "the system keyring" };
        ui.horizontal(|ui| {
            let mut toggled = crate::ui::grid::checkbox_glyph(ui, self.save_password).clicked();
            toggled |=
                ui.add(egui::Label::new(format!("Save password in {store}")).sense(egui::Sense::click())).clicked();
            if toggled {
                self.save_password = !self.save_password;
            }
        });

        if self.engine == Engine::Postgres {
            self.ssl_fields(ui);
        }
        if self.engine == Engine::Oracle {
            ui.add_space(4.0);
            labeled(ui, "Instant Client folder (optional)", full, |ui| {
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 96.0;
                    ui.add_sized([w, 30.0], text_edit(&mut self.client_dir, "uses the system library path if empty"));
                    if ui.add_sized([88.0, 30.0], egui::Button::new(format!("{}  Browse", icon::FOLDER_OPEN))).clicked()
                        && let Some(dir) = rfd::FileDialog::new().pick_folder()
                    {
                        self.client_dir = dir.display().to_string();
                    }
                });
            });
            ui.hyperlink_to(
                RichText::new("Download Oracle Instant Client (Basic package)").small(),
                "https://www.oracle.com/database/technologies/instant-client/downloads.html",
            );
        }
    }

    fn ssl_fields(&mut self, ui: &mut egui::Ui) {
        let full = CONTENT;
        ui.add_space(4.0);
        ui.label(RichText::new("SSL").font(theme::font(12.5, theme::medium())));
        segmented(
            ui,
            &mut self.ssl_mode,
            &[
                (SslMode::Disable, "disable"),
                (SslMode::Prefer, "prefer"),
                (SslMode::Require, "require"),
                (SslMode::VerifyCa, "verify-ca"),
                (SslMode::VerifyFull, "verify-full"),
            ],
        );
        let help = match self.ssl_mode {
            SslMode::Disable => "Never use SSL.",
            SslMode::Prefer => "Use SSL if the server offers it; the certificate is not checked.",
            SslMode::Require => "Always encrypt; the certificate is not checked.",
            SslMode::VerifyCa => "Encrypt, and the certificate must be signed by a trusted CA.",
            SslMode::VerifyFull => "Encrypt, the certificate must be signed by a trusted CA and name the host.",
        };
        ui.label(RichText::new(help).small().color(color::TEXT_WEAK));
        if self.ssl_mode == SslMode::Disable {
            return;
        }
        if self.ssl_mode.verifies() {
            ui.add_space(2.0);
            labeled(ui, "CA certificate (optional)", full, |ui| {
                file_input(ui, &mut self.root_cert, "uses the system's trusted CAs if empty");
            });
        }
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let mut toggled = crate::ui::grid::checkbox_glyph(ui, self.client_cert_on).clicked();
            toggled |= ui.add(egui::Label::new("Client certificate").sense(egui::Sense::click())).clicked();
            if toggled {
                self.client_cert_on = !self.client_cert_on;
            }
            ui.label(RichText::new("for servers that require one").small().color(color::TEXT_WEAK));
        });
        if self.client_cert_on {
            let gap = 12.0;
            let half = (full - gap) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                labeled(ui, "Certificate", half, |ui| file_input(ui, &mut self.client_cert, "client.crt"));
                labeled(ui, "Key (PKCS#8)", half, |ui| file_input(ui, &mut self.client_key, "client.key"));
            });
        }
    }

    fn ssh_fields(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut toggled = crate::ui::grid::checkbox_glyph(ui, self.ssh).clicked();
            toggled |= ui
                .add(
                    egui::Label::new(RichText::new("SSH tunnel").font(theme::font(13.0, theme::semibold())))
                        .sense(egui::Sense::click()),
                )
                .clicked();
            if toggled {
                self.ssh = !self.ssh;
            }
            ui.label(RichText::new("connect through a bastion host").small().color(color::TEXT_WEAK));
        });
        if !self.ssh {
            return;
        }
        ui.add_space(4.0);
        let gap = 12.0;
        let full = CONTENT;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            labeled(ui, "SSH host", full - 96.0 - gap, |ui| input(ui, &mut self.ssh_host, "bastion.example.com"));
            labeled(ui, "Port", 96.0, |ui| input(ui, &mut self.ssh_port, ""));
        });
        let segmented_width = 3.0 * 72.0 + 10.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            labeled(ui, "SSH user", full - segmented_width - gap, |ui| input(ui, &mut self.ssh_user, ""));
            labeled(ui, "Authentication", segmented_width, |ui| {
                segmented(
                    ui,
                    &mut self.ssh_method,
                    &[(SshMethod::Key, "key file"), (SshMethod::Password, "password"), (SshMethod::Agent, "agent")],
                );
            });
        });
        let hint = if self.editing.is_some() { "unchanged" } else { "" };
        match self.ssh_method {
            SshMethod::Password => {
                labeled(ui, "SSH password", full, |ui| {
                    password_input(ui, &mut self.ssh_secret, &mut self.show_ssh_secret, hint);
                });
            }
            SshMethod::Key => {
                labeled(ui, "Private key", full, |ui| {
                    ui.horizontal(|ui| {
                        let w = ui.available_width() - 96.0;
                        ui.add_sized([w, 30.0], text_edit(&mut self.ssh_key, "~/.ssh/id_ed25519"));
                        if ui
                            .add_sized([88.0, 30.0], egui::Button::new(format!("{}  Browse", icon::FOLDER_OPEN)))
                            .clicked()
                        {
                            let mut picker = rfd::FileDialog::new();
                            if let Some(dir) = std::env::home_dir().map(|h| h.join(".ssh")).filter(|d| d.is_dir()) {
                                picker = picker.set_directory(dir);
                            }
                            if let Some(p) = picker.pick_file() {
                                self.ssh_key = p.display().to_string();
                            }
                        }
                    });
                });
                let hint = if self.editing.is_some() { "unchanged" } else { "only if the key is encrypted" };
                labeled(ui, "Passphrase", full, |ui| {
                    password_input(ui, &mut self.ssh_secret, &mut self.show_ssh_secret, hint);
                });
            }
            SshMethod::Agent => {
                ui.label(
                    RichText::new("Uses the keys loaded in ssh-agent (ssh-add -l lists them).")
                        .small()
                        .color(color::TEXT_WEAK),
                );
            }
        }
        ui.add_space(2.0);
        ui.checkbox(&mut self.ssh_verify, "Verify the host key against ~/.ssh/known_hosts").on_hover_text(
            "Protects against a server impersonating the bastion. Connect once with `ssh` to add the host.",
        );
        ui.label(
            RichText::new("Host and port above are as seen from the bastion, e.g. localhost or a private address.")
                .small()
                .color(color::TEXT_WEAK),
        );
    }

    fn sqlite_fields(&mut self, ui: &mut egui::Ui) {
        section(ui, "File");
        let full = CONTENT;
        labeled(ui, "Database file", full, |ui| {
            ui.horizontal(|ui| {
                let w = ui.available_width() - 96.0;
                ui.add_sized([w, 30.0], text_edit(&mut self.path, "path to a .db / .sqlite file"));
                if ui.add_sized([88.0, 30.0], egui::Button::new(format!("{}  Browse", icon::FOLDER_OPEN))).clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter("SQLite database", &["db", "sqlite", "sqlite3", "db3"])
                        .add_filter("All files", &["*"])
                        .pick_file()
                {
                    self.path = p.display().to_string();
                }
            });
        });
        ui.label(RichText::new("The file is created if it does not exist.").small().color(color::TEXT_WEAK));
    }

    fn identification(&mut self, ui: &mut egui::Ui) {
        section(ui, "Identification");
        let placeholder = self.kind().map(|k| default_name(&k)).unwrap_or_default();
        let swatches = 6.0 * 26.0 + 30.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            labeled(ui, "Name", CONTENT - swatches - 12.0, |ui| input(ui, &mut self.name, &placeholder));
            labeled(ui, "Colour", swatches, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(30.0);
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let none = ui
                        .add(
                            egui::Button::new(RichText::new(icon::PROHIBIT).color(color::TEXT_WEAK))
                                .selected(self.color.is_none())
                                .min_size(egui::vec2(22.0, 22.0)),
                        )
                        .on_hover_text("No colour");
                    if none.clicked() {
                        self.color = None;
                    }
                    for c in ConnColor::ALL {
                        let (strong, _) = theme::conn_color(c);
                        let (rect, r) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
                        ui.painter().circle_filled(rect.center(), 8.0, strong);
                        if self.color == Some(c) {
                            ui.painter().circle_stroke(rect.center(), 10.5, Stroke::new(2.0, strong));
                        }
                        if r.on_hover_text(c.label()).clicked() {
                            self.color = Some(c);
                        }
                    }
                });
            });
        });
        ui.label(
            RichText::new("Tabs of this connection are tinted with its colour, e.g. red for production.")
                .small()
                .color(color::TEXT_WEAK),
        );
        ui.add_space(6.0);
        ui.checkbox(&mut self.read_only, "Read-only")
            .on_hover_text("Only queries run; changes to data or schema and grid editing are blocked.");
    }

    /// Test result and validation errors.
    fn banners(&mut self, ui: &mut egui::Ui) {
        let banner = |ui: &mut egui::Ui, fill: Color32, stroke: Color32, text: RichText| {
            ui.add_space(8.0);
            egui::Frame::new()
                .fill(fill)
                .stroke(Stroke::new(1.0, stroke))
                .corner_radius(8)
                .inner_margin(egui::Margin::symmetric(12, 9))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(text);
                });
        };
        match &self.test {
            Some(TestState::Running(_)) => {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Testing connection…").color(color::TEXT_WEAK));
                });
            }
            Some(TestState::Done(Ok(msg))) => banner(
                ui,
                Color32::from_rgb(236, 248, 239),
                Color32::from_rgb(190, 228, 200),
                RichText::new(format!("{}  Connected · {msg}", icon::CHECK)).color(Color32::from_rgb(30, 110, 55)),
            ),
            Some(TestState::Done(Err(e))) => banner(
                ui,
                Color32::from_rgb(253, 238, 238),
                Color32::from_rgb(240, 200, 200),
                RichText::new(format!("{}  {e}", icon::WARNING_CIRCLE)).color(color::DANGER),
            ),
            None => {}
        }
        if let Some(e) = &self.error {
            banner(
                ui,
                Color32::from_rgb(253, 238, 238),
                Color32::from_rgb(240, 200, 200),
                RichText::new(format!("{}  {e}", icon::WARNING_CIRCLE)).color(color::DANGER),
            );
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui) -> Option<DialogAction> {
        let mut action = None;
        let rect = ui.available_rect_before_wrap();
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, color::BORDER));
        egui::Frame::new()
            .fill(color::BG_SUBTLE)
            .corner_radius(egui::CornerRadius { nw: 0, ne: 0, sw: 12, se: 12 })
            .inner_margin(egui::Margin::symmetric(22, 14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let testing = matches!(self.test, Some(TestState::Running(_)));
                    // While testing, a spinner takes the icon's place.
                    let label = if testing {
                        "      Testing…".to_string()
                    } else {
                        format!("{}  Test connection", icon::LIGHTNING)
                    };
                    let test = ui.add_enabled(!testing, egui::Button::new(label));
                    if testing {
                        let at = egui::Rect::from_center_size(
                            egui::pos2(test.rect.left() + 16.0, test.rect.center().y),
                            egui::vec2(13.0, 13.0),
                        );
                        egui::Spinner::new().size(13.0).paint_at(ui, at);
                    }
                    if test.clicked() {
                        self.error = None;
                        match self.to_config() {
                            Ok(config) => {
                                action = Some(DialogAction::Test {
                                    config,
                                    password: self.password(),
                                    ssh_secret: self.ssh_secret(),
                                })
                            }
                            Err(e) => self.error = Some(e),
                        }
                    }
                    if let Some(id) = &self.editing
                        && ui.add(theme::flat_button(RichText::new("Delete").color(color::DANGER))).clicked()
                    {
                        action = Some(DialogAction::Delete(id.clone()));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let shortcut = ui.ctx().format_shortcut(&SAVE_AND_CONNECT);
                        let primary = egui::Button::new(
                            RichText::new(format!("Save and connect   {shortcut}"))
                                .color(Color32::WHITE)
                                .font(theme::font(13.0, theme::medium())),
                        )
                        .fill(Color32::from_rgb(52, 92, 214))
                        .stroke(Stroke::NONE);
                        if ui.add(primary).clicked() {
                            action = self.save(true);
                        }
                        if ui.button("Save").clicked() {
                            action = self.save(false);
                        }
                        if ui.add(theme::flat_button("Cancel")).clicked() {
                            action = Some(DialogAction::Close);
                        }
                    });
                });
            });
        action
    }
}

fn divider(ui: &mut egui::Ui) {
    ui.add_space(10.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(CONTENT, 1.0), egui::Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, color::BORDER));
    ui.add_space(8.0);
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(theme::caption(title));
    ui.add_space(2.0);
}

/// A label above a field of the given width.
fn labeled(ui: &mut egui::Ui, label: &str, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(egui::vec2(width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(RichText::new(label).font(theme::font(12.5, theme::medium())));
        add(ui);
    });
}

fn text_edit<'a>(value: &'a mut String, hint: &str) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(value).hint_text(hint).margin(egui::vec2(10.0, 7.0)).desired_width(f32::INFINITY)
}

/// A path field with a Browse button that picks a file.
fn file_input(ui: &mut egui::Ui, value: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        let w = ui.available_width() - 40.0;
        ui.add_sized([w, 30.0], text_edit(value, hint));
        let browse = ui.add_sized([32.0, 30.0], egui::Button::new(icon::FOLDER_OPEN)).on_hover_text("Browse");
        if browse.clicked()
            && let Some(p) = rfd::FileDialog::new().pick_file()
        {
            *value = p.display().to_string();
        }
    });
}

fn input(ui: &mut egui::Ui, value: &mut String, hint: &str) {
    ui.add_sized([ui.available_width(), 30.0], text_edit(value, hint));
}

/// A password field with a show / hide toggle inside it.
fn password_input(ui: &mut egui::Ui, value: &mut String, show: &mut bool, hint: &str) {
    egui::Frame::new()
        .fill(color::BG)
        .stroke(Stroke::new(1.0, color::BORDER))
        .corner_radius(6)
        .inner_margin(egui::Margin { left: 10, right: 8, top: 2, bottom: 2 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let w = ui.available_width() - 28.0;
                ui.add(
                    egui::TextEdit::singleline(value)
                        .password(!*show)
                        .hint_text(hint)
                        .frame(egui::Frame::NONE)
                        .margin(egui::Margin::ZERO)
                        .desired_width(w),
                );
                let eye = if *show { icon::EYE_SLASH } else { icon::EYE };
                let toggle =
                    ui.add(egui::Label::new(RichText::new(eye).color(color::TEXT_WEAK)).sense(egui::Sense::click()));
                if toggle.on_hover_text(if *show { "Hide password" } else { "Show password" }).clicked() {
                    *show = !*show;
                }
            });
        });
}

fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)]) {
    egui::Frame::new().fill(color::BG_SUNKEN).corner_radius(8).inner_margin(3).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.horizontal(|ui| {
            for (option, label) in options {
                let selected = *value == *option;
                let text = RichText::new(*label).font(theme::font(12.5, theme::medium()));
                let button =
                    egui::Button::new(if selected { text.color(color::TEXT) } else { text.color(color::TEXT_WEAK) })
                        .fill(if selected { color::BG } else { Color32::TRANSPARENT })
                        .stroke(if selected { Stroke::new(1.0, color::BORDER) } else { Stroke::NONE })
                        .corner_radius(6)
                        .min_size(egui::vec2(68.0, 24.0));
                if ui.add(button).clicked() {
                    *value = *option;
                }
            }
        });
    });
}

fn default_name(kind: &DataSourceKind) -> String {
    match kind {
        DataSourceKind::Postgres { host, database, .. } => format!("{database}@{host}"),
        DataSourceKind::Oracle { host, service, user, .. } => format!("{user}@{host}/{service}"),
        DataSourceKind::Sqlite { path } => {
            path.file_name().map_or_else(|| path.display().to_string(), |f| f.to_string_lossy().into_owned())
        }
    }
}

fn new_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("ds-{nanos:x}")
}
