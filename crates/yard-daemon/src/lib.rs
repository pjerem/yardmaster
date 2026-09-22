//! On-demand daemon: owns SQLite, serves the JSON-lines IPC socket, and will
//! grow the sync loops and agent supervision (M2+). See SPEC.md §Architecture.

pub mod forge_github;
pub mod lifecycle;
pub mod provider_github;
pub mod runner_omp;
pub mod secrets;
pub mod server;
pub mod worktree;
