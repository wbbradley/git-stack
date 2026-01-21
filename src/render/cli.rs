//! CLI text rendering for branch tree.

use colored::Colorize;

use super::{
    colors::{string_to_color, theme, ThemeColor},
    tree_data::{RenderableBranch, RenderableTree},
};
use crate::github::PrDisplayState;

/// Dimming factor for display.
const DIM_FACTOR: f32 = 0.75;

fn selection_marker() -> &'static str {
    if cfg!(target_os = "windows") {
        ">"
    } else {
        "→"
    }
}

/// Apply color to a string using the colored crate.
fn apply_color(s: &str, color: ThemeColor) -> colored::ColoredString {
    let (r, g, b) = color.rgb();
    s.truecolor(r, g, b)
}

/// Render the tree to the CLI.
pub fn render_cli(tree: &RenderableTree, verbose: bool) {
    for branch in &tree.branches {
        render_branch(branch, verbose);
    }
}

fn render_branch(branch: &RenderableBranch, verbose: bool) {
    let dim = if branch.is_dimmed { DIM_FACTOR } else { 1.0 };

    // Selection marker
    if branch.is_current {
        print!("{} ", selection_marker().bright_purple().bold());
    } else {
        print!("  ");
    }

    // Tree indentation
    for _ in 0..branch.depth {
        print!("{}", apply_color("┃ ", theme::TREE));
    }

    // Handle remote-only branches without status
    if branch.is_remote_only && branch.status.is_none() {
        let branch_color = theme::GRAY.apply_dim(dim);
        println!("{}", apply_color(&branch.name, branch_color));
        return;
    }

    // Branch name with status-based coloring
    let branch_color = if let Some(ref status) = branch.status {
        if status.is_descendent {
            theme::GREEN.apply_dim(dim)
        } else {
            theme::YELLOW.apply_dim(dim)
        }
    } else {
        theme::GRAY.apply_dim(dim)
    };

    let branch_name = if branch.is_current {
        apply_color(&branch.name, branch_color).bold()
    } else {
        apply_color(&branch.name, branch_color)
    };

    // Diff stats
    let diff_stats = branch.diff_stats.as_ref().map(|ds| {
        let green = theme::GREEN.apply_dim(dim);
        let red = theme::RED.apply_dim(dim);
        let prefix = if ds.reliable { "" } else { "~ " };
        format!(
            " [{}{}{}]",
            prefix,
            apply_color(&format!("+{}", ds.additions), green),
            apply_color(&format!(" -{}", ds.deletions), red)
        )
    }).unwrap_or_default();

    // Local status
    let local_status = branch.local_status.as_ref().map(|ls| {
        let mut parts = Vec::new();
        let green = theme::GREEN.apply_dim(dim);
        let yellow = theme::YELLOW.apply_dim(dim);
        let gray = theme::GRAY.apply_dim(dim);
        if ls.staged > 0 {
            parts.push(apply_color(&format!("+{}", ls.staged), green).to_string());
        }
        if ls.unstaged > 0 {
            parts.push(apply_color(&format!("~{}", ls.unstaged), yellow).to_string());
        }
        if ls.untracked > 0 {
            parts.push(apply_color(&format!("?{}", ls.untracked), gray).to_string());
        }
        format!(" [{}]", parts.join(" "))
    }).unwrap_or_default();

    if verbose {
        render_verbose_line(branch, &branch_name, &diff_stats, &local_status, dim);
    } else {
        render_simple_line(branch, &branch_name, &diff_stats, &local_status, dim);
    }
}

fn render_simple_line(
    branch: &RenderableBranch,
    branch_name: &colored::ColoredString,
    diff_stats: &str,
    local_status: &str,
    dim: f32,
) {
    // PR info
    let pr_info = branch.pr_info.as_ref().map(|pr| {
        let gray = theme::GRAY.apply_dim(dim);
        let green = theme::GREEN.apply_dim(dim);
        let purple = theme::PURPLE.apply_dim(dim);
        let red = theme::RED.apply_dim(dim);

        let state_colored = match pr.state {
            PrDisplayState::Draft => apply_color(&format!("[{}]", pr.state), gray),
            PrDisplayState::Open => apply_color(&format!("[{}]", pr.state), green),
            PrDisplayState::Merged => apply_color(&format!("[{}]", pr.state), purple),
            PrDisplayState::Closed => apply_color(&format!("[{}]", pr.state), red),
        };

        let author_color = string_to_color(&pr.author).apply_dim(dim);
        let author_colored = apply_color(&format!("@{}", pr.author), author_color);

        let pr_num = theme::PR_NUMBER.apply_dim(dim);
        let number_colored = apply_color(&format!("#{}", pr.number), pr_num);

        let arrow = theme::PR_ARROW.apply_dim(dim);
        format!(
            " {} {} {} {}",
            apply_color("", arrow),
            author_colored,
            number_colored,
            state_colored
        )
    }).unwrap_or_default();

    println!("{}{}{}{}", branch_name, diff_stats, local_status, pr_info);
}

fn render_verbose_line(
    branch: &RenderableBranch,
    branch_name: &colored::ColoredString,
    diff_stats: &str,
    local_status: &str,
    dim: f32,
) {
    let Some(ref status) = branch.status else {
        println!("{}", branch_name);
        return;
    };

    let gold = theme::GOLD.apply_dim(dim);
    let stacked_on = theme::STACKED_ON.apply_dim(dim);
    let yellow = theme::YELLOW.apply_dim(dim);
    let red = theme::RED.apply_dim(dim);
    let green = theme::GREEN.apply_dim(dim);
    let upstream_color = theme::UPSTREAM.apply_dim(dim);

    // SHA
    let sha_display = if status.sha.len() >= 8 {
        apply_color(&status.sha[..8], gold)
    } else {
        apply_color(&status.sha, gold)
    };

    // Status details
    let details = if status.exists {
        if status.is_descendent {
            format!(
                "{} {}",
                apply_color("is stacked on", stacked_on),
                apply_color(&status.parent_branch, yellow)
            )
        } else {
            format!(
                "{} {}",
                apply_color("diverges from", red),
                apply_color(&status.parent_branch, yellow)
            )
        }
    } else {
        apply_color("does not exist!", red).to_string()
    };

    // Upstream status
    let upstream_info = if let Some(ref verbose) = branch.verbose {
        if let Some((ref name, synced)) = verbose.upstream_status {
            let synced_str = if synced {
                apply_color("synced", green)
            } else {
                apply_color("not synced", red)
            };
            format!(
                " (upstream {} is {})",
                apply_color(name, upstream_color),
                synced_str
            )
        } else {
            format!(" ({})", apply_color("no upstream", gold))
        }
    } else {
        String::new()
    };

    // LKG parent
    let lkg_info = branch.verbose.as_ref()
        .and_then(|v| v.lkg_parent.as_ref())
        .map(|lkg| format!(" (lkg parent {})", apply_color(lkg, gold)))
        .unwrap_or_default();

    // Stack method
    let method_info = branch.verbose.as_ref()
        .map(|v| {
            let method_color = theme::GREEN.apply_dim(dim);
            format!(" ({})", apply_color(&v.stack_method, method_color))
        })
        .unwrap_or_default();

    println!(
        "{}{}{} ({}) {}{}{}{}",
        branch_name,
        diff_stats,
        local_status,
        sha_display,
        details,
        upstream_info,
        lkg_info,
        method_info,
    );

    // Note preview
    if let Some(ref note) = branch.note_preview {
        print!("  ");
        for _ in 0..branch.depth {
            print!("{}", apply_color("┃ ", theme::TREE));
        }
        let note_display = if branch.is_current {
            note.bright_blue().bold()
        } else {
            note.blue()
        };
        println!(
            "  {} {}",
            apply_color("›", theme::TREE),
            note_display
        );
    }
}
