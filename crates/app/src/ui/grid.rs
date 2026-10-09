//! Virtualized results grid with click-to-sort headers.

use dbm_core::{ResultSet, Value};
use eframe::egui::{self, RichText};
use egui_extras::{Column, TableBuilder};

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

const MAX_CELL_CHARS: usize = 300;

fn cell_text(v: &Value) -> String {
    let mut s: String = v.to_string().chars().take(MAX_CELL_CHARS).collect();
    if s.contains(['\n', '\r']) {
        s = s.replace("\r\n", " ").replace(['\n', '\r'], " ");
    }
    s
}

pub fn show(ui: &mut egui::Ui, id: (u64, usize), rs: &ResultSet, sort: &mut SortState) {
    sort.refresh(rs);
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
                body.rows(row_height, sort.order.len(), |mut row| {
                    let data_row = sort.order[row.index()];
                    row.col(|ui| {
                        ui.weak((data_row + 1).to_string());
                    });
                    for value in &rs.rows[data_row] {
                        row.col(|ui| {
                            let response = if value.is_null() {
                                ui.label(RichText::new("NULL").italics().weak())
                            } else {
                                ui.add(egui::Label::new(RichText::new(cell_text(value)).text_style(mono.clone())).truncate())
                            };
                            response.context_menu(|ui| {
                                if ui.button("Copy value").clicked() {
                                    ui.ctx().copy_text(if value.is_null() { String::new() } else { value.to_string() });
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
}
