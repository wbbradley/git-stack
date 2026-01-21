//! Keyboard event handling for TUI.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

/// Actions that can be triggered by user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppAction {
    /// Move cursor up one item.
    MoveUp,
    /// Move cursor down one item.
    MoveDown,
    /// Select the current item (checkout branch).
    Select,
    /// Quit without action.
    Quit,
    /// No action.
    None,
}

/// Handle a crossterm event and return the corresponding action.
pub fn handle_event(event: Event) -> AppAction {
    match event {
        Event::Key(key_event) => handle_key(key_event),
        _ => AppAction::None,
    }
}

fn handle_key(key: KeyEvent) -> AppAction {
    // Handle Ctrl+C for quit
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return AppAction::Quit;
    }

    match key.code {
        // Navigation
        KeyCode::Up | KeyCode::Char('k') => AppAction::MoveUp,
        KeyCode::Down | KeyCode::Char('j') => AppAction::MoveDown,

        // Selection
        KeyCode::Enter => AppAction::Select,

        // Quit
        KeyCode::Char('q') | KeyCode::Esc => AppAction::Quit,

        _ => AppAction::None,
    }
}
