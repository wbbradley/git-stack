use std::{cell::RefCell, collections::HashMap, time::Duration};

use colored::Colorize;

/// Statistics for a single type of git command
#[derive(Debug, Default, Clone)]
pub struct CommandStats {
    pub count: u64,
    pub total_duration: Duration,
    pub max_duration: Duration,
}

impl CommandStats {
    pub fn record(&mut self, duration: Duration) {
        self.count += 1;
        self.total_duration += duration;
        if duration > self.max_duration {
            self.max_duration = duration;
        }
    }

    pub fn avg_duration(&self) -> Duration {
        if self.count == 0 {
            Duration::ZERO
        } else {
            self.total_duration / self.count as u32
        }
    }
}

/// Aggregated statistics for all git commands
#[derive(Debug, Default)]
pub struct GitStats {
    /// Stats keyed by the first argument (e.g., "rev-parse", "log", "fetch")
    pub by_command: HashMap<String, CommandStats>,
    /// Overall stats
    pub total: CommandStats,
}

impl GitStats {
    pub fn record(&mut self, command: &str, duration: Duration) {
        self.total.record(duration);
        self.by_command
            .entry(command.to_string())
            .or_default()
            .record(duration);
    }
}

// Thread-local storage for accumulating stats during execution
thread_local! {
    static GIT_STATS: RefCell<GitStats> = RefCell::new(GitStats::default());
}

/// Record a git command execution
pub fn record_git_command(args: &[&str], duration: Duration) {
    let command = args.first().copied().unwrap_or("unknown");
    GIT_STATS.with(|stats| {
        stats.borrow_mut().record(command, duration);
    });
}

/// Get a snapshot of current stats
pub fn get_stats() -> GitStats {
    GIT_STATS.with(|stats| {
        let borrowed = stats.borrow();
        GitStats {
            by_command: borrowed.by_command.clone(),
            total: borrowed.total.clone(),
        }
    })
}

/// Reset stats (useful for testing)
#[allow(dead_code)]
pub fn reset_stats() {
    GIT_STATS.with(|stats| {
        *stats.borrow_mut() = GitStats::default();
    });
}

/// Print a benchmark summary to stderr
pub fn print_summary() {
    let stats = get_stats();

    if stats.total.count == 0 {
        return;
    }

    eprintln!();
    eprintln!(
        "{}",
        "=== Git Command Performance Summary ===".yellow().bold()
    );
    eprintln!();

    // Sort commands by total time (descending)
    let mut commands: Vec<_> = stats.by_command.iter().collect();
    commands.sort_by(|a, b| b.1.total_duration.cmp(&a.1.total_duration));

    eprintln!(
        "{:<20} {:>8} {:>12} {:>12} {:>12}",
        "Command", "Count", "Total", "Avg", "Max"
    );
    eprintln!("{}", "-".repeat(64));

    for (cmd, cmd_stats) in commands {
        eprintln!(
            "{:<20} {:>8} {:>12.2?} {:>12.2?} {:>12.2?}",
            format!("git {}", cmd),
            cmd_stats.count,
            cmd_stats.total_duration,
            cmd_stats.avg_duration(),
            cmd_stats.max_duration
        );
    }

    eprintln!("{}", "-".repeat(64));
    eprintln!(
        "{:<20} {:>8} {:>12.2?} {:>12.2?} {:>12.2?}",
        "TOTAL".bold(),
        stats.total.count,
        stats.total.total_duration,
        stats.total.avg_duration(),
        stats.total.max_duration
    );
    eprintln!();
}

/// Output stats as JSON for scripted comparison
pub fn print_json() {
    let stats = get_stats();

    // Manual JSON to avoid adding serde_json dependency for Phase 1
    eprintln!("{{");
    eprintln!("  \"total_commands\": {},", stats.total.count);
    eprintln!(
        "  \"total_duration_ms\": {:.3},",
        stats.total.total_duration.as_secs_f64() * 1000.0
    );

    eprintln!("  \"by_command\": {{");
    let commands: Vec<_> = stats.by_command.iter().collect();
    for (i, (cmd, cmd_stats)) in commands.iter().enumerate() {
        let comma = if i < commands.len() - 1 { "," } else { "" };
        eprintln!(
            "    \"{}\": {{ \"count\": {}, \"total_ms\": {:.3}, \"avg_ms\": {:.3}, \"max_ms\": {:.3} }}{}",
            cmd,
            cmd_stats.count,
            cmd_stats.total_duration.as_secs_f64() * 1000.0,
            cmd_stats.avg_duration().as_secs_f64() * 1000.0,
            cmd_stats.max_duration.as_secs_f64() * 1000.0,
            comma
        );
    }
    eprintln!("  }}");
    eprintln!("}}");
}
