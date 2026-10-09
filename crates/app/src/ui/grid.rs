//! Virtualized results grid with click-to-sort headers and, for editable
//! results, inline cell editing.

use std::collections::BTreeSet;

use dbm_core::{Dialect, ResultSet, Value, export};
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use crate::ui::edits::{Edits, RowRef};

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

// Premultiplied alpha: each channel is already scaled by alpha / 255.
const UPDATED: Color32 = Color32::from_rgba_premultiplied(15, 26, 50, 55);
const INSERTED: Color32 = Color32::from_rgba_premultiplied(10, 33, 16, 50);
const DELETED: Color32 = Color32::from_rgba_premultiplied(43, 12, 12, 50);

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
}

/// Everything the grid needs besides the data.
pub struct GridOptions<'a> {
    pub id: (u64, usize),
    /// Marks cells drawn as foreign-key links.
    pub is_link: &'a dyn Fn(usize, usize) -> bool,
    /// Present when the result can be edited.
    pub edit: Option<GridEdit<'a>>,
    pub selection: &'a mut Selection,
    pub dialect: Dialect,
    /// The single table the rows come from, for "Copy as INSERT".
    pub table: Option<&'a str>,
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

/// Returns the (row, column) of a foreign-key link clicked this frame, in
/// data order. Click, Shift+click and Cmd+click select rows; Cmd+A selects
/// all, Cmd+C copies the selection as TSV and Escape clears it. With
/// `edit`, cells can be edited (double-click), set to NULL and reverted,
/// rows deleted, and added rows are shown after the fetched ones.
pub fn show(ui: &mut egui::Ui, rs: &ResultSet, sort: &mut SortState, opts: GridOptions<'_>) -> Option<(usize, usize)> {
    let GridOptions { id, is_link, mut edit, selection, dialect, table } = opts;
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
    let mut clicked_link = None;
    let mut clicked_header = None;
    let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let mono = egui::TextStyle::Monospace;

    egui::ScrollArea::horizontal().id_salt(("grid-h", &id)).auto_shrink([false, false]).show(ui, |ui| {
        TableBuilder::new(ui)
            .id_salt(("grid", &id))
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::auto().at_least(36.0))
            .columns(Column::initial(140.0).at_least(40.0).clip(true), rs.columns.len())
            .header(row_height + 4.0, |mut header| {
                header.col(|ui| {
                    ui.weak("#");
                });
                for (i, col) in rs.columns.iter().enumerate() {
                    header.col(|ui| {
                        let arrow = match sort.column {
                            Some((c, true)) if c == i => " ^",
                            Some((c, false)) if c == i => " v",
                            _ => "",
                        };
                        let r = ui
                            .add(
                                egui::Label::new(RichText::new(format!("{}{arrow}", col.name)).strong())
                                    .sense(egui::Sense::click())
                                    .selectable(false),
                            )
                            .on_hover_text(if col.type_name.is_empty() {
                                "expression".to_string()
                            } else {
                                col.type_name.clone()
                            });
                        if r.clicked() {
                            clicked_header = Some(i);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, sort.order.len() + added, |mut row| {
                    let idx = row.index();
                    let row_ref = if idx < sort.order.len() {
                        RowRef::Existing(sort.order[idx])
                    } else {
                        RowRef::New(idx - sort.order.len())
                    };
                    let deleted =
                        matches!((row_ref, &edit), (RowRef::Existing(r), Some(e)) if e.edits.deletes.contains(&r));
                    let selected = matches!(row_ref, RowRef::Existing(r) if selection.rows.contains(&r));
                    row.col(|ui| match row_ref {
                        RowRef::Existing(r) => {
                            if selected {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, ui.visuals().selection.bg_fill);
                            }
                            let num = ui.add(
                                egui::Label::new(RichText::new((r + 1).to_string()).weak())
                                    .sense(egui::Sense::click())
                                    .selectable(false),
                            );
                            if num.clicked() {
                                clicked_row = Some((idx, r));
                            }
                        }
                        RowRef::New(_) => {
                            ui.label(RichText::new("+").strong().color(Color32::from_rgb(60, 160, 80)));
                        }
                    });
                    for c in 0..rs.columns.len() {
                        row.col(|ui| {
                            // Pending value (Some(None) = column default on a new row), else the fetched one.
                            let (value, changed): (Option<Value>, bool) = match (row_ref, &edit) {
                                (RowRef::New(i), Some(e)) => (e.edits.inserts[i][c].clone(), true),
                                (RowRef::Existing(r), Some(e)) => match e.edits.updates.get(&(r, c)) {
                                    Some(v) => (Some(v.clone()), true),
                                    None => (Some(rs.rows[r][c].clone()), false),
                                },
                                (RowRef::Existing(r), None) => (Some(rs.rows[r][c].clone()), false),
                                (RowRef::New(_), None) => (None, false),
                            };
                            let tint = if deleted {
                                Some(DELETED)
                            } else if matches!(row_ref, RowRef::New(_)) {
                                Some(INSERTED)
                            } else if changed {
                                Some(UPDATED)
                            } else {
                                None
                            };
                            if let Some(color) = tint {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, color);
                            } else if selected {
                                let fill = ui.visuals().selection.bg_fill.gamma_multiply(0.45);
                                ui.painter().rect_filled(ui.max_rect(), 0.0, fill);
                            }

                            if let Some(e) = edit.as_mut()
                                && let Some(cell) = e.editing.as_mut()
                                && cell.row == row_ref
                                && cell.col == c
                            {
                                let r = ui.add(
                                    egui::TextEdit::singleline(&mut cell.buffer)
                                        .font(mono.clone())
                                        .desired_width(f32::INFINITY),
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

                            let mut text = match &value {
                                None => RichText::new("<default>").italics().weak(),
                                Some(Value::Null) => RichText::new("NULL").italics().weak(),
                                Some(v) => RichText::new(cell_text(v)).text_style(mono.clone()),
                            };
                            if deleted {
                                text = text.strikethrough();
                            }
                            let link = match (row_ref, &value) {
                                (RowRef::Existing(r), Some(v)) if !changed && !v.is_null() && !deleted => {
                                    is_link(r, c).then_some(r)
                                }
                                _ => None,
                            };
                            let response = if let Some(r) = link {
                                let resp = ui.link(text);
                                if resp.clicked() {
                                    clicked_link = Some((r, c));
                                }
                                resp.on_hover_text("Open the referenced row")
                            } else {
                                ui.add(egui::Label::new(text).truncate().sense(egui::Sense::click()).selectable(false))
                            };
                            if let RowRef::Existing(r) = row_ref {
                                if response.clicked() && link.is_none() {
                                    clicked_row = Some((idx, r));
                                }
                                if response.secondary_clicked() && !selection.rows.contains(&r) {
                                    context_row = Some(r);
                                }
                            }
                            let selected_count = selection.rows.len().max(1);
                            if let Some(e) = edit.as_mut()
                                && response.double_clicked()
                                && !deleted
                            {
                                let buffer = match &value {
                                    Some(v) if !v.is_null() => v.to_string(),
                                    _ => String::new(),
                                };
                                *e.editing = Some(EditingCell { row: row_ref, col: c, buffer, focused: false });
                            }
                            response.context_menu(|ui| {
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
        sort.toggle(col);
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
    clicked_link
}
