//! Deterministic domain core: work-item state machine, 🟢🟠🔴 gate engine,
//! config model, append-only event log, and the adapter traits
//! (`TicketProvider`, `AgentRunner`, `Forge`, `Notifier`, `SecretStore`).
//!
//! No network I/O, no LLM calls, no process spawning — those live in
//! `yard-daemon` behind the traits defined here. See SPEC.md §Domain model.
