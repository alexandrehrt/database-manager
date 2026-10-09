//! A query console: SQL editor on top, one results tab per executed
//! statement below.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use dbm_core::driver::is_read_only_query;
use dbm_core::statement_at::statement_at;
use dbm_core::fk_nav::{FkLink, ForeignKeyIndex, link_for_cell};
use dbm_core::{DbError, Dialect, ExecOutcome, Value, sql_split};
use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText};

use crate::ui::grid::{self, SortState};
use crate::ui::sql_highlight;

pub const PAGE_SIZE: usize = 500;

const RUN_STATEMENT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
const RUN_ALL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Enter);

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
}

pub struct Run {
    pub id: u64,
    pub stop: Arc<AtomicBool>,
    pub started: Instant,
    pub total: usize,
    pub done: usize,
    pub mode: RunMode,
    pub params: Vec<Vec<Value>>,
}

#[derive(Clone)]
pub enum RunMode {
    /// Replace all results.
    Fresh,
    /// Re-execute one existing tab ("load more").
    Replace(usize),
    /// FK navigation from tab `from`: add a tab and make it current.
    Navigate { from: usize, title: String },
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
}

pub enum ConsoleAction {
    Run { statements: Vec<Statement>, limit: usize, mode: RunMode },
    Cancel,
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
        }
    }

    /// `fks` holds foreign keys of the tables results came from, as far as
    /// they are loaded; cells covered by one become navigation links.
    pub fn show(&mut self, ui: &mut egui::Ui, history: &[String], fks: &ForeignKeyIndex) -> Option<ConsoleAction> {
        let mut action = None;
        let editor_id = egui::Id::new(("console-editor", self.id));
        let editor_focused = ui.memory(|m| m.has_focus(editor_id));
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

        egui::Panel::top(egui::Id::new(("console-top", self.id)))
            .resizable(true)
            .default_size(220.0)
            .min_size(80.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let running = self.run.is_some();
                    if ui
                        .add_enabled(!running, egui::Button::new("Run"))
                        .on_hover_text("Statement at cursor or selection (Cmd+Enter)")
                        .clicked()
                    {
                        action = self.run_at_cursor(ui, editor_id);
                    }
                    if ui.add_enabled(!running, egui::Button::new("Run all")).on_hover_text("Cmd+Shift+Enter").clicked() {
                        action = self.run_all();
                    }
                    if ui.add_enabled(running, egui::Button::new("Cancel")).clicked() {
                        action = Some(ConsoleAction::Cancel);
                    }
                    ui.menu_button("History", |ui| {
                        if history.is_empty() {
                            ui.weak("No queries yet");
                        }
                        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                            for q in history {
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
                    if let Some(run) = &self.run {
                        ui.spinner();
                        ui.weak(format!(
                            "Running {}/{} - {:.1} s",
                            (run.done + 1).min(run.total),
                            run.total,
                            run.started.elapsed().as_secs_f32()
                        ));
                        ui.ctx().request_repaint_after(Duration::from_millis(100));
                    }
                });
                ui.separator();

                let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
                    let job = sql_highlight::layout(ui, text.as_str(), wrap_width);
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                egui::ScrollArea::vertical().id_salt(("editor-scroll", self.id)).auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_sized(
                        ui.available_size(),
                        egui::TextEdit::multiline(&mut self.sql)
                            .id(editor_id)
                            .code_editor()
                            .lock_focus(true)
                            .desired_width(f32::INFINITY)
                            .hint_text("Write SQL here. Cmd+Enter runs the statement under the cursor.")
                            .layouter(&mut layouter),
                    );
                });
            });

        if self.run.is_none() {
            if run_one {
                action = self.run_at_cursor(ui, editor_id);
            } else if run_all {
                action = self.run_all();
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(a) = self.results_ui(ui, fks) {
                action = Some(a);
            }
        });
        action
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

    fn tab_label(&self, i: usize) -> String {
        self.results[i].title.clone().unwrap_or_else(|| format!("Result {}", i + 1))
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, fks: &ForeignKeyIndex) -> Option<ConsoleAction> {
        if self.results.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak(if self.run.is_some() { "Running…" } else { "Results appear here." });
            });
            return None;
        }
        let mut action = None;
        if self.results.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                for (i, tab) in self.results.iter().enumerate() {
                    let failed = tab.outcome.is_err();
                    let mut text = RichText::new(self.tab_label(i));
                    if failed {
                        text = text.color(ui.visuals().error_fg_color);
                    }
                    if ui.selectable_label(self.active_result == i, text).on_hover_text(&tab.sql).clicked() {
                        self.active_result = i;
                    }
                }
            });
            ui.separator();
        }
        if !self.nav_back.is_empty() || !self.nav_forward.is_empty() {
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.nav_back.is_empty(), egui::Button::new("< Back")).clicked() {
                    self.go_back();
                }
                if ui.add_enabled(!self.nav_forward.is_empty(), egui::Button::new("Forward >")).clicked() {
                    self.go_forward();
                }
                let trail: Vec<String> = self
                    .nav_back
                    .iter()
                    .chain(std::iter::once(&self.active_result))
                    .map(|&i| self.tab_label(i.min(self.results.len() - 1)))
                    .collect();
                ui.weak(trail.join("  >  "));
            });
            ui.separator();
        }
        let idx = self.active_result.min(self.results.len() - 1);
        let console_id = self.id;
        let running = self.run.is_some();
        let tab = &mut self.results[idx];

        ui.horizontal(|ui| match &tab.outcome {
            Ok(ExecOutcome::Rows(rs)) => {
                let more = if rs.truncated { "+" } else { "" };
                ui.label(format!("{}{more} rows", rs.rows.len()));
                ui.weak(format!("in {} ms", tab.elapsed.as_millis()));
                if rs.truncated && is_read_only_query(&tab.sql) {
                    if ui.add_enabled(!running, egui::Button::new("Load more")).clicked() {
                        action = Some(ConsoleAction::Run {
                            statements: vec![(tab.sql.clone(), tab.params.clone())],
                            limit: tab.limit + PAGE_SIZE,
                            mode: RunMode::Replace(idx),
                        });
                    }
                } else if rs.truncated {
                    ui.weak("(more rows not fetched)");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Export JSON").clicked() {
                        export(rs, "json");
                    }
                    if ui.button("Export CSV").clicked() {
                        export(rs, "csv");
                    }
                });
            }
            Ok(ExecOutcome::Affected(n)) => {
                ui.label(format!("{n} row(s) affected"));
                ui.weak(format!("in {} ms", tab.elapsed.as_millis()));
            }
            Err(e) => {
                ui.vertical(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, &e.message);
                    if let Some(detail) = &e.detail {
                        ui.weak(detail);
                    }
                    if let Some(code) = &e.code {
                        ui.weak(format!("code {code}"));
                    }
                });
            }
        });
        ui.separator();
        if let Ok(ExecOutcome::Rows(rs)) = &tab.outcome {
            let is_link = |row: usize, col: usize| link_for_cell(&rs.columns, &rs.rows[row], col, fks).is_some();
            if let Some((row, col)) = grid::show(ui, (console_id, idx), rs, &mut tab.sort, &is_link)
                && !running
                && let Some(link) = link_for_cell(&rs.columns, &rs.rows[row], col, fks)
            {
                action = Some(navigate(self.dialect, idx, &link));
            }
        } else {
            ui.add(egui::Label::new(RichText::new(&tab.sql).monospace().weak()).wrap());
        }
        action
    }
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
    let key: Vec<String> =
        fk.ref_columns.iter().zip(&link.values).map(|(c, v)| format!("{c} = {v}")).collect();
    ConsoleAction::Run {
        statements: vec![(sql, params)],
        limit: PAGE_SIZE,
        mode: RunMode::Navigate { from, title: format!("{} ({})", fk.ref_table, key.join(", ")) },
    }
}

fn export(rs: &dbm_core::ResultSet, format: &str) {
    let Some(path) = rfd::FileDialog::new().add_filter(format.to_uppercase(), &[format]).set_file_name(format!("result.{format}")).save_file()
    else {
        return;
    };
    let result = std::fs::File::create(&path).map_err(|e| e.to_string()).and_then(|file| match format {
        "csv" => dbm_core::export::write_csv(rs, file).map_err(|e| e.to_string()),
        _ => serde_json::to_writer_pretty(file, &dbm_core::export::to_json(rs)).map_err(|e| e.to_string()),
    });
    if let Err(e) = result {
        rfd::MessageDialog::new().set_title("Export failed").set_description(e).set_level(rfd::MessageLevel::Error).show();
    }
}
