use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use dbm_core::config::DataSourceConfig;
use eframe::egui;

use crate::persist;
use crate::ui::console::{Console, ConsoleAction, PAGE_SIZE, ResultTab, Run};
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
    NewConsole(String),
    OpenTable { source: String, schema: String, table: String },
    ShowDdl { source: String, schema: String, table: String },
}

const HISTORY_LIMIT: usize = 200;

enum Tab {
    Console(Console),
    Ddl { id: u64, title: String, text: Loadable<String> },
}

impl Tab {
    fn title(&self) -> &str {
        match self {
            Tab::Console(c) => &c.title,
            Tab::Ddl { title, .. } => title,
        }
    }
}

/// A run requested before its data source finished connecting.
struct PendingRun {
    console: u64,
    statements: Vec<String>,
    limit: usize,
    replace: Option<usize>,
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
    tabs: Vec<Tab>,
    active_tab: usize,
    next_id: u64,
    history: HashMap<String, Vec<String>>,
    pending_runs: HashMap<String, Vec<PendingRun>>,
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
            tabs: Vec::new(),
            active_tab: 0,
            next_id: 1,
            history: persist::load_history(),
            pending_runs: HashMap::new(),
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

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn console_mut(&mut self, id: u64) -> Option<&mut Console> {
        self.tabs.iter_mut().find_map(|t| match t {
            Tab::Console(c) if c.id == id => Some(c),
            _ => None,
        })
    }

    fn open_tab(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
    }

    fn new_console(&mut self, source: &str, title: Option<String>, sql: String) -> Option<u64> {
        let config = self.source(source)?;
        let (dialect, name) = (config.kind.dialect(), config.name.clone());
        let id = self.next_id();
        let title = title.unwrap_or_else(|| format!("console [{name}]"));
        self.open_tab(Tab::Console(Console::new(id, source.to_string(), title, dialect, sql)));
        Some(id)
    }

    /// Starts a run, connecting first if needed.
    fn start_run(&mut self, console_id: u64, statements: Vec<String>, limit: usize, replace: Option<usize>) {
        let Some(source) = self.console_mut(console_id).map(|c| c.source.clone()) else { return };
        let Some(conn) = self.worker.connection(&source) else {
            self.pending_runs.entry(source.clone()).or_default().push(PendingRun {
                console: console_id,
                statements,
                limit,
                replace,
            });
            if self.tree(&source).status != ConnStatus::Connecting {
                self.apply(Action::Connect(source));
            }
            return;
        };
        let run_id = self.next_id();
        let stop = Arc::new(AtomicBool::new(false));
        let Some(console) = self.console_mut(console_id) else { return };
        if replace.is_none() {
            console.results.clear();
            console.active_result = 0;
        }
        console.run = Some(Run {
            id: run_id,
            stop: stop.clone(),
            started: Instant::now(),
            total: statements.len(),
            done: 0,
            replace,
        });
        self.worker.run_statements(conn, console_id, run_id, statements, limit, stop);
    }

    fn remember(&mut self, source: &str, sql: &str) {
        let entries = self.history.entry(source.to_string()).or_default();
        entries.retain(|q| q != sql);
        entries.insert(0, sql.to_string());
        entries.truncate(HISTORY_LIMIT);
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Connected { source, result } => match result {
                Ok(conn) => {
                    self.worker.register(source.clone(), conn);
                    let tree = self.tree(&source);
                    tree.status = ConnStatus::Connected;
                    tree.schemas = Loadable::NotLoaded;
                    for p in self.pending_runs.remove(&source).unwrap_or_default() {
                        self.start_run(p.console, p.statements, p.limit, p.replace);
                    }
                }
                Err(e) => {
                    self.tree(&source).status = ConnStatus::Failed(e.to_string());
                    // Pending runs report the connection error in their consoles.
                    for p in self.pending_runs.remove(&source).unwrap_or_default() {
                        if let Some(c) = self.console_mut(p.console) {
                            c.results = vec![ResultTab {
                                sql: p.statements.join(";\n"),
                                outcome: Err(e.clone()),
                                elapsed: Default::default(),
                                limit: p.limit,
                                sort: Default::default(),
                            }];
                            c.active_result = 0;
                        }
                    }
                }
            },
            Event::StatementDone { console, run, index, result } => {
                let mut source = None;
                if let Some(c) = self.console_mut(console)
                    && let Some(r) = &mut c.run
                    && r.id == run
                {
                    r.done = index + 1;
                    let replace = r.replace;
                    let tab = ResultTab {
                        sql: result.sql.clone(),
                        outcome: result.outcome,
                        elapsed: result.elapsed,
                        limit: 0,
                        sort: Default::default(),
                    };
                    match replace {
                        Some(i) if i < c.results.len() => {
                            let limit = c.results[i].limit + PAGE_SIZE;
                            let sort_column = c.results[i].sort.column;
                            c.results[i] = ResultTab { limit, ..tab };
                            c.results[i].sort.column = sort_column;
                        }
                        _ => {
                            c.results.push(ResultTab { limit: PAGE_SIZE, ..tab });
                            c.active_result = c.results.len() - 1;
                        }
                    }
                    source = Some(c.source.clone());
                }
                if let Some(source) = source {
                    self.remember(&source, &result.sql);
                }
            }
            Event::RunFinished { console, run } => {
                if let Some(c) = self.console_mut(console)
                    && c.run.as_ref().is_some_and(|r| r.id == run)
                {
                    c.run = None;
                }
                if let Err(e) = persist::save_history(&self.history) {
                    self.status = Some(format!("Could not save query history: {e:#}"));
                }
            }
            Event::Ddl { tab, result } => {
                for t in &mut self.tabs {
                    if let Tab::Ddl { id, text, .. } = t
                        && *id == tab
                    {
                        *text = match &result {
                            Ok(ddl) => Loadable::Loaded(ddl.clone()),
                            Err(e) => Loadable::Failed(e.to_string()),
                        };
                    }
                }
            }
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
            Action::NewConsole(source) => {
                self.new_console(&source, None, String::new());
            }
            Action::OpenTable { source, schema, table } => {
                let Some(dialect) = self.source(&source).map(|s| s.kind.dialect()) else { return };
                let sql = dialect.select_all(&schema, &table);
                if let Some(id) = self.new_console(&source, Some(table), sql.clone()) {
                    self.start_run(id, vec![sql], PAGE_SIZE, None);
                }
            }
            Action::ShowDdl { source, schema, table } => {
                let id = self.next_id();
                self.open_tab(Tab::Ddl { id, title: format!("DDL {table}"), text: Loadable::Loading });
                self.worker.with_connection(&source, |conn| async move {
                    Event::Ddl { tab: id, result: conn.ddl(&schema, &table).await }
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
                    if ui.small_button("Dismiss").clicked() {
                        self.status = None;
                    }
                });
            });
        }

        egui::Panel::left("explorer").resizable(true).default_size(300.0).min_size(180.0).show(ui, |ui| {
            explorer::show(ui, &self.sources, &mut self.trees, &mut actions);
        });

        let mut console_actions = Vec::new();
        egui::CentralPanel::default().show(ui, |ui| {
            if self.tabs.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.weak("Double-click a data source to open a console, or a table to see its data.");
                });
                return;
            }
            let mut close = None;
            ui.horizontal_wrapped(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if ui.selectable_label(self.active_tab == i, tab.title()).clicked() {
                            self.active_tab = i;
                        }
                        if ui.small_button("x").on_hover_text("Close tab").clicked() {
                            close = Some(i);
                        }
                    });
                }
            });
            ui.separator();
            if let Some(i) = close {
                if let Tab::Console(c) = &self.tabs[i]
                    && let Some(run) = &c.run
                {
                    run.stop.store(true, Ordering::Relaxed);
                }
                self.tabs.remove(i);
                self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
                return;
            }
            let active = self.active_tab.min(self.tabs.len() - 1);
            match &mut self.tabs[active] {
                Tab::Console(c) => {
                    let history = self.history.get(&c.source).map(Vec::as_slice).unwrap_or_default();
                    if let Some(a) = c.show(ui, history) {
                        console_actions.push((c.id, a));
                    }
                }
                Tab::Ddl { text, .. } => ddl_view(ui, text),
            }
        });
        for (console, action) in console_actions {
            match action {
                ConsoleAction::Run { statements, limit, replace } => self.start_run(console, statements, limit, replace),
                ConsoleAction::Cancel => {
                    if let Some(c) = self.console_mut(console)
                        && let Some(run) = &c.run
                    {
                        run.stop.store(true, Ordering::Relaxed);
                        let source = c.source.clone();
                        if let Some(conn) = self.worker.connection(&source) {
                            self.worker.cancel(conn);
                        }
                    }
                }
            }
        }

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

fn ddl_view(ui: &mut egui::Ui, text: &Loadable<String>) {
    match text {
        Loadable::Loaded(ddl) => {
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(ddl.clone());
            }
            ui.separator();
            let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
                let job = crate::ui::sql_highlight::layout(ui, text.as_str(), wrap_width);
                ui.fonts_mut(|f| f.layout_job(job))
            };
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                let mut view = ddl.as_str();
                ui.add(egui::TextEdit::multiline(&mut view).code_editor().desired_width(f32::INFINITY).layouter(&mut layouter));
            });
        }
        Loadable::Failed(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        _ => {
            ui.spinner();
        }
    }
}
