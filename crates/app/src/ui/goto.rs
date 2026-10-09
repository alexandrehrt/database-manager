//! Cmd+O "Go to table": a fuzzy picker over the tables of connected sources.

use eframe::egui::{self, Key, Modifiers};

pub struct Candidate {
    pub source: String,
    pub source_name: String,
    pub schema: String,
    pub table: String,
    pub is_view: bool,
}

#[derive(Default)]
pub struct GotoTable {
    query: String,
    selected: usize,
    focused: bool,
}

pub enum GotoOutcome {
    Open(usize),
    Close,
}

/// Subsequence match, case-insensitive. Consecutive characters and matches
/// at word starts score higher; shorter names win ties.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let cand: Vec<char> = candidate.to_lowercase().chars().collect();
    let mut score = 0;
    let mut pos = 0;
    let mut prev: Option<usize> = None;
    for q in query.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (pos..cand.len()).find(|&i| cand[i] == q)?;
        score += match prev {
            Some(p) if p + 1 == found => 10,
            _ => 0,
        };
        if found == 0 || matches!(cand[found - 1], '_' | '.' | ' ') {
            score += 15;
        }
        score -= (found - pos) as i32;
        prev = Some(found);
        pos = found + 1;
    }
    Some(score * 4 - cand.len() as i32)
}

impl GotoTable {
    /// Shows the picker; `candidates` are all tables known so far.
    pub fn show(&mut self, ctx: &egui::Context, candidates: &[Candidate], loading: bool) -> Option<GotoOutcome> {
        let mut ranked: Vec<(i32, usize)> = candidates
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let target =
                    if self.query.contains('.') { format!("{}.{}", c.schema, c.table) } else { c.table.clone() };
                fuzzy_score(&self.query, &target).map(|s| (s, i))
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| candidates[a.1].table.cmp(&candidates[b.1].table)));
        ranked.truncate(50);
        self.selected = self.selected.min(ranked.len().saturating_sub(1));

        let mut outcome = None;
        let modal = egui::Modal::new(egui::Id::new("goto_table")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.input_mut(|i| {
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    self.selected = (self.selected + 1).min(ranked.len().saturating_sub(1));
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    self.selected = self.selected.saturating_sub(1);
                }
            });
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Table name (schema.table also works)")
                    .desired_width(f32::INFINITY),
            );
            if !self.focused {
                field.request_focus();
                self.focused = true;
            }
            if field.changed() {
                self.selected = 0;
            }
            if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                outcome = ranked.get(self.selected).map(|&(_, i)| GotoOutcome::Open(i));
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for (row, &(_, i)) in ranked.iter().enumerate() {
                    let c = &candidates[i];
                    let label = format!("{}{}", c.table, if c.is_view { "  (view)" } else { "" });
                    let r = ui.horizontal(|ui| {
                        let r = ui.selectable_label(row == self.selected, label);
                        ui.weak(format!("{}.{}", c.source_name, c.schema));
                        r
                    });
                    if r.inner.clicked() {
                        outcome = Some(GotoOutcome::Open(i));
                    }
                    if row == self.selected {
                        r.inner.scroll_to_me(None);
                    }
                }
                if ranked.is_empty() {
                    ui.weak(if candidates.is_empty() {
                        "No tables loaded yet. Connect a data source to search it."
                    } else {
                        "No matching tables"
                    });
                }
            });
            if loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("Loading tables...");
                });
            }
        });
        if outcome.is_none() && modal.should_close() {
            outcome = Some(GotoOutcome::Close);
        }
        outcome
    }
}
