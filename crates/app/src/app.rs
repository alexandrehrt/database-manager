use std::collections::HashMap;

use dbm_core::config::DataSourceConfig;
use eframe::egui;

use crate::persist;
use crate::ui::datasource_dialog::{DataSourceDialog, DialogAction, TestState};
use crate::ui::explorer::{self, ConnStatus, Loadable, RelationNode, SchemaNode, SourceTree};
use crate::worker::{Event, Worker};

/// Something the UI asked for while drawing; applied after the frame's UI
/// code has released its borrows.
pub enum Action {
    NewSource,
    EditSource(String),
    Connect(String),
    Disconnect(String),
    Refresh(String),
    LoadSchemas(String),
    LoadRelations(String, String),
    LoadDetails(String, String, String),
}

pub struct App {
    worker: Worker,
    sources: Vec<DataSourceConfig>,
    trees: HashMap<String, SourceTree>,
    /// Passwords the user chose not to save, kept for this run only.
    session_passwords: HashMap<String, String>,
    dialog: Option<DataSourceDialog>,
    next_test_nonce: u64,
    status: Option<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (sources, status) = match persist::load_sources() {
            Ok(s) => (s, None),
            Err(e) => (Vec::new(), Some(format!("Could not load saved data sources: {e:#}"))),
        };
        Self {
            worker: Worker::new(cc.egui_ctx.clone()),
            sources,
            trees: HashMap::new(),
            session_passwords: HashMap::new(),
            dialog: None,
            next_test_nonce: 0,
            status,
        }
    }

    fn source(&self, id: &str) -> Option<&DataSourceConfig> {
        self.sources.iter().find(|s| s.id == id)
    }

    fn tree(&mut self, id: &str) -> &mut SourceTree {
        self.trees.entry(id.to_string()).or_default()
    }

    fn persist_sources(&mut self) {
        if let Err(e) = persist::save_sources(&self.sources) {
            self.status = Some(format!("Could not save data sources: {e:#}"));
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Connected { source, result } => match result {
                Ok(conn) => {
                    self.worker.register(source.clone(), conn);
                    let tree = self.tree(&source);
                    tree.status = ConnStatus::Connected;
                    tree.schemas = Loadable::NotLoaded;
                }
                Err(e) => self.tree(&source).status = ConnStatus::Failed(e.to_string()),
            },
            Event::Tested { nonce, result } => {
                if let Some(d) = &mut self.dialog
                    && matches!(d.test, Some(TestState::Running(n)) if n == nonce)
                {
                    d.test = Some(TestState::Done(result.map_err(|e| e.to_string())));
                }
            }
            Event::Schemas { source, result } => {
                self.tree(&source).schemas = match result {
                    Ok(names) => Loadable::Loaded(
                        names.into_iter().map(|name| SchemaNode { name, relations: Loadable::NotLoaded }).collect(),
                    ),
                    Err(e) => Loadable::Failed(e.to_string()),
                };
            }
            Event::Relations { source, schema, result } => {
                if let Loadable::Loaded(schemas) = &mut self.tree(&source).schemas
                    && let Some(node) = schemas.iter_mut().find(|s| s.name == schema)
                {
                    node.relations = match result {
                        Ok(rels) => Loadable::Loaded(
                            rels.into_iter()
                                .map(|relation| RelationNode { relation, details: Loadable::NotLoaded })
                                .collect(),
                        ),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                }
            }
            Event::Details { source, schema, table, result } => {
                if let Loadable::Loaded(schemas) = &mut self.tree(&source).schemas
                    && let Some(node) = schemas.iter_mut().find(|s| s.name == schema)
                    && let Loadable::Loaded(rels) = &mut node.relations
                    && let Some(rel) = rels.iter_mut().find(|r| r.relation.name == table)
                {
                    rel.details = match result {
                        Ok(d) => Loadable::Loaded(d),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                }
            }
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::NewSource => self.dialog = Some(DataSourceDialog::new()),
            Action::EditSource(id) => {
                if let Some(s) = self.source(&id) {
                    self.dialog = Some(DataSourceDialog::edit(s));
                }
            }
            Action::Connect(id) => {
                if let Some(config) = self.source(&id).cloned() {
                    let password = self.session_passwords.get(&id).cloned();
                    self.tree(&id).status = ConnStatus::Connecting;
                    self.worker.connect(config, password);
                }
            }
            Action::Disconnect(id) => {
                self.worker.disconnect(&id);
                self.trees.remove(&id);
            }
            Action::Refresh(id) => {
                self.tree(&id).schemas = Loadable::NotLoaded;
            }
            Action::LoadSchemas(source) => {
                let s = source.clone();
                self.worker.with_connection(&source, |conn| async move {
                    Event::Schemas { source: s, result: conn.schemas().await }
                });
            }
            Action::LoadRelations(source, schema) => {
                let s = source.clone();
                self.worker.with_connection(&source, |conn| async move {
                    let result = conn.relations(&schema).await;
                    Event::Relations { source: s, schema, result }
                });
            }
            Action::LoadDetails(source, schema, table) => {
                let s = source.clone();
                self.worker.with_connection(&source, |conn| async move {
                    let result = conn.table_details(&schema, &table).await;
                    Event::Details { source: s, schema, table, result }
                });
            }
        }
    }

    fn apply_dialog(&mut self, action: DialogAction) {
        match action {
            DialogAction::Close => self.dialog = None,
            DialogAction::Test { config, password } => {
                self.next_test_nonce += 1;
                let nonce = self.next_test_nonce;
                let password = password.or_else(|| self.session_passwords.get(&config.id).cloned());
                if let Some(d) = &mut self.dialog {
                    d.test = Some(TestState::Running(nonce));
                }
                self.worker.test(nonce, config, password);
            }
            DialogAction::Save { config, password, save_password } => {
                if let Some(pw) = password {
                    if save_password {
                        if let Err(e) = persist::save_password(&config.id, &pw) {
                            if let Some(d) = &mut self.dialog {
                                d.set_error(format!("Could not save the password to the Keychain: {e:#}"));
                            }
                            return;
                        }
                        self.session_passwords.remove(&config.id);
                    } else {
                        persist::delete_password(&config.id);
                        self.session_passwords.insert(config.id.clone(), pw);
                    }
                }
                let id = config.id.clone();
                match self.sources.iter_mut().find(|s| s.id == id) {
                    Some(existing) => *existing = config,
                    None => self.sources.push(config),
                }
                self.persist_sources();
                // Settings may have changed, so the next expand reconnects.
                self.worker.disconnect(&id);
                self.trees.remove(&id);
                self.dialog = None;
            }
            DialogAction::Delete(id) => {
                self.sources.retain(|s| s.id != id);
                self.persist_sources();
                persist::delete_password(&id);
                self.session_passwords.remove(&id);
                self.worker.disconnect(&id);
                self.trees.remove(&id);
                self.dialog = None;
            }
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Some(event) = self.worker.try_recv() {
            self.handle_event(event);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut actions = Vec::new();

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New data source…").clicked() {
                        actions.push(Action::NewSource);
                    }
                });
            });
        });

        if let Some(status) = self.status.clone() {
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().warn_fg_color, status);
                    if ui.small_button("✕").clicked() {
                        self.status = None;
                    }
                });
            });
        }

        egui::Panel::left("explorer").resizable(true).default_size(300.0).min_size(180.0).show(ui, |ui| {
            explorer::show(ui, &self.sources, &mut self.trees, &mut actions);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.centered_and_justified(|ui| {
                ui.weak("Expand a data source in the explorer to connect.");
            });
        });

        if let Some(dialog) = &mut self.dialog
            && let Some(action) = dialog.show(ui.ctx())
        {
            self.apply_dialog(action);
        }

        for action in actions {
            self.apply(action);
        }
    }
}
