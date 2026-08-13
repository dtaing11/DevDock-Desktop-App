//! Animated commit graph: lanes, curved edges, and a sci-fi light pulse
//! running through the history.
//!
//! Layout: classic lane assignment over `git log --all` (newest first).
//! Rendering: an egui painter pass draws glowing nodes and bezier edges,
//! then animated light pulses travel along each edge. The panel repaints
//! continuously only while visible.

use super::theme;
use super::App;
use crate::git::Commit;
use egui::{Color32, Pos2, RichText, ScrollArea, Stroke, Vec2};
use std::collections::HashMap;

/// Lane colors cycled across branches.
const LANE_COLORS: &[Color32] = &[
    theme::EMBER,
    theme::TEAL,
    Color32::from_rgb(0xb0, 0x8c, 0xff), // violet
    theme::ADD,
    Color32::from_rgb(0xff, 0x8c, 0xc4), // pink
    theme::WARN,
];

const ROW_H: f32 = 34.0;
const LANE_W: f32 = 26.0;
const MARGIN_X: f32 = 24.0;
const MARGIN_Y: f32 = 20.0;
const NODE_R: f32 = 5.0;

/// One laid-out commit.
#[derive(Debug)]
pub struct GraphNode {
    pub commit: Commit,
    pub lane: usize,
    /// Indices of parent nodes (edges to draw), with the parent's lane.
    pub parent_edges: Vec<(usize, usize)>,
}

/// Assigns lanes to commits (newest first) and resolves parent edges.
pub fn layout(commits: &[Commit]) -> Vec<GraphNode> {
    let index_of: HashMap<&str, usize> =
        commits.iter().enumerate().map(|(i, c)| (c.sha.as_str(), i)).collect();

    // Each lane holds the sha it expects to see next (or None when free).
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut node_lane: Vec<usize> = Vec::with_capacity(commits.len());

    for commit in commits {
        // Find the lane expecting this commit; otherwise allocate one.
        let lane = lanes
            .iter()
            .position(|l| l.as_deref() == Some(commit.sha.as_str()))
            .unwrap_or_else(|| {
                if let Some(free) = lanes.iter().position(|l| l.is_none()) {
                    free
                } else {
                    lanes.push(None);
                    lanes.len() - 1
                }
            });
        // Other lanes expecting this same commit merge into this one: free them.
        for slot in lanes.iter_mut() {
            if slot.as_deref() == Some(commit.sha.as_str()) {
                *slot = None;
            }
        }
        // This lane continues to the first parent.
        lanes.resize(lanes.len().max(lane + 1), None);
        lanes[lane] = commit.parents.first().cloned();
        // Extra parents (merges) occupy their own lanes unless already expected.
        for parent in commit.parents.iter().skip(1) {
            let expected = lanes.iter().any(|l| l.as_deref() == Some(parent.as_str()));
            if !expected {
                if let Some(free) = lanes.iter().position(|l| l.is_none()) {
                    lanes[free] = Some(parent.clone());
                } else {
                    lanes.push(Some(parent.clone()));
                }
            }
        }
        node_lane.push(lane);
    }

    commits
        .iter()
        .enumerate()
        .map(|(i, commit)| {
            let parent_edges = commit
                .parents
                .iter()
                .filter_map(|p| index_of.get(p.as_str()).copied())
                .map(|pi| (pi, node_lane[pi]))
                .collect();
            GraphNode { commit: commit.clone(), lane: node_lane[i], parent_edges }
        })
        .collect()
}

/// Renders the animated graph as a right side panel; the main diff view
/// keeps working next to it. Pure graph: no text column, details on hover.
pub fn draw_side_panel(app: &mut App, ctx: &egui::Context) {
    egui::SidePanel::right("graph-panel")
        .default_width(220.0)
        .min_width(140.0)
        .max_width(420.0)
        .resizable(true)
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(0.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::PANEL2)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("Graph · {}", app.graph.len()))
                                .color(theme::FG_DIM)
                                .small(),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("Reload").clicked() {
                                    app.load_graph();
                                }
                            },
                        );
                    });
                });

            if app.graph.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("no commits").color(theme::FG_DIM).small());
                });
                return;
            }

            let time = ui.input(|i| i.time) as f32;
            ctx.request_repaint();

            let max_lane = app.graph.iter().map(|n| n.lane).max().unwrap_or(0);
            let content_w = (MARGIN_X * 2.0 + (max_lane as f32 + 1.0) * LANE_W)
                .max(ui.available_width());
            let content_h = MARGIN_Y * 2.0 + app.graph.len() as f32 * ROW_H;

            ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(
                    Vec2::new(content_w, content_h),
                    egui::Sense::hover(),
                );
                let painter = ui.painter_at(rect);
                let origin = rect.min;

                let pos_of = |idx: usize, lane: usize| -> Pos2 {
                    Pos2::new(
                        origin.x + MARGIN_X + lane as f32 * LANE_W,
                        origin.y + MARGIN_Y + idx as f32 * ROW_H,
                    )
                };

                // --- edges ---
                for (i, node) in app.graph.iter().enumerate() {
                    let from = pos_of(i, node.lane);
                    for (pi, plane) in &node.parent_edges {
                        let to = pos_of(*pi, *plane);
                        let color = LANE_COLORS[node.lane % LANE_COLORS.len()]
                            .linear_multiply(0.45);
                        draw_edge(&painter, from, to, color);
                    }
                }

                // --- light pulses ---
                for (i, node) in app.graph.iter().enumerate() {
                    let from = pos_of(i, node.lane);
                    for (pi, plane) in &node.parent_edges {
                        let to = pos_of(*pi, *plane);
                        let phase = ((i * 31 + pi * 17) % 97) as f32 / 97.0;
                        let t = (time * 0.35 + phase) % 1.0;
                        let p = edge_point(from, to, t);
                        let color = LANE_COLORS[node.lane % LANE_COLORS.len()];
                        painter.circle_filled(p, 1.8, Color32::WHITE);
                        painter.circle_filled(p, 3.6, color.linear_multiply(0.55));
                        painter.circle_filled(p, 7.0, color.linear_multiply(0.15));
                    }
                }

                // --- nodes ---
                let hover = ui.ctx().pointer_hover_pos();
                let mut hovered_popup: Option<(Pos2, usize)> = None;
                for (i, node) in app.graph.iter().enumerate() {
                    let p = pos_of(i, node.lane);
                    let color = LANE_COLORS[node.lane % LANE_COLORS.len()];
                    let breathe = 0.5 + 0.5 * (time * 2.0 + i as f32 * 0.7).sin();
                    painter.circle_filled(p, NODE_R + 5.0 * breathe, color.linear_multiply(0.10));
                    painter.circle_filled(p, NODE_R, theme::BG);
                    painter.circle_stroke(p, NODE_R, Stroke::new(2.0_f32, color));
                    painter.circle_filled(p, 2.0, color);

                    // Small HEAD marker dot ring for decorated commits.
                    if node.commit.refs.iter().any(|r| r.starts_with("HEAD")) {
                        painter.circle_stroke(
                            p,
                            NODE_R + 3.0,
                            Stroke::new(1.0_f32, theme::EMBER),
                        );
                    }

                    if let Some(h) = hover {
                        if h.distance(p) <= NODE_R + 8.0 {
                            painter.circle_stroke(
                                p,
                                NODE_R + 4.0,
                                Stroke::new(2.0_f32, Color32::WHITE),
                            );
                            hovered_popup = Some((p, i));
                        }
                    }
                }

                // --- hover popup: rendered as an egui tooltip-layer Area
                // so it floats above every panel and is never clipped by
                // this narrow side panel.
                if let Some((p, i)) = hovered_popup {
                    let node = &app.graph[i];
                    let refs = node.commit.refs.clone();
                    let short = node.commit.short_sha.clone();
                    let subject = truncate_str(&node.commit.subject, 46);
                    let meta = format!(
                        "{} · {}",
                        node.commit.author,
                        node.commit.date.get(..10).unwrap_or("")
                    );
                    egui::Area::new(egui::Id::new("graph-hover-popup"))
                        .order(egui::Order::Tooltip)
                        .fixed_pos(p + Vec2::new(-260.0, -10.0))
                        .show(ui.ctx(), |ui| {
                            egui::Frame::new()
                                .fill(theme::PANEL)
                                .stroke(egui::Stroke::new(
                                    1.0_f32,
                                    Color32::WHITE.linear_multiply(0.35),
                                ))
                                .corner_radius(8.0)
                                .inner_margin(egui::Margin::symmetric(12, 8))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(format!("{short}  {subject}"))
                                            .small(),
                                    );
                                    ui.label(
                                        RichText::new(meta).color(theme::FG_DIM).small(),
                                    );
                                    for name in refs.iter().take(6) {
                                        let color = if name.starts_with("HEAD") {
                                            theme::EMBER
                                        } else if name.starts_with("tag:") {
                                            theme::WARN
                                        } else {
                                            theme::TEAL
                                        };
                                        ui.label(
                                            RichText::new(name).color(color).small(),
                                        );
                                    }
                                });
                        });
                }
            });
        });
}

/// Truncates a string to `n` chars with an ellipsis.
fn truncate_str(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

/// Cubic-bezier edge between two commit points (smooth S-curve when the
/// lanes differ, straight when aligned).
fn draw_edge(painter: &egui::Painter, from: Pos2, to: Pos2, color: Color32) {
    if (from.x - to.x).abs() < 0.5 {
        painter.line_segment([from, to], Stroke::new(1.6_f32, color));
        return;
    }
    let steps = 16;
    let mut prev = from;
    for s in 1..=steps {
        let t = s as f32 / steps as f32;
        let p = edge_point(from, to, t);
        painter.line_segment([prev, p], Stroke::new(1.6_f32, color));
        prev = p;
    }
}

/// Point at parameter `t` along the edge's curve.
fn edge_point(from: Pos2, to: Pos2, t: f32) -> Pos2 {
    if (from.x - to.x).abs() < 0.5 {
        return Pos2::new(from.x, from.y + (to.y - from.y) * t);
    }
    // Cubic bezier with vertical control handles for a smooth lane change.
    let c1 = Pos2::new(from.x, from.y + (to.y - from.y) * 0.5);
    let c2 = Pos2::new(to.x, from.y + (to.y - from.y) * 0.5);
    let u = 1.0 - t;
    let x = u * u * u * from.x + 3.0 * u * u * t * c1.x + 3.0 * u * t * t * c2.x + t * t * t * to.x;
    let y = u * u * u * from.y + 3.0 * u * u * t * c1.y + 3.0 * u * t * t * c2.y + t * t * t * to.y;
    Pos2::new(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> Commit {
        Commit {
            sha: sha.into(),
            short_sha: sha.chars().take(7).collect(),
            author: "T".into(),
            email: "t@t".into(),
            date: "2026-01-01".into(),
            subject: format!("c {sha}"),
            body: String::new(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn linear_history_stays_in_one_lane() {
        let commits =
            vec![commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])];
        let nodes = layout(&commits);
        assert!(nodes.iter().all(|n| n.lane == 0));
        assert_eq!(nodes[0].parent_edges, vec![(1, 0)]);
    }

    #[test]
    fn branch_gets_its_own_lane() {
        // main: c -> a ; feature: b -> a (b listed between c and a)
        let commits =
            vec![commit("c", &["a"]), commit("b", &["a"]), commit("a", &[])];
        let nodes = layout(&commits);
        assert_eq!(nodes[0].lane, 0);
        assert_ne!(nodes[1].lane, nodes[0].lane, "parallel branch needs its own lane");
        assert_eq!(nodes[2].lane, 0, "root merges back to the first lane");
    }

    #[test]
    fn merge_commit_has_two_edges() {
        let commits = vec![
            commit("m", &["a", "b"]),
            commit("b", &["r"]),
            commit("a", &["r"]),
            commit("r", &[]),
        ];
        let nodes = layout(&commits);
        assert_eq!(nodes[0].parent_edges.len(), 2);
    }

    #[test]
    fn edge_point_endpoints_match() {
        let from = Pos2::new(10.0, 0.0);
        let to = Pos2::new(60.0, 100.0);
        assert!((edge_point(from, to, 0.0) - from).length() < 0.01);
        assert!((edge_point(from, to, 1.0) - to).length() < 0.01);
    }
}
