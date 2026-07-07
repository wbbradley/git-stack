const LLMS_MD: &str = include_str!("llms.md");

#[derive(clap::Args, Debug)]
/// Print an exhaustive markdown reference aimed at LLM/agent consumers.
///
/// Use this when dropping an agent into a repo that uses `git-stack`:
/// `git stack llms` prints everything an agent needs to drive the
/// checkout → commit → restack → `pr create` → `sync` loop without
/// reading source.
pub struct LlmsArgs {}

pub fn run(_args: LlmsArgs) -> anyhow::Result<()> {
    print!("{LLMS_MD}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_non_empty() {
        assert!(!LLMS_MD.is_empty());
        assert!(
            LLMS_MD.trim_start().starts_with('#'),
            "expected leading markdown heading"
        );
    }

    #[test]
    fn mentions_all_subcommands() {
        for sub in [
            "git stack status",
            "interactive",
            "up",
            "down",
            "edit",
            "restack",
            "log",
            "note",
            "diff",
            "checkout",
            "mount",
            "delete",
            "cleanup",
            "pr",
            "auth",
            "cache",
            "completions",
            "sync",
            "llms",
        ] {
            assert!(LLMS_MD.contains(sub), "missing subcommand {sub}");
        }
    }

    #[test]
    fn mentions_all_pr_actions() {
        for action in ["pr create", "pr view", "pr sync"] {
            assert!(LLMS_MD.contains(action), "missing PrAction {action}");
        }
    }

    #[test]
    fn mentions_all_auth_actions() {
        for action in ["auth login", "auth status", "auth logout"] {
            assert!(LLMS_MD.contains(action), "missing AuthAction {action}");
        }
    }

    #[test]
    fn mentions_all_cache_actions() {
        for action in ["cache clear"] {
            assert!(LLMS_MD.contains(action), "missing CacheAction {action}");
        }
    }

    #[test]
    fn mentions_all_stack_methods() {
        for method in ["apply_merge", "merge"] {
            assert!(LLMS_MD.contains(method), "missing StackMethod {method}");
        }
    }

    #[test]
    fn mentions_file_paths() {
        for path in ["state.yaml", "github.yaml"] {
            assert!(LLMS_MD.contains(path), "missing path {path}");
        }
    }

    #[test]
    fn no_stale_unmount_wording() {
        assert!(
            !LLMS_MD.contains("git stack unmount"),
            "there is no `unmount` subcommand; document `delete` instead"
        );
    }
}
