//! ER diagram of a schema: tables as boxes with their columns, foreign keys
//! as connectors between the referencing and referenced columns. Laid out
//! automatically (referenced tables to the left), with drag, pan and zoom.

use std::collections::{HashMap, HashSet};

use dbm_core::{RelationKind, TableDetails};
use eframe::egui::{self, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};

use crate::ui::theme::{self, color, icon};

const HEADER_H: f32 = 28.0;
const ROW_H: f32 = 19.0;
const PAD_X: f32 = 10.0;
const ICON_W: f32 = 16.0;
const GAP_X: f32 = 90.0;
const GAP_Y: f32 = 34.0;
/// A layout column taller than this wraps into another column.
const MAX_COLUMN_H: f32 = 1500.0;

pub enum DiagramEvent {
    /// Open the table's data.
    OpenTable(String),
}

pub struct Diagram {
    pub schema: String,
    /// Top-left corner of each table's box, in diagram coordinates.
    positions: HashMap<String, Pos2>,
    /// Screen offset of the diagram origin from the canvas corner, and scale.
    pan: Vec2,
    zoom: f32,
    pub selected: Option<String>,
    /// Table to select and centre once it is laid out.
    pub focus: Option<String>,
    /// The tables the current layout was made for.
    laid_out: Vec<String>,
    /// The user moved boxes: new tables are added without redoing the layout.
    moved: bool,
    fit_pending: bool,
    /// Tables whose details were asked for, so a failing one isn't re-asked every frame.
    pub requested: HashSet<String>,
}

impl Diagram {
    pub fn new(schema: String, focus: Option<String>) -> Self {
        Self {
            schema,
            positions: HashMap::new(),
            pan: vec2(20.0, 20.0),
            zoom: 1.0,
            selected: None,
            focus,
            laid_out: Vec::new(),
            moved: false,
            fit_pending: true,
            requested: HashSet::new(),
        }
    }

    /// `tables`: details of the schema's tables loaded so far; `total`: how many
    /// tables and views the schema has.
    pub fn show(&mut self, ui: &mut egui::Ui, tables: &[&TableDetails], total: usize) -> Option<DiagramEvent> {
        let mut event = None;
        let sizes: HashMap<String, Vec2> = tables.iter().map(|t| (t.name.clone(), box_size(ui, t))).collect();
        self.update_layout(tables, &sizes);

        // Toolbar.
        let mut fit = false;
        let mut relayout = false;
        egui::Panel::top(egui::Id::new(("diagram-bar", &self.schema)))
            .frame(
                egui::Frame::new()
                    .fill(color::BG)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .stroke(Stroke::new(1.0, color::BORDER)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::TREE_STRUCTURE).color(color::ACCENT_TEXT));
                    ui.label(RichText::new(&self.schema).font(theme::font(13.0, theme::semibold())));
                    let label = if tables.len() < total {
                        format!("Loading {} of {total} tables…", tables.len())
                    } else {
                        format!("{total} tables")
                    };
                    if tables.len() < total {
                        ui.spinner();
                    }
                    ui.label(RichText::new(label).color(color::TEXT_WEAK));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(theme::flat_button(format!("{}  Re-layout", icon::FLOW_ARROW))).clicked() {
                            relayout = true;
                        }
                        if ui.add(theme::flat_button(format!("{}  Fit", icon::CORNERS_OUT))).clicked() {
                            fit = true;
                        }
                        if ui.add(theme::flat_button(icon::MAGNIFYING_GLASS_PLUS)).on_hover_text("Zoom in").clicked() {
                            self.zoom = (self.zoom * 1.2).min(2.0);
                        }
                        ui.label(RichText::new(format!("{:.0}%", self.zoom * 100.0)).color(color::TEXT_WEAK));
                        if ui.add(theme::flat_button(icon::MAGNIFYING_GLASS_MINUS)).on_hover_text("Zoom out").clicked()
                        {
                            self.zoom = (self.zoom / 1.2).max(0.25);
                        }
                        ui.label(
                            RichText::new("Drag tables to move them · scroll to pan · Cmd+scroll or pinch to zoom")
                                .small()
                                .color(color::TEXT_FAINT),
                        );
                    });
                });
            });
        if relayout {
            self.moved = false;
            self.laid_out.clear();
            self.update_layout(tables, &sizes);
            fit = true;
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(color::BG_SUBTLE)).show(ui, |ui| {
            let canvas = ui.max_rect();
            let background = ui.interact(canvas, ui.id().with("diagram-canvas"), Sense::click_and_drag());
            if fit || std::mem::take(&mut self.fit_pending) && !self.positions.is_empty() {
                self.fit(canvas, &sizes);
            }
            if let Some(focus) = self.focus.clone()
                && let (Some(pos), Some(size)) = (self.positions.get(&focus), sizes.get(&focus))
            {
                // Centre on the focused table.
                let center = *pos + *size / 2.0;
                self.pan = canvas.size() / 2.0 - center.to_vec2() * self.zoom;
                self.selected = Some(focus);
                self.focus = None;
            }

            // Pan with scroll or by dragging the background; zoom around the pointer.
            if background.dragged() {
                self.pan += background.drag_delta();
            }
            if background.clicked() {
                self.selected = None;
            }
            if let Some(pointer) = ui.ctx().pointer_hover_pos().filter(|p| canvas.contains(*p)) {
                let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
                if zoom != 1.0 {
                    let new_zoom = (self.zoom * zoom).clamp(0.25, 2.0);
                    let anchor = pointer - canvas.min - self.pan;
                    self.pan += anchor - anchor * (new_zoom / self.zoom);
                    self.zoom = new_zoom;
                } else {
                    self.pan += scroll;
                }
            }

            let painter = ui.painter_at(canvas);
            let zoom = self.zoom;
            let to_screen = |p: Pos2| canvas.min + self.pan + p.to_vec2() * zoom;
            let rects: HashMap<&str, Rect> = tables
                .iter()
                .filter_map(|t| {
                    let pos = self.positions.get(&t.name)?;
                    let size = sizes.get(&t.name)?;
                    Some((t.name.as_str(), Rect::from_min_size(to_screen(*pos), *size * zoom)))
                })
                .collect();

            // Connectors first, so the boxes sit on top of them.
            let row_y = |table: &TableDetails, column: &str, rect: Rect| {
                let i = table.columns.iter().position(|c| c.name == column).unwrap_or(0);
                rect.top() + (HEADER_H + ROW_H * (i as f32 + 0.5)) * zoom
            };
            for t in tables {
                let Some(&from) = rects.get(t.name.as_str()) else { continue };
                for fk in &t.foreign_keys {
                    if fk.ref_schema != self.schema {
                        continue;
                    }
                    let Some(target) = tables.iter().find(|r| r.name == fk.ref_table) else { continue };
                    let Some(&to) = rects.get(target.name.as_str()) else { continue };
                    let related = self.selected.as_deref().is_some_and(|s| s == t.name || s == target.name);
                    let stroke = if related {
                        Stroke::new(2.0, color::ACCENT)
                    } else {
                        Stroke::new(1.2, Color32::from_rgb(160, 160, 175))
                    };
                    let y0 = row_y(t, &fk.columns[0], from);
                    let y1 = row_y(target, &fk.ref_columns[0], to);
                    connector(&painter, from, y0, to, y1, t.name == target.name, stroke, zoom);
                }
            }

            // Boxes; the selected one last, on top.
            let mut order: Vec<&&TableDetails> = tables.iter().collect();
            order.sort_by_key(|t| self.selected.as_deref() == Some(t.name.as_str()));
            for t in order {
                let Some(&rect) = rects.get(t.name.as_str()) else { continue };
                let response = ui.interact(rect, ui.id().with(("diagram-table", &t.name)), Sense::click_and_drag());
                if response.dragged() {
                    if let Some(p) = self.positions.get_mut(&t.name) {
                        *p += response.drag_delta() / zoom;
                    }
                    self.moved = true;
                    self.selected = Some(t.name.clone());
                }
                if response.clicked() {
                    self.selected = Some(t.name.clone());
                }
                if response.double_clicked() {
                    event = Some(DiagramEvent::OpenTable(t.name.clone()));
                }
                let selected = self.selected.as_deref() == Some(t.name.as_str());
                draw_table(&painter, rect, t, selected, zoom);
                response.on_hover_text("Double-click to open its data");
            }
            if tables.is_empty() {
                painter.text(
                    canvas.center(),
                    egui::Align2::CENTER_CENTER,
                    if total == 0 { "No tables in this schema" } else { "Loading tables…" },
                    FontId::proportional(14.0),
                    color::TEXT_WEAK,
                );
            }
        });
        event
    }

    /// Lays out tables that have no position yet: all of them when the set
    /// changed and nothing was moved by hand, else only the new ones.
    fn update_layout(&mut self, tables: &[&TableDetails], sizes: &HashMap<String, Vec2>) {
        let mut names: Vec<String> = tables.iter().map(|t| t.name.clone()).collect();
        names.sort();
        if names == self.laid_out {
            return;
        }
        if self.moved {
            // Keep the user's arrangement; stack new tables below everything.
            let bottom =
                self.positions.iter().filter_map(|(n, p)| sizes.get(n).map(|s| p.y + s.y)).fold(0.0f32, f32::max);
            let mut x = 0.0;
            for n in &names {
                if !self.positions.contains_key(n) {
                    self.positions.insert(n.clone(), pos2(x, bottom + GAP_Y * 2.0));
                    x += sizes.get(n).map_or(200.0, |s| s.x) + GAP_X;
                }
            }
        } else {
            self.positions = layered_layout(tables, sizes, &self.schema);
            self.fit_pending = true;
        }
        self.laid_out = names;
    }

    fn fit(&mut self, canvas: Rect, sizes: &HashMap<String, Vec2>) {
        let mut bounds = Rect::NOTHING;
        for (n, p) in &self.positions {
            if let Some(s) = sizes.get(n) {
                bounds = bounds.union(Rect::from_min_size(*p, *s));
            }
        }
        if !bounds.is_positive() {
            return;
        }
        let margin = 30.0;
        let zx = (canvas.width() - 2.0 * margin) / bounds.width();
        let zy = (canvas.height() - 2.0 * margin) / bounds.height();
        self.zoom = zx.min(zy).clamp(0.25, 1.0);
        let content = bounds.size() * self.zoom;
        self.pan = (canvas.size() - content) / 2.0 - bounds.min.to_vec2() * self.zoom;
    }
}

/// Columns by foreign-key depth: tables that reference nothing first, each
/// referencing table one column to the right of what it references.
fn layered_layout(tables: &[&TableDetails], sizes: &HashMap<String, Vec2>, schema: &str) -> HashMap<String, Pos2> {
    let names: HashSet<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    let mut depth: HashMap<&str, usize> = tables.iter().map(|t| (t.name.as_str(), 0)).collect();
    // Relax a bounded number of times so cycles can't loop forever.
    for _ in 0..tables.len().min(12) {
        let mut changed = false;
        for t in tables {
            for fk in &t.foreign_keys {
                if fk.ref_schema != schema || fk.ref_table == t.name || !names.contains(fk.ref_table.as_str()) {
                    continue;
                }
                let want = depth[fk.ref_table.as_str()] + 1;
                if depth[t.name.as_str()] < want {
                    depth.insert(t.name.as_str(), want);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let max_depth = depth.values().copied().max().unwrap_or(0);
    let mut positions = HashMap::new();
    let mut x = 0.0;
    for d in 0..=max_depth {
        let mut column: Vec<&str> = depth.iter().filter(|(_, v)| **v == d).map(|(n, _)| *n).collect();
        // Most-connected first, then by name, so related tables sit near the top.
        column.sort_by_key(|n| (std::cmp::Reverse(connections(tables, n)), n.to_string()));
        let mut y = 0.0;
        let mut width: f32 = 0.0;
        for n in column {
            let size = sizes.get(n).copied().unwrap_or(vec2(200.0, 100.0));
            if y > 0.0 && y + size.y > MAX_COLUMN_H {
                x += width + GAP_X;
                y = 0.0;
                width = 0.0;
            }
            positions.insert(n.to_string(), pos2(x, y));
            y += size.y + GAP_Y;
            width = width.max(size.x);
        }
        x += width + GAP_X;
    }
    positions
}

fn connections(tables: &[&TableDetails], name: &str) -> usize {
    tables.iter().map(|t| t.foreign_keys.iter().filter(|fk| t.name == name || fk.ref_table == name).count()).sum()
}

/// Box size at zoom 1: the widest of the title and the column lines.
fn box_size(ui: &egui::Ui, t: &TableDetails) -> Vec2 {
    let width =
        |text: &str, font: FontId| ui.fonts_mut(|f| f.layout_no_wrap(text.to_string(), font, Color32::WHITE).size().x);
    let title = ICON_W + width(&t.name, theme::font(12.5, theme::semibold()));
    let columns = t
        .columns
        .iter()
        .map(|c| {
            ICON_W
                + width(&c.name, FontId::proportional(12.0))
                + 18.0
                + width(&type_label(&c.data_type), theme::mono(11.0))
        })
        .fold(0.0f32, f32::max);
    vec2(title.max(columns).max(140.0) + 2.0 * PAD_X, HEADER_H + ROW_H * t.columns.len() as f32 + 6.0)
}

fn type_label(data_type: &str) -> String {
    data_type.to_lowercase()
}

fn draw_table(painter: &egui::Painter, rect: Rect, t: &TableDetails, selected: bool, zoom: f32) {
    let view = t.kind != RelationKind::Table;
    let border = if selected { Stroke::new(2.0, color::ACCENT) } else { Stroke::new(1.0, color::BORDER) };
    painter.rect_filled(rect.translate(vec2(0.0, 2.0 * zoom)), 6.0 * zoom, Color32::from_black_alpha(14));
    painter.rect_filled(rect, 6.0 * zoom, color::BG);
    let header = Rect::from_min_size(rect.min, vec2(rect.width(), HEADER_H * zoom));
    let header_fill = if selected {
        color::ACCENT_SOFT
    } else if view {
        Color32::from_rgb(236, 244, 250)
    } else {
        color::BG_SUNKEN
    };
    painter.rect_filled(
        header,
        egui::CornerRadius { nw: (6.0 * zoom) as u8, ne: (6.0 * zoom) as u8, sw: 0, se: 0 },
        header_fill,
    );
    painter.hline(rect.x_range(), header.bottom(), Stroke::new(1.0, color::BORDER));
    painter.rect_stroke(rect, 6.0 * zoom, border, egui::StrokeKind::Inside);
    let x = rect.left() + PAD_X * zoom;
    painter.text(
        pos2(x, header.center().y),
        egui::Align2::LEFT_CENTER,
        if view { icon::EYE } else { icon::TABLE },
        FontId::proportional(12.0 * zoom),
        if selected { color::ACCENT_TEXT } else { color::TEXT_WEAK },
    );
    painter.text(
        pos2(x + ICON_W * zoom, header.center().y),
        egui::Align2::LEFT_CENTER,
        &t.name,
        theme::font(12.5 * zoom, theme::semibold()),
        color::TEXT,
    );
    if zoom < 0.45 {
        return; // Too small to read; the outline and title are enough.
    }
    let fk_columns: HashSet<&str> =
        t.foreign_keys.iter().flat_map(|fk| fk.columns.iter().map(String::as_str)).collect();
    for (i, c) in t.columns.iter().enumerate() {
        let y = header.bottom() + (ROW_H * (i as f32 + 0.5) + 3.0) * zoom;
        let (glyph, tint) = if c.pk_position.is_some() {
            (icon::KEY, color::WARNING)
        } else if fk_columns.contains(c.name.as_str()) {
            (icon::LINK_SIMPLE, color::ACCENT)
        } else {
            ("", color::TEXT)
        };
        if !glyph.is_empty() {
            painter.text(pos2(x, y), egui::Align2::LEFT_CENTER, glyph, FontId::proportional(11.0 * zoom), tint);
        }
        let name_color = if c.nullable { color::TEXT } else { Color32::from_rgb(20, 20, 24) };
        painter.text(
            pos2(x + ICON_W * zoom, y),
            egui::Align2::LEFT_CENTER,
            &c.name,
            FontId::proportional(12.0 * zoom),
            name_color,
        );
        painter.text(
            pos2(rect.right() - PAD_X * zoom, y),
            egui::Align2::RIGHT_CENTER,
            type_label(&c.data_type),
            theme::mono(11.0 * zoom),
            color::TEXT_FAINT,
        );
    }
}

/// A curve from the referencing column (crow's foot, "many") to the referenced
/// column (bar, "one"), leaving each box on the side facing the other.
#[allow(clippy::too_many_arguments)]
fn connector(
    painter: &egui::Painter,
    from: Rect,
    y0: f32,
    to: Rect,
    y1: f32,
    self_ref: bool,
    stroke: Stroke,
    zoom: f32,
) {
    let reach = 40.0 * zoom;
    let (start, dir0, end, dir1) = if self_ref {
        (pos2(from.right(), y0), 1.0, pos2(to.right(), y1), 1.0)
    } else if to.center().x < from.center().x {
        (pos2(from.left(), y0), -1.0, pos2(to.right(), y1), 1.0)
    } else {
        (pos2(from.right(), y0), 1.0, pos2(to.left(), y1), -1.0)
    };
    let c0 = start + vec2(dir0 * reach.max((end.x - start.x).abs() / 2.0), 0.0);
    let c1 = end + vec2(dir1 * reach.max((end.x - start.x).abs() / 2.0), 0.0);
    let curve =
        egui::epaint::CubicBezierShape::from_points_stroke([start, c0, c1, end], false, Color32::TRANSPARENT, stroke);
    painter.add(curve);
    // Crow's foot at the referencing end.
    let foot = 9.0 * zoom;
    let tip = start + vec2(dir0 * foot, 0.0);
    for dy in [-5.0 * zoom, 0.0, 5.0 * zoom] {
        painter.line_segment([tip, start + vec2(0.0, dy)], stroke);
    }
    // Bar at the referenced end.
    let bar_x = end.x + dir1 * 6.0 * zoom;
    painter.line_segment([pos2(bar_x, end.y - 5.0 * zoom), pos2(bar_x, end.y + 5.0 * zoom)], stroke);
}
