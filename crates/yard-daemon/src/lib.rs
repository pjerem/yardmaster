//! On-demand daemon: owns SQLite, serves the JSON-lines IPC socket, and will
//! grow the sync loops and agent supervision (M2+). See SPEC.md §Architecture.

pub mod lifecycle;
pub mod server;
