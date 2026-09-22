//! `yard` — single binary exposing the CLI, the TUI (default), and the
//! self-spawned daemon. See SPEC.md for the target surface.

use clap::Parser;

/// Orchestrates N coding agents on N tickets, from intake to merge.
#[derive(Parser)]
#[command(name = "yard", version, about)]
struct Cli {}

fn main() -> anyhow::Result<()> {
    let Cli {} = Cli::parse();
    // Increment 1 (socle) starts here: config load, daemon self-spawn, `status`.
    println!(
        "yardmaster {} — nothing to coordinate yet. See SPEC.md.",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}
