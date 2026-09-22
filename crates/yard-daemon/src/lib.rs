//! On-demand daemon: owns SQLite, runs the adaptive sync loops, supervises
//! agent child processes (no tmux), enforces gate approvals around adapters,
//! and serves the JSON-lines IPC socket for the CLI and TUI.
//! See SPEC.md §Architecture.
