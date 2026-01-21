//! Shared rendering infrastructure for CLI and TUI output.

pub mod cli;
pub mod colors;
pub mod tree_data;

pub use cli::render_cli;
pub use colors::ThemeColor;
pub use tree_data::{BranchRenderStatus, PrRenderInfo, RenderableBranch, RenderableTree, compute_renderable_tree};
