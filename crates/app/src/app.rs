use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use dbm_core::config::DataSourceConfig;
use dbm_core::fk_nav::ForeignKeyIndex;
use dbm_core::{ExecOutcome, TableDetails};
use eframe::egui::{self, Color32, RichText, Stroke};

use crate::persist;
use crate::ui::console::{
    self, Console, ConsoleAction, ConsoleContext, PAGE_SIZE, ResultTab, Run, RunMode, Statement, TableView, TxMode,
    ViewMode,
};
use crate::ui::datasource_dialog::{DataSourceDialog, DialogAction, TestState};
use crate::ui::explorer::{self, ConnStatus, Loadable, SchemaNode, SidebarState, SourceTree};
use crate::ui::goto::{Candidate, GotoOutcome, GotoTable};
use crate::ui::theme::{self, color, icon};
use crate::worker::{Event, Worker};

/// Something the UI asked for while drawing; applied after the frame's UI
/// code has released its borrows.
pub enum Action {
    NewSource,
    OpenFile,
    EditSource(String),
    SelectSource(String),
    SelectSchema(String, String),
    Connect(String),
    Disconnect(String),
    Refresh(String),
    LoadSchemas(String),
    LoadRelations(String, String),
    NewConsole(String),
    OpenTable { source: String, schema: String, table: String },
    ShowDdl { source: String, schema: String, table: String },
}

const HISTORY_LIMIT: usize = 200;

const NEW_SOURCE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::N);
const NEW_CONSOLE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::T);
const GOTO_TABLE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K);
const CLOSE_TAB: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W);

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
    tabs: Vec<Console>,
    active_tab: usize,
    next_id: u64,
    console_count: u64,
    /// The data source the sidebar shows.
    current_source: Option<String>,
    /// Schema the sidebar shows, per data source.
    schema_choice: HashMap<String, String>,
    history: HashMap<String, Vec<String>>,
    /// Runs waiting for their console's connection, by console id.
    pending_runs: HashMap<u64, Vec<PendingRun>>,
    confirm: Option<Confirm>,
    quit_confirmed: bool,
    /// Foreign keys per data source, filled from table details as results need them.
    fk_cache: HashMap<String, ForeignKeyIndex>,
    /// Table details per data source, for editing, the grid and the Structure view.
    table_cache: HashMap<String, HashMap<(String, String), TableDetails>>,
    /// Foreign keys pointing at each table, per data source, for "Referencing rows".
    incoming_cache: HashMap<String, HashMap<(String, String), Vec<dbm_core::IncomingKey>>>,
    /// Last filters / ORDER BY per (source, schema, table), for this session.
    table_filters: HashMap<(String, String, String), (Vec<String>, String)>,
    sidebar_filter: String,
    goto: Option<GotoTable>,
    fk_pending: HashSet<(String, String, String)>,
    /// Consoles whose DDL is being fetched.
    ddl_pending: HashSet<u64>,
    window_title: String,
    /// Restored table tabs that load their rows when first shown.
    restore_pending: HashSet<u64>,
    last_session: persist::Session,
    last_session_save: Instant,
    /// Consoles that ran DDL inside a transaction; the explorer refreshes once it ends.
    ddl_uncommitted: HashSet<u64>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        let (sources, status) = match persist::load_sources() {
            Ok(s) => (s, None),
            Err(e) => (Vec::new(), Some(format!("Could not load saved data sources: {e:#}"))),
        };
        let mut app = Self {
            worker: Worker::new(cc.egui_ctx.clone()),
            current_source: sources.first().map(|s| s.id.clone()),
            sources,
            trees: HashMap::new(),
            session_passwords: HashMap::new(),
            dialog: None,
            next_test_nonce: 0,
            status,
            tabs: Vec::new(),
            active_tab: 0,
            next_id: 1,
            console_count: 0,
            schema_choice: HashMap::new(),
            history: persist::load_history(),
            pending_runs: HashMap::new(),
            confirm: None,
            quit_confirmed: false,
            fk_cache: HashMap::new(),
            table_cache: HashMap::new(),
            incoming_cache: HashMap::new(),
            table_filters: HashMap::new(),
            sidebar_filter: String::new(),
            goto: None,
            fk_pending: HashSet::new(),
            ddl_pending: HashSet::new(),
            window_title: String::new(),
            restore_pending: HashSet::new(),
            last_session: persist::Session::default(),
            last_session_save: Instant::now(),
            ddl_uncommitted: HashSet::new(),
        };
        app.restore_session();
        app
    }

    /// Reopens the tabs of the last run. Nothing runs until a table tab is shown.
    fn restore_session(&mut self) {
        let session = persist::load_session();
        for tab in &session.tabs {
            let Some(dialect) = self.source(&tab.source).map(|s| s.kind.dialect()) else { continue };
            let id = self.next_id();
            let mut c = Console::new(id, tab.source.clone(), tab.title.clone(), dialect, tab.sql.clone());
            c.file = tab.file.clone();
            c.saved_text = tab.saved_text.clone();
            if let Some(t) = &tab.table {
                let mut view = TableView::new(t.schema.clone(), t.table.clone(), t.filters.clone(), t.order.clone());
                view.mode = match t.mode.as_str() {
                    "structure" => ViewMode::Structure,
                    "sql" => ViewMode::Sql,
                    _ => ViewMode::Content,
                };
                c.sql = view.sql(dialect);
                c.table = Some(view);
                self.restore_pending.insert(id);
            } else if let Some(n) = tab.title.strip_prefix("console ").and_then(|n| n.parse::<u64>().ok()) {
                self.console_count = self.console_count.max(n);
            }
            self.tabs.push(c);
        }
        if !self.tabs.is_empty() {
            self.activate(session.active.min(self.tabs.len() - 1));
        }
        self.last_session = self.session_snapshot();
    }

    fn session_snapshot(&self) -> persist::Session {
        let tabs = self
            .tabs
            .iter()
            .map(|c| persist::SessionTab {
                source: c.source.clone(),
                title: c.title.clone(),
                sql: c.sql.clone(),
                file: c.file.clone(),
                saved_text: c.saved_text.clone(),
                table: c.table.as_ref().map(|t| persist::SessionTable {
                    schema: t.schema.clone(),
                    table: t.table.clone(),
                    filters: t.filters.clone(),
                    order: t.order.clone(),
                    mode: match t.mode {
                        ViewMode::Content => "content",
                        ViewMode::Structure => "structure",
                        ViewMode::Sql => "sql",
                    }
                    .into(),
                }),
            })
            .collect();
        persist::Session { tabs, active: self.active_tab }
    }

    /// Saves the open tabs when they changed, at most once a second unless `force`.
    fn save_session(&mut self, force: bool) {
        if !force && self.last_session_save.elapsed().as_secs_f32() < 1.0 {
            return;
        }
        let session = self.session_snapshot();
        if session == self.last_session {
            return;
        }
        self.last_session_save = Instant::now();
        if let Err(e) = persist::save_session(&session) {
            self.status = Some(format!("Could not save the session: {e:#}"));
        }
        self.last_session = session;
    }

    /// Loads a restored table tab's rows the first time it is shown.
    fn load_restored(&mut self) {
        let Some(c) = self.tabs.get(self.active_tab) else { return };
        if !self.restore_pending.remove(&c.id) {
            return;
        }
        let Some(tv) = &c.table else { return };
        let (id, source, schema, table, sql) =
            (c.id, c.source.clone(), tv.schema.clone(), tv.table.clone(), c.sql.clone());
        self.schema_choice.insert(source.clone(), schema.clone());
        self.request_details(&source, id, schema, table);
        self.start_run(id, vec![(sql, Vec::new())], PAGE_SIZE, RunMode::Filter);
    }

    /// Opens a .sql file in a new console on the current data source.
    fn open_file(&mut self) {
        let Some(source) = self.current_source.clone() else {
            self.status = Some("Choose a data source before opening a file.".into());
            return;
        };
        let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).add_filter("All files", &["*"]).pick_file()
        else {
            return;
        };
        if let Some(i) = self.tabs.iter().position(|c| c.file.as_ref() == Some(&path)) {
            return self.activate(i);
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let title = path.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
                if let Some(id) = self.new_console(&source, Some(title), text.clone())
                    && let Some(c) = self.console_mut(id)
                {
                    c.file = Some(path);
                    c.saved_text = Some(text);
                }
            }
            Err(e) => self.status = Some(format!("Could not open {}: {e}", path.display())),
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
        self.tabs.iter_mut().find(|c| c.id == id)
    }

    fn active(&self) -> Option<&Console> {
        self.tabs.get(self.active_tab)
    }

    fn activate(&mut self, i: usize) {
        self.active_tab = i;
        if let Some(c) = self.tabs.get(i) {
            self.current_source = Some(c.source.clone());
        }
    }

    fn new_console(&mut self, source: &str, title: Option<String>, sql: String) -> Option<u64> {
        let dialect = self.source(source)?.kind.dialect();
        let id = self.next_id();
        let title = title.unwrap_or_else(|| {
            self.console_count += 1;
            format!("console {}", self.console_count)
        });
        self.tabs.push(Console::new(id, source.to_string(), title, dialect, sql));
        self.activate(self.tabs.len() - 1);
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

    /// Closes the explorer connection and every console connection of a source;
    /// the server rolls back their open transactions.
    fn disconnect_source(&mut self, id: &str) {
        self.fk_cache.remove(id);
        self.table_cache.remove(id);
        self.incoming_cache.remove(id);
        self.worker.disconnect(id);
        self.trees.remove(id);
        let consoles: Vec<u64> = self.tabs.iter().filter(|c| c.source == id).map(|c| c.id).collect();
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
        let tables: HashSet<(String, String)> =
            rs.columns.iter().filter_map(|c| c.origin.as_ref()).map(|o| (o.schema.clone(), o.table.clone())).collect();
        for (schema, table) in tables {
            self.request_details(source, console, schema, table);
        }
    }

    /// Loads a table's details (columns, keys) and incoming keys, once.
    fn request_details(&mut self, source: &str, console: u64, schema: String, table: String) {
        let cached = self.table_cache.get(source).is_some_and(|m| m.contains_key(&(schema.clone(), table.clone())));
        let key = (source.to_string(), schema.clone(), table.clone());
        if cached || self.fk_pending.contains(&key) {
            return;
        }
        // Prefer the explorer connection; a console in an aborted transaction can't run catalog queries.
        let Some(conn) = self.worker.connection(source).or_else(|| self.worker.console_connection(console)) else {
            return;
        };
        self.fk_pending.insert(key);
        self.worker.incoming(conn.clone(), source.to_string(), schema.clone(), table.clone());
        self.worker.details(conn, source.to_string(), schema, table);
    }

    fn request_ddl(&mut self, console: u64) {
        let Some(c) = self.tabs.iter().find(|c| c.id == console) else { return };
        let Some(tv) = &c.table else { return };
        let (source, schema, table) = (c.source.clone(), tv.schema.clone(), tv.table.clone());
        let Some(conn) = self.worker.connection(&source).or_else(|| self.worker.console_connection(console)) else {
            return;
        };
        if self.ddl_pending.insert(console) {
            self.worker.spawn(async move { Event::Ddl { tab: console, result: conn.ddl(&schema, &table).await } });
        }
    }

    /// (schema, table) of every loaded table and view of a source.
    fn source_tables(&self, source: &str) -> Vec<(String, String)> {
        let Some(SourceTree { schemas: Loadable::Loaded(schemas), .. }) = self.trees.get(source) else {
            return Vec::new();
        };
        schemas
            .iter()
            .filter_map(|s| match &s.relations {
                Loadable::Loaded(rels) => Some(rels.iter().map(|r| (s.name.clone(), r.name.clone()))),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let c = &self.tabs[i];
        if let Some(run) = &c.run {
            run.stop.store(true, Ordering::Relaxed);
        }
        let id = c.id;
        self.worker.close_console(id);
        self.pending_runs.remove(&id);
        self.tabs.remove(i);
        let next = self.active_tab.min(self.tabs.len().saturating_sub(1));
        self.activate(next);
    }

    /// Closes tab `i`, first asking if its console has an open transaction.
    fn request_close_tab(&mut self, i: usize) {
        match self.tabs.get(i) {
            Some(c) if c.in_transaction || c.has_pending_edits() || c.file_dirty() => {
                self.confirm = Some(Confirm::CloseConsole(c.id))
            }
            Some(_) => self.close_tab(i),
            None => {}
        }
    }

    /// Reloads the explorer and drops cached table metadata after a schema change,
    /// keeping what is shown until the new lists arrive.
    fn refresh_after_ddl(&mut self, source: &str) {
        self.fk_cache.remove(source);
        self.table_cache.remove(source);
        self.incoming_cache.remove(source);
        self.fk_pending.retain(|(s, _, _)| s != source);
        let open: Vec<(u64, String, String)> = self
            .tabs
            .iter()
            .filter(|c| c.source == source)
            .filter_map(|c| c.table.as_ref().map(|t| (c.id, t.schema.clone(), t.table.clone())))
            .collect();
        for (id, schema, table) in open {
            self.request_details(source, id, schema, table);
        }
        let mut loaded = Vec::new();
        if let Loadable::Loaded(schemas) = &self.tree(source).schemas {
            loaded
                .extend(schemas.iter().filter(|s| matches!(s.relations, Loadable::Loaded(_))).map(|s| s.name.clone()));
        } else {
            return;
        }
        self.apply(Action::LoadSchemas(source.to_string()));
        for schema in loaded {
            self.apply(Action::LoadRelations(source.to_string(), schema));
        }
    }

    /// Requests whatever schemas and relations of connected sources aren't
    /// loaded yet, so the Go to table picker and completion can search them.
    /// Returns whether anything is still loading.
    fn load_all_tables(&mut self) -> bool {
        let mut loading = false;
        let mut actions = Vec::new();
        for (source, tree) in &mut self.trees {
            if tree.status != ConnStatus::Connected {
                continue;
            }
            match &mut tree.schemas {
                Loadable::NotLoaded => {
                    tree.schemas = Loadable::Loading;
                    actions.push(Action::LoadSchemas(source.clone()));
                    loading = true;
                }
                Loadable::Loading => loading = true,
                Loadable::Loaded(schemas) => {
                    for schema in schemas {
                        match schema.relations {
                            Loadable::NotLoaded => {
                                schema.relations = Loadable::Loading;
                                actions.push(Action::LoadRelations(source.clone(), schema.name.clone()));
                                loading = true;
                            }
                            Loadable::Loading => loading = true,
                            _ => {}
                        }
                    }
                }
                Loadable::Failed(_) => {}
            }
        }
        for action in actions {
            self.apply(action);
        }
        loading
    }

    fn goto_candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for source in &self.sources {
            let Some(SourceTree { schemas: Loadable::Loaded(schemas), .. }) = self.trees.get(&source.id) else {
                continue;
            };
            for schema in schemas {
                if let Loadable::Loaded(rels) = &schema.relations {
                    out.extend(rels.iter().map(|r| Candidate {
                        source: source.id.clone(),
                        source_name: source.name.clone(),
                        schema: schema.name.clone(),
                        table: r.name.clone(),
                        is_view: r.kind != dbm_core::RelationKind::Table,
                    }));
                }
            }
        }
        out
    }

    fn goto_ui(&mut self, ctx: &egui::Context) {
        if self.goto.is_none() {
            return;
        }
        let loading = self.load_all_tables();
        let candidates = self.goto_candidates();
        let Some(goto) = &mut self.goto else { return };
        match goto.show(ctx, &candidates, loading) {
            Some(GotoOutcome::Open(i)) => {
                self.goto = None;
                let c = &candidates[i];
                self.apply(Action::OpenTable {
                    source: c.source.clone(),
                    schema: c.schema.clone(),
                    table: c.table.clone(),
                });
            }
            Some(GotoOutcome::Close) => self.goto = None,
            None => {}
        }
    }

    fn remember(&mut self, source: &str, sql: &str) {
        let entries = self.history.entry(source.to_string()).or_default();
        entries.retain(|q| q != sql);
        entries.insert(0, sql.to_string());
        entries.truncate(HISTORY_LIMIT);
    }

    /// Opens a table's tab in `mode`, reusing an open one.
    fn open_table(&mut self, source: String, schema: String, table: String, mode: ViewMode) {
        let existing = self.tabs.iter().position(|c| {
            c.source == source && c.table.as_ref().is_some_and(|t| t.schema == schema && t.table == table)
        });
        if let Some(i) = existing {
            self.activate(i);
            if let Some(tv) = &mut self.tabs[i].table {
                tv.mode = mode;
            }
            return;
        }
        let Some(dialect) = self.source(&source).map(|s| s.kind.dialect()) else { return };
        let (filters, order) =
            self.table_filters.get(&(source.clone(), schema.clone(), table.clone())).cloned().unwrap_or_default();
        let mut view = TableView::new(schema.clone(), table.clone(), filters, order);
        view.mode = mode;
        let sql = view.sql(dialect);
        if let Some(id) = self.new_console(&source, Some(table.clone()), sql.clone()) {
            if let Some(c) = self.console_mut(id) {
                c.table = Some(view);
            }
            self.schema_choice.insert(source.clone(), schema.clone());
            self.request_details(&source, id, schema, table);
            self.start_run(id, vec![(sql, Vec::new())], PAGE_SIZE, RunMode::Filter);
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
                if !in_transaction
                    && self.ddl_uncommitted.remove(&console)
                    && sql == "COMMIT"
                    && let Some(source) = self.console_mut(console).map(|c| c.source.clone())
                {
                    self.refresh_after_ddl(&source);
                }
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
                let ddl = result.outcome.is_ok() && is_ddl(&result.sql);
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
                if let Some(source) = &source {
                    if ddl {
                        self.ddl_uncommitted.insert(console);
                    }
                    // Outside a transaction the change is visible now (COMMIT typed in
                    // the console also lands here).
                    if !in_transaction && self.ddl_uncommitted.remove(&console) {
                        self.refresh_after_ddl(source);
                    }
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
                self.ddl_pending.remove(&tab);
                if let Some(c) = self.console_mut(tab)
                    && let Some(tv) = &mut c.table
                {
                    tv.ddl = Some(result.map_err(|e| e.to_string()));
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
                // A reload keeps the relations already listed until their own reload lands.
                let mut previous: HashMap<String, Loadable<Vec<dbm_core::Relation>>> =
                    match std::mem::replace(&mut self.tree(&source).schemas, Loadable::NotLoaded) {
                        Loadable::Loaded(nodes) => nodes.into_iter().map(|n| (n.name, n.relations)).collect(),
                        _ => HashMap::new(),
                    };
                self.tree(&source).schemas = match result {
                    Ok(names) => Loadable::Loaded(
                        names
                            .into_iter()
                            .map(|name| {
                                let relations = previous.remove(&name).unwrap_or(Loadable::NotLoaded);
                                SchemaNode { name, relations }
                            })
                            .collect(),
                    ),
                    Err(e) => Loadable::Failed(e.to_string()),
                };
            }
            Event::Relations { source, schema, result } => {
                if let Loadable::Loaded(schemas) = &mut self.tree(&source).schemas
                    && let Some(node) = schemas.iter_mut().find(|s| s.name == schema)
                {
                    node.relations = match result {
                        Ok(rels) => Loadable::Loaded(rels),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                }
            }
            Event::Details { source, schema, table, result } => {
                self.fk_pending.remove(&(source.clone(), schema.clone(), table.clone()));
                if let Ok(d) = result {
                    self.fk_cache
                        .entry(source.clone())
                        .or_default()
                        .insert((schema.clone(), table.clone()), d.foreign_keys.clone());
                    self.table_cache.entry(source).or_default().insert((schema, table), d);
                }
            }
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::NewSource => self.dialog = Some(DataSourceDialog::new()),
            Action::OpenFile => self.open_file(),
            Action::EditSource(id) => {
                if let Some(s) = self.source(&id) {
                    self.dialog = Some(DataSourceDialog::edit(s));
                }
            }
            Action::SelectSource(id) => self.current_source = Some(id),
            Action::SelectSchema(source, schema) => {
                self.schema_choice.insert(source, schema);
            }
            Action::Connect(id) => {
                if let Some(config) = self.source(&id).cloned() {
                    let password = self.session_passwords.get(&id).cloned();
                    self.tree(&id).status = ConnStatus::Connecting;
                    self.worker.connect(config, password);
                }
            }
            Action::Disconnect(id) => {
                if self.tabs.iter().any(|c| c.source == id && c.in_transaction) {
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
            Action::OpenTable { source, schema, table } => self.open_table(source, schema, table, ViewMode::Content),
            Action::ShowDdl { source, schema, table } => self.open_table(source, schema, table, ViewMode::Sql),
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
            DialogAction::Save { config, password, save_password, connect } => {
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
                // Settings may have changed, so the next use reconnects.
                self.disconnect_source(&id);
                self.current_source = Some(id.clone());
                self.dialog = None;
                if connect {
                    self.apply(Action::Connect(id));
                }
            }
            DialogAction::Delete(id) => {
                self.sources.retain(|s| s.id != id);
                self.persist_sources();
                persist::delete_password(&id);
                self.session_passwords.remove(&id);
                self.disconnect_source(&id);
                if self.current_source.as_deref() == Some(id.as_str()) {
                    self.current_source = self.sources.first().map(|s| s.id.clone());
                }
                self.dialog = None;
            }
        }
    }

    fn apply_console(&mut self, console: u64, action: ConsoleAction) {
        match action {
            ConsoleAction::Run { statements, limit, mode } => {
                if matches!(mode, RunMode::Filter)
                    && let Some(c) = self.console_mut(console)
                    && let Some(tv) = &c.table
                {
                    let key = (c.source.clone(), tv.schema.clone(), tv.table.clone());
                    let value = (tv.filters.clone(), tv.order.clone());
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
            ConsoleAction::OpenFile => self.open_file(),
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
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Some(event) = self.worker.try_recv() {
            self.handle_event(event);
        }
        self.load_restored();
        self.save_session(false);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_session(true);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut actions = Vec::new();
        let mut console_actions = Vec::new();
        let mut close_active = false;
        if ui.ctx().input(|i| i.viewport().close_requested())
            && !self.quit_confirmed
            && self.tabs.iter().any(|c| c.in_transaction || c.has_pending_edits() || c.file_dirty())
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Quit);
        }
        if self.dialog.is_none() && self.confirm.is_none() && self.goto.is_none() {
            ui.input_mut(|i| {
                if i.consume_shortcut(&GOTO_TABLE) {
                    self.goto = Some(GotoTable::default());
                }
                if i.consume_shortcut(&console::OPEN_FILE) {
                    actions.push(Action::OpenFile);
                }
                if i.consume_shortcut(&NEW_SOURCE) {
                    actions.push(Action::NewSource);
                }
                if i.consume_shortcut(&CLOSE_TAB) {
                    close_active = true;
                }
                if i.consume_shortcut(&NEW_CONSOLE)
                    && let Some(source) = self.current_source.clone()
                {
                    actions.push(Action::NewConsole(source));
                }
            });
        }
        self.update_window_title(ui.ctx());

        egui::Panel::top("topbar")
            .frame(
                egui::Frame::new()
                    .fill(color::BG)
                    .inner_margin(egui::Margin::symmetric(10, 7))
                    .stroke(Stroke::new(1.0, color::BORDER)),
            )
            .show(ui, |ui| self.top_bar(ui, &mut actions, &mut console_actions));

        if let Some(status) = self.status.clone() {
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(color::WARNING, status);
                    if ui.small_button("Dismiss").clicked() {
                        self.status = None;
                    }
                });
            });
        }

        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(230.0)
            .min_size(180.0)
            .frame(
                egui::Frame::new()
                    .fill(color::BG_SUBTLE)
                    .inner_margin(egui::Margin::symmetric(10, 10))
                    .stroke(Stroke::new(1.0, color::BORDER)),
            )
            .show(ui, |ui| {
                let current = self.current_source.clone();
                let active_table = self
                    .active()
                    .filter(|c| Some(&c.source) == current.as_ref())
                    .and_then(|c| c.table.as_ref())
                    .map(|t| (t.schema.clone(), t.table.clone()));
                let schema = current.as_ref().and_then(|s| self.schema_choice.get(s)).cloned();
                let state = SidebarState {
                    current: current.as_deref(),
                    schema: schema.as_deref(),
                    active_table: active_table.as_ref().map(|(s, t)| (s.as_str(), t.as_str())),
                };
                explorer::show(ui, &self.sources, &mut self.trees, state, &mut self.sidebar_filter, &mut actions);
            });

        // Completion needs the active console's tables; load them in the background.
        let active_source = self.active().map(|c| c.source.clone());
        if active_source.is_some() {
            self.load_all_tables();
        }
        let catalog_tables = active_source.as_deref().map(|s| self.source_tables(s)).unwrap_or_default();
        // Schema that unqualified names resolve to: Oracle uses the user's own schema.
        let default_schema = match active_source.as_deref().and_then(|s| self.source(s)).map(|s| &s.kind) {
            Some(dbm_core::config::DataSourceKind::Sqlite { .. }) => "main".to_string(),
            Some(dbm_core::config::DataSourceKind::Oracle { user, .. }) => user.to_uppercase(),
            _ => "public".to_string(),
        };
        let no_fks = ForeignKeyIndex::new();
        let no_tables = HashMap::new();
        let no_incoming = HashMap::new();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(color::BG)).show(ui, |ui| {
            let mut close = None;
            let mut activate = None;
            egui::Panel::top("tabstrip")
                .frame(
                    egui::Frame::new()
                        .fill(color::BG_SUBTLE)
                        .inner_margin(egui::Margin { left: 8, right: 8, top: 6, bottom: 0 })
                        .stroke(Stroke::new(1.0, color::BORDER)),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (i, c) in self.tabs.iter().enumerate() {
                            match tab_button(ui, c, i == self.active_tab) {
                                TabClick::Activate => activate = Some(i),
                                TabClick::Close => close = Some(i),
                                TabClick::None => {}
                            }
                        }
                        if let Some(source) = self.current_source.clone()
                            && ui.add(theme::flat_button(icon::PLUS)).on_hover_text("New console (Cmd+T)").clicked()
                        {
                            actions.push(Action::NewConsole(source));
                        }
                    });
                });
            if let Some(i) = activate {
                self.activate(i);
            }
            if let Some(i) = close {
                self.request_close_tab(i);
                return;
            }
            let Some(c) = self.tabs.get_mut(self.active_tab) else {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new("Pick a table in the sidebar, press Cmd+K to find one, or + for a console.")
                            .color(color::TEXT_WEAK),
                    );
                });
                return;
            };
            let history = self.history.get(&c.source).map(Vec::as_slice).unwrap_or_default();
            let tables = self.table_cache.get(&c.source).unwrap_or(&no_tables);
            let catalog = crate::ui::completion::Catalog {
                tables: &catalog_tables,
                details: tables,
                default_schema: &default_schema,
            };
            let cx = ConsoleContext {
                history,
                fks: self.fk_cache.get(&c.source).unwrap_or(&no_fks),
                tables,
                incoming: self.incoming_cache.get(&c.source).unwrap_or(&no_incoming),
                catalog: &catalog,
            };
            if let Some(a) = c.show(ui, &cx) {
                console_actions.push((c.id, a));
            }
        });
        if close_active {
            self.request_close_tab(self.active_tab);
        }

        // Metadata the consoles asked for while drawing.
        let mut wanted = Vec::new();
        let mut ddl = Vec::new();
        for c in &mut self.tabs {
            let (source, id) = (c.source.clone(), c.id);
            wanted.extend(std::mem::take(&mut c.wanted_details).into_iter().map(|(s, t)| (source.clone(), id, s, t)));
            if std::mem::take(&mut c.wanted_ddl) {
                ddl.push(id);
            }
        }
        for (source, console, schema, table) in wanted {
            self.request_details(&source, console, schema, table);
        }
        for console in ddl {
            self.request_ddl(console);
        }
        for (console, action) in console_actions {
            self.apply_console(console, action);
        }

        self.confirm_ui(&ui.ctx().clone());
        self.goto_ui(&ui.ctx().clone());

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

enum TabClick {
    None,
    Activate,
    Close,
}

/// One tab of the strip: icon, title, a dot while edits are pending or a
/// transaction is open, and a close button on the active or hovered tab.
fn tab_button(ui: &mut egui::Ui, c: &Console, active: bool) -> TabClick {
    let glyph = if c.table.is_some() { icon::TABLE } else { icon::TERMINAL_WINDOW };
    let mut click = TabClick::None;
    let fill = if active { color::BG } else { Color32::TRANSPARENT };
    let stroke = if active { Stroke::new(1.0, color::BORDER) } else { Stroke::NONE };
    let frame = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(egui::CornerRadius { nw: 7, ne: 7, sw: 0, se: 0 })
        .inner_margin(egui::Margin { left: 10, right: 6, top: 6, bottom: 7 })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let tint = if active { color::ACCENT_TEXT } else { color::TEXT_WEAK };
            ui.label(RichText::new(glyph).color(tint));
            let title = RichText::new(&c.title);
            ui.label(if active {
                title.font(theme::font(13.0, theme::medium()))
            } else {
                title.color(color::TEXT_WEAK)
            });
            if c.file_dirty() && !c.has_pending_edits() && !c.in_transaction {
                ui.label(RichText::new("●").size(8.0).color(color::TEXT_WEAK))
                    .on_hover_text("Unsaved changes to the file");
            }
            if c.has_pending_edits() || c.in_transaction {
                let dot = if c.in_transaction { color::WARNING } else { color::CHANGED_EDGE };
                ui.label(RichText::new("●").size(8.0).color(dot)).on_hover_text(if c.in_transaction {
                    "Transaction open"
                } else {
                    "Unsaved changes"
                });
            }
            // Plain label: the tab's single click area below decides between
            // closing and activating by where the click landed.
            ui.label(RichText::new(icon::X).size(11.0).color(color::TEXT_FAINT)).rect
        });
    let x_rect = frame.inner.expand(4.0);
    let response = frame.response.interact(egui::Sense::click());
    let on_x = response.hover_pos().is_some_and(|p| x_rect.contains(p));
    if on_x {
        ui.painter().rect_filled(x_rect, 4.0, Color32::from_black_alpha(14));
    }
    if response.clicked() {
        click = if on_x { TabClick::Close } else { TabClick::Activate };
    }
    click
}

impl App {
    fn update_window_title(&mut self, ctx: &egui::Context) {
        let title = match self.active() {
            Some(c) => match &c.table {
                Some(tv) => {
                    let mode = match tv.mode {
                        ViewMode::Content => "Content",
                        ViewMode::Structure => "Structure",
                        ViewMode::Sql => "SQL",
                    };
                    format!("Table {} — {mode}", tv.table)
                }
                None => {
                    let source = self.source(&c.source).map(|s| s.name.clone()).unwrap_or_default();
                    format!("{} — {source}", c.title)
                }
            },
            None => "Database Manager".into(),
        };
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
    }

    fn top_bar(
        &mut self,
        ui: &mut egui::Ui,
        actions: &mut Vec<Action>,
        console_actions: &mut Vec<(u64, ConsoleAction)>,
    ) {
        let bar = ui.max_rect();
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            self.source_picker(ui, actions);
            self.breadcrumb(ui, actions);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.commit_menu(ui, console_actions);
                let ctx = ui.ctx().clone();
                let search = egui::Button::new(
                    RichText::new(format!("{}   Go to table…", icon::MAGNIFYING_GLASS)).color(color::TEXT_WEAK),
                )
                .shortcut_text(RichText::new(ctx.format_shortcut(&GOTO_TABLE)).small().color(color::TEXT_FAINT))
                .min_size(egui::vec2(230.0, 28.0))
                .fill(color::BG);
                if ui.add(search).clicked() {
                    self.goto = Some(GotoTable::default());
                }
            });
        });

        // Content / Structure / SQL for table tabs, centred in the bar.
        if let Some(c) = self.tabs.get_mut(self.active_tab)
            && let Some(tv) = &mut c.table
        {
            let size = egui::vec2(250.0, 28.0);
            let rect = egui::Rect::from_center_size(bar.center(), size);
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                segmented(
                    ui,
                    &mut tv.mode,
                    &[(ViewMode::Content, "Content"), (ViewMode::Structure, "Structure"), (ViewMode::Sql, "SQL")],
                );
            });
        }
    }

    fn source_picker(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let current = self.current_source.as_deref().and_then(|id| self.source(id));
        let mut job = egui::text::LayoutJob::default();
        match current {
            Some(s) => {
                let status = self.trees.get(&s.id).map_or(&ConnStatus::Disconnected, |t| &t.status);
                job.append(
                    "●  ",
                    0.0,
                    egui::TextFormat {
                        color: explorer::status_color(status),
                        font_id: theme::font(9.0, egui::FontFamily::Proportional),
                        valign: egui::Align::Center,
                        ..Default::default()
                    },
                );
                job.append(
                    &s.name,
                    0.0,
                    egui::TextFormat {
                        font_id: theme::font(13.0, theme::semibold()),
                        color: color::TEXT,
                        valign: egui::Align::Center,
                        ..Default::default()
                    },
                );
                job.append(
                    &format!("  {}  {}", explorer::source_description(s), icon::CARET_DOWN),
                    0.0,
                    egui::TextFormat {
                        font_id: theme::font(12.0, egui::FontFamily::Proportional),
                        color: color::TEXT_WEAK,
                        valign: egui::Align::Center,
                        ..Default::default()
                    },
                );
            }
            None => job.append(
                &format!("Choose a connection  {}", icon::CARET_DOWN),
                0.0,
                egui::TextFormat { color: color::TEXT_WEAK, ..Default::default() },
            ),
        }
        let sources: Vec<(String, String)> = self.sources.iter().map(|s| (s.id.clone(), s.name.clone())).collect();
        ui.menu_button(egui::WidgetText::from(job), |ui| {
            for (id, name) in sources {
                let selected = self.current_source.as_deref() == Some(id.as_str());
                if ui.selectable_label(selected, name).clicked() {
                    actions.push(Action::SelectSource(id));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button(format!("{}  New connection…", icon::PLUS)).clicked() {
                actions.push(Action::NewSource);
                ui.close();
            }
        });
    }

    fn breadcrumb(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(source) = self.current_source.clone() else { return };
        let Some(SourceTree { schemas: Loadable::Loaded(schemas), .. }) = self.trees.get(&source) else { return };
        let names: Vec<String> = schemas.iter().map(|s| s.name.clone()).collect();
        let schema = self.schema_choice.get(&source).cloned().or_else(|| names.first().cloned()).unwrap_or_default();
        ui.add_space(6.0);
        if names.len() > 1 {
            ui.menu_button(RichText::new(format!("{schema} {}", icon::CARET_DOWN)).color(color::TEXT_WEAK), |ui| {
                for name in names {
                    if ui.selectable_label(name == schema, &name).clicked() {
                        actions.push(Action::SelectSchema(source.clone(), name));
                        ui.close();
                    }
                }
            });
        } else {
            ui.label(RichText::new(&schema).color(color::TEXT_WEAK));
        }
        if let Some(tv) = self.active().filter(|c| c.source == source).and_then(|c| c.table.as_ref()) {
            ui.label(RichText::new(icon::CARET_RIGHT).size(11.0).color(color::TEXT_FAINT));
            ui.label(RichText::new(&tv.table).font(theme::font(13.0, theme::semibold())));
        }
    }

    /// Auto-commit / manual transaction mode of the active console, with
    /// Commit and Rollback while a transaction is open.
    fn commit_menu(&mut self, ui: &mut egui::Ui, console_actions: &mut Vec<(u64, ConsoleAction)>) {
        let Some(c) = self.tabs.get_mut(self.active_tab) else { return };
        let (label, dot) = if c.in_transaction {
            ("Transaction open", color::WARNING)
        } else if c.tx_mode == TxMode::Manual {
            ("Manual commit", color::WARNING)
        } else {
            ("Auto-commit", color::SUCCESS)
        };
        let mut job = egui::text::LayoutJob::default();
        job.append(
            "●  ",
            0.0,
            egui::TextFormat {
                color: dot,
                font_id: theme::font(9.0, egui::FontFamily::Proportional),
                valign: egui::Align::Center,
                ..Default::default()
            },
        );
        job.append(
            &format!("{label}  {}", icon::CARET_DOWN),
            0.0,
            egui::TextFormat { color: color::TEXT, valign: egui::Align::Center, ..Default::default() },
        );
        let can_end = c.in_transaction && c.run.is_none() && !c.tx_busy;
        let id = c.id;
        ui.menu_button(egui::WidgetText::from(job), |ui| {
            if ui
                .radio(c.tx_mode == TxMode::Auto, "Auto-commit")
                .on_hover_text("Each statement commits on its own")
                .clicked()
            {
                c.tx_mode = TxMode::Auto;
            }
            if ui
                .radio(c.tx_mode == TxMode::Manual, "Manual commit")
                .on_hover_text("Statements run in a transaction until you commit or roll back")
                .clicked()
            {
                c.tx_mode = TxMode::Manual;
            }
            ui.separator();
            if ui.add_enabled(can_end, egui::Button::new("Commit")).clicked() {
                console_actions.push((id, ConsoleAction::Commit));
                ui.close();
            }
            if ui.add_enabled(can_end, egui::Button::new("Rollback")).clicked() {
                console_actions.push((id, ConsoleAction::Rollback));
                ui.close();
            }
        });
    }

    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        // What would be lost: unsaved grid edits and/or open transactions.
        let losses = |consoles: Vec<&Console>| {
            let edits = consoles.iter().any(|c| c.has_pending_edits());
            let tx = consoles.iter().any(|c| c.in_transaction);
            let file = consoles.iter().any(|c| c.file_dirty());
            let mut parts = Vec::new();
            if edits {
                parts.push("unsaved grid edits are discarded");
            }
            if tx {
                parts.push("the open transaction is rolled back");
            }
            if file {
                parts.push("unsaved changes to the file are lost");
            }
            parts.join(" and ")
        };
        let (message, proceed) = match confirm {
            Confirm::CloseConsole(id) => (
                format!("If you close this tab, {}.", losses(self.tabs.iter().filter(|c| c.id == *id).collect())),
                "Close anyway",
            ),
            Confirm::Disconnect(_) => (
                "A console on this data source has an open transaction. Disconnecting rolls it back.".to_string(),
                "Roll back and disconnect",
            ),
            Confirm::Quit => (format!("If you quit, {}.", losses(self.tabs.iter().collect())), "Quit anyway"),
        };
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Unsaved work");
            ui.add_space(6.0);
            ui.label(message);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(theme::primary_button(proceed)).clicked() {
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
                if let Some(i) = self.tabs.iter().position(|c| c.id == id) {
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

/// A segmented control: a sunken track with the selected option raised.
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
                        .min_size(egui::vec2(74.0, 22.0));
                if ui.add(button).clicked() {
                    *value = *option;
                }
            }
        });
    });
}

/// Whether a statement changes the schema (CREATE, ALTER, DROP, …).
fn is_ddl(sql: &str) -> bool {
    let mut rest = sql.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    let word: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase();
    matches!(word.as_str(), "CREATE" | "ALTER" | "DROP" | "RENAME" | "COMMENT")
}
