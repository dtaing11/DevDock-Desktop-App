//! The code editor: buffers, syntax highlighting, and everything the
//! language server contributes.
//!
//! The editor is a text area over a [`OpenFile`] buffer, plus the four
//! things that make it more than a text area: diagnostics underlined in
//! place, hover types, navigation (definition, references, outline), and
//! edits the server produces (format, rename).
//!
//! # How it talks to a language server
//!
//! Every LSP request blocks — `rust-analyzer` can take a minute while it
//! indexes — so nothing here calls the server directly. The UI records what
//! it wants as an [`Action`], and [`show`] hands those to the app, which
//! runs them on the worker thread and delivers answers back as
//! [`crate::app::worker::LspReply`]. The one exception is reading
//! diagnostics, which are already in memory and only need a lock.
//!
//! # Offsets
//!
//! Three coordinate systems meet in this file and mixing them silently
//! corrupts a buffer, so each conversion is named:
//!
//! - **char index** — what egui's cursor uses ([`CCursor`]).
//! - **byte offset** — what Rust string slicing uses.
//! - **line + UTF-16 column** — what LSP uses ([`Position`]).

use super::edits::{self, Range};
use super::worker::AiTarget;
use super::{theme, App};
use crate::lsp::protocol::{
    self, CompletionItem, Diagnostic, Location, Position, Severity, Symbol,
};
use egui::text::{CCursor, CCursorRange, LayoutJob};
use egui::{FontId, RichText, ScrollArea, TextFormat};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the buffer must sit still before its text is sent to the server.
/// Every keystroke would be correct and wasteful; this is short enough that
/// diagnostics still feel live.
const SYNC_DEBOUNCE: Duration = Duration::from_millis(350);

/// How long the pointer must rest before a hover is requested.
const HOVER_DELAY: Duration = Duration::from_millis(400);

/// Files larger than this open read-only. The layouter is per-line and
/// re-runs on every frame the buffer changes; past this size that stops
/// being interactive, and a git client is not where you edit a 5 MB blob.
pub const MAX_EDITABLE_BYTES: usize = 2_000_000;

/// One open buffer.
pub struct OpenFile {
    pub path: PathBuf,
    /// Repo-relative path, for tabs and messages.
    pub rel: String,
    pub text: String,
    /// Text as it is on disk, for the dirty marker.
    pub saved: String,
    pub lang: super::syntax::Lang,
    /// Symbol outline, refreshed on open and save.
    pub symbols: Vec<Symbol>,
    /// Cursor as a byte offset, tracked so requests can name a position.
    pub cursor: usize,
    /// A line to scroll to on the next frame, from navigation.
    pub reveal: Option<u32>,
    /// When the buffer last changed, for debounced syncing.
    pub dirty_since: Option<Instant>,
    /// Whether the server has this file open.
    pub synced: bool,
    /// Read-only because it is too large to edit comfortably.
    pub read_only: bool,
}

impl OpenFile {
    pub fn is_dirty(&self) -> bool {
        self.text != self.saved
    }

    /// The LSP position of the cursor.
    pub fn cursor_position(&self) -> Position {
        protocol::offset_to_position(&self.text, self.cursor)
    }

    /// The word the cursor sits in, as a byte range — the anchor a
    /// completion replaces.
    fn word_at(&self, offset: usize) -> (usize, usize) {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let start = self.text[..offset]
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_word(*c))
            .last()
            .map(|(i, _)| i)
            .unwrap_or(offset);
        let end = offset
            + self.text[offset..]
                .char_indices()
                .take_while(|(_, c)| is_word(*c))
                .map(|(i, c)| i + c.len_utf8())
                .last()
                .unwrap_or(0);
        (start, end)
    }
}

/// The completion popup.
#[derive(Default)]
pub struct Completion {
    pub open: bool,
    pub items: Vec<CompletionItem>,
    /// What has been typed since the popup opened, used to filter.
    pub filter: String,
    pub selected: usize,
    /// Byte offset where the word being completed starts.
    pub anchor: usize,
    pub requesting: bool,
    /// Where to draw it, from the galley.
    pub screen_pos: Option<egui::Pos2>,
}

impl Completion {
    /// Items matching what has been typed, best first. Case-insensitive
    /// prefix matches rank above contains-matches, which is what every
    /// editor does and what makes the first entry usually right.
    pub fn filtered(&self) -> Vec<&CompletionItem> {
        if self.filter.is_empty() {
            return self.items.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        let mut prefix = Vec::new();
        let mut contains = Vec::new();
        for item in &self.items {
            let label = item.label.to_lowercase();
            if label.starts_with(&needle) {
                prefix.push(item);
            } else if label.contains(&needle) {
                contains.push(item);
            }
        }
        prefix.extend(contains);
        prefix
    }

    pub fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.filter.clear();
        self.selected = 0;
    }
}

/// A hover tooltip waiting on, or holding, an answer.
#[derive(Default)]
pub struct Hover {
    pub text: Option<String>,
    pub at: Option<Position>,
    pub requesting: bool,
    pub screen_pos: Option<egui::Pos2>,
    /// When the pointer arrived where it now rests.
    pub resting_since: Option<Instant>,
    pub resting_at: Option<egui::Pos2>,
}

/// What the bottom panel shows.
#[derive(PartialEq, Eq, Clone, Copy, Default)]
pub enum BottomPanel {
    #[default]
    Diagnostics,
    References,
}

/// An in-progress rename: the symbol, and the name being typed for it.
pub struct Rename {
    pub path: PathBuf,
    pub position: Position,
    pub old_name: String,
    pub new_name: String,
}

/// The file finder: a filter over every tracked file.
#[derive(Default)]
pub struct QuickOpen {
    pub open: bool,
    pub query: String,
    /// Tracked paths, loaded once per repository.
    pub files: Vec<String>,
    pub loading: bool,
    pub selected: usize,
}

impl QuickOpen {
    /// Paths matching the query, best first: a match in the file name beats
    /// one anywhere in the path, which is what you mean when you type a
    /// name rather than a directory.
    pub fn matches(&self) -> Vec<&String> {
        let needle = self.query.trim().to_lowercase();
        if needle.is_empty() {
            return self.files.iter().take(200).collect();
        }
        let mut by_name = Vec::new();
        let mut by_path = Vec::new();
        for path in &self.files {
            let lower = path.to_lowercase();
            let name = lower.rsplit('/').next().unwrap_or(&lower).to_string();
            if name.contains(&needle) {
                by_name.push(path);
            } else if lower.contains(&needle) {
                by_path.push(path);
            }
        }
        by_name.extend(by_path);
        by_name.truncate(200);
        by_name
    }
}

/// One node of the work tree.
#[derive(Debug, Clone, Default)]
pub struct TreeNode {
    /// The last path component, which is what is shown.
    pub name: String,
    /// Repo-relative path.
    pub path: String,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    pub fn is_dir(&self) -> bool {
        !self.children.is_empty()
    }

    /// Builds a tree from repo-relative paths.
    ///
    /// Directories are inferred from the paths themselves: git tracks files,
    /// not directories, and an empty directory is not part of the work tree
    /// in any sense that matters here.
    pub fn build(paths: &[String]) -> Vec<TreeNode> {
        let mut root: Vec<TreeNode> = Vec::new();
        for path in paths {
            let mut level = &mut root;
            let mut walked = String::new();
            let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
            for (i, part) in parts.iter().enumerate() {
                if !walked.is_empty() {
                    walked.push('/');
                }
                walked.push_str(part);
                let existing = level.iter().position(|n| n.name == *part);
                let index = match existing {
                    Some(index) => index,
                    None => {
                        level.push(TreeNode {
                            name: (*part).to_string(),
                            path: walked.clone(),
                            children: Vec::new(),
                        });
                        level.len() - 1
                    }
                };
                if i + 1 == parts.len() {
                    break;
                }
                level = &mut level[index].children;
            }
        }
        sort(&mut root);
        root
    }
}

/// Directories first, then files, each alphabetically — the order every
/// file browser uses, because it is the one people scan by.
fn sort(nodes: &mut [TreeNode]) {
    for node in nodes.iter_mut() {
        sort(&mut node.children);
    }
    nodes.sort_by(|a, b| {
        b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// The find-and-replace bar.
#[derive(Default)]
pub struct Find {
    pub open: bool,
    pub replacing: bool,
    pub query: String,
    pub replacement: String,
    pub options: super::edits::MatchOptions,
    /// Matches in the active buffer, recomputed as the query changes.
    pub matches: Vec<super::edits::Range>,
    pub current: usize,
    /// Set when the bar has just opened, so it can take focus once.
    pub focus: bool,
}

/// Searching the contents of every tracked file.
#[derive(Default)]
pub struct ProjectSearch {
    /// Whether the sidebar is showing search instead of the tree.
    pub open: bool,
    pub query: String,
    pub options: crate::git::GrepOptions,
    pub hits: Vec<crate::git::GrepHit>,
    pub running: bool,
    /// Set when the query returned nothing, to say so rather than showing
    /// an empty panel that looks like it is still loading.
    pub searched: bool,
}

/// Everything the editor tab owns.
#[derive(Default)]
pub struct EditorState {
    pub files: Vec<OpenFile>,
    pub active: Option<usize>,
    pub completion: Completion,
    pub hover: Hover,
    pub rename: Option<Rename>,
    pub bottom: BottomPanel,
    /// Results of the last find-references.
    pub references: Vec<Location>,
    pub outline_open: bool,
    /// Run the server's formatter on save.
    pub format_on_save: bool,
    /// Language server status line and last error.
    pub status: Option<String>,
    pub error: Option<String>,
    /// Requests in flight, so the UI can say so.
    pub busy: usize,
    pub quick_open: QuickOpen,
    /// The work tree, built once from the tracked file list.
    pub tree: Vec<TreeNode>,
    /// Directories the user has opened.
    pub expanded: std::collections::HashSet<String>,
    /// Filter applied to the tree.
    pub tree_filter: String,
    pub find: Find,
    /// Project-wide content search, in the sidebar beside the tree.
    pub search: ProjectSearch,
    /// The go-to-line box, when it is open.
    pub goto_line: Option<String>,
    /// Selection in the active buffer, as byte offsets.
    pub selection: Range,
    pub wrap: bool,
}

impl EditorState {
    pub fn active_file(&self) -> Option<&OpenFile> {
        self.active.and_then(|i| self.files.get(i))
    }

    pub fn active_file_mut(&mut self) -> Option<&mut OpenFile> {
        match self.active {
            Some(i) => self.files.get_mut(i),
            None => None,
        }
    }

    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.files.iter().position(|f| f.path == path)
    }

    pub fn file_mut(&mut self, path: &Path) -> Option<&mut OpenFile> {
        self.files.iter_mut().find(|f| f.path == path)
    }

    /// Files with unsaved edits, for the close-the-repo warning.
    pub fn dirty_files(&self) -> Vec<&OpenFile> {
        self.files.iter().filter(|f| f.is_dirty()).collect()
    }
}

/// Something the UI decided to do, carried out after rendering so the
/// borrow of `app.editor` can end first.
pub enum Action {
    Open { path: PathBuf, reveal: Option<u32> },
    Close(usize),
    Save,
    Format,
    Sync(PathBuf),
    Hover { path: PathBuf, position: Position, screen_pos: egui::Pos2 },
    Definition { path: PathBuf, position: Position },
    References { path: PathBuf, position: Position },
    Symbols(PathBuf),
    Completion { path: PathBuf, position: Position, anchor: usize },
    StartRename { path: PathBuf, position: Position, old_name: String },
    ApplyRename { path: PathBuf, position: Position, new_name: String },
    RestartServers,
    /// Open the file finder, loading the tracked file list if needed.
    QuickOpen,
    /// Load the tracked file list, for the work tree.
    LoadTree,
    /// Run the project-wide content search.
    Search,
    /// Ask the AI about the selection.
    Assist(crate::agent::assist::Kind),
}

/// The editor's half of the sidebar: what to open, and what is wrong with
/// it. The code itself lives in the viewport — a 340pt column is no place
/// to read a file.
pub fn editor_sidebar(app: &mut App, ui: &mut egui::Ui) {
    let mut actions: Vec<Action> = Vec::new();

    if app.repo.is_none() {
        ui.label(RichText::new("Open a repository to edit files.").color(theme::fg_dim()));
        return;
    }
    // The tree is the tracked file list; ask for it the first time it is
    // needed rather than at startup.
    if app.editor.tree.is_empty() && !app.editor.quick_open.loading {
        actions.push(Action::LoadTree);
    }

    ui.horizontal(|ui| {
        if ui
            .selectable_label(!app.editor.search.open, "Files")
            .on_hover_text("The work tree")
            .clicked()
        {
            app.editor.search.open = false;
        }
        if ui
            .selectable_label(app.editor.search.open, "Search")
            .on_hover_text("Search the contents of every tracked file")
            .clicked()
        {
            app.editor.search.open = true;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("Open…")
                .on_hover_text("Filter every tracked file (Cmd/Ctrl+O)")
                .clicked()
            {
                actions.push(Action::QuickOpen);
            }
        });
    });
    quick_open(app, ui, &mut actions);

    if app.editor.search.open {
        search_panel(app, ui, &mut actions);
        run_actions(app, actions);
        return;
    }

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut app.editor.tree_filter)
                .hint_text(super::views::dim_hint("filter the tree"))
                .desired_width(f32::INFINITY),
        );
    });
    ui.separator();

    if app.editor.tree.is_empty() {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("reading the work tree…").small().color(theme::fg_dim()));
        });
        run_actions(app, actions);
        return;
    }

    let filter = app.editor.tree_filter.trim().to_lowercase();
    let tree = app.editor.tree.clone();
    let open: Vec<String> = app.editor.files.iter().map(|f| f.rel.clone()).collect();
    let active = app.editor.active_file().map(|f| f.rel.clone());
    let dirty: Vec<String> = app
        .editor
        .files
        .iter()
        .filter(|f| f.is_dirty())
        .map(|f| f.rel.clone())
        .collect();

    ScrollArea::vertical().auto_shrink([false, false]).id_salt("work-tree").show(ui, |ui| {
        let context = TreeContext { filter: &filter, open: &open, active: &active, dirty: &dirty };
        for node in &tree {
            tree_node(app, ui, node, &context, &mut actions);
        }
    });

    run_actions(app, actions);
}

/// Search across the contents of every tracked file.
fn search_panel(app: &mut App, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    let mut run = false;
    ui.horizontal(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut app.editor.search.query)
                .hint_text(super::views::dim_hint("search files"))
                .desired_width(f32::INFINITY),
        );
        // On Enter, not on every keystroke: each search is a git process.
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            run = true;
        }
    });
    ui.horizontal(|ui| {
        let options = &mut app.editor.search.options;
        if ui
            .selectable_label(options.case_sensitive, "Aa")
            .on_hover_text("Match case")
            .clicked()
        {
            options.case_sensitive = !options.case_sensitive;
            run = true;
        }
        if ui.selectable_label(options.whole_word, "W").on_hover_text("Whole word").clicked() {
            options.whole_word = !options.whole_word;
            run = true;
        }
        if ui
            .selectable_label(options.regex, ".*")
            .on_hover_text("Regular expression")
            .clicked()
        {
            options.regex = !options.regex;
            run = true;
        }
        if ui.small_button("Search").clicked() {
            run = true;
        }
        if app.editor.search.running {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    ui.separator();

    if run && !app.editor.search.query.trim().is_empty() {
        actions.push(Action::Search);
    }

    let hits = app.editor.search.hits.clone();
    if hits.is_empty() {
        if app.editor.search.searched && !app.editor.search.running {
            ui.label(RichText::new("No matches.").small().color(theme::fg_dim()));
        }
        return;
    }

    let files = hits.iter().map(|h| &h.path).collect::<std::collections::HashSet<_>>().len();
    ui.label(
        RichText::new(format!("{} hit(s) in {files} file(s)", hits.len()))
            .small()
            .color(theme::fg_dim()),
    );

    let Some(repo) = app.repo.clone() else { return };
    ScrollArea::vertical().auto_shrink([false, false]).id_salt("search-hits").show(ui, |ui| {
        let mut last_path: Option<&str> = None;
        for hit in &hits {
            // Grouped by file, with the path shown once.
            if last_path != Some(hit.path.as_str()) {
                ui.add_space(4.0);
                ui.label(RichText::new(&hit.path).small().color(theme::teal()));
                last_path = Some(hit.path.as_str());
            }
            let line = format!("{:>5}  {}", hit.line, hit.text.trim());
            if ui
                .selectable_label(false, RichText::new(line).small().monospace())
                .on_hover_text("Open here")
                .clicked()
            {
                actions.push(Action::Open {
                    path: repo.path().join(&hit.path),
                    reveal: Some(hit.line.saturating_sub(1)),
                });
            }
        }
    });
}

/// What the tree needs to know about the editor while it draws.
struct TreeContext<'a> {
    filter: &'a str,
    open: &'a [String],
    active: &'a Option<String>,
    dirty: &'a [String],
}

/// Whether a subtree contains anything matching the filter.
fn matches(node: &TreeNode, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    node.path.to_lowercase().contains(filter)
        || node.children.iter().any(|child| matches(child, filter))
}

/// One directory or file in the work tree.
fn tree_node(
    app: &mut App,
    ui: &mut egui::Ui,
    node: &TreeNode,
    context: &TreeContext<'_>,
    actions: &mut Vec<Action>,
) {
    if !matches(node, context.filter) {
        return;
    }

    if node.is_dir() {
        // A filter is a search: showing its hits collapsed would hide them.
        let default_open = !context.filter.is_empty()
            || app.editor.expanded.contains(&node.path);
        let header = egui::CollapsingHeader::new(
            RichText::new(&node.name).color(theme::teal()),
        )
        .id_salt(("tree", &node.path))
        .default_open(default_open);

        let response = header.show(ui, |ui| {
            for child in &node.children {
                tree_node(app, ui, child, context, actions);
            }
        });
        // Remember what the user opened, so it survives a rebuild.
        if response.fully_open() {
            app.editor.expanded.insert(node.path.clone());
        } else {
            app.editor.expanded.remove(&node.path);
        }
        return;
    }

    let is_open = context.open.contains(&node.path);
    let is_dirty = context.dirty.contains(&node.path);
    let label = format!("{}{}", node.name, if is_dirty { " •" } else { "" });
    let color = if is_open { theme::fg() } else { theme::fg_dim() };
    let selected = context.active.as_deref() == Some(node.path.as_str());
    if ui
        .selectable_label(selected, RichText::new(label).color(color))
        .on_hover_text(&node.path)
        .clicked()
    {
        if let Some(repo) = app.repo.clone() {
            actions.push(Action::Open { path: repo.path().join(&node.path), reveal: None });
        }
    }
}

/// The editor's viewport: the file, full size.
pub fn editor_viewport(app: &mut App, ui: &mut egui::Ui) {
    let mut actions: Vec<Action> = Vec::new();

    if app.editor.active_file().is_none() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No file open").color(theme::fg_dim()));
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Open one from the Editor panel, double-click a file in Changes, \
                     or press Cmd/Ctrl+O.",
                )
                .color(theme::fg_dim())
                .small(),
            );
        });
        return;
    }

    viewport_header(app, ui, &mut actions);
    find_bar(app, ui);
    goto_line_bar(app, ui);

    // Diagnostics come straight from the server's shared state; they change
    // without anything in the UI asking, so they are read fresh each frame.
    let diagnostics: Vec<Diagnostic> = app
        .editor
        .active_file()
        .map(|f| app.lsp.diagnostics(&f.path))
        .unwrap_or_default();

    let available = ui.available_height();
    let bottom_height = (available * 0.26).clamp(80.0, 240.0);

    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(ui.available_width() - if app.editor.outline_open { 190.0 } else { 0.0 });
            code_area(app, ui, available - bottom_height, &diagnostics, &mut actions);
            ui.separator();
            bottom_panel(app, ui, bottom_height, &diagnostics, &mut actions);
        });
        if app.editor.outline_open {
            ui.separator();
            outline(app, ui, available, &mut actions);
        }
    });
    run_actions(app, actions);
}

/// Find, and optionally replace, within the open file.
fn find_bar(app: &mut App, ui: &mut egui::Ui) {
    if !app.editor.find.open {
        return;
    }
    let Some(index) = app.editor.active else { return };

    let mut changed = false;
    let mut step = 0isize;
    let mut replace_one = false;
    let mut replace_all = false;

    egui::Frame::new().fill(theme::panel2()).inner_margin(6.0).show(ui, |ui| {
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut app.editor.find.query)
                    .hint_text(super::views::dim_hint("find"))
                    .desired_width(240.0),
            );
            if app.editor.find.focus {
                response.request_focus();
                app.editor.find.focus = false;
            }
            if response.changed() {
                changed = true;
            }
            // Enter steps through matches, which is what the key means in
            // every editor's find bar.
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                step = 1;
                app.editor.find.focus = true;
            }

            let count = app.editor.find.matches.len();
            let label = if count == 0 {
                if app.editor.find.query.is_empty() {
                    String::new()
                } else {
                    "no matches".into()
                }
            } else {
                format!("{} of {count}", app.editor.find.current + 1)
            };
            ui.label(RichText::new(label).small().color(theme::fg_dim()));
            if ui.small_button("‹").on_hover_text("Previous").clicked() {
                step = -1;
            }
            if ui.small_button("›").on_hover_text("Next (Cmd/Ctrl+G)").clicked() {
                step = 1;
            }
            if ui
                .selectable_label(app.editor.find.options.case_sensitive, "Aa")
                .on_hover_text("Match case")
                .clicked()
            {
                app.editor.find.options.case_sensitive =
                    !app.editor.find.options.case_sensitive;
                changed = true;
            }
            if ui
                .selectable_label(app.editor.find.options.whole_word, "W")
                .on_hover_text("Whole word")
                .clicked()
            {
                app.editor.find.options.whole_word = !app.editor.find.options.whole_word;
                changed = true;
            }
            if ui
                .selectable_label(app.editor.find.replacing, "Replace")
                .on_hover_text("Cmd/Ctrl+Alt+F")
                .clicked()
            {
                app.editor.find.replacing = !app.editor.find.replacing;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("×").on_hover_text("Close (Esc)").clicked() {
                    app.editor.find.open = false;
                }
            });
        });

        if app.editor.find.replacing {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.editor.find.replacement)
                        .hint_text(super::views::dim_hint("replace with"))
                        .desired_width(240.0),
                );
                let any = !app.editor.find.matches.is_empty();
                if ui.add_enabled(any, egui::Button::new("Replace")).clicked() {
                    replace_one = true;
                }
                if ui
                    .add_enabled(any, egui::Button::new("Replace all"))
                    .on_hover_text("Every match in this file")
                    .clicked()
                {
                    replace_all = true;
                }
            });
        }
    });

    if changed {
        refresh_matches(app, index);
    }
    if step != 0 {
        step_match(app, index, step);
    }
    if replace_one {
        let current = app.editor.find.current;
        if let Some(range) = app.editor.find.matches.get(current).copied() {
            let replacement = app.editor.find.replacement.clone();
            let text = edits::replace_at(&app.editor.files[index].text, range, &replacement);
            let end = range.0 + replacement.len();
            apply_edit(app, index, text, (range.0, end), ui.ctx());
            refresh_matches(app, index);
        }
    }
    if replace_all {
        let (query, replacement, options) = (
            app.editor.find.query.clone(),
            app.editor.find.replacement.clone(),
            app.editor.find.options,
        );
        let (text, count) =
            edits::replace_all(&app.editor.files[index].text, &query, &replacement, options);
        let cursor = app.editor.files[index].cursor.min(text.len());
        apply_edit(app, index, text, (cursor, cursor), ui.ctx());
        refresh_matches(app, index);
        app.toast(format!("Replaced {count} occurrence(s)."), false);
    }
}

/// Jump to a line number.
fn goto_line_bar(app: &mut App, ui: &mut egui::Ui) {
    let Some(mut input) = app.editor.goto_line.clone() else { return };
    let Some(index) = app.editor.active else { return };

    let mut go = false;
    egui::Frame::new().fill(theme::panel2()).inner_margin(6.0).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Go to line").small());
            let response = ui.add(
                egui::TextEdit::singleline(&mut input).desired_width(80.0).hint_text("1"),
            );
            response.request_focus();
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            let lines = app.editor.files[index].text.lines().count();
            ui.label(RichText::new(format!("of {lines}")).small().color(theme::fg_dim()));
            if ui.small_button("Go").clicked() {
                go = true;
            }
        });
    });

    if go {
        if let Ok(line) = input.trim().parse::<u32>() {
            let offset = edits::line_start(&app.editor.files[index].text, line);
            app.editor.selection = (offset, offset);
            app.editor.files[index].cursor = offset;
            app.editor.files[index].reveal = Some(line.saturating_sub(1));
        }
        app.editor.goto_line = None;
    } else {
        app.editor.goto_line = Some(input);
    }
}

/// File name, dirty marker, and the actions that act on the open file.
fn viewport_header(app: &mut App, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    let Some(file) = app.editor.active_file() else { return };
    let (rel, dirty) = (file.rel.clone(), file.is_dirty());
    ui.horizontal(|ui| {
        ui.label(RichText::new(&rel).strong().color(theme::ember()));
        if dirty {
            ui.label(RichText::new("• unsaved").small().color(theme::warn()));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if super::views::panel_button(ui, "Save", dirty)
                .on_hover_text("Ctrl+S")
                .clicked()
            {
                actions.push(Action::Save);
            }
            if super::views::panel_button(ui, "Format", true)
                .on_hover_text("Format with the language server")
                .clicked()
            {
                actions.push(Action::Format);
            }
            if app.editor.busy > 0 {
                ui.add(egui::Spinner::new().size(12.0));
            }
            ui.checkbox(&mut app.editor.outline_open, "Outline");
            ui.checkbox(&mut app.editor.format_on_save, "Format on save");

            // The AI actions that act on what you are looking at. They
            // report into the Agent tab, where changes are reviewed.
            ui.menu_button("AI ▾", |ui| {
                use crate::agent::assist::Kind;
                let selected = {
                    let (a, b) = app.editor.selection;
                    a != b
                };
                ui.label(
                    RichText::new(if selected {
                        "on the selection"
                    } else {
                        "on the whole file"
                    })
                    .small()
                    .color(theme::fg_dim()),
                );
                for kind in [Kind::Explain, Kind::Fix, Kind::Tests, Kind::Document] {
                    if ui.button(kind.label()).clicked() {
                        actions.push(Action::Assist(kind));
                        ui.close();
                    }
                }
            });
        });
    });
    ui.separator();
}

/// The file finder, shown inline above the tabs while it is open.
fn quick_open(app: &mut App, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    if !app.editor.quick_open.open {
        return;
    }
    let Some(repo) = app.repo.clone() else { return };

    egui::Frame::popup(ui.style()).show(ui, |ui| {
        ui.set_min_width(520.0);
        let response = ui.add(
            egui::TextEdit::singleline(&mut app.editor.quick_open.query)
                .hint_text("file name or path")
                .desired_width(f32::INFINITY),
        );
        response.request_focus();

        if app.editor.quick_open.loading {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new("listing files…").small().color(theme::fg_dim()));
            });
            return;
        }

        let matches: Vec<String> =
            app.editor.quick_open.matches().into_iter().cloned().collect();
        if matches.is_empty() {
            ui.label(RichText::new("no matching file").small().color(theme::fg_dim()));
            return;
        }
        let selected = app.editor.quick_open.selected.min(matches.len() - 1);

        let (up, down, accept, escape) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            )
        });
        if down {
            app.editor.quick_open.selected = (selected + 1) % matches.len();
        }
        if up {
            app.editor.quick_open.selected = (selected + matches.len() - 1) % matches.len();
        }
        if escape {
            app.editor.quick_open.open = false;
        }

        let mut chosen: Option<String> = None;
        if accept {
            chosen = matches.get(selected).cloned();
        }
        ScrollArea::vertical().max_height(260.0).id_salt("quick-open").show(ui, |ui| {
            for (i, path) in matches.iter().enumerate() {
                if ui
                    .selectable_label(i == selected, RichText::new(path).monospace().small())
                    .clicked()
                {
                    chosen = Some(path.clone());
                }
            }
        });

        if let Some(path) = chosen {
            app.editor.quick_open.open = false;
            app.editor.quick_open.query.clear();
            app.editor.quick_open.selected = 0;
            actions.push(Action::Open { path: repo.path().join(path), reveal: None });
        }
    });
}

/// The outline, from `textDocument/documentSymbol`.
fn outline(app: &mut App, ui: &mut egui::Ui, height: f32, actions: &mut Vec<Action>) {
    let Some(file) = app.editor.active_file() else { return };
    let path = file.path.clone();
    let symbols = file.symbols.clone();
    ui.vertical(|ui| {
        ui.label(theme::overline("OUTLINE"));
        if symbols.is_empty() {
            ui.label(RichText::new("no symbols").small().color(theme::fg_dim()));
            return;
        }
        ScrollArea::vertical().max_height(height).id_salt("editor-outline").show(ui, |ui| {
            for symbol in &symbols {
                let indent = "  ".repeat(symbol.depth);
                let label = format!("{indent}{}", symbol.name);
                if ui
                    .selectable_label(false, RichText::new(label).small())
                    .on_hover_text(symbol.kind_label())
                    .clicked()
                {
                    actions.push(Action::Open {
                        path: path.clone(),
                        reveal: Some(symbol.range.start.line),
                    });
                }
            }
        });
    });
}

/// The text area itself, with the gutter, highlighting, and every keyboard
/// interaction the language server drives.
fn code_area(
    app: &mut App,
    ui: &mut egui::Ui,
    height: f32,
    diagnostics: &[Diagnostic],
    actions: &mut Vec<Action>,
) {
    let Some(index) = app.editor.active else { return };
    let font = egui::FontId::monospace(13.0);
    let row_height = ui.fonts(|f| f.row_height(&font));

    // Keys are consumed before the text area sees them, so the completion
    // popup can own the arrows and Enter while it is open.
    let keys = read_keys(ui, app.editor.completion.open);

    let path = app.editor.files[index].path.clone();
    let read_only = app.editor.files[index].read_only;
    let reveal = app.editor.files[index].reveal.take();

    // The layouter closes over the diagnostics so their ranges can be
    // underlined in place, which is the whole point of having them here.
    let lang = app.editor.files[index].lang;
    let diagnostics = diagnostics.to_vec();
    let mut layouter = move |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap: f32| {
        let mut job = highlight(buffer.as_str(), lang, &diagnostics, font.clone());
        job.wrap.max_width = wrap;
        ui.fonts(|f| f.layout_job(job))
    };

    let mut scroll = ScrollArea::vertical().id_salt("editor-code").max_height(height);
    if let Some(line) = reveal {
        // Put the target line a third of the way down rather than at the very
        // top, so the code around it is visible too.
        let offset = (line as f32 * row_height - height / 3.0).max(0.0);
        scroll = scroll.vertical_scroll_offset(offset);
    }

    let response = scroll.show(ui, |ui| {
        ui.horizontal_top(|ui| {
            gutter(app, ui, index, row_height, &diagnostics_by_line(&app.lsp.diagnostics(&path)));
            let file = &mut app.editor.files[index];
            let output = egui::TextEdit::multiline(&mut file.text)
                .id(egui::Id::new(("editor-buffer", &file.path)))
                .code_editor()
                .desired_width(f32::INFINITY)
                .desired_rows(24)
                .interactive(!read_only)
                .layouter(&mut layouter)
                .show(ui);
            output
        })
        .inner
    });
    let output = response.inner;

    // Scoped: the keyboard handling below needs `app` whole, and this
    // borrow would otherwise still be alive there.
    {
    let file = &mut app.editor.files[index];

    // Cursor and selection, as byte offsets.
    if let Some(range) = output.cursor_range {
        file.cursor = char_to_byte(&file.text, range.primary.index);
        let secondary = char_to_byte(&file.text, range.secondary.index);
        app.editor.selection = (secondary, file.cursor);
    }

    // Typing: mark for a debounced didChange, and keep the completion filter
    // in step with what has been typed since the popup opened.
    if output.response.changed() {
        file.dirty_since = Some(Instant::now());
        if app.editor.completion.open {
            let anchor = app.editor.completion.anchor.min(file.text.len());
            let cursor = file.cursor.min(file.text.len());
            if cursor >= anchor {
                app.editor.completion.filter = file.text[anchor..cursor].to_string();
            } else {
                app.editor.completion.close();
            }
        }
    }

    // Debounced sync: send the buffer once typing pauses.
    let should_sync = file
        .dirty_since
        .is_some_and(|at| at.elapsed() >= SYNC_DEBOUNCE);
    if should_sync {
        file.dirty_since = None;
        actions.push(Action::Sync(path.clone()));
    } else if file.dirty_since.is_some() {
        // Keep frames coming so the debounce actually fires while idle.
        ui.ctx().request_repaint_after(SYNC_DEBOUNCE);
    }

    }

    let position = app.editor.files[index].cursor_position();
    let cursor_screen = output
        .cursor_range
        .map(|r| output.galley.pos_from_cursor(r.primary))
        .map(|rect| output.galley_pos + rect.left_bottom().to_vec2());

    // -- keyboard ----------------------------------------------------------
    if keys.save {
        actions.push(Action::Save);
    }
    if keys.format {
        actions.push(Action::Format);
    }
    if keys.definition {
        actions.push(Action::Definition { path: path.clone(), position });
    }
    if keys.references {
        actions.push(Action::References { path: path.clone(), position });
    }
    if keys.rename {
        let file = &app.editor.files[index];
        let (start, end) = file.word_at(file.cursor);
        actions.push(Action::StartRename {
            path: path.clone(),
            position,
            old_name: file.text[start..end].to_string(),
        });
    }
    if keys.completion && !app.editor.completion.requesting {
        let file = &app.editor.files[index];
        let (start, cursor) = (file.word_at(file.cursor).0, file.cursor);
        let filter = file.text[start..cursor].to_string();
        app.editor.completion.anchor = start;
        app.editor.completion.filter = filter;
        app.editor.completion.screen_pos = cursor_screen;
        actions.push(Action::Completion { path: path.clone(), position, anchor: start });
    }
    if keys.escape {
        app.editor.completion.close();
        app.editor.hover.text = None;
        app.editor.find.open = false;
        app.editor.goto_line = None;
    }
    if keys.find || keys.replace {
        app.editor.find.open = true;
        app.editor.find.replacing = keys.replace;
        app.editor.find.focus = true;
        // Seed the box with the selection, the way every editor does.
        let file = &app.editor.files[index];
        let (start, end) = app.editor.selection;
        if start != end && end <= file.text.len() {
            let (a, b) = (start.min(end), start.max(end));
            if !file.text[a..b].contains('\n') {
                app.editor.find.query = file.text[a..b].to_string();
            }
        }
        refresh_matches(app, index);
    }
    if keys.find_next {
        step_match(app, index, 1);
    }
    if keys.goto_line {
        app.editor.goto_line = Some(String::new());
    }
    if keys.comment {
        let lang = app.editor.files[index].lang;
        if let Some(marker) = edits::line_comment(lang) {
            let file = &mut app.editor.files[index];
            let (text, selection) =
                edits::toggle_comment(&file.text, app.editor.selection, marker);
            apply_edit(app, index, text, selection, ui.ctx());
        }
    }
    if keys.duplicate {
        let file = &app.editor.files[index];
        let (text, selection) = edits::duplicate_lines(&file.text, app.editor.selection);
        apply_edit(app, index, text, selection, ui.ctx());
    }
    if keys.move_up || keys.move_down {
        let file = &app.editor.files[index];
        let (text, selection) =
            edits::move_lines(&file.text, app.editor.selection, keys.move_up);
        apply_edit(app, index, text, selection, ui.ctx());
    }

    // Ctrl/Cmd-click is go-to-definition, the way every editor does it.
    if output.response.clicked() && (keys.command_down) {
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            let text = &app.editor.files[index].text;
            let cursor = output.galley.cursor_from_pos(pos - output.galley_pos);
            let offset = char_to_byte(text, cursor.index);
            actions.push(Action::Definition {
                path: path.clone(),
                position: protocol::offset_to_position(text, offset),
            });
        }
    }

    // -- hover -------------------------------------------------------------
    hover_interaction(app, ui, index, &output, actions);

    // -- completion popup --------------------------------------------------
    completion_popup(app, ui, index, keys, actions);
}

/// Line numbers plus a marker for the worst diagnostic on each line.
fn gutter(
    app: &App,
    ui: &mut egui::Ui,
    index: usize,
    row_height: f32,
    by_line: &std::collections::HashMap<u32, Severity>,
) {
    let file = &app.editor.files[index];
    let lines = file.text.lines().count().max(1);
    let width = format!("{lines}").len() as f32 * 9.0 + 18.0;

    ui.vertical(|ui| {
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 0.0;
        for line in 0..lines {
            let marker = by_line.get(&(line as u32));
            let (glyph, color) = match marker {
                Some(Severity::Error) => ("●", theme::danger()),
                Some(Severity::Warning) => ("●", theme::warn()),
                Some(_) => ("·", theme::fg_dim()),
                None => (" ", theme::fg_dim()),
            };
            ui.horizontal(|ui| {
                ui.set_height(row_height);
                ui.label(RichText::new(glyph).color(color).monospace().size(11.0));
                ui.label(
                    RichText::new(format!("{:>width$}", line + 1, width = format!("{lines}").len()))
                        .color(theme::fg_dim())
                        .monospace()
                        .size(11.0),
                );
            });
        }
    });
}

/// The worst severity per line, for the gutter.
fn diagnostics_by_line(
    diagnostics: &[Diagnostic],
) -> std::collections::HashMap<u32, Severity> {
    let mut map = std::collections::HashMap::new();
    for d in diagnostics {
        let entry = map.entry(d.range.start.line).or_insert(d.severity);
        if d.severity > *entry {
            *entry = d.severity;
        }
    }
    map
}

/// Requests a hover when the pointer rests, and shows the last answer.
fn hover_interaction(
    app: &mut App,
    ui: &mut egui::Ui,
    index: usize,
    output: &egui::text_edit::TextEditOutput,
    actions: &mut Vec<Action>,
) {
    let Some(pointer) = ui.ctx().pointer_hover_pos() else {
        app.editor.hover.resting_since = None;
        return;
    };
    if !output.response.hovered() {
        app.editor.hover.text = None;
        app.editor.hover.resting_since = None;
        return;
    }

    // Restart the clock whenever the pointer moves more than a character.
    let moved = app
        .editor
        .hover
        .resting_at
        .is_none_or(|at| (at - pointer).length() > 6.0);
    if moved {
        app.editor.hover.resting_at = Some(pointer);
        app.editor.hover.resting_since = Some(Instant::now());
        app.editor.hover.text = None;
        ui.ctx().request_repaint_after(HOVER_DELAY);
        return;
    }

    let rested = app
        .editor
        .hover
        .resting_since
        .is_some_and(|since| since.elapsed() >= HOVER_DELAY);
    if rested && app.editor.hover.text.is_none() && !app.editor.hover.requesting {
        let file = &app.editor.files[index];
        let cursor = output.galley.cursor_from_pos(pointer - output.galley_pos);
        let offset = char_to_byte(&file.text, cursor.index);
        let position = protocol::offset_to_position(&file.text, offset);
        app.editor.hover.requesting = true;
        app.editor.hover.screen_pos = Some(pointer);
        actions.push(Action::Hover {
            path: file.path.clone(),
            position,
            screen_pos: pointer,
        });
    }

    if let Some(text) = app.editor.hover.text.clone() {
        let anchor = app.editor.hover.screen_pos.unwrap_or(pointer) + egui::vec2(0.0, 18.0);
        egui::Tooltip::always_open(
            ui.ctx().clone(),
            ui.layer_id(),
            egui::Id::new("editor-hover"),
            egui::PopupAnchor::Position(anchor),
        )
        .width(520.0)
        .show(|ui| {
            super::markdown::render(ui, &text);
        });
    }
}

/// The completion list, drawn at the cursor.
fn completion_popup(
    app: &mut App,
    ui: &mut egui::Ui,
    index: usize,
    keys: Keys,
    _actions: &mut [Action],
) {
    if !app.editor.completion.open {
        return;
    }
    let filtered: Vec<CompletionItem> =
        app.editor.completion.filtered().into_iter().cloned().collect();
    if filtered.is_empty() {
        app.editor.completion.close();
        return;
    }

    let count = filtered.len();
    if keys.down {
        app.editor.completion.selected = (app.editor.completion.selected + 1) % count;
    }
    if keys.up {
        app.editor.completion.selected =
            (app.editor.completion.selected + count - 1) % count;
    }
    let selected = app.editor.completion.selected.min(count - 1);

    let pos = app
        .editor
        .completion
        .screen_pos
        .unwrap_or_else(|| ui.next_widget_position());

    let mut chosen: Option<CompletionItem> = None;
    egui::Area::new(egui::Id::new("editor-completion"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos + egui::vec2(0.0, 4.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(460.0);
                ScrollArea::vertical().max_height(220.0).id_salt("completion-list").show(
                    ui,
                    |ui| {
                        for (i, item) in filtered.iter().enumerate() {
                            let row = ui.horizontal(|ui| {
                                let kind = item.kind_label();
                                if !kind.is_empty() {
                                    ui.label(
                                        RichText::new(kind)
                                            .small()
                                            .color(theme::teal())
                                            .monospace(),
                                    );
                                }
                                let label = RichText::new(&item.label).monospace();
                                let label =
                                    if i == selected { label.strong() } else { label };
                                ui.label(label);
                                if let Some(detail) = &item.detail {
                                    ui.label(
                                        RichText::new(detail).small().color(theme::fg_dim()),
                                    );
                                }
                            });
                            let response = row.response.interact(egui::Sense::click());
                            if i == selected {
                                ui.painter().rect_stroke(
                                    response.rect.expand(1.0),
                                    2.0_f32,
                                    egui::Stroke::new(1.0_f32, theme::ember()),
                                    egui::StrokeKind::Outside,
                                );
                            }
                            if response.clicked() {
                                chosen = Some(item.clone());
                            }
                        }
                    },
                );
            });
        });

    if keys.accept {
        chosen = filtered.get(selected).cloned();
    }
    if let Some(item) = chosen {
        insert_completion(app, ui.ctx(), index, &item);
        app.editor.completion.close();
    }
}

/// Writes a chosen completion into the buffer and puts the cursor after it.
fn insert_completion(
    app: &mut App,
    ctx: &egui::Context,
    index: usize,
    item: &CompletionItem,
) {
    let anchor = app.editor.completion.anchor;
    let file = &mut app.editor.files[index];

    // A server-supplied range wins: it knows what it means to replace,
    // including the dot in `foo.bar`.
    let (start, end) = match item.range {
        Some(range) => protocol::range_to_offsets(&file.text, range),
        None => (anchor.min(file.text.len()), file.cursor.min(file.text.len())),
    };
    let (start, end) = (start.min(file.text.len()), end.min(file.text.len()));
    if start > end {
        return;
    }
    file.text.replace_range(start..end, &item.insert);
    file.cursor = start + item.insert.len();
    file.dirty_since = Some(Instant::now());

    // Move egui's own cursor, or the caret jumps back to where it was.
    let id = egui::Id::new(("editor-buffer", &file.path));
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let chars = byte_to_char(&file.text, file.cursor);
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(chars))));
        state.store(ctx, id);
    }
}

/// Diagnostics or references, under the code.
fn bottom_panel(
    app: &mut App,
    ui: &mut egui::Ui,
    height: f32,
    diagnostics: &[Diagnostic],
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        let errors = diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
        let warnings = diagnostics.iter().filter(|d| d.severity == Severity::Warning).count();
        let label = format!("Problems ({errors} errors, {warnings} warnings)");
        if ui
            .selectable_label(app.editor.bottom == BottomPanel::Diagnostics, label)
            .clicked()
        {
            app.editor.bottom = BottomPanel::Diagnostics;
        }
        let label = format!("References ({})", app.editor.references.len());
        if ui
            .selectable_label(app.editor.bottom == BottomPanel::References, label)
            .clicked()
        {
            app.editor.bottom = BottomPanel::References;
        }
    });

    ScrollArea::vertical().max_height(height).id_salt("editor-bottom").show(ui, |ui| {
        match app.editor.bottom {
            BottomPanel::Diagnostics => {
                let path = app.editor.active_file().map(|f| f.path.clone());
                if diagnostics.is_empty() {
                    ui.label(RichText::new("No problems reported.").small().color(theme::add()));
                }
                for d in diagnostics {
                    let color = match d.severity {
                        Severity::Error => theme::danger(),
                        Severity::Warning => theme::warn(),
                        _ => theme::fg_dim(),
                    };
                    let line = d.line();
                    if ui
                        .selectable_label(false, RichText::new(line).color(color).small())
                        .clicked()
                    {
                        if let Some(path) = path.clone() {
                            actions.push(Action::Open {
                                path,
                                reveal: Some(d.range.start.line),
                            });
                        }
                    }
                }
            }
            BottomPanel::References => {
                if app.editor.references.is_empty() {
                    ui.label(
                        RichText::new("Put the cursor on a symbol and press Shift+F12.")
                            .small()
                            .color(theme::fg_dim()),
                    );
                }
                let references = app.editor.references.clone();
                for reference in &references {
                    let Some(path) = protocol::uri_to_path(&reference.uri) else { continue };
                    let name = app
                        .repo
                        .as_ref()
                        .and_then(|r| path.strip_prefix(r.path()).ok())
                        .unwrap_or(&path)
                        .display()
                        .to_string();
                    let label = format!("{name}:{}", reference.range.start.line + 1);
                    if ui.selectable_label(false, RichText::new(label).small().monospace()).clicked()
                    {
                        actions.push(Action::Open {
                            path: path.clone(),
                            reveal: Some(reference.range.start.line),
                        });
                    }
                }
            }
        }
    });
}

/// Writes an edit into a buffer and puts the selection where it belongs.
fn apply_edit(app: &mut App, index: usize, text: String, selection: Range, ctx: &egui::Context) {
    let file = &mut app.editor.files[index];
    if file.read_only || file.text == text {
        return;
    }
    file.text = text;
    file.cursor = selection.1.min(file.text.len());
    file.dirty_since = Some(Instant::now());
    app.editor.selection = selection;

    // egui keeps its own cursor; without this the caret jumps to the top
    // after any programmatic edit.
    let id = egui::Id::new(("editor-buffer", &file.path));
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let range = CCursorRange::two(
            CCursor::new(byte_to_char(&file.text, selection.0)),
            CCursor::new(byte_to_char(&file.text, selection.1)),
        );
        state.cursor.set_char_range(Some(range));
        state.store(ctx, id);
    }
}

/// Recomputes the matches for the current query.
fn refresh_matches(app: &mut App, index: usize) {
    let query = app.editor.find.query.clone();
    let options = app.editor.find.options;
    let text = &app.editor.files[index].text;
    app.editor.find.matches = edits::find_all(text, &query, options);
    let from = app.editor.selection.1;
    app.editor.find.current =
        edits::next_match(&app.editor.find.matches, from).unwrap_or(0);
}

/// Moves to the next or previous match and selects it.
fn step_match(app: &mut App, index: usize, delta: isize) {
    if app.editor.find.matches.is_empty() {
        return;
    }
    let count = app.editor.find.matches.len();
    let current = app.editor.find.current;
    let next = if delta >= 0 { (current + 1) % count } else { (current + count - 1) % count };
    app.editor.find.current = next;
    let found = app.editor.find.matches[next];
    let line = edits::line_col(&app.editor.files[index].text, found.0).0;
    app.editor.files[index].reveal = Some(line.saturating_sub(1));
    app.editor.selection = found;
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

/// The editor's key bindings for one frame, already consumed so the text
/// area never sees them.
#[derive(Clone, Copy, Default)]
pub struct Keys {
    pub save: bool,
    pub format: bool,
    pub find: bool,
    pub replace: bool,
    pub find_next: bool,
    pub goto_line: bool,
    pub comment: bool,
    pub duplicate: bool,
    pub move_up: bool,
    pub move_down: bool,
    pub definition: bool,
    pub references: bool,
    pub rename: bool,
    pub completion: bool,
    pub escape: bool,
    pub up: bool,
    pub down: bool,
    pub accept: bool,
    pub command_down: bool,
}

fn read_keys(ui: &egui::Ui, completion_open: bool) -> Keys {
    use egui::{Key, Modifiers};
    let command = Modifiers::COMMAND;
    ui.input_mut(|i| Keys {
        save: i.consume_key(command, Key::S),
        format: i.consume_key(command | Modifiers::SHIFT, Key::F),
        find: i.consume_key(command, Key::F),
        replace: i.consume_key(command | Modifiers::ALT, Key::F),
        find_next: i.consume_key(command, Key::G),
        goto_line: i.consume_key(command | Modifiers::SHIFT, Key::G),
        comment: i.consume_key(command, Key::Slash),
        duplicate: i.consume_key(command | Modifiers::SHIFT, Key::D),
        move_up: i.consume_key(Modifiers::ALT, Key::ArrowUp),
        move_down: i.consume_key(Modifiers::ALT, Key::ArrowDown),
        definition: i.consume_key(Modifiers::NONE, Key::F12),
        references: i.consume_key(Modifiers::SHIFT, Key::F12),
        rename: i.consume_key(Modifiers::NONE, Key::F2),
        completion: i.consume_key(command, Key::Space),
        escape: i.consume_key(Modifiers::NONE, Key::Escape),
        // The popup owns the arrows and Enter only while it is open, so
        // normal editing keeps working when it is not.
        up: completion_open && i.consume_key(Modifiers::NONE, Key::ArrowUp),
        down: completion_open && i.consume_key(Modifiers::NONE, Key::ArrowDown),
        accept: completion_open
            && (i.consume_key(Modifiers::NONE, Key::Enter)
                || i.consume_key(Modifiers::NONE, Key::Tab)),
        command_down: i.modifiers.command,
    })
}

// ---------------------------------------------------------------------------
// Highlighting
// ---------------------------------------------------------------------------

/// Lays out the buffer: syntax colours, with diagnostic ranges underlined.
pub fn highlight(
    text: &str,
    lang: super::syntax::Lang,
    diagnostics: &[Diagnostic],
    font: FontId,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let lines: Vec<&str> = text.split('\n').collect();
    let langs = super::syntax::langs_per_line(&lines, lang);

    for (number, line) in lines.iter().enumerate() {
        let line_lang = langs.get(number).copied().unwrap_or(lang);
        let underlines = underline_ranges(diagnostics, number as u32, line);
        // Spans carry text, not offsets, so the offset is accumulated as
        // they are walked — that is what the diagnostic ranges are in.
        let mut at = 0usize;
        for span in super::syntax::highlight_line(line_lang, line, theme::fg()) {
            let (span_start, span_end) = (at, at + span.text.len());
            at = span_end;
            // A span can straddle the start or end of a diagnostic, so it is
            // split at every boundary rather than underlined wholesale.
            for (start, end, severity) in split_span(span_start, span_end, &underlines) {
                let mut format = TextFormat::simple(font.clone(), span.color);
                if let Some(severity) = severity {
                    format.underline = egui::Stroke::new(
                        if severity == Severity::Error { 2.0_f32 } else { 1.0_f32 },
                        match severity {
                            Severity::Error => theme::danger(),
                            Severity::Warning => theme::warn(),
                            _ => theme::fg_dim(),
                        },
                    );
                }
                job.append(&span.text[start - span_start..end - span_start], 0.0, format);
            }
        }
        if number + 1 < lines.len() {
            job.append("\n", 0.0, TextFormat::simple(font.clone(), theme::fg()));
        }
    }
    job
}

/// Byte ranges within one line that a diagnostic covers.
fn underline_ranges(
    diagnostics: &[Diagnostic],
    line: u32,
    text: &str,
) -> Vec<(usize, usize, Severity)> {
    let mut out = Vec::new();
    for d in diagnostics {
        if line < d.range.start.line || line > d.range.end.line {
            continue;
        }
        let start = if line == d.range.start.line {
            protocol::utf16_to_byte(text, d.range.start.character)
        } else {
            0
        };
        let end = if line == d.range.end.line {
            protocol::utf16_to_byte(text, d.range.end.character)
        } else {
            text.len()
        };
        // A zero-width diagnostic (a missing token) still needs something to
        // underline, so it claims one character.
        let end = if end > start { end } else { (start + 1).min(text.len()) };
        if start < end {
            out.push((start, end, d.severity));
        }
    }
    out.sort_by_key(|(start, ..)| *start);
    out
}

/// Splits `start..end` at every underline boundary, tagging each piece.
fn split_span(
    start: usize,
    end: usize,
    underlines: &[(usize, usize, Severity)],
) -> Vec<(usize, usize, Option<Severity>)> {
    let mut cuts: Vec<usize> = vec![start, end];
    for (u_start, u_end, _) in underlines {
        if *u_start > start && *u_start < end {
            cuts.push(*u_start);
        }
        if *u_end > start && *u_end < end {
            cuts.push(*u_end);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();

    let mut out = Vec::new();
    for pair in cuts.windows(2) {
        let (piece_start, piece_end) = (pair[0], pair[1]);
        if piece_start >= piece_end {
            continue;
        }
        let severity = underlines
            .iter()
            .filter(|(u_start, u_end, _)| *u_start <= piece_start && *u_end >= piece_end)
            .map(|(.., severity)| *severity)
            .max();
        out.push((piece_start, piece_end, severity));
    }
    if out.is_empty() && start < end {
        out.push((start, end, None));
    }
    out
}

// ---------------------------------------------------------------------------
// Offsets
// ---------------------------------------------------------------------------

/// Byte offset for a character index, as egui's cursor counts them.
pub fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(text.len())
}

/// Character index for a byte offset.
pub fn byte_to_char(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].chars().count()
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Carries out what the UI decided, now that `app.editor` is no longer
/// borrowed.
fn run_actions(app: &mut App, actions: Vec<Action>) {
    for action in actions {
        match action {
            Action::Open { path, reveal } => app.editor_open(&path, reveal),
            Action::Close(index) => app.editor_close(index),
            Action::Save => app.editor_save(),
            Action::Format => app.editor_format(),
            Action::Sync(path) => app.lsp_sync(&path),
            Action::Hover { path, position, screen_pos } => {
                app.editor.hover.screen_pos = Some(screen_pos);
                app.lsp_hover(&path, position);
            }
            Action::Definition { path, position } => app.lsp_definition(&path, position),
            Action::References { path, position } => app.lsp_references(&path, position),
            Action::Symbols(path) => app.lsp_symbols(&path),
            Action::Completion { path, position, anchor } => {
                app.lsp_completion(&path, position, anchor)
            }
            Action::StartRename { path, position, old_name } => {
                app.editor.rename = Some(Rename {
                    path,
                    position,
                    new_name: old_name.clone(),
                    old_name,
                });
                app.dialog = super::Dialog::Rename;
            }
            Action::ApplyRename { path, position, new_name } => {
                app.lsp_rename(&path, position, &new_name)
            }
            Action::RestartServers => app.lsp_restart(),
            Action::QuickOpen => app.editor_quick_open(),
            Action::LoadTree => app.editor_load_tree(),
            Action::Search => app.editor_search(),
            Action::Assist(kind) => app.editor_assist(kind),
        }
    }
}

/// Unused import guard: the AI model picker is used by the agent tab, which
/// shares this module's toolbar helpers.
#[allow(dead_code)]
fn _target() -> AiTarget {
    AiTarget::Review
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::protocol::Range;

    fn diagnostic(line: u32, start: u32, end: u32, severity: Severity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position::new(line, start),
                end: Position::new(line, end),
            },
            severity,
            code: None,
            message: "problem".into(),
            source: None,
        }
    }

    fn file(text: &str) -> OpenFile {
        OpenFile {
            path: PathBuf::from("/repo/src/main.rs"),
            rel: "src/main.rs".into(),
            text: text.to_string(),
            saved: text.to_string(),
            lang: super::super::syntax::Lang::Rust,
            symbols: Vec::new(),
            cursor: 0,
            reveal: None,
            dirty_since: None,
            synced: false,
            read_only: false,
        }
    }

    #[test]
    fn a_buffer_is_dirty_only_once_it_differs_from_disk() {
        let mut f = file("fn main() {}\n");
        assert!(!f.is_dirty());
        f.text.push_str("// edit\n");
        assert!(f.is_dirty());
        f.saved = f.text.clone();
        assert!(!f.is_dirty());
    }

    #[test]
    fn the_word_under_the_cursor_is_found_in_both_directions() {
        let f = file("let value = compute_it(x);\n");
        let offset = f.text.find("compute_it").unwrap() + 3;
        let (start, end) = f.word_at(offset);
        assert_eq!(&f.text[start..end], "compute_it");
    }

    #[test]
    fn the_word_at_a_boundary_is_empty_rather_than_wrong() {
        let f = file("a + b\n");
        let offset = f.text.find('+').unwrap();
        let (start, end) = f.word_at(offset);
        assert_eq!(&f.text[start..end], "");
    }

    #[test]
    fn char_and_byte_offsets_round_trip_through_wide_characters() {
        let text = "let s = \"🦀 crab\";\n";
        let byte = text.find("crab").unwrap();
        let chars = byte_to_char(text, byte);
        assert_eq!(char_to_byte(text, chars), byte);
        assert_eq!(char_to_byte(text, 9999), text.len());
    }

    #[test]
    fn a_diagnostic_underlines_only_its_own_range() {
        let text = "let x = broken();";
        let start = protocol::byte_to_utf16(text, text.find("broken").unwrap());
        let end = protocol::byte_to_utf16(text, text.find("()").unwrap());
        let diagnostics = vec![diagnostic(0, start, end, Severity::Error)];
        let ranges = underline_ranges(&diagnostics, 0, text);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].0..ranges[0].1], "broken");
    }

    #[test]
    fn a_zero_width_diagnostic_still_underlines_something() {
        let text = "let x = ;";
        let at = protocol::byte_to_utf16(text, 8);
        let ranges = underline_ranges(&[diagnostic(0, at, at, Severity::Error)], 0, text);
        assert_eq!(ranges.len(), 1);
        assert!(ranges[0].1 > ranges[0].0);
    }

    #[test]
    fn a_multi_line_diagnostic_covers_whole_middle_lines() {
        let d = Diagnostic {
            range: Range {
                start: Position::new(0, 4),
                end: Position::new(2, 2),
            },
            severity: Severity::Warning,
            code: None,
            message: "spans lines".into(),
            source: None,
        };
        let middle = underline_ranges(std::slice::from_ref(&d), 1, "the whole line");
        assert_eq!(middle, vec![(0, 14, Severity::Warning)]);
        let last = underline_ranges(&[d], 2, "abcdef");
        assert_eq!(last, vec![(0, 2, Severity::Warning)]);
    }

    #[test]
    fn spans_split_at_diagnostic_boundaries() {
        // A highlight span 0..10 with a diagnostic covering 4..7 becomes
        // three pieces, only the middle one underlined.
        let pieces = split_span(0, 10, &[(4, 7, Severity::Error)]);
        assert_eq!(
            pieces,
            vec![
                (0, 4, None),
                (4, 7, Some(Severity::Error)),
                (7, 10, None),
            ]
        );
    }

    #[test]
    fn a_span_outside_every_diagnostic_is_one_piece() {
        assert_eq!(split_span(20, 30, &[(4, 7, Severity::Error)]), vec![(20, 30, None)]);
    }

    #[test]
    fn highlighting_reproduces_the_buffer_exactly() {
        // The layout job must contain the text verbatim: a highlighter that
        // drops or duplicates a character puts the cursor in the wrong place
        // for the rest of the session.
        let text = "fn main() {\n    let x = broken(); // 🦀\n}\n";
        let d = vec![diagnostic(1, 12, 18, Severity::Error)];
        let job = highlight(text, super::super::syntax::Lang::Rust, &d, FontId::monospace(13.0));
        assert_eq!(job.text, text);
    }

    #[test]
    fn the_worst_severity_wins_in_the_gutter() {
        let by_line = diagnostics_by_line(&[
            diagnostic(3, 0, 1, Severity::Warning),
            diagnostic(3, 2, 3, Severity::Error),
            diagnostic(4, 0, 1, Severity::Info),
        ]);
        assert_eq!(by_line[&3], Severity::Error);
        assert_eq!(by_line[&4], Severity::Info);
    }

    #[test]
    fn completion_filtering_puts_prefix_matches_first() {
        let item = |label: &str| CompletionItem {
            label: label.into(),
            detail: None,
            insert: label.into(),
            range: None,
            sort_text: None,
            kind: None,
        };
        let mut completion = Completion {
            open: true,
            items: vec![item("unwrap_or"), item("map"), item("unwrap")],
            ..Default::default()
        };
        completion.filter = "unwrap".into();
        let labels: Vec<&str> =
            completion.filtered().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["unwrap_or", "unwrap"]);

        // Nothing starts with "ap", so all three are contains-matches and
        // the server's own order is preserved.
        completion.filter = "ap".into();
        let labels: Vec<&str> =
            completion.filtered().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["unwrap_or", "map", "unwrap"]);

        completion.close();
        assert!(!completion.open && completion.items.is_empty());
    }
}
