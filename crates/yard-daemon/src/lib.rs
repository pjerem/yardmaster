//! On-demand daemon: owns SQLite, serves the JSON-lines IPC socket, and runs
//! the work-item scheduler (adapters, gates, agent supervision). Sync loops
//! and CI-watch land in M4. See SPEC.md §Architecture.

pub mod forge_github;
pub mod lifecycle;
pub mod provider_github;
pub mod runner_omp;
pub mod scheduler;
pub mod secrets;
pub mod server;
pub mod worktree;
