//! TUI application state and rendering.

use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};

use super::input::{AppAction, handle_event};
use crate::{
    github::PrDisplayState,
    render::{
        RenderableBranch,
        RenderableTree,
        colors::{string_to_color, theme},
    },
};

/// TUI application state.
pub struct App {
    /// The renderable tree data.
    pub tree: RenderableTree,
    /// Current cursor position (index into tree.branches).
    pub cursor: usize,
    /// Whether the app should quit.
    pub should_quit: bool,
    /// Branch to checkout after quitting (if any).
    pub checkout_branch: Option<String>,
    /// Whether to show verbose details.
    pub verbose: bool,
    /// List state for ratatui.
    list_state: ListState,
}

impl App {
    /// Create a new App from a renderable tree.
    pub fn new(tree: RenderableTree, verbose: bool) -> Self {
        // Start cursor at current branch if present, else 0
        let cursor = tree.current_branch_index.unwrap_or(0);
        let mut list_state = ListState::default();
        list_state.select(Some(cursor));

        Self {
            tree,
            cursor,
            should_quit: false,
            checkout_branch: None,
            verbose,
            list_state,
        }
    }

    /// Move cursor up.
    pub fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.list_state.select(Some(self.cursor));
        }
    }

    /// Move cursor down.
    pub fn move_down(&mut self) {
        if self.cursor < self.tree.branches.len().saturating_sub(1) {
            self.cursor += 1;
            self.list_state.select(Some(self.cursor));
        }
    }

    /// Select the current branch for checkout.
    pub fn select(&mut self) {
        if let Some(branch) = self.tree.branches.get(self.cursor) {
            self.checkout_branch = Some(branch.name.clone());
            self.should_quit = true;
        }
    }

    /// Quit without selecting.
    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Handle an action.
    pub fn handle_action(&mut self, action: AppAction) {
        match action {
            AppAction::MoveUp => self.move_up(),
            AppAction::MoveDown => self.move_down(),
            AppAction::Select => self.select(),
            AppAction::Quit => self.quit(),
            AppAction::None => {}
        }
    }
}

/// Terminal type alias.
type Terminal = ratatui::Terminal<CrosstermBackend<Stdout>>;

/// Set up the terminal for TUI mode.
fn setup_terminal() -> Result<Terminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = ratatui::Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to normal mode.
fn restore_terminal(terminal: &mut Terminal) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Run the TUI application. Returns the branch to checkout, if any.
pub fn run_tui(tree: RenderableTree, verbose: bool) -> Result<Option<String>> {
    let mut terminal = setup_terminal()?;
    let mut app = App::new(tree, verbose);

    // Main event loop
    let result = run_event_loop(&mut terminal, &mut app);

    // Always restore terminal, even on error
    restore_terminal(&mut terminal)?;

    result?;
    Ok(app.checkout_branch)
}

fn run_event_loop(terminal: &mut Terminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| render(frame, app))?;

        // Wait for an event with a timeout
        if event::poll(std::time::Duration::from_millis(100))? {
            let event = event::read()?;
            let action = handle_event(event);
            app.handle_action(action);
        }
    }
    Ok(())
}

/// Render the TUI.
fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Create the main block with border
    let block = Block::default()
        .title(" git-stack status ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(theme::TREE.0, theme::TREE.1, theme::TREE.2)));

    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    // Create list items from branches
    let items: Vec<ListItem> = app
        .tree
        .branches
        .iter()
        .enumerate()
        .map(|(i, branch)| render_branch_item(branch, i == app.cursor, app.verbose))
        .collect();

    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::Rgb(40, 40, 45))
            .add_modifier(Modifier::BOLD),
    );

    frame.render_stateful_widget(list, inner_area, &mut app.list_state);

    // Render help text at bottom
    render_help(frame, area);
}

/// Render a single branch as a ListItem.
fn render_branch_item(
    branch: &RenderableBranch,
    is_selected: bool,
    verbose: bool,
) -> ListItem<'static> {
    let dim = if branch.is_dimmed { 0.75 } else { 1.0 };

    let mut spans = Vec::new();

    // Arrow prefix: selection arrow takes precedence over HEAD indicator
    let arrow = if is_selected {
        Span::styled("→ ", Style::default().fg(Color::White))
    } else if branch.is_current {
        Span::styled("→ ", Style::default().fg(Color::Rgb(80, 80, 80))) // faint gray
    } else {
        Span::raw("  ") // spacing to maintain alignment
    };
    spans.push(arrow);

    // Tree indentation
    for _ in 0..branch.depth {
        spans.push(Span::styled(
            "┃ ",
            Style::default().fg(Color::Rgb(theme::TREE.0, theme::TREE.1, theme::TREE.2)),
        ));
    }

    // Branch name with status-based coloring
    let branch_color = if let Some(ref status) = branch.status {
        if status.is_descendent {
            apply_dim(theme::GREEN, dim)
        } else {
            apply_dim(theme::YELLOW, dim)
        }
    } else {
        apply_dim(theme::GRAY, dim)
    };

    let mut name_style = Style::default().fg(branch_color);
    if branch.is_current {
        name_style = name_style.add_modifier(Modifier::BOLD);
    }
    spans.push(Span::styled(branch.name.clone(), name_style));

    // Diff stats
    if let Some(ref ds) = branch.diff_stats {
        let prefix = if ds.reliable { "" } else { "~ " };
        spans.push(Span::raw(" ["));
        spans.push(Span::raw(prefix));
        spans.push(Span::styled(
            format!("+{}", ds.additions),
            Style::default().fg(apply_dim(theme::GREEN, dim)),
        ));
        spans.push(Span::styled(
            format!(" -{}", ds.deletions),
            Style::default().fg(apply_dim(theme::RED, dim)),
        ));
        spans.push(Span::raw("]"));
    }

    // Local status (for current branch)
    if let Some(ref ls) = branch.local_status {
        spans.push(Span::raw(" ["));
        let mut parts = Vec::new();
        if ls.staged > 0 {
            parts.push(Span::styled(
                format!("+{}", ls.staged),
                Style::default().fg(apply_dim(theme::GREEN, dim)),
            ));
        }
        if ls.unstaged > 0 {
            if !parts.is_empty() {
                parts.push(Span::raw(" "));
            }
            parts.push(Span::styled(
                format!("~{}", ls.unstaged),
                Style::default().fg(apply_dim(theme::YELLOW, dim)),
            ));
        }
        if ls.untracked > 0 {
            if !parts.is_empty() {
                parts.push(Span::raw(" "));
            }
            parts.push(Span::styled(
                format!("?{}", ls.untracked),
                Style::default().fg(apply_dim(theme::GRAY, dim)),
            ));
        }
        spans.extend(parts);
        spans.push(Span::raw("]"));
    }

    // PR info (non-verbose mode)
    if !verbose && let Some(ref pr) = branch.pr_info {
        let state_color = match pr.state {
            PrDisplayState::Draft => apply_dim(theme::GRAY, dim),
            PrDisplayState::Open => apply_dim(theme::GREEN, dim),
            PrDisplayState::Merged => apply_dim(theme::PURPLE, dim),
            PrDisplayState::Closed => apply_dim(theme::RED, dim),
        };

        let author_rgb = string_to_color(&pr.author);
        let author_color = apply_dim(author_rgb, dim);

        spans.push(Span::styled(
            " ",
            Style::default().fg(apply_dim(theme::PR_ARROW, dim)),
        ));
        spans.push(Span::styled(
            format!("@{}", pr.author),
            Style::default().fg(author_color),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("#{}", pr.number),
            Style::default().fg(apply_dim(theme::PR_NUMBER, dim)),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("[{}]", pr.state),
            Style::default().fg(state_color),
        ));
    }

    ListItem::new(Line::from(spans))
}

/// Render help text at the bottom.
fn render_help(frame: &mut Frame, area: Rect) {
    let help_text = Line::from(vec![
        Span::styled(" j/↓", Style::default().fg(Color::Yellow)),
        Span::raw(" down  "),
        Span::styled("k/↑", Style::default().fg(Color::Yellow)),
        Span::raw(" up  "),
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::raw(" checkout  "),
        Span::styled("q/Esc", Style::default().fg(Color::Yellow)),
        Span::raw(" quit"),
    ]);

    let help_area = Rect {
        x: area.x + 2,
        y: area.y + area.height.saturating_sub(1),
        width: area.width.saturating_sub(4),
        height: 1,
    };

    frame.render_widget(ratatui::widgets::Paragraph::new(help_text), help_area);
}

/// Apply dimming to a ThemeColor and convert to ratatui Color.
fn apply_dim(color: crate::render::ThemeColor, factor: f32) -> Color {
    let dimmed = color.apply_dim(factor);
    Color::Rgb(dimmed.0, dimmed.1, dimmed.2)
}
