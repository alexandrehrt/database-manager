//! Database explorer: data source → schema → tables/views → columns, keys
//! and indexes. Each level loads when it is first expanded.

use std::collections::HashMap;

use dbm_core::config::DataSourceConfig;
use dbm_core::{Relation, RelationKind, TableDetails};
use eframe::egui::{self, RichText};

use crate::app::Action;

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
    pub relations: Loadable<Vec<RelationNode>>,
}

pub struct RelationNode {
    pub relation: Relation,
    pub details: Loadable<TableDetails>,
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

/// `name` with the part matching `filter` highlighted.
fn highlighted(ui: &egui::Ui, name: &str, filter: &str) -> egui::WidgetText {
    let Some(range) = find_match(name, filter).filter(|r| !r.is_empty()) else {
        return name.into();
    };
    let plain = egui::TextFormat { color: ui.visuals().text_color(), ..Default::default() };
    let hit = egui::TextFormat {
        color: ui.visuals().strong_text_color(),
        background: egui::Color32::from_rgba_premultiplied(120, 95, 0, 110),
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(&name[..range.start], 0.0, plain.clone());
    job.append(&name[range.clone()], 0.0, hit);
    job.append(&name[range.end..], 0.0, plain);
    job.into()
}

fn schema_matches(node: &SchemaNode, filter: &str) -> bool {
    match &node.relations {
        Loadable::Loaded(rels) => rels.iter().any(|r| find_match(&r.relation.name, filter).is_some()),
        // Still loading: keep it visible so the spinner shows.
        Loadable::NotLoaded | Loadable::Loading => true,
        Loadable::Failed(_) => false,
    }
}

/// `filter` hides tables whose names don't contain it, opens the parents of
/// the ones that do and loads the relations of every schema of connected
/// sources so they can be searched.
pub fn show(
    ui: &mut egui::Ui,
    sources: &[DataSourceConfig],
    trees: &mut HashMap<String, SourceTree>,
    filter: &mut String,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.strong("Database Explorer");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("+").on_hover_text("New data source").clicked() {
                actions.push(Action::NewSource);
            }
        });
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(filter)
                .hint_text("Filter tables (Cmd+O to go to one)")
                .desired_width(ui.available_width() - 24.0),
        );
        if !filter.is_empty() && ui.small_button("x").on_hover_text("Clear filter").clicked() {
            filter.clear();
        }
    });
    ui.separator();
    let filter = filter.trim().to_string();
    let filter = filter.as_str();

    if sources.is_empty() {
        ui.weak("No data sources yet. Click + to add one.");
        return;
    }

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        for source in sources {
            let tree = trees.entry(source.id.clone()).or_default();
            source_node(ui, source, tree, filter, actions);
        }
    });
}

fn status_dot(status: &ConnStatus, ui: &egui::Ui) -> egui::Color32 {
    match status {
        ConnStatus::Connected => egui::Color32::from_rgb(80, 180, 90),
        ConnStatus::Connecting => egui::Color32::from_rgb(220, 170, 50),
        ConnStatus::Failed(_) => ui.visuals().error_fg_color,
        ConnStatus::Disconnected => ui.visuals().weak_text_color(),
    }
}

fn source_node(
    ui: &mut egui::Ui,
    source: &DataSourceConfig,
    tree: &mut SourceTree,
    filter: &str,
    actions: &mut Vec<Action>,
) {
    if !filter.is_empty() && tree.status == ConnStatus::Connected {
        let id = ui.make_persistent_id(("source", &source.id));
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        state.set_open(true);
        state.store(ui.ctx());
    }
    let id = source.id.clone();
    let engine = match source.kind.dialect() {
        dbm_core::Dialect::Postgres => "pg",
        dbm_core::Dialect::Sqlite => "sqlite",
    };
    let dot = status_dot(&tree.status, ui);
    let state_id = ui.make_persistent_id(("source", &id));
    let header = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), state_id, false)
        .show_header(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, dot);
            ui.add(egui::Label::new(RichText::new(&source.name).strong()).sense(egui::Sense::click()).selectable(false))
        });
    let (_, name_response, _) = header.body(|ui| match &tree.status {
        ConnStatus::Disconnected => {
            tree.status = ConnStatus::Connecting;
            actions.push(Action::Connect(id.clone()));
        }
        ConnStatus::Connecting => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Connecting…");
            });
        }
        ConnStatus::Failed(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
            if ui.small_button("Retry").clicked() {
                actions.push(Action::Connect(id.clone()));
            }
        }
        ConnStatus::Connected => schemas(ui, &id, &mut tree.schemas, filter, actions),
    });
    let name_response = name_response.inner;
    if name_response.clicked()
        && let Some(mut state) = egui::collapsing_header::CollapsingState::load(ui.ctx(), state_id)
    {
        state.toggle(ui);
        state.store(ui.ctx());
    }

    let header_response = name_response.on_hover_text(engine);
    if header_response.double_clicked() {
        actions.push(Action::NewConsole(id.clone()));
    }
    header_response.context_menu(|ui| {
        if ui.button("New console").clicked() {
            actions.push(Action::NewConsole(id.clone()));
        }
        ui.separator();
        if tree.status == ConnStatus::Connected {
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

fn loading_row(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.weak("Loading…");
    });
}

fn schemas(
    ui: &mut egui::Ui,
    source: &str,
    schemas: &mut Loadable<Vec<SchemaNode>>,
    filter: &str,
    actions: &mut Vec<Action>,
) {
    match schemas {
        Loadable::NotLoaded => {
            *schemas = Loadable::Loading;
            actions.push(Action::LoadSchemas(source.to_string()));
        }
        Loadable::Loading => loading_row(ui),
        Loadable::Failed(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e.as_str());
        }
        Loadable::Loaded(nodes) => {
            let only_one = nodes.len() == 1;
            let searching = !filter.is_empty();
            for node in nodes.iter_mut() {
                if searching && matches!(node.relations, Loadable::NotLoaded) {
                    node.relations = Loadable::Loading;
                    actions.push(Action::LoadRelations(source.to_string(), node.name.clone()));
                }
                if searching && !schema_matches(node, filter) {
                    continue;
                }
                egui::CollapsingHeader::new(&node.name)
                    .id_salt(("schema", source, &node.name))
                    .default_open(only_one)
                    .open(searching.then_some(true))
                    .show(ui, |ui| relations(ui, source, &node.name, &mut node.relations, filter, actions));
            }
            if searching && !nodes.iter().any(|n| schema_matches(n, filter)) {
                ui.weak("No matching tables");
            }
        }
    }
}

fn relations(
    ui: &mut egui::Ui,
    source: &str,
    schema: &str,
    relations: &mut Loadable<Vec<RelationNode>>,
    filter: &str,
    actions: &mut Vec<Action>,
) {
    match relations {
        Loadable::NotLoaded => {
            *relations = Loadable::Loading;
            actions.push(Action::LoadRelations(source.to_string(), schema.to_string()));
        }
        Loadable::Loading => loading_row(ui),
        Loadable::Failed(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e.as_str());
        }
        Loadable::Loaded(nodes) if nodes.is_empty() => {
            ui.weak("(empty)");
        }
        Loadable::Loaded(nodes) => {
            for (label, kinds) in [
                ("tables", vec![RelationKind::Table]),
                ("views", vec![RelationKind::View, RelationKind::MaterializedView]),
            ] {
                let shown = |n: &RelationNode| {
                    kinds.contains(&n.relation.kind) && find_match(&n.relation.name, filter).is_some()
                };
                let count = nodes.iter().filter(|n| shown(n)).count();
                if count == 0 {
                    continue;
                }
                egui::CollapsingHeader::new(format!("{label}  {count}"))
                    .id_salt(("group", source, schema, label))
                    .default_open(true)
                    .open((!filter.is_empty()).then_some(true))
                    .show(ui, |ui| {
                        for node in nodes.iter_mut().filter(|n| shown(n)) {
                            relation_node(ui, source, schema, node, filter, actions);
                        }
                    });
            }
        }
    }
}

fn relation_node(
    ui: &mut egui::Ui,
    source: &str,
    schema: &str,
    node: &mut RelationNode,
    filter: &str,
    actions: &mut Vec<Action>,
) {
    let name = node.relation.name.clone();
    let open_table =
        || Action::OpenTable { source: source.to_string(), schema: schema.to_string(), table: name.clone() };
    let response = egui::CollapsingHeader::new(highlighted(ui, &name, filter))
        .id_salt(("relation", source, schema, &name))
        .show(ui, |ui| match &mut node.details {
            Loadable::NotLoaded => {
                node.details = Loadable::Loading;
                actions.push(Action::LoadDetails(source.to_string(), schema.to_string(), name.clone()));
            }
            Loadable::Loading => loading_row(ui),
            Loadable::Failed(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e.as_str());
            }
            Loadable::Loaded(details) => table_details(ui, details),
        })
        .header_response;
    if response.double_clicked() {
        actions.push(open_table());
    }
    response.on_hover_text("Double-click to open data").context_menu(|ui| {
        if ui.button("Open data").clicked() {
            actions.push(open_table());
        }
        if ui.button("Show DDL").clicked() {
            actions.push(Action::ShowDdl {
                source: source.to_string(),
                schema: schema.to_string(),
                table: name.clone(),
            });
        }
    });
}

fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) -> egui::Response {
    ui.label(RichText::new(text).small().strong().color(color))
}

fn table_details(ui: &mut egui::Ui, d: &TableDetails) {
    let pk_color = egui::Color32::from_rgb(214, 160, 40);
    let fk_color = egui::Color32::from_rgb(70, 140, 220);
    egui::CollapsingHeader::new(format!("columns  {}", d.columns.len()))
        .id_salt(("columns", &d.schema, &d.name))
        .default_open(true)
        .show(ui, |ui| {
            for col in &d.columns {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(&col.name);
                    ui.weak(format!("{}{}", col.data_type, if col.nullable { "" } else { " not null" }));
                    if col.pk_position.is_some() {
                        badge(ui, "PK", pk_color);
                    }
                    if let Some(fk) = d.foreign_key_for(&col.name) {
                        badge(ui, "FK", fk_color).on_hover_text(format!(
                            "references {}.{} ({})",
                            fk.ref_schema,
                            fk.ref_table,
                            fk.ref_columns.join(", ")
                        ));
                    }
                });
            }
        });
    if !d.foreign_keys.is_empty() {
        egui::CollapsingHeader::new(format!("foreign keys  {}", d.foreign_keys.len()))
            .id_salt(("fks", &d.schema, &d.name))
            .show(ui, |ui| {
                for fk in &d.foreign_keys {
                    ui.label(format!(
                        "{} ({}) -> {}.{} ({})",
                        fk.name,
                        fk.columns.join(", "),
                        fk.ref_schema,
                        fk.ref_table,
                        fk.ref_columns.join(", ")
                    ));
                }
            });
    }
    if !d.indexes.is_empty() {
        egui::CollapsingHeader::new(format!("indexes  {}", d.indexes.len()))
            .id_salt(("indexes", &d.schema, &d.name))
            .show(ui, |ui| {
                for idx in &d.indexes {
                    let kind = if idx.primary {
                        " primary"
                    } else if idx.unique {
                        " unique"
                    } else {
                        ""
                    };
                    ui.horizontal(|ui| {
                        ui.label(&idx.name);
                        ui.weak(format!("({}){kind}", idx.columns.join(", ")));
                    });
                }
            });
    }
}
