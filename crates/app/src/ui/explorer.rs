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

pub fn show(
    ui: &mut egui::Ui,
    sources: &[DataSourceConfig],
    trees: &mut HashMap<String, SourceTree>,
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
    ui.separator();

    if sources.is_empty() {
        ui.weak("No data sources yet. Click + to add one.");
        return;
    }

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        for source in sources {
            let tree = trees.entry(source.id.clone()).or_default();
            source_node(ui, source, tree, actions);
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

fn source_node(ui: &mut egui::Ui, source: &DataSourceConfig, tree: &mut SourceTree, actions: &mut Vec<Action>) {
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
        ConnStatus::Connected => schemas(ui, &id, &mut tree.schemas, actions),
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

fn schemas(ui: &mut egui::Ui, source: &str, schemas: &mut Loadable<Vec<SchemaNode>>, actions: &mut Vec<Action>) {
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
            for node in nodes {
                egui::CollapsingHeader::new(&node.name)
                    .id_salt(("schema", source, &node.name))
                    .default_open(only_one)
                    .show(ui, |ui| relations(ui, source, &node.name, &mut node.relations, actions));
            }
        }
    }
}

fn relations(
    ui: &mut egui::Ui,
    source: &str,
    schema: &str,
    relations: &mut Loadable<Vec<RelationNode>>,
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
            for (label, kinds) in [("tables", vec![RelationKind::Table]), ("views", vec![RelationKind::View, RelationKind::MaterializedView])] {
                let count = nodes.iter().filter(|n| kinds.contains(&n.relation.kind)).count();
                if count == 0 {
                    continue;
                }
                egui::CollapsingHeader::new(format!("{label}  {count}"))
                    .id_salt(("group", source, schema, label))
                    .default_open(true)
                    .show(ui, |ui| {
                        for node in nodes.iter_mut().filter(|n| kinds.contains(&n.relation.kind)) {
                            relation_node(ui, source, schema, node, actions);
                        }
                    });
            }
        }
    }
}

fn relation_node(ui: &mut egui::Ui, source: &str, schema: &str, node: &mut RelationNode, actions: &mut Vec<Action>) {
    let name = node.relation.name.clone();
    let open_table = || Action::OpenTable { source: source.to_string(), schema: schema.to_string(), table: name.clone() };
    let response = egui::CollapsingHeader::new(&name)
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
            actions.push(Action::ShowDdl { source: source.to_string(), schema: schema.to_string(), table: name.clone() });
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
                    let kind = if idx.primary { " primary" } else if idx.unique { " unique" } else { "" };
                    ui.horizontal(|ui| {
                        ui.label(&idx.name);
                        ui.weak(format!("({}){kind}", idx.columns.join(", ")));
                    });
                }
            });
    }
}
