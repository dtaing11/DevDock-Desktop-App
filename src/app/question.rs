//! A question from an agent, drawn to be read: the lead as prose with its
//! inline code, the choices it offers — `(A) …; (B) …; or (C) …`, or a
//! list — each on a row of its own with a button to pick it, the one it
//! recommends marked, and the answer box underneath for anything the
//! choices do not cover. One widget for every place a question shows: a
//! run's card, the Agent tab, the Runs tab.

use eframe::egui::{self, RichText};

use super::backlog::PendingQuestion;
use super::{markdown, theme};

/// A question split into what it asks and what it offers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    /// What comes before the choices: the context and the question itself.
    pub lead: String,
    pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// `A`, `B`, `1`, `2` — as the agent wrote it.
    pub label: String,
    pub text: String,
    /// The agent said this is the one it would pick.
    pub recommended: bool,
}

/// What the developer did with the widget this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    None,
    /// Send what is in the draft.
    Answer,
}

/// The labels a run of choices may use, in order: letters or numbers.
fn sequence(first: &str) -> Option<Vec<String>> {
    match first {
        "A" => Some(('A'..='H').map(|c| c.to_string()).collect()),
        "a" => Some(('a'..='h').map(|c| c.to_string()).collect()),
        "1" => Some((1..=8).map(|n| n.to_string()).collect()),
        _ => None,
    }
}

/// Splits a question into its lead and its choices. A question that
/// offers none comes back whole, as the lead.
pub fn parse(question: &str) -> Parsed {
    let question = question.trim();
    by_lines(question).or_else(|| inline(question)).unwrap_or_else(|| Parsed { lead: question.to_string(), choices: Vec::new() })
}

/// Choices as a list: lines that start `A)`, `(B)`, `1.`, `- C:`.
fn by_lines(question: &str) -> Option<Parsed> {
    let marker = regex::Regex::new(r"^\s*(?:[-*•]\s*)?\*{0,2}\(?([A-Ha-h]|[1-8])[\).:]\*{0,2}\s+(.+)$").ok()?;
    let mut lead = Vec::new();
    let mut raw: Vec<(String, String)> = Vec::new();
    let mut expected: Option<Vec<String>> = None;
    for line in question.lines() {
        if let Some(caps) = marker.captures(line) {
            let label = caps[1].to_string();
            let fits = match &expected {
                None => sequence(&label).map(|s| expected = Some(s)).is_some(),
                Some(seq) => seq.get(raw.len()).is_some_and(|l| *l == label),
            };
            if fits {
                raw.push((label, caps[2].to_string()));
                continue;
            }
        }
        match raw.last_mut() {
            // A wrapped line belongs to the choice above it.
            Some((_, text)) if !line.trim().is_empty() => {
                text.push(' ');
                text.push_str(line.trim());
            }
            Some(_) => {}
            None => lead.push(line),
        }
    }
    (raw.len() >= 2).then(|| Parsed { lead: lead.join("\n").trim().to_string(), choices: raw.iter().map(|(label, text)| choice(label, text)).collect() })
}

/// Choices in a sentence: `… do you want (A) this; (B) that; or (C) both?`
fn inline(question: &str) -> Option<Parsed> {
    let marker = regex::Regex::new(r"\(([A-Ha-h]|[1-8])\)\s").ok()?;
    let mut found: Vec<(usize, usize, String)> = Vec::new();
    let mut expected: Option<Vec<String>> = None;
    for caps in marker.captures_iter(question) {
        let whole = caps.get(0)?;
        let label = caps[1].to_string();
        let fits = match &expected {
            None => sequence(&label).map(|s| expected = Some(s)).is_some(),
            Some(seq) => seq.get(found.len()).is_some_and(|l| *l == label),
        };
        if fits {
            found.push((whole.start(), whole.end(), label));
        }
    }
    if found.len() < 2 {
        return None;
    }
    let lead = question[..found[0].0].trim().trim_end_matches([',', ':', ';']).trim().to_string();
    let mut choices = Vec::new();
    for (i, (_, end, label)) in found.iter().enumerate() {
        let until = found.get(i + 1).map(|n| n.0).unwrap_or(question.len());
        choices.push(choice(label, &question[*end..until]));
    }
    Some(Parsed { lead, choices })
}

/// One choice, its joining words and punctuation trimmed, and whether the
/// agent recommends it — said in a parenthesis, which is then dropped.
fn choice(label: &str, text: &str) -> Choice {
    let mut text = text.trim().to_string();
    for tail in [" or", " and", ", or", "; or", ", and", "; and"] {
        if let Some(rest) = text.strip_suffix(tail) {
            text = rest.to_string();
        }
    }
    let mut text = text.trim().trim_end_matches([';', ',', '?']).trim().to_string();
    let lower = text.to_lowercase();
    let recommended = lower.contains("recommend");
    if let Ok(aside) = regex::Regex::new(r"(?i)\s*\((?:my |the )?recommend[a-z]*\)") {
        text = aside.replace_all(&text, "").trim().to_string();
    }
    Choice { label: label.to_string(), text, recommended }
}

/// What picking a choice writes into the answer box.
fn pick_text(choice: &Choice) -> String {
    format!("Option {}", choice.label)
}

/// The question, its choices, the answer box and its buttons. `Answer`
/// means send the draft as it stands — empty when the developer let the
/// agent decide.
pub fn show(ui: &mut egui::Ui, q: &mut PendingQuestion) -> Outcome {
    let mut outcome = Outcome::None;
    let parsed = parse(&q.question);
    egui::Frame::new()
        .fill(theme::ember().linear_multiply(0.10))
        .stroke(egui::Stroke::new(1.0_f32, theme::ember()))
        .corner_radius(theme::RADIUS_MD as f32)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.label(RichText::new("The agent asks").font(theme::semibold(theme::TEXT)).color(theme::ember()));
            ui.add_space(theme::UNIT);
            if !parsed.lead.is_empty() {
                ui.scope(|ui| markdown::render(ui, &parsed.lead));
            }
            if !parsed.choices.is_empty() {
                ui.add_space(theme::UNIT * 2.0);
            }
            for choice in &parsed.choices {
                let picked = q.draft.trim_start().starts_with(&pick_text(choice));
                egui::Frame::new()
                    .fill(if picked { theme::ember().linear_multiply(0.22) } else { theme::panel2() })
                    .corner_radius(theme::RADIUS_MD as f32)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal_top(|ui| {
                            let button = egui::Button::new(RichText::new(&choice.label).font(theme::semibold(theme::TEXT)))
                                .selected(picked)
                                .min_size(egui::vec2(28.0, 24.0));
                            if ui.add(button).on_hover_text("Pick this one; you can add to it below before answering").clicked() {
                                q.draft = pick_text(choice);
                            }
                            ui.vertical(|ui| {
                                if choice.recommended {
                                    ui.label(RichText::new("recommended by the agent").size(theme::SMALL).color(theme::teal()));
                                }
                                ui.scope(|ui| markdown::render(ui, &choice.text));
                            });
                        });
                    });
                ui.add_space(theme::UNIT);
            }
            ui.add_space(theme::UNIT);
            let hint = if parsed.choices.is_empty() { "Your answer — it is waiting" } else { "Pick one above, or write your own answer — it is waiting" };
            super::views::prose_box(ui, &mut q.draft, 2, hint);
            ui.add_space(theme::UNIT);
            ui.horizontal_wrapped(|ui| {
                let ready = !q.draft.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new(RichText::new("Answer").color(egui::Color32::BLACK)).fill(theme::ember())).clicked() {
                    outcome = Outcome::Answer;
                }
                if ui.small_button("Let it decide").on_hover_text("Sends no answer; the agent decides and states its assumption.").clicked() {
                    q.draft.clear();
                    outcome = Outcome::Answer;
                }
            });
        });
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_in_a_sentence_become_rows() {
        let parsed = parse(
            "For data the app cannot shift with the field (parquet-backed visualizations, and soil grids), do you want \
             (A) a pre-save confirmation listing what stays behind, with no schema change (my recommendation); \
             (B) option A plus a persisted `needs_recalibration` flag shown in the soil grid; or \
             (C) a render-time offset stored on parquet visualization documents, with A or B for the shared data?",
        );
        assert_eq!(parsed.lead, "For data the app cannot shift with the field (parquet-backed visualizations, and soil grids), do you want");
        let labels: Vec<&str> = parsed.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["A", "B", "C"]);
        assert_eq!(parsed.choices[0].text, "a pre-save confirmation listing what stays behind, with no schema change");
        assert!(parsed.choices[0].recommended && !parsed.choices[1].recommended);
        assert_eq!(parsed.choices[1].text, "option A plus a persisted `needs_recalibration` flag shown in the soil grid");
        assert_eq!(parsed.choices[2].text, "a render-time offset stored on parquet visualization documents, with A or B for the shared data");
    }

    #[test]
    fn choices_as_a_list_become_rows_and_prose_stays_prose() {
        let parsed = parse("Which database?\n\n1. Postgres, as the rest of the stack\n   uses it (recommended)\n2. SQLite\n3. Neither: keep the JSON file");
        assert_eq!(parsed.lead, "Which database?");
        assert_eq!(parsed.choices.len(), 3);
        assert_eq!(parsed.choices[0].text, "Postgres, as the rest of the stack uses it");
        assert!(parsed.choices[0].recommended);
        assert_eq!(parsed.choices[2].label, "3");

        let parsed = parse("- A) rename it\n- B) leave it");
        assert_eq!(parsed.choices.len(), 2);
        assert_eq!(parsed.lead, "");

        // No choices: a parenthesis with a letter in it is not a list of one,
        // and a sequence that starts at B is not a sequence.
        for plain in ["Should the CSV include the archived rows too?", "Is plan (B) still the one you want, or has (D) replaced it?", "See section (A) of the spec: is that current?"] {
            let parsed = parse(plain);
            assert!(parsed.choices.is_empty(), "{plain}: {parsed:?}");
            assert_eq!(parsed.lead, plain);
        }
    }

    #[test]
    fn picking_a_choice_writes_the_answer() {
        let c = choice("B", " this one; or");
        assert_eq!(c.text, "this one");
        assert_eq!(pick_text(&c), "Option B");
    }
}
