use std::path::PathBuf;

use dbm_core::config::{DataSourceConfig, DataSourceKind, SslMode};
use eframe::egui;

#[derive(Clone, Copy, PartialEq)]
enum Engine {
    Postgres,
    Sqlite,
    Oracle,
}

pub enum TestState {
    Running(u64),
    Done(Result<String, String>),
}

pub struct DataSourceDialog {
    /// `Some(id)` when editing an existing source.
    editing: Option<String>,
    name: String,
    engine: Engine,
    host: String,
    port: String,
    database: String,
    user: String,
    password: String,
    save_password: bool,
    ssl_mode: SslMode,
    path: String,
    service: String,
    /// Oracle Instant Client folder; empty means the system library path.
    client_dir: String,
    pub test: Option<TestState>,
    error: Option<String>,
}

pub enum DialogAction {
    Save { config: DataSourceConfig, password: Option<String>, save_password: bool },
    Test { config: DataSourceConfig, password: Option<String> },
    Delete(String),
    Close,
}

impl DataSourceDialog {
    pub fn new() -> Self {
        Self {
            editing: None,
            name: String::new(),
            engine: Engine::Postgres,
            host: "localhost".into(),
            port: "5432".into(),
            database: "postgres".into(),
            user: "postgres".into(),
            password: String::new(),
            save_password: true,
            ssl_mode: SslMode::Prefer,
            path: String::new(),
            service: "FREEPDB1".into(),
            client_dir: String::new(),
            test: None,
            error: None,
        }
    }

    pub fn edit(config: &DataSourceConfig) -> Self {
        let mut d = Self::new();
        d.editing = Some(config.id.clone());
        d.name = config.name.clone();
        match &config.kind {
            DataSourceKind::Postgres { host, port, database, user, ssl_mode } => {
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

    fn to_config(&self) -> Result<DataSourceConfig, String> {
        let kind = match self.engine {
            Engine::Postgres => {
                if self.host.trim().is_empty() {
                    return Err("Host is required".into());
                }
                DataSourceKind::Postgres {
                    host: self.host.trim().to_string(),
                    port: self.port.trim().parse().map_err(|_| "Port must be a number between 1 and 65535")?,
                    database: self.database.trim().to_string(),
                    user: self.user.trim().to_string(),
                    ssl_mode: self.ssl_mode,
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
                    port: self.port.trim().parse().map_err(|_| "Port must be a number between 1 and 65535")?,
                    service: self.service.trim().to_string(),
                    user: self.user.trim().to_string(),
                    client_dir: Some(self.client_dir.trim()).filter(|d| !d.is_empty()).map(PathBuf::from),
                }
            }
        };
        let name = match self.name.trim() {
            "" => default_name(&kind),
            n => n.to_string(),
        };
        let id = self.editing.clone().unwrap_or_else(new_id);
        Ok(DataSourceConfig { id, name, kind })
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<DialogAction> {
        let mut action = None;
        let title = if self.editing.is_some() { "Edit data source" } else { "New data source" };
        let modal = egui::Modal::new(egui::Id::new("datasource_dialog")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading(title);
            ui.add_space(8.0);
            egui::Grid::new("ds_form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut self.name).hint_text("optional").desired_width(f32::INFINITY));
                ui.end_row();

                ui.label("Type");
                ui.horizontal(|ui| {
                    let before = self.engine;
                    ui.selectable_value(&mut self.engine, Engine::Postgres, "PostgreSQL");
                    ui.selectable_value(&mut self.engine, Engine::Sqlite, "SQLite");
                    ui.selectable_value(&mut self.engine, Engine::Oracle, "Oracle");
                    if self.engine != before {
                        self.switch_defaults(before);
                    }
                });
                ui.end_row();

                match self.engine {
                    Engine::Postgres => self.postgres_fields(ui),
                    Engine::Sqlite => self.sqlite_fields(ui),
                    Engine::Oracle => self.oracle_fields(ui),
                }
            });

            ui.add_space(8.0);
            match &self.test {
                Some(TestState::Running(_)) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Testing connection…");
                    });
                }
                Some(TestState::Done(Ok(msg))) => {
                    ui.colored_label(ui.visuals().widgets.active.fg_stroke.color, msg.as_str());
                }
                Some(TestState::Done(Err(e))) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                None => {}
            }
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }

            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Test connection").clicked() {
                    match self.to_config() {
                        Ok(config) => action = Some(DialogAction::Test { config, password: self.password() }),
                        Err(e) => self.error = Some(e),
                    }
                }
                if let Some(id) = &self.editing
                    && ui.button("Delete").clicked()
                {
                    action = Some(DialogAction::Delete(id.clone()));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Save").clicked() {
                        match self.to_config() {
                            Ok(config) => {
                                action = Some(DialogAction::Save {
                                    config,
                                    password: self.password(),
                                    save_password: self.save_password,
                                })
                            }
                            Err(e) => self.error = Some(e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(DialogAction::Close);
                    }
                });
            });
        });
        if modal.should_close() && action.is_none() {
            action = Some(DialogAction::Close);
        }
        action
    }

    fn postgres_fields(&mut self, ui: &mut egui::Ui) {
        ui.label("Host");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(260.0));
            ui.label("Port");
            ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(f32::INFINITY));
        });
        ui.end_row();

        ui.label("Database");
        ui.add(egui::TextEdit::singleline(&mut self.database).desired_width(f32::INFINITY));
        ui.end_row();

        ui.label("User");
        ui.add(egui::TextEdit::singleline(&mut self.user).desired_width(f32::INFINITY));
        ui.end_row();

        ui.label("Password");
        let hint = if self.editing.is_some() { "leave empty to keep the saved password" } else { "" };
        ui.add(
            egui::TextEdit::singleline(&mut self.password).password(true).hint_text(hint).desired_width(f32::INFINITY),
        );
        ui.end_row();

        ui.label("");
        ui.checkbox(&mut self.save_password, "Save password in Keychain");
        ui.end_row();

        ui.label("SSL");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.ssl_mode, SslMode::Disable, "disable");
            ui.selectable_value(&mut self.ssl_mode, SslMode::Prefer, "prefer");
            ui.selectable_value(&mut self.ssl_mode, SslMode::Require, "require");
        });
        ui.end_row();
    }

    /// Swaps the port and user defaults when they still hold the previous engine's.
    fn switch_defaults(&mut self, from: Engine) {
        let (old_port, old_user) = match from {
            Engine::Postgres => ("5432", "postgres"),
            Engine::Oracle => ("1521", ""),
            Engine::Sqlite => return,
        };
        let (new_port, new_user) = match self.engine {
            Engine::Postgres => ("5432", "postgres"),
            Engine::Oracle => ("1521", ""),
            Engine::Sqlite => return,
        };
        if self.port == old_port {
            self.port = new_port.into();
        }
        if self.user == old_user {
            self.user = new_user.into();
        }
    }

    fn oracle_fields(&mut self, ui: &mut egui::Ui) {
        ui.label("Host");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(260.0));
            ui.label("Port");
            ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(f32::INFINITY));
        });
        ui.end_row();

        ui.label("Service name");
        ui.add(
            egui::TextEdit::singleline(&mut self.service)
                .hint_text("e.g. FREEPDB1, ORCLPDB1")
                .desired_width(f32::INFINITY),
        );
        ui.end_row();

        ui.label("User");
        ui.add(egui::TextEdit::singleline(&mut self.user).desired_width(f32::INFINITY));
        ui.end_row();

        ui.label("Password");
        let hint = if self.editing.is_some() { "leave empty to keep the saved password" } else { "" };
        ui.add(
            egui::TextEdit::singleline(&mut self.password).password(true).hint_text(hint).desired_width(f32::INFINITY),
        );
        ui.end_row();

        ui.label("");
        ui.checkbox(&mut self.save_password, "Save password in Keychain");
        ui.end_row();

        ui.label("Instant Client");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.client_dir)
                    .hint_text("folder, if not on the library path")
                    .desired_width(260.0),
            );
            if ui.button("Browse…").clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                self.client_dir = dir.display().to_string();
            }
        });
        ui.end_row();
        ui.label("");
        ui.hyperlink_to(
            "Download Oracle Instant Client (Basic)",
            "https://www.oracle.com/database/technologies/instant-client/downloads.html",
        );
        ui.end_row();
    }

    fn sqlite_fields(&mut self, ui: &mut egui::Ui) {
        ui.label("File");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(300.0));
            if ui.button("Browse…").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter("SQLite database", &["db", "sqlite", "sqlite3", "db3"])
                    .add_filter("All files", &["*"])
                    .pick_file()
            {
                self.path = p.display().to_string();
            }
        });
        ui.end_row();
        ui.label("");
        ui.weak("The file is created if it does not exist.");
        ui.end_row();
    }

    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }
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
