//! Virtualized results grid: typed two-line headers, click-to-sort, row
//! selection and copying, and for editable results inline cell editing.

use std::collections::BTreeSet;

use dbm_core::{Dialect, ResultSet, Value, export};
use eframe::egui::{self, Color32, RichText, Stroke};
use egui_extras::{Column, TableBuilder};

use crate::ui::edits::{Edits, RowRef};
use crate::ui::theme::{self, color, icon};

/// The cell being edited and its text so far.
pub struct EditingCell {
    pub row: RowRef,
    pub col: usize,
    pub buffer: String,
    focused: bool,
}

/// Editing state handed to the grid when the result can be written back.
pub struct GridEdit<'a> {
    pub edits: &'a mut Edits,
    pub editing: &'a mut Option<EditingCell>,
}

/// What the grid shows about each result column beyond its name and type.
#[derive(Clone, Default)]
pub struct ColumnMeta {
    pub primary_key: bool,
    pub not_null: bool,
    pub boolean: bool,
}

/// Client-side sort state: column index and ascending flag, plus the row
/// order it produces.
#[derive(Default)]
pub struct SortState {
    pub column: Option<(usize, bool)>,
    order: Vec<usize>,
    sorted_len: usize,
}

impl SortState {
    fn toggle(&mut self, col: usize) {
        self.column = match self.column {
            Some((c, true)) if c == col => Some((col, false)),
            Some((c, false)) if c == col => None,
            _ => Some((col, true)),
        };
        self.sorted_len = usize::MAX;
    }

    fn refresh(&mut self, rs: &ResultSet) {
        if self.sorted_len == rs.rows.len() && self.order.len() == rs.rows.len() {
            return;
        }
        self.order = (0..rs.rows.len()).collect();
        if let Some((col, asc)) = self.column {
            self.order.sort_by(|&a, &b| {
                let ord = rs.rows[a][col].sort_cmp(&rs.rows[b][col]);
                if asc { ord } else { ord.reverse() }
            });
        }
        self.sorted_len = rs.rows.len();
    }
}

/// Selected fetched rows (data indices) and the anchor for Shift+click ranges.
#[derive(Default)]
pub struct Selection {
    pub rows: BTreeSet<usize>,
    anchor: Option<usize>,
}

impl Selection {
    /// Applies a click on the row shown at `display` (data row `row`).
    fn click(&mut self, display: usize, row: usize, order: &[usize], modifiers: egui::Modifiers) {
        if modifiers.shift
            && let Some(anchor) = self.anchor
        {
            let (a, b) = (anchor.min(display), anchor.max(display));
            if !modifiers.command {
                self.rows.clear();
            }
            self.rows.extend(order[a..=b.min(order.len() - 1)].iter().copied());
        } else if modifiers.command {
            if !self.rows.remove(&row) {
                self.rows.insert(row);
            }
            self.anchor = Some(display);
        } else {
            self.rows = BTreeSet::from([row]);
            self.anchor = Some(display);
        }
    }

    /// Selected rows in the order they are shown.
    fn in_display_order(&self, order: &[usize]) -> Vec<usize> {
        order.iter().copied().filter(|r| self.rows.contains(r)).collect()
    }

    /// The selected row when exactly one is selected.
    pub fn single(&self) -> Option<usize> {
        (self.rows.len() == 1).then(|| self.rows.first().copied()).flatten()
    }
}

/// Everything the grid needs besides the data.
pub struct GridOptions<'a> {
    pub id: (u64, usize),
    pub columns: &'a [ColumnMeta],
    /// Marks cells drawn as foreign-key links.
    pub is_link: &'a dyn Fn(usize, usize) -> bool,
    /// Present when the result can be edited.
    pub edit: Option<GridEdit<'a>>,
    pub selection: &'a mut Selection,
    pub dialect: Dialect,
    /// The single table the rows come from, for "Copy as INSERT".
    pub table: Option<&'a str>,
    /// For a fetched row: the referencing tables that can be opened, as
    /// (key index, label).
    pub referencing: &'a dyn Fn(usize) -> Vec<(usize, String)>,
    /// `Some` when the data is sorted by the server (table data): holds the
    /// current sort for the header arrows, and header clicks are reported as
    /// [`GridEvent::SortBy`] instead of sorting the loaded rows.
    pub server_sort: Option<Option<(usize, bool)>>,
}

/// Navigation the caller performs, in data-row terms.
pub enum GridEvent {
    /// A foreign-key link cell (row, column) was clicked.
    Link(usize, usize),
    /// Open the rows of referencing key `key` that point at `row`.
    Referencing { row: usize, key: usize },
    /// Header of column `col` clicked while sorting is server-side.
    SortBy(usize),
}

#[derive(Clone, Copy)]
enum CopyFormat {
    Tsv,
    Csv,
    Json,
    Markdown,
    Insert,
}

fn copy_rows(
    ctx: &egui::Context,
    rs: &ResultSet,
    rows: &[usize],
    format: CopyFormat,
    dialect: Dialect,
    table: Option<&str>,
) {
    let subset = export::subset(rs, rows);
    let text = match format {
        CopyFormat::Tsv => export::to_tsv(&subset),
        CopyFormat::Csv => {
            let mut buf = Vec::new();
            if export::write_csv(&subset, &mut buf).is_err() {
                return;
            }
            String::from_utf8_lossy(&buf).into_owned()
        }
        CopyFormat::Json => serde_json::to_string_pretty(&export::to_json(&subset)).unwrap_or_default(),
        CopyFormat::Markdown => export::to_markdown(&subset),
        CopyFormat::Insert => match table {
            Some(t) => export::to_inserts(dialect, t, &subset),
            None => return,
        },
    };
    ctx.copy_text(text);
}

const SELECT_ALL: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::A);

const MAX_CELL_CHARS: usize = 300;

fn cell_text(v: &Value) -> String {
    let mut s: String = v.to_string().chars().take(MAX_CELL_CHARS).collect();
    if s.contains(['\n', '\r']) {
        s = s.replace("\r\n", " ").replace(['\n', '\r'], " ");
    }
    s
}

/// The boolean a cell shows as a checkbox: real booleans, and SQLite's 0/1
/// in BOOLEAN columns.
pub fn as_bool(v: &Value, boolean_column: bool) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Int(i @ (0 | 1)) if boolean_column => Some(*i == 1),
        _ => None,
    }
}

fn is_numeric(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_) | Value::Numeric(_))
}

/// A checkbox drawn in the accent colour.
pub fn checkbox_glyph(ui: &mut egui::Ui, checked: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(15.0, 15.0), egui::Sense::click());
    let painter = ui.painter();
    if checked {
        painter.rect_filled(rect, 3.0, color::ACCENT);
        let a = rect.left_center() + egui::vec2(3.5, 0.0);
        let b = rect.center_bottom() + egui::vec2(-1.0, -4.0);
        let c = rect.right_top() + egui::vec2(-3.5, 4.0);
        painter.line(vec![a, b, c], Stroke::new(1.8, Color32::WHITE));
    } else {
        painter.rect_stroke(rect, 3.0, Stroke::new(1.0, Color32::from_rgb(190, 190, 200)), egui::StrokeKind::Inside);
    }
    response
}

/// Returns the navigation the user asked for this frame (FK link,
/// referencing rows or a server-side sort), in data order. Click,
/// Shift+click and Cmd+click select rows; Cmd+A selects all, Cmd+C copies
/// the selection as TSV and Escape clears it. With `edit`, cells can be
/// edited (double-click; checkboxes toggle), set to NULL and reverted, rows
/// deleted, and added rows are shown after the fetched ones.
pub fn show(ui: &mut egui::Ui, rs: &ResultSet, sort: &mut SortState, opts: GridOptions<'_>) -> Option<GridEvent> {
    let GridOptions { id, columns, is_link, mut edit, selection, dialect, table, referencing, server_sort } = opts;
    let meta = |c: usize| columns.get(c).cloned().unwrap_or_default();
    let shown_sort = server_sort.unwrap_or(sort.column);
    sort.refresh(rs);
    selection.rows.retain(|&r| r < rs.rows.len());

    // Grid shortcuts apply when no text field has the keyboard.
    let free = ui.memory(|m| m.focused().is_none());
    let hovered = ui.rect_contains_pointer(ui.available_rect_before_wrap());
    if free && hovered {
        if ui.input_mut(|i| i.consume_shortcut(&SELECT_ALL)) {
            selection.rows = (0..rs.rows.len()).collect();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            selection.rows.clear();
        }
    }
    if free && !selection.rows.is_empty() && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
        copy_rows(ui.ctx(), rs, &selection.in_display_order(&sort.order), CopyFormat::Tsv, dialect, table);
    }
    let modifiers = ui.input(|i| i.modifiers);
    let mut clicked_row: Option<(usize, usize)> = None;
    let mut context_row: Option<usize> = None;
    let mut copy_request: Option<CopyFormat> = None;
    let added = edit.as_ref().map_or(0, |e| e.edits.inserts.len());
    let mut event = None;
    let mut clicked_header = None;
    let mono = theme::mono(12.5);

    egui::ScrollArea::horizontal().id_salt(("grid-h", &id)).auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 0.0);
        TableBuilder::new(ui)
            .id_salt(("grid", &id))
            .striped(false)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(40.0))
            .columns(Column::initial(150.0).at_least(48.0).clip(true), rs.columns.len())
            .header(38.0, |mut header| {
                header.col(|ui| {
                    let cell = ui.max_rect().expand2(egui::vec2(3.0, 0.0));
                    ui.painter().rect_filled(cell, 0.0, color::BG_SUBTLE);
                    ui.painter().hline(cell.x_range(), cell.bottom(), Stroke::new(1.0, color::BORDER));
                });
                for (i, col) in rs.columns.iter().enumerate() {
                    header.col(|ui| {
                        let cell = ui.max_rect().expand2(egui::vec2(3.0, 0.0));
                        ui.painter().rect_filled(cell, 0.0, color::BG_SUBTLE);
                        ui.painter().vline(cell.left(), cell.y_range(), Stroke::new(1.0, color::BORDER));
                        ui.painter().hline(cell.x_range(), cell.bottom(), Stroke::new(1.0, color::BORDER));
                        let m = meta(i);
                        let arrow = match shown_sort {
                            Some((c, true)) if c == i => format!(" {}", icon::ARROW_UP),
                            Some((c, false)) if c == i => format!(" {}", icon::ARROW_DOWN),
                            _ => String::new(),
                        };
                        let r = ui
                            .vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 1.0;
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    if m.primary_key {
                                        ui.label(RichText::new(icon::KEY).size(11.0).color(color::WARNING));
                                    }
                                    ui.label(
                                        RichText::new(format!("{}{arrow}", col.name))
                                            .font(theme::font(12.5, theme::semibold())),
                                    );
                                });
                                let mut ty = if col.type_name.is_empty() {
                                    "expression".to_string()
                                } else {
                                    col.type_name.to_lowercase()
                                };
                                if m.not_null {
                                    ty.push_str(" · not null");
                                }
                                ui.label(RichText::new(ty).size(10.5).color(color::TEXT_WEAK));
                            })
                            .response
                            .interact(egui::Sense::click());
                        if r.clicked() {
                            clicked_header = Some(i);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(26.0, sort.order.len() + added, |mut row| {
                    let idx = row.index();
                    let row_ref = if idx < sort.order.len() {
                        RowRef::Existing(sort.order[idx])
                    } else {
                        RowRef::New(idx - sort.order.len())
                    };
                    let deleted =
                        matches!((row_ref, &edit), (RowRef::Existing(r), Some(e)) if e.edits.deletes.contains(&r));
                    let selected = matches!(row_ref, RowRef::Existing(r) if selection.rows.contains(&r));
                    let row_fill = if deleted {
                        Some(color::DELETED)
                    } else if matches!(row_ref, RowRef::New(_)) {
                        Some(color::ADDED)
                    } else if selected {
                        Some(color::ACCENT_SOFT)
                    } else {
                        None
                    };
                    row.col(|ui| {
                        let cell = ui.max_rect().expand2(egui::vec2(3.0, 0.0));
                        if let Some(fill) = row_fill {
                            ui.painter().rect_filled(cell, 0.0, fill);
                        }
                        ui.painter().hline(cell.x_range(), cell.bottom(), Stroke::new(1.0, color::BORDER));
                        let (text, tint) = match row_ref {
                            RowRef::Existing(r) => {
                                ((r + 1).to_string(), if selected { color::ACCENT_TEXT } else { color::TEXT_FAINT })
                            }
                            RowRef::New(_) => (icon::PLUS.to_string(), color::SUCCESS),
                        };
                        let num = ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(4.0);
                            ui.label(RichText::new(text).size(11.5).color(tint))
                        });
                        let click = ui.interact(cell, ui.id().with(("rownum", idx)), egui::Sense::click());
                        if let RowRef::Existing(r) = row_ref
                            && (click.clicked() || num.inner.clicked())
                        {
                            clicked_row = Some((idx, r));
                        }
                    });
                    for c in 0..rs.columns.len() {
                        row.col(|ui| {
                            let m = meta(c);
                            // Pending value (None = column default on a new row), else the fetched one.
                            let (value, changed): (Option<Value>, bool) = match (row_ref, &edit) {
                                (RowRef::New(i), Some(e)) => (e.edits.inserts[i][c].clone(), true),
                                (RowRef::Existing(r), Some(e)) => match e.edits.updates.get(&(r, c)) {
                                    Some(v) => (Some(v.clone()), true),
                                    None => (Some(rs.rows[r][c].clone()), false),
                                },
                                (RowRef::Existing(r), None) => (Some(rs.rows[r][c].clone()), false),
                                (RowRef::New(_), None) => (None, false),
                            };
                            let cell = ui.max_rect().expand2(egui::vec2(3.0, 0.0));
                            let painter = ui.painter().clone();
                            if changed && matches!(row_ref, RowRef::Existing(_)) && !deleted {
                                painter.rect_filled(cell, 0.0, color::CHANGED);
                                let edge = egui::Rect::from_min_size(cell.min, egui::vec2(2.0, cell.height()));
                                painter.rect_filled(edge, 0.0, color::CHANGED_EDGE);
                            } else if let Some(fill) = row_fill {
                                painter.rect_filled(cell, 0.0, fill);
                            }
                            painter.hline(cell.x_range(), cell.bottom(), Stroke::new(1.0, color::BORDER));
                            painter.vline(cell.left(), cell.y_range(), Stroke::new(1.0, color::BORDER));

                            if let Some(e) = edit.as_mut()
                                && let Some(cell) = e.editing.as_mut()
                                && cell.row == row_ref
                                && cell.col == c
                            {
                                let r = ui.add(
                                    egui::TextEdit::singleline(&mut cell.buffer)
                                        .font(mono.clone())
                                        .desired_width(f32::INFINITY)
                                        .margin(egui::vec2(3.0, 1.0)),
                                );
                                if !cell.focused {
                                    r.request_focus();
                                    cell.focused = true;
                                }
                                let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
                                if escape {
                                    *e.editing = None;
                                } else if r.lost_focus() {
                                    let text = std::mem::take(&mut cell.buffer);
                                    *e.editing = None;
                                    e.edits.set(rs, row_ref, c, Value::Text(text));
                                }
                                return;
                            }

                            let boolean = value.as_ref().and_then(|v| as_bool(v, m.boolean));
                            let numeric = value.as_ref().is_some_and(is_numeric);
                            let link = match row_ref {
                                RowRef::Existing(r) if !changed && !deleted && is_link(r, c) => Some(r),
                                _ => None,
                            };
                            let layout = if numeric && boolean.is_none() && link.is_none() {
                                egui::Layout::right_to_left(egui::Align::Center)
                            } else {
                                egui::Layout::left_to_right(egui::Align::Center)
                            };
                            let mut toggled = false;
                            let response = ui
                                .with_layout(layout, |ui| match (&value, boolean) {
                                    (None, _) => {
                                        let placeholder = if m.primary_key { "auto" } else { "default" };
                                        ui.label(RichText::new(placeholder).font(mono.clone()).color(color::TEXT_FAINT))
                                    }
                                    (Some(Value::Null), _) => theme::pill(
                                        ui,
                                        RichText::new("NULL").size(10.0).color(color::TEXT_WEAK),
                                        color::BG_SUNKEN,
                                    ),
                                    (Some(_), Some(b)) => {
                                        let r = checkbox_glyph(ui, b);
                                        if r.clicked() && !deleted && edit.is_some() {
                                            toggled = true;
                                        }
                                        r
                                    }
                                    (Some(v), None) => {
                                        let mut text = RichText::new(cell_text(v)).font(mono.clone());
                                        if deleted {
                                            text = text.strikethrough().color(color::TEXT_WEAK);
                                        }
                                        if link.is_some() {
                                            ui.link(text.color(color::LINK)).on_hover_text("Open the referenced row")
                                        } else {
                                            ui.add(egui::Label::new(text).truncate().selectable(false))
                                        }
                                    }
                                })
                                .inner;
                            if toggled && let (Some(e), Some(b)) = (edit.as_mut(), boolean) {
                                e.edits.set(rs, row_ref, c, Value::Bool(!b));
                            }
                            if let Some(r) = link
                                && response.clicked()
                            {
                                event = Some(GridEvent::Link(r, c));
                            }
                            // The whole cell selects the row, not just its text.
                            let whole = ui.interact(cell, ui.id().with(("cell", idx, c)), egui::Sense::click());
                            if let RowRef::Existing(r) = row_ref {
                                if whole.clicked() && link.is_none() {
                                    clicked_row = Some((idx, r));
                                }
                                if whole.secondary_clicked() && !selection.rows.contains(&r) {
                                    context_row = Some(r);
                                }
                            }
                            if let Some(e) = edit.as_mut()
                                && whole.double_clicked()
                                && !deleted
                                && boolean.is_none()
                            {
                                let buffer = match &value {
                                    Some(v) if !v.is_null() => v.to_string(),
                                    _ => String::new(),
                                };
                                *e.editing = Some(EditingCell { row: row_ref, col: c, buffer, focused: false });
                            }
                            let selected_count = selection.rows.len().max(1);
                            whole.context_menu(|ui| {
                                if let RowRef::Existing(r) = row_ref {
                                    let targets = referencing(r);
                                    if !targets.is_empty() {
                                        ui.menu_button("Referencing rows", |ui| {
                                            for (key, label) in targets {
                                                if ui.button(label).clicked() {
                                                    event = Some(GridEvent::Referencing { row: r, key });
                                                }
                                            }
                                        });
                                        ui.separator();
                                    }
                                }
                                if ui.button("Copy value").clicked() {
                                    let copied = match &value {
                                        Some(v) if !v.is_null() => v.to_string(),
                                        _ => String::new(),
                                    };
                                    ui.ctx().copy_text(copied);
                                }
                                if matches!(row_ref, RowRef::Existing(_)) {
                                    let label = if selected_count == 1 {
                                        "Copy row as".to_string()
                                    } else {
                                        format!("Copy {selected_count} rows as")
                                    };
                                    ui.menu_button(label, |ui| {
                                        for (name, format) in [
                                            ("TSV (spreadsheet)", CopyFormat::Tsv),
                                            ("CSV", CopyFormat::Csv),
                                            ("JSON", CopyFormat::Json),
                                            ("Markdown table", CopyFormat::Markdown),
                                        ] {
                                            if ui.button(name).clicked() {
                                                copy_request = Some(format);
                                            }
                                        }
                                        let insert = ui
                                            .add_enabled(table.is_some(), egui::Button::new("INSERT statements"))
                                            .on_disabled_hover_text("Rows must come from a single table");
                                        if insert.clicked() {
                                            copy_request = Some(CopyFormat::Insert);
                                        }
                                    });
                                }
                                let Some(e) = edit.as_mut() else { return };
                                ui.separator();
                                if !deleted && ui.button("Set NULL").clicked() {
                                    e.edits.set(rs, row_ref, c, Value::Null);
                                }
                                match row_ref {
                                    RowRef::Existing(r) => {
                                        if changed && ui.button("Revert cell").clicked() {
                                            e.edits.updates.remove(&(r, c));
                                        }
                                        if deleted {
                                            if ui.button("Undo delete").clicked() {
                                                e.edits.deletes.remove(&r);
                                            }
                                        } else if ui.button("Delete row").clicked() {
                                            e.edits.deletes.insert(r);
                                        }
                                    }
                                    RowRef::New(i) => {
                                        if ui.button("Remove new row").clicked() {
                                            e.edits.inserts.remove(i);
                                            *e.editing = None;
                                        }
                                    }
                                }
                            });
                        });
                    }
                });
            });
    });

    if let Some(col) = clicked_header {
        if server_sort.is_some() {
            event = Some(GridEvent::SortBy(col));
        } else {
            sort.toggle(col);
        }
    }
    if let Some((display, row)) = clicked_row {
        selection.click(display, row, &sort.order, modifiers);
    }
    if let Some(row) = context_row {
        selection.rows = BTreeSet::from([row]);
        selection.anchor = sort.order.iter().position(|&r| r == row);
    }
    if let Some(format) = copy_request {
        copy_rows(ui.ctx(), rs, &selection.in_display_order(&sort.order), format, dialect, table);
    }
    event
}
