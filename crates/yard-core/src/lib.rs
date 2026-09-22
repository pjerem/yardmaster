//! Deterministic domain core: config model, paths, append-only event log +
//! SQLite storage, and the IPC protocol types shared by daemon and clients.
//!
//! No network I/O, no LLM calls, no process spawning — those live in
//! `yard-daemon`. See SPEC.md §Domain model.

pub mod config;
pub mod ipc;
pub mod paths;
pub mod storage;
