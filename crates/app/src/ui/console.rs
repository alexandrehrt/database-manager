//! A query console: SQL editor on top, results below with a status bar and
//! a row panel. Consoles opened on a table's data also have Structure and
//! SQL (DDL) views.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use dbm_core::driver::is_read_only_query;
use dbm_core::fk_nav::{FkLink, ForeignKeyIndex, link_for_cell, referencing_query, referencing_values};
use dbm_core::statement_at::statement_at;
use dbm_core::{DbError, Dialect, ExecOutcome, IncomingKey, ResultSet, TableDetails, Value, sql_split};
use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText, Stroke};

use crate::ui::completion::{self, Catalog, Item};
use crate::ui::edits::{self, EditStatement, EditTarget, Edits, RowRef};
use crate::ui::grid::{self, ColumnMeta, EditingCell, GridEdit, GridEvent, GridOptions, Selection, SortState};
use crate::ui::theme::{self, color, icon};
use crate::ui::{editor_ops, sql_format, sql_highlight};

pub const PAGE_SIZE: usize = 500;

const RUN_STATEMENT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
const RUN_ALL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Enter);
const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const REFRESH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);
pub const SAVE_AS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S);
pub const OPEN_FILE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
const DUPLICATE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::D);
const MOVE_UP: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowUp);
const MOVE_DOWN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowDown);
const FIND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
const COMMENT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Slash);
const FORMAT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::ALT), Key::L);

/// SQL plus its bound parameters.
pub type Statement = (String, Vec<Value>);

pub struct ResultTab {
    /// Label for tabs opened by FK navigation; others are numbered.
    pub title: Option<String>,
    pub sql: String,
    pub params: Vec<Value>,
    pub outcome: Result<ExecOutcome, DbError>,
    pub elapsed: Duration,
    /// Row limit the statement ran with; "load more" re-runs it with a larger one.
    pub limit: usize,
    pub sort: SortState,
    pub selection: Selection,
    pub edits: Edits,
    pub editing: Option<EditingCell>,
    /// A submit is in flight.
    pub submitting: bool,
    /// Why the last submit failed.
    pub edit_error: Option<String>,
}

impl ResultTab {
    pub fn new(title: Option<String>, sql: String, params: Vec<Value>, outcome: Result<ExecOutcome, DbError>) -> Self {
        Self {
            title,
            sql,
            params,
            outcome,
            elapsed: Duration::default(),
            limit: PAGE_SIZE,
            sort: SortState::default(),
            selection: Selection::default(),
            edits: Edits::default(),
            editing: None,
            submitting: false,
            edit_error: None,
        }
    }
}

pub struct Run {
    pub id: u64,
    pub stop: Arc<AtomicBool>,
    pub started: Instant,
    pub total: usize,
    pub done: usize,
    pub mode: RunMode,
    pub params: Vec<Vec<Value>>,
    /// Row limit of this run, kept by the tab it fills.
    pub limit: usize,
}

#[derive(Clone)]
pub enum RunMode {
    /// Replace all results.
    Fresh,
    /// Re-execute one existing tab ("load more").
    Replace(usize),
    /// FK navigation from tab `from`: add a tab and make it current.
    Navigate { from: usize, title: String },
    /// Re-query table data with a new filter/sort: replaces the first tab on
    /// success, keeps it and reports the error on failure.
    Filter,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TxMode {
    /// Every statement commits on its own unless the user runs BEGIN.
    Auto,
    /// The first statement opens a transaction that stays open until Commit or Rollback.
    Manual,
}

/// What a table tab shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Content,
    Structure,
    Sql,
}

/// Filters and sort applied server-side to a console opened on a table's data.
pub struct TableView {
    pub schema: String,
    pub table: String,
    /// WHERE conditions, combined with AND; each is a chip.
    pub filters: Vec<String>,
    pub order: String,
    /// Column sorted by a header click and its direction.
    pub sort: Option<(usize, bool)>,
    /// Error of the last filter run; the previous rows stay visible.
    pub error: Option<String>,
    pub mode: ViewMode,
    /// The table's DDL once loaded.
    pub ddl: Option<Result<String, String>>,
    /// Text of the filter being typed, while the "+ Filter" field is open.
    new_filter: Option<String>,
    focus_filter: bool,
}

impl TableView {
    pub fn new(schema: String, table: String, filters: Vec<String>, order: String) -> Self {
        Self {
            schema,
            table,
            filters,
            order,
            sort: None,
            error: None,
            mode: ViewMode::Content,
            ddl: None,
            new_filter: None,
            focus_filter: false,
        }
    }

    /// Clauses go on their own lines so a `--` comment in one can't swallow the next.
    pub fn sql(&self, dialect: Dialect) -> String {
        let mut sql = dialect.select_all(&self.schema, &self.table);
        let conditions: Vec<&str> = self.filters.iter().map(|f| f.trim()).filter(|f| !f.is_empty()).collect();
        if !conditions.is_empty() {
            sql.push_str(&format!("\nWHERE {}", conditions.join("\n  AND ")));
        }
        if !self.order.trim().is_empty() {
            sql.push_str(&format!("\nORDER BY {}", self.order.trim()));
        }
        sql
    }
}

/// Editor text and table filters / order / sort, captured before a frame's
/// action so a cancelled discard can restore them.
type Snapshot = (String, Option<(Vec<String>, String, Option<(usize, bool)>)>);

/// An action that would throw away unsaved grid edits, held until the user
/// confirms. Cancelling restores what the frame changed on the way.
struct Guard {
    action: Option<ConsoleAction>,
    /// Result tab to close once confirmed.
    close_result: Option<usize>,
    /// Result tabs whose edits are discarded.
    tabs: Vec<usize>,
    /// Restored on cancel.
    restore: Snapshot,
}

/// The open completion list.
pub struct CompletionPopup {
    items: Vec<Item>,
    selected: usize,
    /// Byte offset of the word being completed.
    start: usize,
    anchor: egui::Pos2,
}

/// The editor's find / replace bar.
#[derive(Default)]
struct FindBar {
    query: String,
    replacement: String,
    case_sensitive: bool,
    /// Index of the current match.
    current: usize,
    /// Focus the query field next frame.
    focus: bool,
    /// Scroll the editor to the current match and select it.
    reveal: bool,
}

/// A run waiting for parameter values.
struct ParamPrompt {
    action: ConsoleAction,
    names: Vec<String>,
    focus: bool,
}

/// A parameter's value as typed, and whether it is an SQL expression rather
/// than a value to quote.
#[derive(Clone, Default)]
struct ParamValue {
    text: String,
    expression: bool,
}

impl ParamValue {
    /// SQL text for the value: numbers, NULL, booleans and already-quoted text
    /// as typed, anything else as a string literal.
    fn sql(&self, dialect: Dialect) -> String {
        let t = self.text.trim();
        let number = !t.is_empty()
            && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
            && t.parse::<f64>().is_ok();
        let quoted = t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'');
        if self.expression || number || quoted {
            t.to_string()
        } else if ["null", "true", "false"].iter().any(|k| t.eq_ignore_ascii_case(k)) {
            t.to_ascii_uppercase()
        } else {
            dbm_core::export::sql_literal(dialect, &Value::Text(self.text.clone()))
        }
    }
}

/// Field buffers of the row panel, for the row they were filled from.
struct RowPanel {
    tab: usize,
    row: usize,
    buffers: Vec<String>,
}

pub struct Console {
    pub id: u64,
    pub source: String,
    pub title: String,
    pub dialect: Dialect,
    pub sql: String,
    pub results: Vec<ResultTab>,
    pub active_result: usize,
    pub run: Option<Run>,
    /// Result tabs to return to with Back / Forward.
    pub nav_back: Vec<usize>,
    pub nav_forward: Vec<usize>,
    pub tx_mode: TxMode,
    /// Last known state of this console's connection.
    pub in_transaction: bool,
    /// A Commit / Rollback is in flight.
    pub tx_busy: bool,
    /// Outcome of the last Commit / Rollback, shown in the status bar.
    pub tx_notice: Option<Result<String, String>>,
    /// Set for consoles opened on a table's data.
    pub table: Option<TableView>,
    pub completion: Option<CompletionPopup>,
    /// Recompute completion once metadata arrives (columns were missing).
    completion_waiting: bool,
    /// Tables whose details are needed; the app loads and drains these.
    pub wanted_details: Vec<(String, String)>,
    /// The SQL view needs the table's DDL; the app loads it.
    pub wanted_ddl: bool,
    /// The `.sql` file this console is bound to, and its text when last loaded or saved.
    pub file: Option<std::path::PathBuf>,
    pub saved_text: Option<String>,
    /// Cmd+S this frame: saves grid edits if there are any, else the file.
    save_requested: bool,
    find: Option<FindBar>,
    /// A run held for confirmation: UPDATE / DELETE statements without WHERE.
    unrestricted: Option<(ConsoleAction, Vec<(String, &'static str)>)>,
    param_prompt: Option<ParamPrompt>,
    /// Last values typed for each parameter name.
    param_values: HashMap<String, ParamValue>,
    /// Put the editor cursor at this byte offset next frame (an error position).
    jump_to: Option<usize>,
    /// Scroll the editor to this byte offset once it is drawn.
    reveal_byte: Option<usize>,
    /// Closing halves auto-inserted while typing, innermost last, as (closer,
    /// distance from the end of the text). Typing inside the pair doesn't move
    /// that distance, so it locates the closer without tracking every edit.
    auto_closers: Vec<(char, usize)>,
    editor_collapsed: bool,
    panel_open: bool,
    panel: Option<RowPanel>,
    /// Column shown in the row panel's value viewer, instead of all fields.
    value_col: Option<usize>,
    /// Text of the value viewer, for the (tab, row, column) it was filled from.
    value_buffer: Option<((usize, usize, usize), String)>,
    /// The statements "View SQL" shows.
    view_sql: Option<String>,
    /// An action waiting for the user to discard pending grid edits.
    guard: Option<Guard>,
    /// Rows of the statement under the cursor last frame, for its highlight.
    statement_rect: Option<egui::Rect>,
}

pub enum ConsoleAction {
    Run {
        statements: Vec<Statement>,
        limit: usize,
        mode: RunMode,
    },
    Cancel,
    Commit,
    Rollback,
    /// Ask the app to open a `.sql` file in a new console.
    OpenFile,
    /// Apply the pending edits of result tab `tab`.
    SubmitEdits {
        tab: usize,
        statements: Vec<EditStatement>,
    },
}

/// What a console needs from the app to draw its results.
pub struct ConsoleContext<'a> {
    pub history: &'a [String],
    pub fks: &'a ForeignKeyIndex,
    pub tables: &'a HashMap<(String, String), TableDetails>,
    pub incoming: &'a HashMap<(String, String), Vec<IncomingKey>>,
    pub catalog: &'a Catalog<'a>,
    /// The data source is read-only: writing statements and grid edits are blocked.
    pub read_only: bool,
}

impl Console {
    pub fn new(id: u64, source: String, title: String, dialect: Dialect, sql: String) -> Self {
        Self {
            id,
            source,
            title,
            dialect,
            sql,
            results: Vec::new(),
            active_result: 0,
            run: None,
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
            tx_mode: TxMode::Auto,
            in_transaction: false,
            tx_busy: false,
            tx_notice: None,
            table: None,
            completion: None,
            completion_waiting: false,
            wanted_details: Vec::new(),
            wanted_ddl: false,
            file: None,
            saved_text: None,
            save_requested: false,
            find: None,
            unrestricted: None,
            param_prompt: None,
            param_values: HashMap::new(),
            jump_to: None,
            reveal_byte: None,
            auto_closers: Vec::new(),
            editor_collapsed: false,
            panel_open: true,
            panel: None,
            value_col: None,
            value_buffer: None,
            view_sql: None,
            guard: None,
            statement_rect: None,
        }
    }

    /// The editor has changes not yet written to its `.sql` file.
    pub fn file_dirty(&self) -> bool {
        self.file.is_some() && self.saved_text.as_deref() != Some(self.sql.as_str())
    }

    /// Writes the editor to its file, asking for a path when there is none
    /// (or for Save As). The outcome shows in the status bar.
    fn save_file(&mut self, ask_path: bool) {
        let path = match (&self.file, ask_path) {
            (Some(p), false) => p.clone(),
            _ => {
                let name = format!("{}.sql", self.title.trim_end_matches(".sql"));
                let mut dialog = rfd::FileDialog::new().add_filter("SQL", &["sql"]).set_file_name(&name);
                if let Some(dir) = self.file.as_ref().and_then(|p| p.parent()) {
                    dialog = dialog.set_directory(dir);
                }
                match dialog.save_file() {
                    Some(p) => p,
                    None => return,
                }
            }
        };
        match std::fs::write(&path, &self.sql) {
            Ok(()) => {
                self.title = path.file_name().map_or_else(|| self.title.clone(), |n| n.to_string_lossy().into_owned());
                self.tx_notice = Some(Ok(format!("Saved {}", path.display())));
                self.saved_text = Some(self.sql.clone());
                self.file = Some(path);
            }
            Err(e) => self.tx_notice = Some(Err(format!("Could not save {}: {e}", path.display()))),
        }
    }

    /// Where result `idx`'s error is in the editor: (byte offset, line, column),
    /// 1-based line and column. None without a position or if the statement
    /// isn't in the editor text any more.
    fn error_location(&self, idx: usize) -> Option<(usize, usize, usize)> {
        let tab = self.results.get(idx)?;
        let pos = tab.outcome.as_ref().err()?.position?;
        let start = self.sql.find(tab.sql.as_str())?;
        let offset = tab.sql.char_indices().nth(pos).map_or(tab.sql.len(), |(b, _)| b);
        let at = start + offset;
        let before = &self.sql[..at];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        Some((at, line, column))
    }

    /// Moves the editor cursor to result `idx`'s error, if it has a position.
    pub fn reveal_error(&mut self, idx: usize) {
        if let Some((at, _, _)) = self.error_location(idx) {
            self.jump_to = Some(at);
            self.editor_collapsed = false;
        }
    }

    /// Whether any result tab has unsaved grid edits.
    pub fn has_pending_edits(&self) -> bool {
        self.results.iter().any(|t| t.edits.row_count() > 0)
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &ConsoleContext<'_>) -> Option<ConsoleAction> {
        let before = (self.sql.clone(), self.table.as_ref().map(|t| (t.filters.clone(), t.order.clone(), t.sort)));
        // Save As first: Cmd+S also matches Shift+Cmd+S logically.
        let (save_as, save) = ui.input_mut(|i| (i.consume_shortcut(&SAVE_AS), i.consume_shortcut(&SAVE)));
        self.save_requested = save;
        let action = self.show_inner(ui, cx);
        let action = self.hold_params(action).or_else(|| self.params_modal(ui));
        let action = self.block_writes(action, cx.read_only);
        let action = self.hold_unrestricted(action).or_else(|| self.unrestricted_modal(ui));
        // Cmd+S not taken by pending grid edits saves the file.
        if std::mem::take(&mut self.save_requested) {
            self.save_file(false);
        }
        if save_as {
            self.save_file(true);
        }
        let action = self.guarded(action, before);
        self.guard_modal(ui).or(action)
    }

    fn show_inner(&mut self, ui: &mut egui::Ui, cx: &ConsoleContext<'_>) -> Option<ConsoleAction> {
        match self.table.as_ref().map(|t| t.mode) {
            Some(ViewMode::Structure) => {
                self.structure_view(ui, cx);
                return None;
            }
            Some(ViewMode::Sql) => {
                self.ddl_view(ui);
                return None;
            }
            _ => {}
        }
        let mut action = self.editor_panel(ui, cx);
        if self.results.is_empty() {
            egui::CentralPanel::default().frame(egui::Frame::new().fill(color::BG)).show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    let text = if self.run.is_some() { "Running…" } else { "Results appear here." };
                    ui.label(RichText::new(text).color(color::TEXT_WEAK));
                });
            });
            if let Some(a) = self.status_bar_when_empty(ui) {
                action = Some(a);
            }
            return action;
        }
        if let Some(a) = self.results_area(ui, cx) {
            action = Some(a);
        }
        self.view_sql_modal(ui);
        action
    }

    // ----- editor -------------------------------------------------------

    fn editor_panel(&mut self, ui: &mut egui::Ui, cx: &ConsoleContext<'_>) -> Option<ConsoleAction> {
        let mut action = None;
        let editor_id = egui::Id::new(("console-editor", self.id));
        let editor_focused = ui.memory(|m| m.has_focus(editor_id));
        if !editor_focused {
            self.completion = None;
        }
        // Completion keys, taken before the editor would act on them.
        let manual = editor_focused && ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Space));
        let (down, up, accept, dismiss) = if self.completion.is_some() {
            ui.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab),
                    i.consume_key(Modifiers::NONE, Key::Escape),
                )
            })
        } else {
            (false, false, false, false)
        };
        // Consume the shortcuts before the editor sees them, or Enter would insert a newline.
        let (run_one, run_all) = if editor_focused {
            // RUN_ALL first: Cmd+Enter also matches Cmd+Shift+Enter logically.
            ui.input_mut(|i| {
                let all = i.consume_shortcut(&RUN_ALL);
                (i.consume_shortcut(&RUN_STATEMENT), all)
            })
        } else {
            (false, false)
        };
        if ui.input_mut(|i| i.consume_shortcut(&FIND)) {
            self.open_find(ui, editor_id);
        }
        if editor_focused {
            self.editor_keys(ui, editor_id);
        }
        if editor_focused {
            let (comment, format) = ui.input_mut(|i| (i.consume_shortcut(&COMMENT), i.consume_shortcut(&FORMAT)));
            if comment {
                self.toggle_comment(ui, editor_id);
            }
            if format {
                self.format_sql(ui, editor_id);
            }
        }
        let matches =
            self.find.as_ref().map(|f| sql_format::find_all(&self.sql, &f.query, f.case_sensitive)).unwrap_or_default();
        if let Some(f) = &mut self.find {
            f.current = f.current.min(matches.len().saturating_sub(1));
        }
        let current_match = self.find.as_ref().filter(|_| !matches.is_empty()).map(|f| f.current);
        if let Some(at) = self.jump_to.take().filter(|&at| at <= self.sql.len() && self.sql.is_char_boundary(at)) {
            self.select_in_editor(ui.ctx(), editor_id, at..at);
            self.reveal_byte = Some(at);
        }
        let mut editor_out = None;
        let mut marker_run = None;

        let panel = egui::Panel::top(egui::Id::new(("console-top", self.id, self.editor_collapsed)))
            .frame(egui::Frame::new().fill(color::BG).inner_margin(egui::Margin::symmetric(10, 8)));
        let panel = if self.editor_collapsed {
            panel.resizable(false)
        } else {
            panel.resizable(true).default_size(190.0).min_size(90.0)
        };
        panel.show(ui, |ui| {
            ui.horizontal(|ui| {
                let running = self.run.is_some();
                let ctx = ui.ctx().clone();
                let run =
                    theme::primary_button(format!("{}  Run   {}", icon::PLAY, ctx.format_shortcut(&RUN_STATEMENT)));
                if ui.add_enabled(!running, run).on_hover_text("Statement at the cursor, or the selection").clicked() {
                    action = self.run_at_cursor(ui, editor_id);
                }
                let all = egui::Button::new(format!("Run all   {}", ctx.format_shortcut(&RUN_ALL)));
                if ui.add_enabled(!running, all).clicked() {
                    action = self.run_all();
                }
                if running && ui.button(format!("{}  Cancel", icon::STOP)).clicked() {
                    action = Some(ConsoleAction::Cancel);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let chevron = if self.editor_collapsed { icon::CARET_DOWN } else { icon::CARET_UP };
                    if ui
                        .add(theme::flat_button(chevron))
                        .on_hover_text(if self.editor_collapsed { "Show editor" } else { "Hide editor" })
                        .clicked()
                    {
                        self.editor_collapsed = !self.editor_collapsed;
                    }
                    ui.menu_button(format!("{}  File", icon::FILE_TEXT), |ui| {
                        let ctx = ui.ctx().clone();
                        if ui.add(egui::Button::new("Open…").shortcut_text(ctx.format_shortcut(&OPEN_FILE))).clicked()
                        {
                            action = Some(ConsoleAction::OpenFile);
                            ui.close();
                        }
                        if ui.add(egui::Button::new("Save").shortcut_text(ctx.format_shortcut(&SAVE))).clicked() {
                            self.save_file(false);
                            ui.close();
                        }
                        if ui.add(egui::Button::new("Save As…").shortcut_text(ctx.format_shortcut(&SAVE_AS))).clicked()
                        {
                            self.save_file(true);
                            ui.close();
                        }
                        if let Some(path) = &self.file {
                            ui.separator();
                            ui.label(RichText::new(path.display().to_string()).small().color(color::TEXT_WEAK));
                        }
                    });
                    ui.menu_button(format!("{}  Edit", icon::PENCIL_SIMPLE), |ui| {
                        let ctx = ui.ctx().clone();
                        if ui
                            .add(egui::Button::new("Find / Replace").shortcut_text(ctx.format_shortcut(&FIND)))
                            .clicked()
                        {
                            self.open_find(ui, editor_id);
                            ui.close();
                        }
                        if ui
                            .add(egui::Button::new("Toggle comment").shortcut_text(ctx.format_shortcut(&COMMENT)))
                            .clicked()
                        {
                            self.toggle_comment(ui, editor_id);
                            ui.close();
                        }
                        if ui.add(egui::Button::new("Format SQL").shortcut_text(ctx.format_shortcut(&FORMAT))).clicked()
                        {
                            self.format_sql(ui, editor_id);
                            ui.close();
                        }
                    });
                    ui.menu_button(format!("{}  History", icon::CLOCK_COUNTER_CLOCKWISE), |ui| {
                        if cx.history.is_empty() {
                            ui.weak("No queries yet");
                        }
                        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                            for q in cx.history {
                                let label: String = q.split_whitespace().collect::<Vec<_>>().join(" ");
                                let label: String = label.chars().take(80).collect();
                                if ui.button(label).on_hover_text(q).clicked() {
                                    if !self.sql.is_empty() && !self.sql.ends_with('\n') {
                                        self.sql.push('\n');
                                    }
                                    self.sql.push_str(q);
                                    self.sql.push_str(";\n");
                                    ui.close();
                                }
                            }
                        });
                    });
                });
            });
            if self.editor_collapsed {
                return;
            }
            ui.add_space(6.0);
            if self.find.is_some() {
                self.find_bar(ui, editor_id, &matches);
                ui.add_space(4.0);
            }
            let self_len = self.sql.len();
            let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
                let mut job = sql_highlight::layout(ui, text.as_str(), wrap_width);
                // Ranges are only valid for the text they were found in.
                if text.as_str().len() == self_len {
                    sql_highlight::mark(&mut job, &matches, current_match);
                }
                ui.fonts_mut(|f| f.layout_job(job))
            };
            egui::ScrollArea::vertical().id_salt(("editor-scroll", self.id)).auto_shrink([false, false]).show(
                ui,
                |ui| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let (gutter, _) = ui.allocate_exact_size(egui::vec2(42.0, 1.0), egui::Sense::hover());
                        if let Some(rect) = self.statement_rect {
                            let band =
                                egui::Rect::from_x_y_ranges(gutter.right()..=ui.max_rect().right(), rect.y_range());
                            ui.painter().rect_filled(band.expand2(egui::vec2(0.0, 1.0)), 0.0, color::ACCENT_SOFT);
                        }
                        let output = egui::TextEdit::multiline(&mut self.sql)
                            .id(editor_id)
                            .code_editor()
                            .frame(egui::Frame::NONE)
                            .lock_focus(true)
                            .desired_width(f32::INFINITY)
                            .min_size(ui.available_size())
                            .hint_text("Write SQL here. Cmd+Enter runs the statement under the cursor.")
                            .layouter(&mut layouter)
                            .show(ui);
                        let cursor = output.cursor_range.map(|r| r.primary.index.0);
                        if let Some(f) = &mut self.find
                            && std::mem::take(&mut f.reveal)
                            && let Some(m) = current_match.and_then(|i| matches.get(i))
                        {
                            let (a, b) = (self.sql[..m.start].chars().count(), self.sql[..m.end].chars().count());
                            let rect = output.galley.pos_from_cursor(egui::text::CCursor::new(a));
                            ui.scroll_to_rect(rect.translate(output.galley_pos.to_vec2()), Some(egui::Align::Center));
                            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), editor_id) {
                                let range = egui::text::CCursorRange::two(
                                    egui::text::CCursor::new(a),
                                    egui::text::CCursor::new(b),
                                );
                                state.cursor.set_char_range(Some(range));
                                state.store(ui.ctx(), editor_id);
                            }
                        }
                        if let Some(at) = self.reveal_byte.take().filter(|&at| at <= self.sql.len()) {
                            let c = self.sql[..at].chars().count();
                            let rect = output.galley.pos_from_cursor(egui::text::CCursor::new(c));
                            // Minimal scrolling: just enough to bring it into view.
                            ui.scroll_to_rect(rect.translate(output.galley_pos.to_vec2()), None);
                        }
                        marker_run = self.paint_gutter(ui, gutter, &output.galley, output.galley_pos, cursor);
                        editor_out = Some((
                            output.response.response.changed(),
                            cursor,
                            output.galley.clone(),
                            output.galley_pos,
                        ));
                    });
                },
            );
        });

        if let Some((changed, Some(cursor), galley, galley_pos)) = editor_out {
            self.update_completion(ui, editor_id, cx.catalog, (changed, cursor, &galley, galley_pos), manual);
            if let Some(popup) = &mut self.completion {
                let last = popup.items.len().saturating_sub(1);
                if down {
                    popup.selected = (popup.selected + 1).min(last);
                }
                if up {
                    popup.selected = popup.selected.saturating_sub(1);
                }
            }
            if dismiss {
                self.completion = None;
            }
            let clicked = self.completion_popup(ui);
            if let Some(i) = clicked.or_else(|| accept.then(|| self.completion.as_ref().map(|p| p.selected)).flatten())
            {
                self.accept_completion(ui, editor_id, cursor, i);
            }
        }
        if self.run.is_none() {
            if let Some(statement) = marker_run {
                action = run_fresh(vec![statement]);
            } else if run_one {
                action = self.run_at_cursor(ui, editor_id);
            } else if run_all {
                action = self.run_all();
            }
        }
        action
    }

    /// Line numbers, a ▸ marker at each statement start (click to run it) and
    /// the highlight of the statement under the cursor. Returns a statement
    /// whose marker was clicked.
    fn paint_gutter(
        &mut self,
        ui: &egui::Ui,
        gutter: egui::Rect,
        galley: &Arc<egui::Galley>,
        galley_pos: egui::Pos2,
        cursor_char: Option<usize>,
    ) -> Option<String> {
        let painter = ui.painter();
        let number_font = theme::mono(11.0);
        let mut line = 1;
        let mut new_line = true;
        for placed in &galley.rows {
            if new_line {
                let y = galley_pos.y + placed.rect().center().y;
                painter.text(
                    egui::pos2(gutter.right() - 8.0, y),
                    egui::Align2::RIGHT_CENTER,
                    line.to_string(),
                    number_font.clone(),
                    color::TEXT_FAINT,
                );
                line += 1;
            }
            new_line = placed.ends_with_newline;
        }

        let to_char = |byte: usize| self.sql[..byte.min(self.sql.len())].chars().count();
        let row_y = |char_idx: usize| {
            let r = galley.pos_from_cursor(egui::text::CCursor::new(char_idx));
            (galley_pos.y + r.top())..=(galley_pos.y + r.bottom())
        };
        let cursor_byte = cursor_char.map(|c| self.sql.char_indices().nth(c).map_or(self.sql.len(), |(b, _)| b));
        let current = cursor_byte.and_then(|b| statement_at(&self.sql, b, self.dialect));
        self.statement_rect = current.as_ref().map(|span| {
            let (a, b) = (row_y(to_char(span.start)), row_y(to_char(span.end)));
            egui::Rect::from_x_y_ranges(gutter.x_range(), *a.start()..=*b.end())
        });
        if let Some(rect) = self.statement_rect {
            let bar = egui::Rect::from_x_y_ranges(gutter.right() - 2.0..=gutter.right(), rect.y_range());
            painter.rect_filled(bar, 0.0, color::ACCENT);
        }

        let mut clicked = None;
        for span in sql_split::split(&self.sql, self.dialect) {
            let y = row_y(to_char(span.start));
            let center = egui::pos2(gutter.left() + 9.0, (*y.start() + *y.end()) / 2.0);
            let hit = egui::Rect::from_center_size(center, egui::vec2(16.0, 16.0));
            let response = ui.interact(hit, ui.id().with(("run-marker", span.start)), egui::Sense::click());
            let tint = if response.hovered() { color::ACCENT } else { color::SUCCESS };
            painter.text(
                center,
                egui::Align2::CENTER_CENTER,
                icon::PLAY,
                theme::font(9.0, egui::FontFamily::Proportional),
                tint,
            );
            if response.on_hover_text("Run this statement").clicked() {
                clicked = Some(self.sql[span].to_string());
            }
        }
        clicked
    }

    /// Opens, refreshes or closes the completion list after this frame's edit.
    fn update_completion(
        &mut self,
        ui: &egui::Ui,
        editor_id: egui::Id,
        catalog: &Catalog<'_>,
        (changed, cursor_char, galley, galley_pos): (bool, usize, &Arc<egui::Galley>, egui::Pos2),
        manual: bool,
    ) {
        if !ui.memory(|m| m.has_focus(editor_id)) {
            return;
        }
        let cursor = self.sql.char_indices().nth(cursor_char).map_or(self.sql.len(), |(b, _)| b);
        let typed_ident =
            changed && self.sql[..cursor].chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
        let open = self.completion.is_some();
        if !(manual || typed_ident || (open && changed) || self.completion_waiting) {
            // The cursor moved away from the word being completed.
            if let Some(p) = &self.completion
                && completion::context(&self.sql, cursor).start != p.start
            {
                self.completion = None;
            }
            return;
        }
        if changed && !typed_ident {
            self.completion = None;
            self.completion_waiting = false;
            return;
        }
        if completion::in_literal_or_comment(&self.sql, cursor) {
            self.completion = None;
            return;
        }
        let ctx = completion::context(&self.sql, cursor);
        let statement = statement_at(&self.sql, cursor, self.dialect).map_or("", |r| &self.sql[r]);
        let (items, missing) = completion::candidates(self.dialect, &ctx, statement, catalog);
        self.completion_waiting = items.is_empty() && !missing.is_empty();
        self.wanted_details.extend(missing);
        if items.is_empty() || (!manual && !open && ctx.prefix.is_empty() && ctx.qualifier.is_none()) {
            self.completion = None;
            return;
        }
        let at = galley.pos_from_cursor(egui::text::CCursor::new(cursor_char));
        let anchor = galley_pos + at.left_bottom().to_vec2() + egui::vec2(0.0, 2.0);
        let selected = self.completion.as_ref().filter(|p| p.start == ctx.start).map_or(0, |p| p.selected);
        let selected = selected.min(items.len() - 1);
        self.completion = Some(CompletionPopup { items, selected, start: ctx.start, anchor });
    }

    /// Draws the completion list; returns a clicked item.
    fn completion_popup(&self, ui: &egui::Ui) -> Option<usize> {
        let popup = self.completion.as_ref()?;
        let mut clicked = None;
        egui::Area::new(egui::Id::new(("completion", self.id)))
            .fixed_pos(popup.anchor)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(260.0);
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        for (i, item) in popup.items.iter().enumerate().take(200) {
                            let r = ui
                                .horizontal(|ui| {
                                    let r = ui
                                        .selectable_label(i == popup.selected, RichText::new(&item.label).monospace());
                                    ui.label(RichText::new(&item.detail).color(color::TEXT_WEAK));
                                    r
                                })
                                .inner;
                            if r.clicked() {
                                clicked = Some(i);
                            }
                            if i == popup.selected {
                                r.scroll_to_me(None);
                            }
                        }
                    });
                });
            });
        clicked
    }

    /// Replaces the word being completed with item `i` and puts the cursor after it.
    fn accept_completion(&mut self, ui: &egui::Ui, editor_id: egui::Id, cursor_char: usize, i: usize) {
        let Some(popup) = self.completion.take() else { return };
        let Some(item) = popup.items.get(i) else { return };
        let cursor = self.sql.char_indices().nth(cursor_char).map_or(self.sql.len(), |(b, _)| b);
        let start = popup.start.min(cursor);
        self.sql.replace_range(start..cursor, &item.insert);
        let new_cursor = self.sql[..start].chars().count() + item.insert.chars().count();
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), editor_id) {
            let at = egui::text::CCursor::new(new_cursor);
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(at)));
            state.store(ui.ctx(), editor_id);
        }
        ui.memory_mut(|m| m.request_focus(editor_id));
        self.completion_waiting = false;
    }

    /// The editor's selection as byte offsets (start, end), if it has a cursor.
    fn editor_selection(&self, ctx: &egui::Context, editor_id: egui::Id) -> Option<(usize, usize)> {
        let r = egui::TextEdit::load_state(ctx, editor_id)?.cursor.char_range()?;
        let (a, b) = (r.primary.index.0.min(r.secondary.index.0), r.primary.index.0.max(r.secondary.index.0));
        let to_byte = |c: usize| self.sql.char_indices().nth(c).map_or(self.sql.len(), |(b, _)| b);
        Some((to_byte(a), to_byte(b)))
    }

    /// Selects the byte range `range` of the editor text and focuses it.
    fn select_in_editor(&self, ctx: &egui::Context, editor_id: egui::Id, range: std::ops::Range<usize>) {
        let (a, b) = (self.sql[..range.start].chars().count(), self.sql[..range.end].chars().count());
        let mut state = egui::TextEdit::load_state(ctx, editor_id).unwrap_or_default();
        let range = egui::text::CCursorRange::two(egui::text::CCursor::new(a), egui::text::CCursor::new(b));
        state.cursor.set_char_range(Some(range));
        state.store(ctx, editor_id);
        ctx.memory_mut(|m| m.request_focus(editor_id));
    }

    fn open_find(&mut self, ui: &egui::Ui, editor_id: egui::Id) {
        // A single-line selection becomes the search text.
        let selected = self
            .editor_selection(ui.ctx(), editor_id)
            .filter(|(a, b)| a < b)
            .map(|(a, b)| self.sql[a..b].to_string())
            .filter(|t| !t.contains('\n'));
        let find = self.find.get_or_insert_with(FindBar::default);
        if let Some(text) = selected {
            find.query = text;
            find.current = 0;
        }
        find.focus = true;
        self.editor_collapsed = false;
    }

    fn find_bar(&mut self, ui: &mut egui::Ui, editor_id: egui::Id, matches: &[std::ops::Range<usize>]) {
        let Some(find) = &mut self.find else { return };
        let count = matches.len();
        let (mut next, mut prev, mut replace_one, mut replace_all, mut close) = (false, false, false, false, false);
        egui::Frame::new()
            .fill(color::BG_SUBTLE)
            .stroke(Stroke::new(1.0, color::BORDER))
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut find.query)
                            .font(theme::mono(12.5))
                            .hint_text(format!("{}  Find", icon::MAGNIFYING_GLASS))
                            .desired_width(220.0),
                    );
                    if std::mem::take(&mut find.focus) {
                        r.request_focus();
                    }
                    if r.changed() {
                        find.current = 0;
                        find.reveal = true;
                    }
                    let (enter, shift, escape) =
                        ui.input(|i| (i.key_pressed(Key::Enter), i.modifiers.shift, i.key_pressed(Key::Escape)));
                    if r.lost_focus() && enter {
                        if shift {
                            prev = true
                        } else {
                            next = true
                        }
                        r.request_focus();
                    }
                    if (r.has_focus() || r.lost_focus()) && escape {
                        close = true;
                    }
                    let case = RichText::new("Aa").color(if find.case_sensitive {
                        color::ACCENT_TEXT
                    } else {
                        color::TEXT_WEAK
                    });
                    if ui.selectable_label(find.case_sensitive, case).on_hover_text("Match case").clicked() {
                        find.case_sensitive = !find.case_sensitive;
                        find.current = 0;
                        find.reveal = true;
                    }
                    let label = match count {
                        0 if find.query.is_empty() => String::new(),
                        0 => "No results".into(),
                        n => format!("{} of {n}", find.current + 1),
                    };
                    ui.label(RichText::new(label).small().color(color::TEXT_WEAK));
                    if ui
                        .add_enabled(count > 0, theme::flat_button(icon::ARROW_UP))
                        .on_hover_text("Previous (Shift+Enter)")
                        .clicked()
                    {
                        prev = true;
                    }
                    if ui
                        .add_enabled(count > 0, theme::flat_button(icon::ARROW_DOWN))
                        .on_hover_text("Next (Enter)")
                        .clicked()
                    {
                        next = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(theme::flat_button(icon::X)).on_hover_text("Close (Esc)").clicked() {
                            close = true;
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut find.replacement)
                            .font(theme::mono(12.5))
                            .hint_text("Replace with")
                            .desired_width(220.0),
                    );
                    if ui.add_enabled(count > 0, theme::flat_button("Replace")).clicked() {
                        replace_one = true;
                    }
                    if ui.add_enabled(count > 0, theme::flat_button("Replace all")).clicked() {
                        replace_all = true;
                    }
                });
            });
        if count > 0 {
            if next {
                find.current = (find.current + 1) % count;
                find.reveal = true;
            }
            if prev {
                find.current = (find.current + count - 1) % count;
                find.reveal = true;
            }
        }
        let replacement = find.replacement.clone();
        if replace_one && let Some(m) = matches.get(find.current) {
            // The next match moves into the same index.
            self.sql.replace_range(m.clone(), &replacement);
            if let Some(f) = &mut self.find {
                f.reveal = true;
            }
        }
        if replace_all {
            for m in matches.iter().rev() {
                self.sql.replace_range(m.clone(), &replacement);
            }
        }
        if close {
            let current = self.find.take().and_then(|f| matches.get(f.current).cloned());
            match current {
                Some(m) if m.end <= self.sql.len() => self.select_in_editor(ui.ctx(), editor_id, m),
                _ => ui.memory_mut(|m| m.request_focus(editor_id)),
            }
        }
    }

    /// Line commands and typing helpers, applied before the text field sees the
    /// keys: Cmd+D, Alt+Up / Down, Enter with indentation, paired brackets and
    /// quotes, and Backspace inside an empty pair.
    fn editor_keys(&mut self, ui: &egui::Ui, editor_id: egui::Id) {
        let Some((mut start, mut end)) = self.editor_selection(ui.ctx(), editor_id) else { return };
        let mut text = self.sql.clone();
        let mut changed = false;
        let (dup, up, down) = ui.input_mut(|i| {
            (i.consume_shortcut(&DUPLICATE), i.consume_shortcut(&MOVE_UP), i.consume_shortcut(&MOVE_DOWN))
        });
        let line_edit = if dup {
            Some(editor_ops::duplicate_lines(&text, start, end))
        } else if up || down {
            editor_ops::move_lines(&text, start, end, up)
        } else {
            None
        };
        if let Some((t, a, b)) = line_edit {
            (text, start, end, changed) = (t, a, b, true);
        }
        ui.input_mut(|i| {
            let mut keep = Vec::with_capacity(i.events.len());
            // Once the text field must handle an edit, it handles the rest of the
            // frame's edits too, so they apply in the order they were typed.
            let mut intercept = true;
            for event in std::mem::take(&mut i.events) {
                if intercept {
                    // Drop closers the cursor has left (or that were edited away).
                    let closers = &mut self.auto_closers;
                    while let Some(&(ch, from_end)) = closers.last() {
                        let at = text.len().checked_sub(from_end);
                        let inside = at
                            .is_some_and(|at| at >= end && text[at..].starts_with(ch) && !text[end..at].contains('\n'));
                        if inside {
                            break;
                        }
                        closers.pop();
                    }
                    let result = match &event {
                        // On macOS typed text can also arrive as an IME commit.
                        egui::Event::Text(t) | egui::Event::Ime(egui::ImeEvent::Commit(t)) => {
                            let mut chars = t.chars();
                            match (chars.next(), chars.next()) {
                                (Some(c), None) => {
                                    let r = editor_ops::type_char(&text, start, end, c);
                                    // A new pair (not a wrapped selection): remember its closer.
                                    if let Some((t, _, b)) = &r
                                        && start == end
                                        && t.len() == text.len() + 2 * c.len_utf8()
                                    {
                                        let closer = t[*b..].chars().next().unwrap_or(c);
                                        closers.push((closer, t.len() - *b));
                                    }
                                    r
                                }
                                _ => None,
                            }
                        }
                        // Tab right after typing inside an auto-closed pair: step out of it.
                        egui::Event::Key { key: Key::Tab, pressed: true, modifiers, .. }
                            if modifiers.is_none() && start == end =>
                        {
                            closers.pop().map(|(ch, from_end)| {
                                let at = text.len() - from_end + ch.len_utf8();
                                (text.clone(), at, at)
                            })
                        }
                        egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. } if modifiers.is_none() => {
                            Some(editor_ops::newline(&text, start, end))
                        }
                        egui::Event::Key { key: Key::Backspace, pressed: true, modifiers, .. }
                            if modifiers.is_none() =>
                        {
                            editor_ops::backspace(&text, start, end)
                        }
                        _ => None,
                    };
                    match result {
                        Some((t, a, b)) => {
                            (text, start, end, changed) = (t, a, b, true);
                            continue;
                        }
                        None => {
                            // Only events that change the text or move the cursor end
                            // interception. A typed character also brings a key press
                            // (and on macOS IME preedit events) before its text; those
                            // must not stop us from seeing the text.
                            let edits_or_moves = match &event {
                                egui::Event::Text(_)
                                | egui::Event::Paste(_)
                                | egui::Event::Ime(egui::ImeEvent::Commit(_)) => true,
                                egui::Event::Key { key, pressed: true, .. } => matches!(
                                    key,
                                    Key::Enter
                                        | Key::Backspace
                                        | Key::Delete
                                        | Key::Tab
                                        | Key::ArrowLeft
                                        | Key::ArrowRight
                                        | Key::ArrowUp
                                        | Key::ArrowDown
                                        | Key::Home
                                        | Key::End
                                        | Key::PageUp
                                        | Key::PageDown
                                ),
                                _ => false,
                            };
                            if edits_or_moves {
                                intercept = false;
                            }
                        }
                    }
                }
                keep.push(event);
            }
            i.events = keep;
        });
        if changed {
            self.sql = text;
            self.select_in_editor(ui.ctx(), editor_id, start..end);
            self.reveal_byte = Some(end);
        }
    }

    /// Cmd+/: toggles `--` on the selected lines, or the cursor's line.
    fn toggle_comment(&mut self, ui: &egui::Ui, editor_id: egui::Id) {
        let (a, b) = self.editor_selection(ui.ctx(), editor_id).unwrap_or((self.sql.len(), self.sql.len()));
        let (sql, lines) = sql_format::toggle_comment(&self.sql, a, b);
        self.sql = sql;
        if a == b {
            // Keep a plain cursor at the end of its line.
            self.select_in_editor(ui.ctx(), editor_id, lines.end..lines.end);
        } else {
            self.select_in_editor(ui.ctx(), editor_id, lines);
        }
    }

    /// Formats the selection, or the statement under the cursor.
    fn format_sql(&mut self, ui: &egui::Ui, editor_id: egui::Id) {
        let range = match self.editor_selection(ui.ctx(), editor_id) {
            Some((a, b)) if a < b => a..b,
            Some((a, _)) => match statement_at(&self.sql, a, self.dialect) {
                Some(r) => r,
                None => return,
            },
            None => return,
        };
        let formatted = sql_format::format(&self.sql[range.clone()]);
        if formatted == self.sql[range.clone()] {
            return;
        }
        self.sql.replace_range(range.clone(), &formatted);
        self.select_in_editor(ui.ctx(), editor_id, range.start..range.start + formatted.len());
    }

    fn run_all(&self) -> Option<ConsoleAction> {
        let statements: Vec<String> =
            sql_split::split(&self.sql, self.dialect).into_iter().map(|s| self.sql[s].to_string()).collect();
        run_fresh(statements)
    }

    fn run_at_cursor(&self, ui: &egui::Ui, editor_id: egui::Id) -> Option<ConsoleAction> {
        let range = egui::TextEdit::load_state(ui.ctx(), editor_id).and_then(|s| s.cursor.char_range());
        let to_byte = |char_idx: usize| self.sql.char_indices().nth(char_idx).map_or(self.sql.len(), |(b, _)| b);
        let statements: Vec<String> = match range {
            Some(r) if r.primary != r.secondary => {
                let (a, b) = (r.primary.index.0.min(r.secondary.index.0), r.primary.index.0.max(r.secondary.index.0));
                let selected = &self.sql[to_byte(a)..to_byte(b)];
                sql_split::split(selected, self.dialect).into_iter().map(|s| selected[s].to_string()).collect()
            }
            other => {
                let cursor = other.map_or(0, |r| to_byte(r.primary.index.0));
                statement_at(&self.sql, cursor, self.dialect).map(|s| self.sql[s].to_string()).into_iter().collect()
            }
        };
        run_fresh(statements)
    }

    // ----- result tabs and navigation ------------------------------------

    /// Switching tabs by hand counts as a navigation step, like following a
    /// link in a browser, so Back returns to the tab that was showing.
    fn select_result(&mut self, i: usize) {
        if i != self.active_result {
            self.nav_back.push(self.active_result);
            self.nav_forward.clear();
            self.active_result = i;
        }
    }

    fn go_back(&mut self) {
        if let Some(prev) = self.nav_back.pop() {
            self.nav_forward.push(self.active_result);
            self.active_result = prev;
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.nav_forward.pop() {
            self.nav_back.push(self.active_result);
            self.active_result = next;
        }
    }

    /// Removes result tab `i`, keeping the active tab and the Back/Forward
    /// stacks pointing at the same results.
    fn close_result(&mut self, i: usize) {
        if i >= self.results.len() {
            return;
        }
        self.results.remove(i);
        let shift = |stack: &mut Vec<usize>| {
            stack.retain(|&t| t != i);
            stack.iter_mut().filter(|t| **t > i).for_each(|t| *t -= 1);
            stack.dedup();
        };
        shift(&mut self.nav_back);
        shift(&mut self.nav_forward);
        if self.active_result == i {
            self.active_result = self.nav_back.pop().unwrap_or(i.saturating_sub(1));
        } else if self.active_result > i {
            self.active_result -= 1;
        }
        self.active_result = self.active_result.min(self.results.len().saturating_sub(1));
        self.panel = None;
    }

    fn tab_label(&self, i: usize) -> String {
        self.results[i].title.clone().unwrap_or_else(|| format!("Result {}", i + 1))
    }

    fn filter_run(&mut self) -> Option<ConsoleAction> {
        let tv = self.table.as_ref()?;
        let sql = tv.sql(self.dialect);
        self.sql = sql.clone();
        Some(ConsoleAction::Run { statements: vec![(sql, Vec::new())], limit: PAGE_SIZE, mode: RunMode::Filter })
    }

    /// Header click on table data: ascending, then descending, then unsorted.
    fn sort_table_by(&mut self, col: usize, name: &str) -> Option<ConsoleAction> {
        let dialect = self.dialect;
        let tv = self.table.as_mut()?;
        tv.sort = match tv.sort {
            Some((c, true)) if c == col => Some((col, false)),
            Some((c, false)) if c == col => None,
            _ => Some((col, true)),
        };
        tv.order = match tv.sort {
            Some((_, asc)) => format!("{} {}", dialect.quote_ident(name), if asc { "ASC" } else { "DESC" }),
            None => String::new(),
        };
        self.filter_run()
    }

    // ----- results area ----------------------------------------------------

    fn results_area(&mut self, ui: &mut egui::Ui, cx: &ConsoleContext<'_>) -> Option<ConsoleAction> {
        let mut action = None;
        let idx = self.active_result.min(self.results.len() - 1);
        let running = self.run.is_some();
        let target = match &self.results[idx].outcome {
            Ok(ExecOutcome::Rows(_)) if cx.read_only => Some(Err("Read-only connection".to_string())),
            Ok(ExecOutcome::Rows(rs)) => Some(edits::edit_target(rs, cx.tables)),
            _ => None,
        };
        let editable = matches!(target, Some(Ok(_)));
        let mut close_result = None;
        let mut refresh = ui.input_mut(|i| i.consume_shortcut(&REFRESH));

        // Chips, result tabs and the + Row / Export buttons.
        egui::Panel::top(egui::Id::new(("results-bar", self.id)))
            .frame(
                egui::Frame::new()
                    .fill(color::BG)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .stroke(Stroke::new(1.0, color::BORDER)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if idx == 0
                        && self.table.is_some()
                        && let Some(a) = self.chips(ui, running)
                    {
                        action = Some(a);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let shortcut = ui.ctx().format_shortcut(&REFRESH);
                        if ui
                            .add_enabled(!running, egui::Button::new(format!("{}  Refresh", icon::ARROWS_CLOCKWISE)))
                            .on_hover_text(format!("Run this result's query again ({shortcut})"))
                            .clicked()
                        {
                            refresh = true;
                        }
                        let tab = &mut self.results[idx];
                        if let Ok(ExecOutcome::Rows(rs)) = &tab.outcome {
                            ui.menu_button(format!("{}  Export  {}", icon::DOWNLOAD_SIMPLE, icon::CARET_DOWN), |ui| {
                                if ui.button("CSV…").clicked() {
                                    export(rs, "csv");
                                    ui.close();
                                }
                                if ui.button("JSON…").clicked() {
                                    export(rs, "json");
                                    ui.close();
                                }
                            });
                            if editable
                                && ui
                                    .add_enabled(
                                        !running && !tab.submitting,
                                        egui::Button::new(format!("{}  Row", icon::PLUS)),
                                    )
                                    .clicked()
                            {
                                tab.edits.inserts.push(vec![None; rs.columns.len()]);
                            }
                            if let Some(Ok(t)) = &target {
                                let selected: Vec<usize> = tab.selection.rows.iter().copied().collect();
                                let can = !running && !tab.submitting && !selected.is_empty();
                                let n = selected.len();
                                let all_deleted = can && selected.iter().all(|r| tab.edits.deletes.contains(r));
                                let (label, tip) = if all_deleted {
                                    (
                                        format!("{}  Undo delete", icon::ARROW_COUNTER_CLOCKWISE),
                                        "Keep the selected rows",
                                    )
                                } else {
                                    (
                                        format!("{}  Delete", icon::TRASH),
                                        "Mark the selected rows for deletion; Save applies it",
                                    )
                                };
                                if ui
                                    .add_enabled(can, egui::Button::new(label))
                                    .on_hover_text(tip)
                                    .on_disabled_hover_text("Select rows first")
                                    .clicked()
                                {
                                    for r in &selected {
                                        if all_deleted {
                                            tab.edits.deletes.remove(r);
                                        } else {
                                            tab.edits.deletes.insert(*r);
                                        }
                                    }
                                }
                                let dup = ui
                                    .add_enabled(can, egui::Button::new(format!("{}  Duplicate", icon::COPY)))
                                    .on_hover_text(format!("Add a copy of the {} selected row(s) as new rows", n))
                                    .on_disabled_hover_text("Select rows first");
                                if dup.clicked() {
                                    let details = cx.tables.get(&(t.schema.clone(), t.table.clone()));
                                    duplicate_selected(
                                        rs,
                                        &mut tab.edits,
                                        &tab.selection,
                                        &tab.sort,
                                        t,
                                        details,
                                        self.dialect,
                                    );
                                }
                            }
                            if let Some(Err(reason)) = &target {
                                ui.label(RichText::new("read-only").small().color(color::TEXT_WEAK))
                                    .on_hover_text(reason);
                            }
                        }
                    });
                });
                if let Some(e) = self.table.as_ref().and_then(|t| t.error.as_ref()).filter(|_| idx == 0) {
                    ui.colored_label(color::DANGER, e);
                }
                close_result = self.result_tabs(ui, running);
            });

        if let Some(a) = self.status_bar(ui, idx, target.as_ref()) {
            action = Some(a);
        }
        self.edit_error_banner(ui, idx);

        // Row panel for the selected row.
        let selected_row = match &self.results[idx].outcome {
            Ok(ExecOutcome::Rows(_)) => self.results[idx].selection.single(),
            _ => None,
        };
        if self.panel.as_ref().is_some_and(|p| Some(p.row) != selected_row || p.tab != idx) {
            self.panel = None;
            self.panel_open = true;
        }
        if let Some(row) = selected_row.filter(|_| self.panel_open)
            && let Some(a) = self.row_panel(ui, cx, idx, row, target.as_ref())
        {
            action = Some(a);
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(color::BG)).show(ui, |ui| {
            if let Some(a) = self.grid(ui, cx, idx, target.as_ref(), running) {
                action = Some(a);
            }
        });
        if refresh
            && !running
            && let Some(a) = self.refresh(idx)
        {
            action = Some(a);
        }
        if let Some(i) = close_result {
            if self.results.get(i).is_some_and(|t| t.edits.row_count() > 0) {
                self.guard = Some(Guard {
                    action: None,
                    close_result: Some(i),
                    tabs: vec![i],
                    restore: (self.sql.clone(), None),
                });
            } else {
                self.close_result(i);
            }
        }
        action
    }

    /// Filter and order chips of a table's data.
    fn chips(&mut self, ui: &mut egui::Ui, running: bool) -> Option<ConsoleAction> {
        let tv = self.table.as_mut()?;
        let mut rerun = false;
        ui.label(RichText::new("Filters").color(color::TEXT_WEAK));
        let mut remove = None;
        for (i, f) in tv.filters.iter().enumerate() {
            if chip(ui, &RichText::new(f).font(theme::mono(12.0))) {
                remove = Some(i);
            }
        }
        if let Some(i) = remove {
            tv.filters.remove(i);
            rerun = true;
        }
        match &mut tv.new_filter {
            Some(text) => {
                let r = ui.add(
                    egui::TextEdit::singleline(text)
                        .font(theme::mono(12.0))
                        .hint_text("e.g. vip = true")
                        .desired_width(180.0),
                );
                // Focus only when it opens: grabbing it every frame would undo the
                // focus loss Enter / Escape / clicking elsewhere cause, and keep it forever.
                if std::mem::take(&mut tv.focus_filter) {
                    r.request_focus();
                }
                let (enter, escape) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
                if escape {
                    tv.new_filter = None;
                } else if r.lost_focus() {
                    let condition = text.trim().to_string();
                    tv.new_filter = None;
                    if enter && !condition.is_empty() {
                        tv.filters.push(condition);
                        rerun = true;
                    }
                }
            }
            None => {
                if ui.add_enabled(!running, theme::flat_button(format!("{}  Filter", icon::PLUS))).clicked() {
                    tv.new_filter = Some(String::new());
                    tv.focus_filter = true;
                }
            }
        }
        ui.add(egui::Separator::default().vertical().spacing(10.0));
        ui.label(RichText::new("Order").color(color::TEXT_WEAK));
        if !tv.order.trim().is_empty() {
            let label = match tv.sort {
                Some((_, asc)) => {
                    let name = tv.order.split_whitespace().next().unwrap_or_default().trim_matches('"').to_string();
                    format!("{name} {}", if asc { icon::ARROW_UP } else { icon::ARROW_DOWN })
                }
                None => tv.order.clone(),
            };
            if chip(ui, &RichText::new(label).font(theme::mono(12.0))) {
                tv.order.clear();
                tv.sort = None;
                rerun = true;
            }
        } else {
            ui.label(RichText::new("click a column header").small().color(color::TEXT_FAINT));
        }
        if rerun && !running { self.filter_run() } else { None }
    }

    /// Result tabs (several statements or navigation) and Back / Forward.
    /// Returns a result tab whose close button was clicked; the caller closes
    /// it once the frame is drawn, since the rest of the frame indexes results.
    fn result_tabs(&mut self, ui: &mut egui::Ui, running: bool) -> Option<usize> {
        let navigated = self.results.iter().any(|t| t.title.is_some());
        if self.results.len() < 2 && !navigated {
            return None;
        }
        ui.add_space(4.0);
        let (mut close, mut select) = (None, None);
        ui.horizontal_wrapped(|ui| {
            if navigated {
                if ui
                    .add_enabled(!self.nav_back.is_empty(), theme::flat_button(icon::ARROW_LEFT))
                    .on_hover_text("Back")
                    .clicked()
                {
                    self.go_back();
                }
                if ui
                    .add_enabled(!self.nav_forward.is_empty(), theme::flat_button(icon::ARROW_RIGHT))
                    .on_hover_text("Forward")
                    .clicked()
                {
                    self.go_forward();
                }
            }
            for i in 0..self.results.len() {
                let failed = self.results[i].outcome.is_err();
                let mut text = RichText::new(self.tab_label(i));
                if failed {
                    text = text.color(color::DANGER);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if ui.selectable_label(self.active_result == i, text).on_hover_text(&self.results[i].sql).clicked()
                    {
                        select = Some(i);
                    }
                    // Indices shift on close, so wait for a running statement to land first.
                    if ui
                        .add_enabled(!running, theme::flat_button(RichText::new(icon::X).small()))
                        .on_hover_text("Close result")
                        .clicked()
                    {
                        close = Some(i);
                    }
                });
            }
        });
        if let Some(i) = select {
            self.select_result(i);
        }
        close
    }

    fn grid(
        &mut self,
        ui: &mut egui::Ui,
        cx: &ConsoleContext<'_>,
        idx: usize,
        target: Option<&Result<EditTarget, String>>,
        running: bool,
    ) -> Option<ConsoleAction> {
        let console_id = self.id;
        let dialect = self.dialect;
        let server_sort = self.table.as_ref().filter(|_| idx == 0).map(|tv| tv.sort);
        let location = if self.table.is_none() { self.error_location(idx) } else { None };
        if let Some(Ok(t)) = target
            && !running
        {
            self.grid_keys(ui, cx, idx, t);
        }
        let mut go_to_error = false;
        let tab = &mut self.results[idx];
        let rs = match &tab.outcome {
            Ok(ExecOutcome::Rows(rs)) => rs,
            Ok(ExecOutcome::Affected(n)) => {
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(format!("{n} row(s) affected")).font(theme::font(14.0, theme::medium())));
                });
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(&tab.sql).font(theme::mono(12.0)).color(color::TEXT_WEAK));
                });
                return None;
            }
            Err(e) => {
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&e.message).font(theme::font(14.0, theme::medium())).color(color::DANGER),
                        );
                        if let Some(detail) = &e.detail {
                            ui.label(RichText::new(detail).color(color::TEXT_WEAK));
                        }
                        if let Some(code) = &e.code {
                            ui.label(RichText::new(format!("code {code}")).small().color(color::TEXT_WEAK));
                        }
                        if let Some((_, line, column)) = location {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("Line {line}, column {column}"))
                                        .small()
                                        .color(color::TEXT_WEAK),
                                );
                                if ui.link(RichText::new("Show in editor").small().color(color::LINK)).clicked() {
                                    go_to_error = true;
                                }
                            });
                        }
                        ui.add_space(8.0);
                        ui.label(RichText::new(&tab.sql).font(theme::mono(12.0)).color(color::TEXT_WEAK));
                    });
                });
                if go_to_error {
                    self.reveal_error(idx);
                }
                return None;
            }
        };
        let fks = cx.fks;
        let is_link = |row: usize, col: usize| link_for_cell(&rs.columns, &rs.rows[row], col, fks).is_some();
        let meta = column_meta(rs, cx.tables);
        let edit = matches!(target, Some(Ok(_))).then(|| GridEdit { edits: &mut tab.edits, editing: &mut tab.editing });
        let table = single_table(rs);
        let keys = incoming_keys(rs, cx.incoming);
        let referencing = |row: usize| -> Vec<(usize, String)> {
            keys.iter()
                .enumerate()
                .filter(|(_, k)| referencing_values(&rs.columns, &rs.rows[row], k).is_some())
                .map(|(i, k)| (i, format!("{} ({})", k.table, k.foreign_key.columns.join(", "))))
                .collect()
        };
        let opts = GridOptions {
            id: (console_id, idx),
            columns: &meta,
            is_link: &is_link,
            edit,
            selection: &mut tab.selection,
            dialect,
            table: table.as_deref(),
            referencing: &referencing,
            server_sort,
            filterable: server_sort.is_some(),
        };
        let mut sort_request = None;
        let mut filter_request = None;
        let mut action = None;
        let event = grid::show(ui, rs, &mut tab.sort, opts);
        // The open value viewer follows the clicked cell.
        if self.value_col.is_some()
            && let Some(c) = tab.selection.cell
        {
            self.value_col = Some(c);
        }
        match event {
            Some(_) if running => {}
            Some(GridEvent::Link(row, col)) => {
                if let Some(link) = link_for_cell(&rs.columns, &rs.rows[row], col, fks) {
                    action = Some(navigate(dialect, idx, &link));
                }
            }
            Some(GridEvent::SortBy(col)) => sort_request = Some((col, rs.columns[col].name.clone())),
            Some(GridEvent::FilterBy(row, col)) => {
                let value = &rs.rows[row][col];
                let column = dialect.quote_ident(&rs.columns[col].name);
                let condition = if value.is_null() {
                    format!("{column} IS NULL")
                } else {
                    format!("{column} = {}", dbm_core::export::sql_literal(dialect, value))
                };
                filter_request = Some(condition);
            }
            Some(GridEvent::ViewValue(col)) => {
                self.value_col = Some(col);
                self.panel_open = true;
            }
            Some(GridEvent::Referencing { row, key }) => {
                if let Some(k) = keys.get(key)
                    && let Some(values) = referencing_values(&rs.columns, &rs.rows[row], k)
                {
                    action = Some(navigate_referencing(dialect, idx, k, &values));
                }
            }
            None => {}
        }
        if let Some((col, name)) = sort_request {
            action = self.sort_table_by(col, &name);
        }
        if let Some(condition) = filter_request
            && let Some(tv) = &mut self.table
        {
            if !tv.filters.contains(&condition) {
                tv.filters.push(condition);
            }
            action = self.filter_run();
        }
        action
    }

    /// Grid keys while no text field has focus: Delete / Backspace mark the
    /// selected rows for deletion, Cmd+D duplicates them, Enter / F2 edit the
    /// selected cell.
    fn grid_keys(&mut self, ui: &egui::Ui, cx: &ConsoleContext<'_>, idx: usize, t: &EditTarget) {
        if ui.ctx().memory(|m| m.focused().is_some()) || self.guard.is_some() || self.unrestricted.is_some() {
            return;
        }
        let tab = &mut self.results[idx];
        if tab.editing.is_some() || tab.submitting || tab.selection.rows.is_empty() {
            return;
        }
        let Ok(ExecOutcome::Rows(rs)) = &tab.outcome else { return };
        let (delete, duplicate, edit) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace),
                i.consume_shortcut(&DUPLICATE),
                i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::F2),
            )
        });
        if delete {
            tab.edits.deletes.extend(tab.selection.rows.iter().copied());
        }
        if duplicate {
            let details = cx.tables.get(&(t.schema.clone(), t.table.clone()));
            duplicate_selected(rs, &mut tab.edits, &tab.selection, &tab.sort, t, details, self.dialect);
        }
        if edit && let Some(row) = tab.selection.single() {
            let col = tab.selection.cell.unwrap_or(0).min(rs.columns.len().saturating_sub(1));
            if !tab.edits.deletes.contains(&row) {
                let value = tab.edits.updates.get(&(row, col)).cloned().unwrap_or_else(|| rs.rows[row][col].clone());
                let buffer = if value.is_null() { String::new() } else { value.to_string() };
                tab.editing = Some(EditingCell::new(RowRef::Existing(row), col, buffer));
            }
        }
    }

    // ----- status bar ------------------------------------------------------

    fn status_frame() -> egui::Frame {
        egui::Frame::new()
            .fill(color::BG_SUBTLE)
            .inner_margin(egui::Margin::symmetric(12, 7))
            .stroke(Stroke::new(1.0, color::BORDER))
    }

    fn run_status(&self, ui: &mut egui::Ui) -> Option<ConsoleAction> {
        let run = self.run.as_ref()?;
        ui.spinner();
        ui.label(
            RichText::new(format!(
                "Running {}/{} · {:.1} s",
                (run.done + 1).min(run.total),
                run.total,
                run.started.elapsed().as_secs_f32()
            ))
            .color(color::TEXT_WEAK),
        );
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        ui.add(theme::flat_button(format!("{}  Cancel", icon::STOP))).clicked().then_some(ConsoleAction::Cancel)
    }

    fn tx_notice(&self, ui: &mut egui::Ui) {
        match &self.tx_notice {
            Some(Ok(msg)) => {
                ui.label(RichText::new(msg).color(color::TEXT_WEAK));
            }
            Some(Err(e)) => {
                ui.colored_label(color::DANGER, e);
            }
            None => {}
        }
    }

    fn status_bar_when_empty(&mut self, ui: &mut egui::Ui) -> Option<ConsoleAction> {
        let mut action = None;
        egui::Panel::bottom(egui::Id::new(("status", self.id))).frame(Self::status_frame()).show(ui, |ui| {
            ui.horizontal(|ui| {
                action = self.run_status(ui);
                self.tx_notice(ui);
            });
        });
        action
    }

    fn status_bar(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        target: Option<&Result<EditTarget, String>>,
    ) -> Option<ConsoleAction> {
        let mut action = None;
        let dialect = self.dialect;
        let save = self.save_requested;
        egui::Panel::bottom(egui::Id::new(("status", self.id))).frame(Self::status_frame()).show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(a) = self.run_status(ui) {
                    action = Some(a);
                }
                let running = self.run.is_some();
                let tab = &self.results[idx];
                match &tab.outcome {
                    Ok(ExecOutcome::Rows(rs)) => {
                        let shown = if rs.truncated {
                            format!("{}+ rows", rs.rows.len())
                        } else {
                            format!("{} rows", rs.rows.len())
                        };
                        ui.label(RichText::new(shown).font(theme::font(12.5, theme::semibold())));
                        ui.label(RichText::new(format!("· {} ms", tab.elapsed.as_millis())).color(color::TEXT_WEAK));
                        let selected = tab.selection.rows.len();
                        if selected > 0 {
                            ui.label(RichText::new(format!("· {selected} selected")).color(color::ACCENT_TEXT));
                        }
                        if rs.truncated && is_read_only_query(&tab.sql) {
                            if ui.add_enabled(!running, egui::Link::new("Load more")).clicked() {
                                action = Some(ConsoleAction::Run {
                                    statements: vec![(tab.sql.clone(), tab.params.clone())],
                                    limit: tab.limit + PAGE_SIZE,
                                    mode: RunMode::Replace(idx),
                                });
                            }
                        } else if rs.truncated {
                            ui.label(RichText::new("(more rows not fetched)").color(color::TEXT_WEAK));
                        }
                    }
                    Ok(ExecOutcome::Affected(n)) => {
                        ui.label(format!("{n} row(s) affected"));
                        ui.label(RichText::new(format!("· {} ms", tab.elapsed.as_millis())).color(color::TEXT_WEAK));
                    }
                    Err(_) => {
                        ui.colored_label(color::DANGER, "Statement failed");
                    }
                }
                self.tx_notice(ui);

                let Some(Ok(t)) = target else { return };
                let pending = tab.edits.row_count();
                if pending == 0 {
                    return;
                }
                let idle = !running && !tab.submitting;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ctx = ui.ctx().clone();
                    let Ok(ExecOutcome::Rows(rs)) = &self.results[idx].outcome else { return };
                    let statements = || edits::statements(dialect, t, rs, &self.results[idx].edits);
                    let save_button = theme::primary_button(format!("Save   {}", ctx.format_shortcut(&SAVE)));
                    if save && pending > 0 {
                        self.save_requested = false;
                    }
                    if (ui.add_enabled(idle, save_button).clicked() || (save && idle)) && pending > 0 {
                        action = Some(ConsoleAction::SubmitEdits { tab: idx, statements: statements() });
                    }
                    let discard = ui.add_enabled(idle, egui::Button::new("Discard")).clicked();
                    if ui.button("View SQL").clicked() {
                        let text = statements()
                            .iter()
                            .map(|(sql, params, _)| format!("{sql};\n-- params: {}", format_params(params)))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        self.view_sql = Some(text);
                    }
                    if self.results[idx].submitting {
                        ui.spinner();
                    }
                    theme::pill(
                        ui,
                        RichText::new(format!("●  {pending} pending change{}", if pending == 1 { "" } else { "s" }))
                            .color(Color32::from_rgb(140, 105, 0)),
                        color::CHANGED,
                    );
                    if discard {
                        let tab = &mut self.results[idx];
                        tab.edits = Edits::default();
                        tab.editing = None;
                        tab.edit_error = None;
                        self.panel = None;
                    }
                });
            });
        });
        action
    }

    /// Why the last Save failed, above the status bar, until dismissed or saved again.
    fn edit_error_banner(&mut self, ui: &mut egui::Ui, idx: usize) {
        let Some(error) = self.results[idx].edit_error.clone() else { return };
        let (reason, sql) = error.split_once("\n\n").unwrap_or((error.as_str(), ""));
        let mut dismiss = false;
        egui::Panel::bottom(egui::Id::new(("edit-error", self.id)))
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(253, 236, 236))
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .stroke(Stroke::new(1.0, color::DANGER)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::WARNING_CIRCLE).color(color::DANGER));
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(RichText::new(reason).color(color::DANGER)).wrap());
                        if !sql.is_empty() {
                            ui.add(
                                egui::Label::new(RichText::new(sql).font(theme::mono(11.5)).color(color::TEXT_WEAK))
                                    .truncate(),
                            )
                            .on_hover_text(sql);
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        if ui.add(theme::flat_button(icon::X)).on_hover_text("Dismiss").clicked() {
                            dismiss = true;
                        }
                    });
                });
            });
        if dismiss {
            self.results[idx].edit_error = None;
        }
    }

    /// Re-runs result tab `idx` in place with its SQL, parameters and row limit.
    fn refresh(&self, idx: usize) -> Option<ConsoleAction> {
        let tab = self.results.get(idx)?;
        Some(ConsoleAction::Run {
            statements: vec![(tab.sql.clone(), tab.params.clone())],
            limit: tab.limit,
            mode: RunMode::Replace(idx),
        })
    }

    /// Result tabs whose pending edits `action` would throw away.
    fn discarded_by(&self, action: &ConsoleAction) -> Vec<usize> {
        let with_edits = |i: usize| self.results.get(i).is_some_and(|t| t.edits.row_count() > 0);
        match action {
            ConsoleAction::Run { mode: RunMode::Fresh, .. } => {
                (0..self.results.len()).filter(|&i| with_edits(i)).collect()
            }
            ConsoleAction::Run { mode: RunMode::Replace(i), .. } => {
                [*i].into_iter().filter(|&i| with_edits(i)).collect()
            }
            ConsoleAction::Run { mode: RunMode::Filter, .. } => [0].into_iter().filter(|&i| with_edits(i)).collect(),
            _ => Vec::new(),
        }
    }

    /// Holds `action` behind a confirmation when it would discard pending edits.
    /// Holds a run from the editor whose statements have placeholders until
    /// their values are entered.
    fn hold_params(&mut self, action: Option<ConsoleAction>) -> Option<ConsoleAction> {
        let a = action?;
        let ConsoleAction::Run { statements, mode: RunMode::Fresh, .. } = &a else { return Some(a) };
        let mut names: Vec<String> = Vec::new();
        for (sql, _) in statements {
            for p in sql_format::parameters(sql, self.dialect) {
                if !names.contains(&p.name) {
                    names.push(p.name);
                }
            }
        }
        if names.is_empty() {
            return Some(a);
        }
        self.param_prompt = Some(ParamPrompt { action: a, names, focus: true });
        None
    }

    fn params_modal(&mut self, ui: &egui::Ui) -> Option<ConsoleAction> {
        let prompt = self.param_prompt.as_mut()?;
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new(("params", self.id))).show(ui.ctx(), |ui| {
            ui.set_width(440.0);
            ui.heading("Query parameters");
            ui.add_space(4.0);
            ui.label(
                RichText::new("Numbers, NULL and true / false are used as typed; other text is quoted. Tick SQL to insert an expression such as now().")
                    .small()
                    .color(color::TEXT_WEAK),
            );
            ui.add_space(8.0);
            egui::Grid::new(("params-grid", self.id)).num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
                for (i, name) in prompt.names.iter().enumerate() {
                    let value = self.param_values.entry(name.clone()).or_default();
                    ui.label(RichText::new(name).font(theme::mono(12.5)));
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut value.text)
                            .font(theme::mono(12.5))
                            .desired_width(280.0)
                            .hint_text("value"),
                    );
                    if i == 0 && std::mem::take(&mut prompt.focus) {
                        r.request_focus();
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        decision = Some(true);
                    }
                    ui.checkbox(&mut value.expression, "SQL").on_hover_text("Insert the text as an SQL expression");
                    ui.end_row();
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(theme::primary_button(format!("{}  Run", icon::PLAY))).clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        });
        if decision.is_none() && modal.should_close() {
            decision = Some(false);
        }
        let proceed = decision?;
        let prompt = self.param_prompt.take()?;
        if !proceed || self.run.is_some() {
            return None;
        }
        let ConsoleAction::Run { statements, limit, mode } = prompt.action else { return None };
        let dialect = self.dialect;
        let values = &self.param_values;
        let statements = statements
            .into_iter()
            .map(|(sql, params)| {
                let found = sql_format::parameters(&sql, dialect);
                let text = sql_format::substitute(&sql, &found, |name| {
                    values.get(name).map_or_else(|| "NULL".to_string(), |v| v.sql(dialect))
                });
                (text, params)
            })
            .collect();
        Some(ConsoleAction::Run { statements, limit, mode })
    }

    /// On a read-only connection, refuses runs with statements that may write.
    fn block_writes(&mut self, action: Option<ConsoleAction>, read_only: bool) -> Option<ConsoleAction> {
        let a = action?;
        let ConsoleAction::Run { statements, .. } = &a else { return Some(a) };
        if !read_only {
            return Some(a);
        }
        let allowed = |sql: &str| {
            is_read_only_query(sql) || {
                let word = sql_format::first_word(sql).to_ascii_uppercase();
                matches!(word.as_str(), "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "END" | "SET" | "DESCRIBE" | "DESC")
            }
        };
        match statements.iter().find(|(sql, _)| !allowed(sql)) {
            None => Some(a),
            Some((sql, _)) => {
                let head: String = sql.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
                self.tx_notice = Some(Err(format!("Read-only connection: \"{head} …\" was not run")));
                None
            }
        }
    }

    /// Holds a run with an UPDATE / DELETE that has no WHERE until confirmed.
    fn hold_unrestricted(&mut self, action: Option<ConsoleAction>) -> Option<ConsoleAction> {
        let a = action?;
        let ConsoleAction::Run { statements, mode: RunMode::Fresh, .. } = &a else { return Some(a) };
        let targets: Vec<(String, &'static str)> =
            statements.iter().filter_map(|(sql, _)| sql_format::unrestricted_write(sql)).collect();
        if targets.is_empty() {
            return Some(a);
        }
        self.unrestricted = Some((a, targets));
        None
    }

    fn unrestricted_modal(&mut self, ui: &egui::Ui) -> Option<ConsoleAction> {
        let (_, targets) = self.unrestricted.as_ref()?;
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new(("unrestricted", self.id))).show(ui.ctx(), |ui| {
            ui.set_width(420.0);
            ui.heading("Run destructive statement?");
            ui.add_space(6.0);
            ui.label(if targets.len() == 1 {
                "This statement can't be undone easily:"
            } else {
                "These statements can't be undone easily:"
            });
            ui.add_space(4.0);
            for (target, reason) in targets {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("•  {target}")).font(theme::mono(12.5)));
                    ui.label(RichText::new(format!("— {reason}")).color(color::TEXT_WEAK));
                });
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let run = egui::Button::new(RichText::new("Run anyway").color(Color32::WHITE)).fill(color::DANGER);
                    if ui.add(run).clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        });
        if decision.is_none() && modal.should_close() {
            decision = Some(false);
        }
        let proceed = decision?;
        let (action, _) = self.unrestricted.take()?;
        proceed.then_some(action).filter(|_| self.run.is_none())
    }

    fn guarded(&mut self, action: Option<ConsoleAction>, before: Snapshot) -> Option<ConsoleAction> {
        let a = action?;
        let tabs = self.discarded_by(&a);
        if tabs.is_empty() {
            return Some(a);
        }
        self.guard = Some(Guard { action: Some(a), close_result: None, tabs, restore: before });
        None
    }

    fn guard_modal(&mut self, ui: &egui::Ui) -> Option<ConsoleAction> {
        let guard = self.guard.as_ref()?;
        let pending: usize = guard.tabs.iter().filter_map(|&i| self.results.get(i)).map(|t| t.edits.row_count()).sum();
        let what = if guard.close_result.is_some() { "Closing this result" } else { "This" };
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new(("discard-guard", self.id))).show(ui.ctx(), |ui| {
            ui.set_width(390.0);
            ui.heading("Discard pending changes?");
            ui.add_space(6.0);
            ui.label(format!(
                "{what} throws away {pending} unsaved change{}. Save them first with Save (Cmd+S) to keep them.",
                if pending == 1 { "" } else { "s" }
            ));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(theme::primary_button("Discard changes")).clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        });
        if decision.is_none() && modal.should_close() {
            decision = Some(false);
        }
        let proceed = decision?;
        let guard = self.guard.take()?;
        if !proceed {
            let (sql, table) = guard.restore;
            self.sql = sql;
            if let (Some(tv), Some((filters, order, sort))) = (&mut self.table, table) {
                tv.filters = filters;
                tv.order = order;
                tv.sort = sort;
            }
            return None;
        }
        for i in guard.tabs {
            if let Some(tab) = self.results.get_mut(i) {
                tab.edits = Edits::default();
                tab.editing = None;
                tab.edit_error = None;
            }
        }
        self.panel = None;
        if let Some(i) = guard.close_result {
            self.close_result(i);
        }
        guard.action.filter(|_| self.run.is_none())
    }

    fn view_sql_modal(&mut self, ui: &egui::Ui) {
        let Some(text) = &self.view_sql else { return };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new(("view-sql", self.id))).show(ui.ctx(), |ui| {
            ui.set_width(560.0);
            ui.heading("Pending changes as SQL");
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                let mut view = text.as_str();
                let mut layouter = |ui: &egui::Ui, t: &dyn egui::TextBuffer, w: f32| {
                    let job = sql_highlight::layout(ui, t.as_str(), w);
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                ui.add(
                    egui::TextEdit::multiline(&mut view)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .layouter(&mut layouter),
                );
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(text.clone());
                    }
                });
            });
        });
        if close || modal.should_close() {
            self.view_sql = None;
        }
    }

    // ----- row panel -------------------------------------------------------

    fn row_panel(
        &mut self,
        ui: &mut egui::Ui,
        cx: &ConsoleContext<'_>,
        idx: usize,
        row: usize,
        target: Option<&Result<EditTarget, String>>,
    ) -> Option<ConsoleAction> {
        let mut action = None;
        let dialect = self.dialect;
        let editable = matches!(target, Some(Ok(_)));
        let tab = &mut self.results[idx];
        let Ok(ExecOutcome::Rows(rs)) = &tab.outcome else { return None };
        if row >= rs.rows.len() {
            return None;
        }
        let deleted = tab.edits.deletes.contains(&row);
        let current =
            |c: usize, edits: &Edits| edits.updates.get(&(row, c)).cloned().unwrap_or_else(|| rs.rows[row][c].clone());
        let panel = self.panel.get_or_insert_with(|| RowPanel {
            tab: idx,
            row,
            buffers: (0..rs.columns.len())
                .map(|c| match current(c, &tab.edits) {
                    Value::Null => String::new(),
                    v => v.to_string(),
                })
                .collect(),
        });
        let meta = column_meta(rs, cx.tables);
        let pk = meta.iter().position(|m| m.primary_key);
        let title = match pk {
            Some(c) => format!("Row · {} {}", rs.columns[c].name, rs.rows[row][c]),
            None => format!("Row {}", row + 1),
        };
        let mut close = false;
        let mut back = false;
        let value_col = self.value_col.filter(|&c| c < rs.columns.len());
        let value_buffer = &mut self.value_buffer;
        let mut open_value = None;
        egui::Panel::right(egui::Id::new(("row-panel", self.id)))
            .resizable(true)
            .default_size(270.0)
            .min_size(200.0)
            .frame(
                egui::Frame::new()
                    .fill(color::BG_SUBTLE)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .stroke(Stroke::new(1.0, color::BORDER)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).font(theme::font(13.5, theme::semibold())));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(theme::flat_button(icon::X)).on_hover_text("Close").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(6.0);
                if let Some(c) = value_col {
                    let m = &meta[c];
                    let value = current(c, &tab.edits);
                    let original = &rs.rows[row][c];
                    let changed = tab.edits.updates.contains_key(&(row, c));
                    let json =
                        matches!(original, Value::Json(_)) || rs.columns[c].type_name.to_lowercase().contains("json");
                    let binary = matches!(original, Value::Bytes(_) | Value::Array(_));
                    let can_edit = editable && !deleted && !m.primary_key && !binary;
                    let key = (idx, row, c);
                    if value_buffer.as_ref().is_none_or(|(k, _)| *k != key) {
                        *value_buffer = Some((key, value_text(&value)));
                    }
                    let Some((_, buffer)) = value_buffer.as_mut() else { return };
                    ui.horizontal(|ui| {
                        if ui.add(theme::flat_button(icon::ARROW_LEFT)).on_hover_text("All fields").clicked() {
                            back = true;
                        }
                        ui.label(RichText::new(&rs.columns[c].name).font(theme::font(13.0, theme::medium())));
                        ui.label(RichText::new(rs.columns[c].type_name.to_lowercase()).small().color(color::TEXT_WEAK));
                        if value.is_null() {
                            theme::pill(ui, RichText::new("NULL").size(10.0).color(color::TEXT_WEAK), color::BG_SUNKEN);
                        }
                        if changed {
                            ui.label(RichText::new("changed").small().color(color::WARNING));
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.add(theme::flat_button(format!("{}  Copy", icon::COPY))).clicked() {
                            ui.ctx().copy_text(buffer.clone());
                        }
                        if can_edit && ui.add(theme::flat_button("Set NULL")).clicked() {
                            buffer.clear();
                            tab.edits.set(rs, RowRef::Existing(row), c, Value::Null);
                        }
                        if json
                            && can_edit
                            && let Ok(v) = serde_json::from_str::<serde_json::Value>(buffer)
                            && ui.add(theme::flat_button("Format")).clicked()
                        {
                            *buffer = serde_json::to_string_pretty(&v).unwrap_or_default();
                            tab.edits.set(rs, RowRef::Existing(row), c, Value::Text(buffer.clone()));
                        }
                        if changed && ui.add(theme::flat_button("Revert")).clicked() {
                            tab.edits.updates.remove(&(row, c));
                            *buffer = value_text(original);
                        }
                    });
                    if json
                        && !buffer.is_empty()
                        && let Err(e) = serde_json::from_str::<serde_json::Value>(buffer)
                    {
                        ui.label(RichText::new(format!("Invalid JSON: {e}")).small().color(color::DANGER));
                    }
                    ui.add_space(4.0);
                    let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap: f32| {
                        let job = if json {
                            sql_highlight::json_layout(ui, text.as_str(), wrap)
                        } else {
                            let mut job = egui::text::LayoutJob::simple(
                                text.as_str().to_owned(),
                                theme::mono(12.5),
                                ui.visuals().text_color(),
                                wrap,
                            );
                            job.wrap.max_width = wrap;
                            job
                        };
                        ui.fonts_mut(|f| f.layout_job(job))
                    };
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        let mut edit = |text: &mut dyn egui::TextBuffer, ui: &mut egui::Ui| {
                            ui.add(
                                egui::TextEdit::multiline(text)
                                    .font(theme::mono(12.5))
                                    .hint_text(if value.is_null() { "NULL" } else { "" })
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(12)
                                    .layouter(&mut layouter),
                            )
                        };
                        if can_edit {
                            if edit(buffer, ui).changed() {
                                let typed =
                                    edits::typed_value(buffer.clone(), &rs.columns[c].type_name, original.is_null());
                                tab.edits.set(rs, RowRef::Existing(row), c, typed);
                            }
                        } else {
                            // Read-only values stay selectable through an immutable buffer.
                            edit(&mut buffer.as_str(), ui);
                        }
                    });
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (c, col) in rs.columns.iter().enumerate() {
                        let m = &meta[c];
                        let value = current(c, &tab.edits);
                        let changed = tab.edits.updates.contains_key(&(row, c));
                        let can_edit = editable && !deleted && !m.primary_key;
                        ui.add_space(4.0);
                        if let Some(b) = grid::as_bool(&value, m.boolean) {
                            ui.horizontal(|ui| {
                                let r = grid::checkbox_glyph(ui, b);
                                ui.label(&col.name);
                                if changed {
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.label(RichText::new("changed").small().color(color::WARNING));
                                    });
                                }
                                if r.clicked() && can_edit {
                                    tab.edits.set(rs, RowRef::Existing(row), c, Value::Bool(!b));
                                }
                            });
                            continue;
                        }
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&col.name).color(color::TEXT));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .add(theme::flat_button(RichText::new(icon::ARROWS_OUT_SIMPLE).size(11.0)))
                                    .on_hover_text("Open in the value viewer")
                                    .clicked()
                                {
                                    open_value = Some(c);
                                }
                                let detail = if changed {
                                    RichText::new("changed").small().color(color::WARNING)
                                } else {
                                    let mut d = col.type_name.to_lowercase();
                                    if m.primary_key {
                                        d.push_str(" · PK");
                                    }
                                    RichText::new(d).small().color(color::TEXT_WEAK)
                                };
                                ui.label(detail);
                            });
                        });
                        let fill = if changed {
                            color::CHANGED
                        } else if can_edit {
                            color::BG
                        } else {
                            color::BG_SUNKEN
                        };
                        let stroke = if changed { color::CHANGED_EDGE } else { color::BORDER };
                        let r = egui::Frame::new()
                            .fill(fill)
                            .stroke(Stroke::new(1.0, stroke))
                            .corner_radius(6)
                            .inner_margin(egui::Margin::symmetric(8, 5))
                            .show(ui, |ui| {
                                ui.add_enabled(
                                    can_edit,
                                    egui::TextEdit::singleline(&mut panel.buffers[c])
                                        .frame(egui::Frame::NONE)
                                        .font(theme::mono(12.5))
                                        .hint_text(if value.is_null() { "NULL" } else { "" })
                                        .desired_width(f32::INFINITY),
                                )
                            })
                            .inner;
                        if r.changed() {
                            let typed =
                                edits::typed_value(panel.buffers[c].clone(), &col.type_name, rs.rows[row][c].is_null());
                            tab.edits.set(rs, RowRef::Existing(row), c, typed);
                        }
                        if can_edit {
                            r.context_menu(|ui| {
                                if ui.button("Set NULL").clicked() {
                                    panel.buffers[c].clear();
                                    tab.edits.set(rs, RowRef::Existing(row), c, Value::Null);
                                }
                                if let Some((label, now)) = edits::now_value(&col.type_name, dialect)
                                    && ui.button(label).clicked()
                                {
                                    panel.buffers[c] = now.to_string();
                                    tab.edits.set(rs, RowRef::Existing(row), c, now);
                                }
                                if changed && ui.button("Revert").clicked() {
                                    tab.edits.updates.remove(&(row, c));
                                    panel.buffers[c] = match &rs.rows[row][c] {
                                        Value::Null => String::new(),
                                        v => v.to_string(),
                                    };
                                }
                            });
                        }
                    }

                    let keys = incoming_keys(rs, cx.incoming);
                    let links: Vec<(&IncomingKey, Vec<Value>)> = keys
                        .iter()
                        .filter_map(|k| referencing_values(&rs.columns, &rs.rows[row], k).map(|v| (*k, v)))
                        .collect();
                    if !links.is_empty() {
                        ui.add_space(12.0);
                        ui.separator();
                        ui.label(theme::caption("Referenced by"));
                        for (k, values) in links {
                            ui.horizontal(|ui| {
                                let label = format!("{}.{}", k.table, k.foreign_key.columns.join(", "));
                                if ui.link(RichText::new(label).color(color::LINK)).clicked() {
                                    action = Some(navigate_referencing(dialect, idx, k, &values));
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(RichText::new(icon::ARROW_RIGHT).color(color::TEXT_WEAK));
                                });
                            });
                        }
                    }
                });
            });
        if close {
            self.panel_open = false;
            self.panel = None;
            self.value_col = None;
        }
        if back {
            // Field buffers are rebuilt from the edits made in the viewer.
            self.value_col = None;
            self.panel = None;
        }
        if let Some(c) = open_value {
            self.value_col = Some(c);
            self.value_buffer = None;
            self.results[idx].selection.cell = Some(c);
        }
        action
    }

    // ----- structure and DDL views ------------------------------------------

    fn structure_view(&mut self, ui: &mut egui::Ui, cx: &ConsoleContext<'_>) {
        let Some(tv) = &self.table else { return };
        let key = (tv.schema.clone(), tv.table.clone());
        egui::CentralPanel::default().frame(egui::Frame::new().fill(color::BG).inner_margin(16)).show(ui, |ui| {
            let Some(d) = cx.tables.get(&key) else {
                if !self.wanted_details.contains(&key) {
                    self.wanted_details.push(key.clone());
                }
                ui.spinner();
                return;
            };
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                section(ui, "Columns", d.columns.len());
                structure_table(ui, "cols", &["Name", "Type", "Nullable", "Default", "Key"], |ui| {
                    for c in &d.columns {
                        let fk = d.foreign_key_for(&c.name);
                        ui.horizontal(|ui| {
                            if c.pk_position.is_some() {
                                ui.label(RichText::new(icon::KEY).color(color::WARNING));
                            }
                            ui.label(RichText::new(&c.name).font(theme::font(13.0, theme::medium())));
                        });
                        ui.label(RichText::new(&c.data_type).font(theme::mono(12.0)));
                        ui.label(if c.nullable { "yes" } else { "no" });
                        ui.label(
                            RichText::new(c.default.clone().unwrap_or_default())
                                .font(theme::mono(12.0))
                                .color(color::TEXT_WEAK),
                        );
                        let mut keys = Vec::new();
                        if c.pk_position.is_some() {
                            keys.push("PK".to_string());
                        }
                        if let Some(fk) = fk {
                            keys.push(format!("FK → {}.{}", fk.ref_table, fk.ref_columns.join(", ")));
                        }
                        ui.label(RichText::new(keys.join("  ")).color(color::TEXT_WEAK));
                        ui.end_row();
                    }
                });
                section(ui, "Indexes", d.indexes.len());
                structure_table(ui, "idx", &["Name", "Columns", "Kind"], |ui| {
                    for i in &d.indexes {
                        ui.label(&i.name);
                        ui.label(RichText::new(i.columns.join(", ")).font(theme::mono(12.0)));
                        ui.label(
                            RichText::new(if i.primary {
                                "primary"
                            } else if i.unique {
                                "unique"
                            } else {
                                ""
                            })
                            .color(color::TEXT_WEAK),
                        );
                        ui.end_row();
                    }
                });
                section(ui, "Foreign keys", d.foreign_keys.len());
                structure_table(ui, "fks", &["Name", "Columns", "References"], |ui| {
                    for fk in &d.foreign_keys {
                        ui.label(&fk.name);
                        ui.label(RichText::new(fk.columns.join(", ")).font(theme::mono(12.0)));
                        ui.label(
                            RichText::new(format!(
                                "{}.{} ({})",
                                fk.ref_schema,
                                fk.ref_table,
                                fk.ref_columns.join(", ")
                            ))
                            .font(theme::mono(12.0)),
                        );
                        ui.end_row();
                    }
                });
                let incoming = cx.incoming.get(&key).map(Vec::as_slice).unwrap_or_default();
                section(ui, "Referenced by", incoming.len());
                structure_table(ui, "inc", &["Table", "Columns"], |ui| {
                    for k in incoming {
                        ui.label(format!("{}.{}", k.schema, k.table));
                        ui.label(RichText::new(k.foreign_key.columns.join(", ")).font(theme::mono(12.0)));
                        ui.end_row();
                    }
                });
            });
        });
    }

    fn ddl_view(&mut self, ui: &mut egui::Ui) {
        let Some(tv) = &self.table else { return };
        if tv.ddl.is_none() {
            self.wanted_ddl = true;
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(color::BG).inner_margin(16)).show(
            ui,
            |ui| match &tv.ddl {
                Some(Ok(ddl)) => {
                    if ui.button(format!("{}  Copy", icon::COPY)).clicked() {
                        ui.ctx().copy_text(ddl.clone());
                    }
                    ui.add_space(8.0);
                    let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
                        let job = sql_highlight::layout(ui, text.as_str(), wrap_width);
                        ui.fonts_mut(|f| f.layout_job(job))
                    };
                    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                        let mut view = ddl.as_str();
                        ui.add(
                            egui::TextEdit::multiline(&mut view)
                                .code_editor()
                                .frame(egui::Frame::NONE)
                                .desired_width(f32::INFINITY)
                                .layouter(&mut layouter),
                        );
                    });
                }
                Some(Err(e)) => {
                    ui.colored_label(color::DANGER, e);
                }
                None => {
                    ui.spinner();
                }
            },
        );
    }
}

/// A removable chip; returns whether its × was clicked.
fn chip(ui: &mut egui::Ui, text: &RichText) -> bool {
    let mut removed = false;
    egui::Frame::new()
        .fill(color::ACCENT_SOFT)
        .stroke(Stroke::new(1.0, Color32::from_rgb(214, 206, 250)))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(text.clone().color(color::ACCENT_TEXT));
            let x = ui.add(
                egui::Label::new(RichText::new(icon::X).size(11.0).color(color::ACCENT_TEXT))
                    .sense(egui::Sense::click()),
            );
            removed = x.on_hover_text("Remove").clicked();
        });
    removed
}

fn section(ui: &mut egui::Ui, title: &str, count: usize) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(theme::caption(title));
        ui.label(RichText::new(count.to_string()).small().color(color::TEXT_FAINT));
    });
    ui.add_space(4.0);
}

fn structure_table(ui: &mut egui::Ui, id: &str, headers: &[&str], body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().stroke(Stroke::new(1.0, color::BORDER)).corner_radius(8).inner_margin(10).show(ui, |ui| {
        egui::Grid::new(id).striped(true).spacing([24.0, 8.0]).min_col_width(60.0).show(ui, |ui| {
            for h in headers {
                ui.label(RichText::new(*h).small().font(theme::font(11.0, theme::semibold())).color(color::TEXT_WEAK));
            }
            ui.end_row();
            body(ui);
        });
    });
}

/// Per result column: whether it is the primary key, NOT NULL, or boolean,
/// from the origin table's details when loaded.
fn column_meta(rs: &ResultSet, tables: &HashMap<(String, String), TableDetails>) -> Vec<ColumnMeta> {
    rs.columns
        .iter()
        .map(|col| {
            let info = col.origin.as_ref().and_then(|o| {
                tables.get(&(o.schema.clone(), o.table.clone()))?.columns.iter().find(|c| c.name == o.column)
            });
            ColumnMeta {
                primary_key: info.is_some_and(|c| c.pk_position.is_some()),
                not_null: info.is_some_and(|c| !c.nullable),
                boolean: col.type_name.to_ascii_lowercase().starts_with("bool"),
            }
        })
        .collect()
}

/// Keys in other tables that point at any table this result reads from.
fn incoming_keys<'a>(
    rs: &ResultSet,
    incoming: &'a HashMap<(String, String), Vec<IncomingKey>>,
) -> Vec<&'a IncomingKey> {
    let origin_tables: std::collections::BTreeSet<(String, String)> =
        rs.columns.iter().filter_map(|c| c.origin.as_ref()).map(|o| (o.schema.clone(), o.table.clone())).collect();
    origin_tables.iter().filter_map(|t| incoming.get(t)).flatten().collect()
}

fn format_params(params: &[Value]) -> String {
    params.iter().map(|v| if v.is_null() { "NULL".into() } else { format!("'{v}'") }).collect::<Vec<_>>().join(", ")
}

/// The table every column of `rs` was read from, if there is exactly one.
fn single_table(rs: &ResultSet) -> Option<String> {
    let mut tables = rs.columns.iter().map(|c| c.origin.as_ref().map(|o| &o.table));
    let first = tables.next()??;
    tables.all(|t| t == Some(first)).then(|| first.clone())
}

fn run_fresh(statements: Vec<String>) -> Option<ConsoleAction> {
    (!statements.is_empty()).then(|| ConsoleAction::Run {
        statements: statements.into_iter().map(|s| (s, Vec::new())).collect(),
        limit: PAGE_SIZE,
        mode: RunMode::Fresh,
    })
}

fn navigate(dialect: Dialect, from: usize, link: &FkLink) -> ConsoleAction {
    let (sql, params) = dbm_core::fk_nav::navigation_query(dialect, link);
    let fk = &link.foreign_key;
    let key: Vec<String> = fk.ref_columns.iter().zip(&link.values).map(|(c, v)| format!("{c} = {v}")).collect();
    ConsoleAction::Run {
        statements: vec![(sql, params)],
        limit: PAGE_SIZE,
        mode: RunMode::Navigate { from, title: format!("{} ({})", fk.ref_table, key.join(", ")) },
    }
}

fn navigate_referencing(dialect: Dialect, from: usize, key: &IncomingKey, values: &[Value]) -> ConsoleAction {
    let (sql, params) = referencing_query(dialect, key, values);
    let cond: Vec<String> = key.foreign_key.columns.iter().zip(values).map(|(c, v)| format!("{c} = {v}")).collect();
    ConsoleAction::Run {
        statements: vec![(sql, params)],
        limit: PAGE_SIZE,
        mode: RunMode::Navigate { from, title: format!("{} ({})", key.table, cond.join(", ")) },
    }
}

fn export(rs: &ResultSet, format: &str) {
    let Some(path) = rfd::FileDialog::new()
        .add_filter(format.to_uppercase(), &[format])
        .set_file_name(format!("result.{format}"))
        .save_file()
    else {
        return;
    };
    let result = std::fs::File::create(&path).map_err(|e| e.to_string()).and_then(|file| match format {
        "csv" => dbm_core::export::write_csv(rs, file).map_err(|e| e.to_string()),
        _ => serde_json::to_writer_pretty(file, &dbm_core::export::to_json(rs)).map_err(|e| e.to_string()),
    });
    if let Err(e) = result {
        rfd::MessageDialog::new()
            .set_title("Export failed")
            .set_description(e)
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
}

/// A value as the value viewer shows it: JSON pretty-printed, bytes as a hex dump.
fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Json(j) => serde_json::to_string_pretty(j).unwrap_or_else(|_| j.to_string()),
        Value::Bytes(b) => b
            .chunks(16)
            .enumerate()
            .map(|(i, chunk)| {
                let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
                let ascii: String =
                    chunk.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }).collect();
                format!("{:08x}  {:<47}  {ascii}", i * 16, hex.join(" "))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        v => v.to_string(),
    }
}

/// Adds copies of the selected rows as new rows, in the order they are shown.
fn duplicate_selected(
    rs: &ResultSet,
    edits: &mut Edits,
    selection: &Selection,
    sort: &SortState,
    t: &EditTarget,
    details: Option<&TableDetails>,
    dialect: Dialect,
) {
    let mut order: Vec<usize> = sort.order().iter().copied().filter(|r| selection.rows.contains(r)).collect();
    if order.is_empty() {
        order = selection.rows.iter().copied().collect();
    }
    for r in order {
        let copy = edits::duplicate(rs, edits, r, t, details, dialect);
        edits.inserts.push(copy);
    }
}
