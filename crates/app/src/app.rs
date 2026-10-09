use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use dbm_core::config::DataSourceConfig;
use dbm_core::fk_nav::ForeignKeyIndex;
use dbm_core::{ExecOutcome, TableDetails};
use eframe::egui;

use crate::persist;
use crate::ui::console::{Console, ConsoleAction, PAGE_SIZE, ResultTab, Run, RunMode, Statement, TableView, TxMode};
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

const NEW_SOURCE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::N);
const NEW_CONSOLE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::T);
const CLOSE_TAB: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W);

enum Tab {
    Console(Box<Console>),
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
    statements: Vec<Statement>,
    limit: usize,
    mode: RunMode,
}

/// An action held back because it would discard an open transaction.
enum Confirm {
    CloseConsole(u64),
    Disconnect(String),
    Quit,
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
    /// Runs waiting for their console's connection, by console id.
    pending_runs: HashMap<u64, Vec<PendingRun>>,
    confirm: Option<Confirm>,
    quit_confirmed: bool,
    /// Foreign keys per data source, filled from table details as results need them.
    fk_cache: HashMap<String, ForeignKeyIndex>,
    /// Table details per data source, for deciding whether results are editable.
    table_cache: HashMap<String, HashMap<(String, String), TableDetails>>,
    /// Foreign keys pointing at each table, per data source, for "Referencing rows".
    incoming_cache: HashMap<String, HashMap<(String, String), Vec<dbm_core::IncomingKey>>>,
    /// Last WHERE / ORDER BY per (source, schema, table), for this session.
    table_filters: HashMap<(String, String, String), (String, String)>,
    fk_pending: HashSet<(String, String, String)>,
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
            confirm: None,
            quit_confirmed: false,
            fk_cache: HashMap::new(),
            table_cache: HashMap::new(),
            incoming_cache: HashMap::new(),
            table_filters: HashMap::new(),
            fk_pending: HashSet::new(),
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
            Tab::Console(c) if c.id == id => Some(c.as_mut()),
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
        self.open_tab(Tab::Console(Box::new(Console::new(id, source.to_string(), title, dialect, sql))));
        Some(id)
    }

    /// Starts a run, connecting first if needed.
    fn start_run(&mut self, console_id: u64, statements: Vec<Statement>, limit: usize, mode: RunMode) {
        let Some((source, tx_mode)) = self.console_mut(console_id).map(|c| (c.source.clone(), c.tx_mode)) else {
            return;
        };
        let Some(conn) = self.worker.console_connection(console_id) else {
            let pending = self.pending_runs.entry(console_id).or_default();
            let first = pending.is_empty();
            pending.push(PendingRun { statements, limit, mode });
            if first && let Some(config) = self.source(&source).cloned() {
                let password = self.session_passwords.get(&source).cloned();
                self.worker.connect_console(console_id, config, password);
            }
            // The explorer connection serves metadata, such as foreign keys for links.
            if self.worker.connection(&source).is_none()
                && !matches!(self.tree(&source).status, ConnStatus::Connecting | ConnStatus::Connected)
            {
                self.apply(Action::Connect(source));
            }
            return;
        };
        let run_id = self.next_id();
        let stop = Arc::new(AtomicBool::new(false));
        let Some(console) = self.console_mut(console_id) else { return };
        console.tx_notice = None;
        if matches!(mode, RunMode::Fresh) {
            console.results.clear();
            console.active_result = 0;
            console.nav_back.clear();
            console.nav_forward.clear();
        }
        console.run = Some(Run {
            id: run_id,
            stop: stop.clone(),
            started: Instant::now(),
            total: statements.len(),
            done: 0,
            mode,
            params: statements.iter().map(|(_, p)| p.clone()).collect(),
            limit,
        });
        let begin_first = tx_mode == TxMode::Manual;
        self.worker.run_statements(conn, console_id, run_id, statements, limit, stop, begin_first);
    }

    /// Shows a connection failure in the console whose runs were waiting for it.
    fn fail_pending(&mut self, console_id: u64, error: dbm_core::DbError) {
        let pending = self.pending_runs.remove(&console_id).unwrap_or_default();
        if let Some(c) = self.console_mut(console_id) {
            c.results = pending
                .into_iter()
                .map(|p| {
                    let sql = p.statements.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(";\n");
                    ResultTab::new(None, sql, Vec::new(), Err(error.clone()))
                })
                .collect();
            c.active_result = 0;
        }
    }

    fn consoles(&self) -> impl Iterator<Item = &Console> {
        self.tabs.iter().filter_map(|t| match t {
            Tab::Console(c) => Some(c.as_ref()),
            _ => None,
        })
    }

    /// Closes the explorer connection and every console connection of a source;
    /// the server rolls back their open transactions.
    fn disconnect_source(&mut self, id: &str) {
        self.fk_cache.remove(id);
        self.table_cache.remove(id);
        self.incoming_cache.remove(id);
        self.worker.disconnect(id);
        self.trees.remove(id);
        let consoles: Vec<u64> = self.consoles().filter(|c| c.source == id).map(|c| c.id).collect();
        for console in consoles {
            self.worker.close_console(console);
            self.pending_runs.remove(&console);
            if let Some(c) = self.console_mut(console) {
                if c.in_transaction {
                    c.tx_notice = Some(Err("Disconnected: the open transaction was rolled back".into()));
                }
                c.in_transaction = false;
            }
        }
    }

    /// Requests foreign keys for every table a result set read from.
    fn load_foreign_keys(&mut self, source: &str, console: u64, rs: &dbm_core::ResultSet) {
        // Prefer the explorer connection; a console in an aborted transaction can't run catalog queries.
        let Some(conn) = self.worker.connection(source).or_else(|| self.worker.console_connection(console)) else {
            return;
        };
        let tables: HashSet<(String, String)> =
            rs.columns.iter().filter_map(|c| c.origin.as_ref()).map(|o| (o.schema.clone(), o.table.clone())).collect();
        for (schema, table) in tables {
            let cached =
                self.fk_cache.get(source).is_some_and(|idx| idx.contains_key(&(schema.clone(), table.clone())));
            let key = (source.to_string(), schema.clone(), table.clone());
            if cached || !self.fk_pending.insert(key) {
                continue;
            }
            self.worker.incoming(conn.clone(), source.to_string(), schema.clone(), table.clone());
            self.worker.details(conn.clone(), source.to_string(), schema, table);
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        if let Tab::Console(c) = &self.tabs[i] {
            if let Some(run) = &c.run {
                run.stop.store(true, Ordering::Relaxed);
            }
            let id = c.id;
            self.worker.close_console(id);
            self.pending_runs.remove(&id);
        }
        self.tabs.remove(i);
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
    }

    /// Closes tab `i`, first asking if its console has an open transaction.
    fn request_close_tab(&mut self, i: usize) {
        match self.tabs.get(i) {
            Some(Tab::Console(c)) if c.in_transaction => self.confirm = Some(Confirm::CloseConsole(c.id)),
            Some(_) => self.close_tab(i),
            None => {}
        }
    }

    /// The data source a new console should use: the active console's, else the first one.
    fn current_source(&self) -> Option<String> {
        match self.tabs.get(self.active_tab) {
            Some(Tab::Console(c)) => Some(c.source.clone()),
            _ => self.sources.first().map(|s| s.id.clone()),
        }
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
                }
                Err(e) => self.tree(&source).status = ConnStatus::Failed(e.to_string()),
            },
            Event::ConsoleConnected { console, result } => match result {
                Ok(conn) => {
                    if self.console_mut(console).is_none() {
                        return; // closed while connecting
                    }
                    self.worker.register_console(console, conn);
                    for p in self.pending_runs.remove(&console).unwrap_or_default() {
                        self.start_run(console, p.statements, p.limit, p.mode);
                    }
                }
                Err(e) => self.fail_pending(console, e),
            },
            Event::Incoming { source, schema, table, result } => {
                // On failure the menu simply has no referencing entries for this table.
                if let Ok(keys) = result {
                    self.incoming_cache.entry(source).or_default().insert((schema, table), keys);
                }
            }
            Event::EditsSubmitted { console, tab, result, in_transaction } => {
                let mut refresh = None;
                if let Some(c) = self.console_mut(console) {
                    c.in_transaction = in_transaction;
                    if let Some(t) = c.results.get_mut(tab) {
                        t.submitting = false;
                        match result {
                            Ok(()) => {
                                t.edits = Default::default();
                                refresh = Some((vec![(t.sql.clone(), t.params.clone())], t.limit));
                            }
                            Err(e) => t.edit_error = Some(e),
                        }
                    }
                }
                if let Some((statements, limit)) = refresh {
                    self.start_run(console, statements, limit, RunMode::Replace(tab));
                }
            }
            Event::ControlDone { console, sql, result, in_transaction } => {
                if let Some(c) = self.console_mut(console) {
                    c.tx_busy = false;
                    c.in_transaction = in_transaction;
                    c.tx_notice = Some(match result {
                        Ok(()) if sql == "COMMIT" => Ok("Committed".into()),
                        Ok(()) => Ok("Rolled back".into()),
                        Err(e) => Err(format!("{sql} failed: {}", e.message)),
                    });
                }
            }
            Event::StatementDone { console, run, index, result, in_transaction } => {
                let rows = match &result.outcome {
                    Ok(ExecOutcome::Rows(rs)) => Some(rs.clone()),
                    _ => None,
                };
                let mut source = None;
                if let Some(c) = self.console_mut(console)
                    && let Some(r) = &mut c.run
                    && r.id == run
                {
                    r.done = index + 1;
                    c.in_transaction = in_transaction;
                    let mode = r.mode.clone();
                    let params = r.params.get(index).cloned().unwrap_or_default();
                    let mut tab = ResultTab::new(None, result.sql.clone(), params, result.outcome);
                    tab.elapsed = result.elapsed;
                    tab.limit = r.limit;
                    match mode {
                        RunMode::Replace(i) if i < c.results.len() => {
                            tab.title = c.results[i].title.clone();
                            tab.sort.column = c.results[i].sort.column;
                            c.results[i] = tab;
                        }
                        RunMode::Filter => {
                            let failed = tab.outcome.as_ref().err().map(|e| e.message.clone());
                            match failed {
                                Some(message) if !c.results.is_empty() => {
                                    if let Some(tv) = &mut c.table {
                                        tv.error = Some(message);
                                    }
                                }
                                _ => {
                                    if let Some(tv) = &mut c.table {
                                        tv.error = None;
                                    }
                                    if c.results.is_empty() {
                                        c.results.push(tab);
                                    } else {
                                        c.results[0] = tab;
                                    }
                                    c.active_result = 0;
                                    c.nav_back.clear();
                                    c.nav_forward.clear();
                                }
                            }
                        }
                        RunMode::Navigate { from, title } => {
                            tab.title = Some(title);
                            c.results.push(tab);
                            c.nav_back.push(from);
                            c.nav_forward.clear();
                            c.active_result = c.results.len() - 1;
                        }
                        _ => {
                            c.results.push(tab);
                            c.active_result = c.results.len() - 1;
                        }
                    }
                    source = Some(c.source.clone());
                }
                if let Some(source) = source {
                    self.remember(&source, &result.sql);
                    if let Some(rs) = rows {
                        self.load_foreign_keys(&source, console, &rs);
                    }
                }
            }
            Event::RunFinished { console, run, in_transaction } => {
                if let Some(c) = self.console_mut(console)
                    && c.run.as_ref().is_some_and(|r| r.id == run)
                {
                    c.run = None;
                    c.in_transaction = in_transaction;
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
                self.fk_pending.remove(&(source.clone(), schema.clone(), table.clone()));
                if let Ok(d) = &result {
                    self.fk_cache
                        .entry(source.clone())
                        .or_default()
                        .insert((schema.clone(), table.clone()), d.foreign_keys.clone());
                    self.table_cache
                        .entry(source.clone())
                        .or_default()
                        .insert((schema.clone(), table.clone()), d.clone());
                }
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
                if self.consoles().any(|c| c.source == id && c.in_transaction) {
                    self.confirm = Some(Confirm::Disconnect(id));
                } else {
                    self.disconnect_source(&id);
                }
            }
            Action::Refresh(id) => {
                self.fk_cache.remove(&id);
                self.table_cache.remove(&id);
                self.incoming_cache.remove(&id);
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
                let (filter, order) = self
                    .table_filters
                    .get(&(source.clone(), schema.clone(), table.clone()))
                    .cloned()
                    .unwrap_or_default();
                let view = TableView::new(schema, table.clone(), filter, order);
                let sql = view.sql(dialect);
                if let Some(id) = self.new_console(&source, Some(table), sql.clone()) {
                    if let Some(c) = self.console_mut(id) {
                        c.table = Some(view);
                    }
                    self.start_run(id, vec![(sql, Vec::new())], PAGE_SIZE, RunMode::Filter);
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
                self.disconnect_source(&id);
                self.dialog = None;
            }
            DialogAction::Delete(id) => {
                self.sources.retain(|s| s.id != id);
                self.persist_sources();
                persist::delete_password(&id);
                self.session_passwords.remove(&id);
                self.disconnect_source(&id);
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
        let mut close_active = false;
        if ui.ctx().input(|i| i.viewport().close_requested())
            && !self.quit_confirmed
            && self.consoles().any(|c| c.in_transaction)
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Quit);
        }
        if self.dialog.is_none() && self.confirm.is_none() {
            ui.input_mut(|i| {
                if i.consume_shortcut(&NEW_SOURCE) {
                    actions.push(Action::NewSource);
                }
                if i.consume_shortcut(&CLOSE_TAB) {
                    close_active = true;
                }
                if i.consume_shortcut(&NEW_CONSOLE)
                    && let Some(source) = self.current_source()
                {
                    actions.push(Action::NewConsole(source));
                }
            });
        }

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    let ctx = ui.ctx().clone();
                    if ui
                        .add(egui::Button::new("New data source…").shortcut_text(ctx.format_shortcut(&NEW_SOURCE)))
                        .clicked()
                    {
                        actions.push(Action::NewSource);
                    }
                    let source = self.current_source();
                    if ui
                        .add_enabled(
                            source.is_some(),
                            egui::Button::new("New console").shortcut_text(ctx.format_shortcut(&NEW_CONSOLE)),
                        )
                        .clicked()
                        && let Some(source) = source
                    {
                        actions.push(Action::NewConsole(source));
                    }
                    if ui
                        .add_enabled(
                            !self.tabs.is_empty(),
                            egui::Button::new("Close tab").shortcut_text(ctx.format_shortcut(&CLOSE_TAB)),
                        )
                        .clicked()
                    {
                        close_active = true;
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
        let no_fks = ForeignKeyIndex::new();
        let no_tables = HashMap::new();
        let no_incoming = HashMap::new();
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
                        let in_tx = matches!(tab, Tab::Console(c) if c.in_transaction);
                        let title = if in_tx { format!("{} (tx)", tab.title()) } else { tab.title().to_string() };
                        if ui.selectable_label(self.active_tab == i, title).clicked() {
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
                self.request_close_tab(i);
                return;
            }
            let active = self.active_tab.min(self.tabs.len() - 1);
            match &mut self.tabs[active] {
                Tab::Console(c) => {
                    let history = self.history.get(&c.source).map(Vec::as_slice).unwrap_or_default();
                    let fks = self.fk_cache.get(&c.source).unwrap_or(&no_fks);
                    let tables = self.table_cache.get(&c.source).unwrap_or(&no_tables);
                    let incoming = self.incoming_cache.get(&c.source).unwrap_or(&no_incoming);
                    if let Some(a) = c.show(ui, history, fks, tables, incoming) {
                        console_actions.push((c.id, a));
                    }
                }
                Tab::Ddl { text, .. } => ddl_view(ui, text),
            }
        });
        if close_active {
            self.request_close_tab(self.active_tab);
        }
        for (console, action) in console_actions {
            match action {
                ConsoleAction::Run { statements, limit, mode } => {
                    if matches!(mode, RunMode::Filter)
                        && let Some(c) = self.console_mut(console)
                        && let Some(tv) = &c.table
                    {
                        let key = (c.source.clone(), tv.schema.clone(), tv.table.clone());
                        let value = (tv.filter.clone(), tv.order.clone());
                        self.table_filters.insert(key, value);
                    }
                    self.start_run(console, statements, limit, mode)
                }
                ConsoleAction::Cancel => {
                    if let Some(c) = self.console_mut(console)
                        && let Some(run) = &c.run
                    {
                        run.stop.store(true, Ordering::Relaxed);
                        if let Some(conn) = self.worker.console_connection(console) {
                            self.worker.cancel(conn);
                        }
                    }
                }
                ConsoleAction::SubmitEdits { tab, statements } => {
                    if let Some(conn) = self.worker.console_connection(console)
                        && let Some(c) = self.console_mut(console)
                        && let Some(t) = c.results.get_mut(tab)
                    {
                        t.submitting = true;
                        t.edit_error = None;
                        t.editing = None;
                        self.worker.submit_edits(conn, console, tab, statements);
                    }
                }
                ConsoleAction::Commit | ConsoleAction::Rollback => {
                    let sql = if matches!(action, ConsoleAction::Commit) { "COMMIT" } else { "ROLLBACK" };
                    if let Some(conn) = self.worker.console_connection(console)
                        && let Some(c) = self.console_mut(console)
                    {
                        c.tx_busy = true;
                        c.tx_notice = None;
                        self.worker.control(conn, console, sql);
                    }
                }
            }
        }

        self.confirm_ui(&ui.ctx().clone());

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

impl App {
    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        let (message, proceed) = match confirm {
            Confirm::CloseConsole(_) => {
                ("This console has an open transaction. Closing it rolls the transaction back.", "Roll back and close")
            }
            Confirm::Disconnect(_) => (
                "A console on this data source has an open transaction. Disconnecting rolls it back.",
                "Roll back and disconnect",
            ),
            Confirm::Quit => ("A console has an open transaction. Quitting rolls it back.", "Roll back and quit"),
        };
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Uncommitted changes");
            ui.add_space(6.0);
            ui.label(message);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(proceed).clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        let Some(proceed) = decision else { return };
        let Some(confirm) = self.confirm.take() else { return };
        if !proceed {
            return;
        }
        match confirm {
            Confirm::CloseConsole(id) => {
                if let Some(i) = self.tabs.iter().position(|t| matches!(t, Tab::Console(c) if c.id == id)) {
                    self.close_tab(i);
                }
            }
            Confirm::Disconnect(id) => self.disconnect_source(&id),
            Confirm::Quit => {
                self.quit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
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
                ui.add(
                    egui::TextEdit::multiline(&mut view)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .layouter(&mut layouter),
                );
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
