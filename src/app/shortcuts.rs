//! Configurable keyboard shortcuts.
//!
//! Bindings are serializable (stored in the app config), shown in Settings,
//! and rebindable by clicking a binding and pressing the new combination.

use serde::{Deserialize, Serialize};

/// Actions that can be bound to a shortcut.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Commit,
    Refresh,
    Push,
    Pull,
    RepoPicker,
    ToggleHistory,
    /// Open the editor's file finder.
    QuickOpen,
}

impl Action {
    /// All actions in display order.
    pub const ALL: &'static [Action] = &[
        Action::Commit,
        Action::Refresh,
        Action::Push,
        Action::Pull,
        Action::RepoPicker,
        Action::ToggleHistory,
        Action::QuickOpen,
    ];

    /// Human-readable label for Settings.
    pub fn label(self) -> &'static str {
        match self {
            Action::Commit => "Commit",
            Action::Refresh => "Refresh status",
            Action::Push => "Push",
            Action::Pull => "Pull",
            Action::RepoPicker => "Open repository picker",
            Action::ToggleHistory => "Toggle Changes/History tab",
            Action::QuickOpen => "Open a file in the editor",
        }
    }
}

/// One key combination: modifiers + key, serializable as e.g. "Cmd+Enter".
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub command: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: egui::Key,
}

impl Binding {
    pub fn new(command: bool, shift: bool, alt: bool, key: egui::Key) -> Self {
        Self { command, shift, alt, key }
    }

    /// Modifiers in egui terms.
    fn modifiers(&self) -> egui::Modifiers {
        let mut m = egui::Modifiers::NONE;
        if self.command {
            m |= egui::Modifiers::COMMAND;
        }
        if self.shift {
            m |= egui::Modifiers::SHIFT;
        }
        if self.alt {
            m |= egui::Modifiers::ALT;
        }
        m
    }

    /// Consumes the binding from input if pressed this frame.
    pub fn consume(&self, input: &mut egui::InputState) -> bool {
        input.consume_key(self.modifiers(), self.key)
    }

    /// Display string, e.g. `Ctrl+Shift+P` (`Cmd` on macOS).
    pub fn display(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.command {
            parts.push(if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" });
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.alt {
            parts.push("Alt");
        }
        let key = self.key.name();
        parts.push(key);
        parts.join("+")
    }
}

/// The full shortcut map. Missing entries fall back to defaults on load.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Shortcuts {
    pub commit: Binding,
    pub refresh: Binding,
    pub push: Binding,
    pub pull: Binding,
    pub repo_picker: Binding,
    pub toggle_history: Binding,
    /// Added after the first release: an older config file has no entry for
    /// it, and a missing field must not invalidate the whole config.
    #[serde(default = "default_quick_open")]
    pub quick_open: Binding,
}

fn default_quick_open() -> Binding {
    Binding::new(true, false, false, egui::Key::O)
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self {
            commit: Binding::new(true, false, false, egui::Key::Enter),
            refresh: Binding::new(true, false, false, egui::Key::R),
            push: Binding::new(true, false, false, egui::Key::P),
            pull: Binding::new(true, true, false, egui::Key::P),
            repo_picker: Binding::new(true, false, false, egui::Key::K),
            toggle_history: Binding::new(true, false, false, egui::Key::H),
            quick_open: default_quick_open(),
        }
    }
}

impl Shortcuts {
    pub fn get(&self, action: Action) -> Binding {
        match action {
            Action::Commit => self.commit,
            Action::Refresh => self.refresh,
            Action::Push => self.push,
            Action::Pull => self.pull,
            Action::RepoPicker => self.repo_picker,
            Action::ToggleHistory => self.toggle_history,
            Action::QuickOpen => self.quick_open,
        }
    }

    pub fn set(&mut self, action: Action, binding: Binding) {
        match action {
            Action::Commit => self.commit = binding,
            Action::Refresh => self.refresh = binding,
            Action::Push => self.push = binding,
            Action::Pull => self.pull = binding,
            Action::RepoPicker => self.repo_picker = binding,
            Action::ToggleHistory => self.toggle_history = binding,
            Action::QuickOpen => self.quick_open = binding,
        }
    }

    /// Actions triggered this frame. Bindings with more modifiers are
    /// checked first so `Cmd+Shift+P` wins over `Cmd+P`.
    pub fn pressed(&self, input: &mut egui::InputState) -> Vec<Action> {
        let mut order: Vec<Action> = Action::ALL.to_vec();
        order.sort_by_key(|a| {
            let b = self.get(*a);
            std::cmp::Reverse(b.command as u8 + b.shift as u8 + b.alt as u8)
        });
        order.into_iter().filter(|a| self.get(*a).consume(input)).collect()
    }

    /// The first conflicting pair of actions, if any two share a binding.
    pub fn conflict(&self) -> Option<(Action, Action)> {
        for (i, a) in Action::ALL.iter().enumerate() {
            for b in &Action::ALL[i + 1..] {
                if self.get(*a) == self.get(*b) {
                    return Some((*a, *b));
                }
            }
        }
        None
    }
}

/// Captures the key combination pressed this frame, for rebinding UI.
/// Ignores bare modifier presses.
pub fn capture(input: &egui::InputState) -> Option<Binding> {
    for event in &input.events {
        if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
            if matches!(key, egui::Key::Escape) {
                continue; // Escape cancels capture, handled by caller
            }
            return Some(Binding {
                command: modifiers.command,
                shift: modifiers.shift,
                alt: modifiers.alt,
                key: *key,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config written before a binding existed must still load: serde
    /// fails the whole struct on a missing field, and `Config::load` falls
    /// back to defaults for everything when that happens — silently
    /// resetting every other setting the user has.
    #[test]
    fn an_older_config_without_the_newest_binding_still_loads() {
        let json = r#"{
            "commit": {"command": true, "shift": false, "alt": false, "key": "Enter"},
            "refresh": {"command": true, "shift": false, "alt": false, "key": "R"},
            "push": {"command": true, "shift": false, "alt": false, "key": "P"},
            "pull": {"command": true, "shift": true, "alt": false, "key": "P"},
            "repo_picker": {"command": true, "shift": false, "alt": false, "key": "K"},
            "toggle_history": {"command": true, "shift": false, "alt": false, "key": "H"}
        }"#;
        let shortcuts: Shortcuts = serde_json::from_str(json).expect("old config must load");
        assert_eq!(shortcuts.quick_open, default_quick_open());
        assert_eq!(shortcuts.commit.key, egui::Key::Enter);
    }

    #[test]
    fn defaults_have_no_conflicts() {
        assert!(Shortcuts::default().conflict().is_none());
    }

    #[test]
    fn conflict_detected_when_bindings_collide() {
        let mut s = Shortcuts::default();
        s.set(Action::Push, s.get(Action::Commit));
        assert!(s.conflict().is_some());
    }

    #[test]
    fn display_renders_modifiers() {
        let b = Binding::new(true, true, false, egui::Key::P);
        let d = b.display();
        assert!(d.ends_with("Shift+P") || d.contains("Shift"));
    }

    #[test]
    fn roundtrips_through_json() {
        let s = Shortcuts::default();
        let json = serde_json::to_string(&s).unwrap();
        let back: Shortcuts = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
