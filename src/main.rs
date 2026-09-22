//! `yard` — single binary exposing the CLI, the TUI (default, M3), and the
//! self-spawned daemon. See SPEC.md for the target surface.

mod client;

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use clap::{CommandFactory, Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use yard_core::config::Config;
use yard_core::ipc::{Method, StatusReport};
use yard_core::paths;

/// Orchestrates N coding agents on N tickets, from intake to merge.
#[derive(Parser)]
#[command(name = "yard", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show daemon and work-item status (auto-starts the daemon).
    Status {
        /// Emit the raw status report as JSON instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Explicit daemon control; the daemon is otherwise spawned on demand.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Run the daemon in the foreground (what the self-spawn executes).
    Run,
    /// Ask the running daemon to shut down cleanly.
    Stop,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let Some(command) = cli.command else {
        // Bare `yard` becomes the TUI dashboard in M3; help until then.
        Cli::command().print_help().context("printing help")?;
        return Ok(());
    };

    // Validate the config before any daemon is spawned or contacted, so a
    // broken file surfaces as one precise CLI error instead of a daemon that
    // silently fails to appear.
    Config::load(None).context("loading configuration")?;
    let state_dir = paths::state_dir();

    match command {
        Command::Status { json } => cmd_status(&state_dir, json),
        Command::Daemon {
            command: DaemonCommand::Run,
        } => yard_daemon::lifecycle::run(state_dir),
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => cmd_stop(&state_dir),
    }
}

/// `yard status [--json]`: connect (spawning the daemon if needed), fetch the
/// report, render it.
fn cmd_status(state_dir: &Path, json: bool) -> anyhow::Result<()> {
    let mut client = ensure_daemon(state_dir)?;
    let data = client.request(Method::Status)?;
    let report: StatusReport =
        serde_json::from_value(data).context("parsing status report from daemon")?;
    if json {
        // Re-serialized from the typed report: field order is the struct
        // declaration order, i.e. stable across runs and releases.
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }
    Ok(())
}

/// `yard daemon stop`: ask for shutdown and wait for the cleanup to land so
/// `yard daemon stop && …` scripting is race-free. No daemon → exit 0.
fn cmd_stop(state_dir: &Path) -> anyhow::Result<()> {
    let mut client = match client::Client::connect(state_dir) {
        Ok(client) => client,
        Err(err) => {
            tracing::debug!(error = %err, "no daemon reachable");
            println!("no daemon running");
            return Ok(());
        }
    };
    let pid = client.daemon_pid;
    // The daemon aborts live connections while shutting down, so the
    // response to `shutdown` may never arrive; the request itself is enough.
    if let Err(err) = client.request(Method::Shutdown) {
        tracing::debug!(error = %err, "shutdown response lost (daemon already exiting)");
    }
    let lock = paths::lock_path(state_dir);
    let deadline = Instant::now() + Duration::from_secs(5);
    while lock.exists() {
        if Instant::now() >= deadline {
            bail!("daemon (pid {pid}) did not shut down within 5s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("daemon (pid {pid}) stopped");
    Ok(())
}

/// Connects to the daemon, spawning one first when nothing answers, retrying
/// with backoff for up to ~3s while it boots.
fn ensure_daemon(state_dir: &Path) -> anyhow::Result<client::Client> {
    match client::Client::connect(state_dir) {
        Ok(client) => return Ok(client),
        Err(err) => tracing::debug!(error = %err, "no daemon reachable; spawning one"),
    }
    spawn_daemon(state_dir)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match client::Client::connect(state_dir) {
            Ok(client) => return Ok(client),
            Err(err) if Instant::now() >= deadline => {
                return Err(err).context("daemon unreachable after spawn attempt");
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Spawns `current_exe() daemon run` detached: null stdio, its own process
/// group (survives this terminal), and deliberately not waited on — the
/// socket appearing is the readiness signal.
fn spawn_daemon(state_dir: &Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let mut command = std::process::Command::new(exe);
    command
        .args(["daemon", "run"])
        // Pin the state dir explicitly so the daemon can never disagree with
        // the directory this CLI resolved.
        .env(paths::STATE_DIR_ENV, state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let child = command.spawn().context("spawning daemon")?;
    tracing::debug!(pid = child.id(), "daemon spawn requested");
    Ok(())
}

/// Human rendering: one daemon line, then an aligned work-item table (or a
/// friendly empty-backlog note).
fn print_human(report: &StatusReport) {
    let daemon = &report.daemon;
    let uptime = match uptime_secs(&daemon.started_at) {
        Some(secs) => format!("up {}", format_duration(secs)),
        None => format!("started {}", daemon.started_at),
    };
    println!(
        "daemon v{} (pid {}) — {}, state dir {}",
        daemon.version, daemon.pid, uptime, daemon.state_dir
    );
    println!();

    if report.items.is_empty() {
        println!("no work items yet");
        return;
    }

    const HEADERS: [&str; 7] = [
        "ID",
        "TICKET",
        "REPO",
        "STATE",
        "AGENT",
        "GATES",
        "MERGEABLE",
    ];
    let rows: Vec<[String; 7]> = report
        .items
        .iter()
        .map(|item| {
            [
                item.id.to_string(),
                item.ticket.clone(),
                item.repo.clone(),
                item.state.clone(),
                item.agent.clone().unwrap_or_else(|| "-".to_owned()),
                item.pending_gates.to_string(),
                match item.mergeable {
                    Some(true) => "yes".to_owned(),
                    Some(false) => "no".to_owned(),
                    None => "-".to_owned(),
                },
            ]
        })
        .collect();

    let mut widths: [usize; 7] = HEADERS.map(str::len);
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let print_row = |cells: [&str; 7]| {
        let mut line = String::new();
        for (i, (cell, width)) in cells.iter().zip(widths).enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            if i < cells.len() - 1 {
                line.extend(std::iter::repeat_n(' ', width - cell.len()));
            }
        }
        println!("{line}");
    };
    print_row(HEADERS);
    for row in &rows {
        print_row([
            &row[0], &row[1], &row[2], &row[3], &row[4], &row[5], &row[6],
        ]);
    }
}

/// Seconds elapsed since an RFC3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`,
/// the exact shape the daemon emits). Any other shape → `None`.
fn uptime_secs(started_at: &str) -> Option<i64> {
    let bytes = started_at.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| started_at[range].parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    // days_from_civil (Howard Hinnant) — inverse of the daemon's formatter.
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let started = days * 86_400 + h * 3600 + mi * 60 + s;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some((now - started).max(0))
}

fn format_duration(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_roundtrip_epoch_reference() {
        // 2024-03-01T00:00:00Z == 1709251200 — leap-year boundary, the civil
        // algorithm's trickiest spot.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let up = uptime_secs("2024-03-01T00:00:00Z").unwrap();
        assert!((now - 1_709_251_200 - up).abs() <= 1, "up={up}");
    }

    #[test]
    fn uptime_rejects_garbage() {
        assert_eq!(uptime_secs("yesterday"), None);
        assert_eq!(uptime_secs("2024-13-01T00:00:00Z"), None);
        assert_eq!(uptime_secs("2024-03-01 00:00:00"), None);
    }

    #[test]
    fn durations_humanize() {
        assert_eq!(format_duration(5), "5s");
        assert_eq!(format_duration(65), "1m05s");
        assert_eq!(format_duration(3_720), "1h02m");
    }
}
