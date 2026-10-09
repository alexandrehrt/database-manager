//! The sidebar: objects of the current data source (tables and views of the
//! selected schema, loaded on demand) and the list of connections.

use std::collections::HashMap;

use dbm_core::config::DataSourceConfig;
use dbm_core::{Relation, RelationKind};
use eframe::egui::{self, Color32, RichText, Sense};

use crate::app::Action;
use crate::ui::theme::{self, color, icon};

pub enum Loadable<T> {
    NotLoaded,
    Loading,
    Loaded(T),
    Failed(String),
}

#[derive(PartialEq)]
pub enum ConnStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

pub struct SourceTree {
    pub status: ConnStatus,
    pub schemas: Loadable<Vec<SchemaNode>>,
}

impl Default for SourceTree {
    fn default() -> Self {
        Self { status: ConnStatus::Disconnected, schemas: Loadable::NotLoaded }
    }
}

pub struct SchemaNode {
    pub name: String,
    pub relations: Loadable<Vec<Relation>>,
}

/// Case-insensitive substring match; returns the byte range of the match.
pub fn find_match(name: &str, filter: &str) -> Option<std::ops::Range<usize>> {
    if filter.is_empty() {
        return Some(0..0);
    }
    let start = name.to_lowercase().find(&filter.to_lowercase())?;
    // Lowercasing can change byte lengths outside ASCII; fall back to no highlight.
    name.is_char_boundary(start + filter.len()).then(|| start..start + filter.len()).or(Some(0..0))
}

pub fn status_color(status: &ConnStatus) -> Color32 {
    match status {
        ConnStatus::Connected => color::SUCCESS,
        ConnStatus::Connecting => color::WARNING,
        ConnStatus::Failed(_) => color::DANGER,
        ConnStatus::Disconnected => color::TEXT_FAINT,
    }
}

/// What the sidebar needs to know about the main area.
pub struct SidebarState<'a> {
    pub current: Option<&'a str>,
    pub schema: Option<&'a str>,
    /// (schema, table) of the active table tab, highlighted in the list.
    pub active_table: Option<(&'a str, &'a str)>,
}

/// A full-width clickable row with an optional highlight.
fn row(ui: &mut egui::Ui, selected: bool, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let height = 26.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
    let fill = if selected {
        color::ACCENT_SOFT
    } else if response.hovered() {
        color::BG_SUNKEN
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.spacing_mut().item_spacing.x = 8.0;
    add(&mut child);
    response
}

fn highlighted(name: &str, filter: &str, strong: bool) -> egui::WidgetText {
    let base = egui::TextFormat {
        font_id: theme::font(13.0, if strong { theme::medium() } else { egui::FontFamily::Proportional }),
        color: if strong { color::ACCENT_TEXT } else { color::TEXT },
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    match find_match(name, filter).filter(|r| !r.is_empty()) {
        None => job.append(name, 0.0, base),
        Some(range) => {
            let hit = egui::TextFormat { background: Color32::from_rgb(255, 236, 160), ..base.clone() };
            job.append(&name[..range.start], 0.0, base.clone());
            job.append(&name[range.clone()], 0.0, hit);
            job.append(&name[range.end..], 0.0, base);
        }
    }
    job.into()
}

fn section_header(ui: &mut egui::Ui, title: &str, count: Option<usize>) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(theme::caption(title));
        if let Some(n) = count {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(n.to_string()).small().color(color::TEXT_WEAK));
            });
        }
    });
    ui.add_space(2.0);
}

fn spinner_row(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.spinner();
        ui.label(RichText::new(text).color(color::TEXT_WEAK));
    });
}

pub fn show(
    ui: &mut egui::Ui,
    sources: &[DataSourceConfig],
    trees: &mut HashMap<String, SourceTree>,
    state: SidebarState<'_>,
    filter: &mut String,
    actions: &mut Vec<Action>,
) {
    ui.add(
        egui::TextEdit::singleline(filter)
            .hint_text(format!("{}  Filter objects", icon::MAGNIFYING_GLASS))
            .desired_width(f32::INFINITY)
            .margin(egui::vec2(8.0, 5.0)),
    );
    let filter = filter.trim().to_string();

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if let Some(current) = state.current {
            let tree = trees.entry(current.to_string()).or_default();
            objects(ui, current, tree, &state, &filter, actions);
        } else if sources.is_empty() {
            ui.add_space(10.0);
            ui.label(RichText::new("Add a connection to get started.").color(color::TEXT_WEAK));
        }

        ui.add_space(8.0);
        ui.separator();
        section_header(ui, "Connections", None);
        for source in sources {
            let status = trees.get(&source.id).map_or(&ConnStatus::Disconnected, |t| &t.status);
            let dot = status_color(status);
            let is_current = state.current == Some(source.id.as_str());
            let response = row(ui, false, |ui| {
                ui.label(RichText::new("●").font(theme::font(9.0, egui::FontFamily::Proportional)).color(dot));
                let text = RichText::new(&source.name);
                ui.label(if is_current { text.font(theme::font(13.0, theme::medium())) } else { text });
            });
            let id = source.id.clone();
            let connected = *status == ConnStatus::Connected;
            let hover = match status {
                ConnStatus::Failed(e) => e.clone(),
                _ => source_description(source),
            };
            if response.clicked() {
                actions.push(Action::SelectSource(id.clone()));
            }
            response.on_hover_text(hover).context_menu(|ui| {
                if ui.button("New console").clicked() {
                    actions.push(Action::NewConsole(id.clone()));
                }
                if connected {
                    if ui.button("Refresh").clicked() {
                        actions.push(Action::Refresh(id.clone()));
                    }
                    if ui.button("Disconnect").clicked() {
                        actions.push(Action::Disconnect(id.clone()));
                    }
                } else if ui.button("Connect").clicked() {
                    actions.push(Action::Connect(id.clone()));
                }
                ui.separator();
                if ui.button("Edit…").clicked() {
                    actions.push(Action::EditSource(id.clone()));
                }
            });
        }
        let add = row(ui, false, |ui| {
            ui.label(RichText::new(icon::PLUS).color(color::TEXT_WEAK));
            ui.label(RichText::new("New connection").color(color::TEXT_WEAK));
        });
        if add.clicked() {
            actions.push(Action::NewSource);
        }
    });
}

/// "SQLite · local" / "PostgreSQL · host:port".
pub fn source_description(source: &DataSourceConfig) -> String {
    match &source.kind {
        dbm_core::config::DataSourceKind::Sqlite { .. } => "SQLite · local".into(),
        dbm_core::config::DataSourceKind::Postgres { host, port, .. } => format!("PostgreSQL · {host}:{port}"),
        dbm_core::config::DataSourceKind::Oracle { host, port, service, .. } => {
            format!("Oracle · {host}:{port}/{service}")
        }
    }
}

fn objects(
    ui: &mut egui::Ui,
    source: &str,
    tree: &mut SourceTree,
    state: &SidebarState<'_>,
    filter: &str,
    actions: &mut Vec<Action>,
) {
    match &tree.status {
        ConnStatus::Disconnected => {
            tree.status = ConnStatus::Connecting;
            actions.push(Action::Connect(source.to_string()));
            return;
        }
        ConnStatus::Connecting => return spinner_row(ui, "Connecting…"),
        ConnStatus::Failed(e) => {
            ui.add_space(8.0);
            ui.colored_label(color::DANGER, e);
            if ui.button("Retry").clicked() {
                actions.push(Action::Connect(source.to_string()));
            }
            return;
        }
        ConnStatus::Connected => {}
    }
    let schemas = match &mut tree.schemas {
        Loadable::NotLoaded => {
            tree.schemas = Loadable::Loading;
            actions.push(Action::LoadSchemas(source.to_string()));
            return;
        }
        Loadable::Loading => return spinner_row(ui, "Loading…"),
        Loadable::Failed(e) => {
            ui.colored_label(color::DANGER, e.as_str());
            return;
        }
        Loadable::Loaded(schemas) => schemas,
    };
    let index = state.schema.and_then(|name| schemas.iter().position(|s| s.name == name)).unwrap_or(0);
    let Some(node) = schemas.get_mut(index) else {
        ui.label(RichText::new("No schemas").color(color::TEXT_WEAK));
        return;
    };
    let relations = match &mut node.relations {
        Loadable::NotLoaded => {
            node.relations = Loadable::Loading;
            actions.push(Action::LoadRelations(source.to_string(), node.name.clone()));
            return;
        }
        Loadable::Loading => return spinner_row(ui, "Loading…"),
        Loadable::Failed(e) => {
            ui.colored_label(color::DANGER, e.as_str());
            return;
        }
        Loadable::Loaded(rels) => rels,
    };
    let schema = node.name.clone();
    for (title, kinds, glyph) in [
        ("Tables", &[RelationKind::Table][..], icon::TABLE),
        ("Views", &[RelationKind::View, RelationKind::MaterializedView][..], icon::EYE),
    ] {
        let shown: Vec<&Relation> =
            relations.iter().filter(|r| kinds.contains(&r.kind) && find_match(&r.name, filter).is_some()).collect();
        if shown.is_empty() && !filter.is_empty() {
            continue;
        }
        section_header(ui, title, Some(shown.len()));
        for rel in shown {
            let selected = state.active_table == Some((schema.as_str(), rel.name.as_str()));
            let response = row(ui, selected, |ui| {
                ui.label(RichText::new(glyph).color(if selected { color::ACCENT_TEXT } else { color::TEXT_WEAK }));
                ui.label(highlighted(&rel.name, filter, selected));
            });
            let open =
                || Action::OpenTable { source: source.to_string(), schema: schema.clone(), table: rel.name.clone() };
            if response.clicked() {
                actions.push(open());
            }
            response.context_menu(|ui| {
                if ui.button("Open data").clicked() {
                    actions.push(open());
                }
                if ui.button("Show DDL").clicked() {
                    actions.push(Action::ShowDdl {
                        source: source.to_string(),
                        schema: schema.clone(),
                        table: rel.name.clone(),
                    });
                }
            });
        }
    }
}
