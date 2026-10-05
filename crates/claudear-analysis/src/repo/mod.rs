//! Multi-repository support module.
//!
//! Provides git operations, dependency tracking, cascading changes, and repository indexing.

pub mod code_index;
mod dependencies;
mod git;
pub(crate) mod index;
mod relationships;

pub use abnegate_vcs::DependencyDiscovery;
pub use abnegate_vcs::DiscoveredDependency;
pub use abnegate_vcs::Manifest;
pub use claudear_core::types::IndexedRepo;
pub use claudear_core::types::RepoIndex;
pub use dependencies::discover_configured;
pub use dependencies::save_discovered;
pub use git::worktree_path;
pub use git::GitOps;
pub use index::build_repo_index;
pub use index::index_repo_files;
pub use relationships::dependency_type;
pub use relationships::CascadingChange;
pub use relationships::DependencyGraph;
pub use relationships::DependencyType;
pub use relationships::RepoRelationships;
pub use relationships::Repository;
