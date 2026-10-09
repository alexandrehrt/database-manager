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
use crate::ui::sql_highlight;
use crate::ui::theme::{self, color, icon};

pub const PAGE_SIZE: usize = 500;

const RUN_STATEMENT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
const RUN_ALL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Enter);
const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const REFRESH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);
pub const SAVE_AS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S);
pub const OPEN_FILE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);

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
            let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
                let job = sql_highlight::layout(ui, text.as_str(), wrap_width);
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
                r.request_focus();
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
                        ui.add_space(8.0);
                        ui.label(RichText::new(&tab.sql).font(theme::mono(12.0)).color(color::TEXT_WEAK));
                    });
                });
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
        };
        let mut sort_request = None;
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
        action
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
                if let Some(e) = &tab.edit_error {
                    ui.colored_label(color::DANGER, e.lines().next().unwrap_or_default()).on_hover_text(e);
                }

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
                                tab.edits.set(rs, RowRef::Existing(row), c, Value::Text(buffer.clone()));
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
                            tab.edits.set(rs, RowRef::Existing(row), c, Value::Text(panel.buffers[c].clone()));
                        }
                        if can_edit {
                            r.context_menu(|ui| {
                                if ui.button("Set NULL").clicked() {
                                    panel.buffers[c].clear();
                                    tab.edits.set(rs, RowRef::Existing(row), c, Value::Null);
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
