//! Filesystem locations for yardmaster configuration and state.
//!
//! Same layout on macOS and Linux: config under `~/.config/yardmaster`,
//! mutable state under `~/.local/state/yardmaster`. Both are overridable
//! through environment variables so tests and multi-instance setups never
//! touch the real home directory.

use std::env;
use std::path::{Path, PathBuf};

/// Overrides the config file location when set.
pub const CONFIG_ENV: &str = "YARDMASTER_CONFIG";
/// Overrides the state directory location when set.
pub const STATE_DIR_ENV: &str = "YARDMASTER_STATE_DIR";

/// Home resolution via `$HOME` only — no platform crate. A missing `$HOME`
/// degrades to the current directory rather than aborting the daemon.
fn home_dir() -> PathBuf {
    match env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => {
            tracing::warn!("HOME is not set; falling back to current directory");
            PathBuf::from(".")
        }
    }
}

/// Path of the central config file:
/// `$YARDMASTER_CONFIG` else `~/.config/yardmaster/config.toml`.
pub fn config_path() -> PathBuf {
    match env::var_os(CONFIG_ENV) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home_dir()
            .join(".config")
            .join("yardmaster")
            .join("config.toml"),
    }
}

/// Directory holding all mutable state (db, socket, lock):
/// `$YARDMASTER_STATE_DIR` else `~/.local/state/yardmaster`.
pub fn state_dir() -> PathBuf {
    match env::var_os(STATE_DIR_ENV) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home_dir().join(".local").join("state").join("yardmaster"),
    }
}

/// SQLite database inside a given state directory.
pub fn db_path(state_dir: &Path) -> PathBuf {
    state_dir.join("yardmaster.db")
}

/// Daemon IPC unix socket inside a given state directory.
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon.sock")
}

/// Daemon single-instance lock file inside a given state directory.
pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon.lock")
}

/// Expands a leading `~` or `~/` to `$HOME`. Any other path (including
/// `~user` forms, which we do not support) is returned verbatim.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest),
        None => PathBuf::from(path),
    }
}

/// Serializes env-mutating tests across the whole crate: `paths` and
/// `config` tests both touch `YARDMASTER_*`/`HOME`, and the test harness
/// runs modules in parallel threads.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn env_overrides_take_precedence() {
        let _g = lock();
        unsafe {
            env::set_var(CONFIG_ENV, "/tmp/custom.toml");
            env::set_var(STATE_DIR_ENV, "/tmp/custom-state");
        }
        assert_eq!(config_path(), PathBuf::from("/tmp/custom.toml"));
        assert_eq!(state_dir(), PathBuf::from("/tmp/custom-state"));
        unsafe {
            env::remove_var(CONFIG_ENV);
            env::remove_var(STATE_DIR_ENV);
        }
    }

    #[test]
    fn defaults_derive_from_home() {
        let _g = lock();
        let prev_home = env::var_os("HOME");
        unsafe {
            env::remove_var(CONFIG_ENV);
            env::remove_var(STATE_DIR_ENV);
            env::set_var("HOME", "/home/yard");
        }
        assert_eq!(
            config_path(),
            PathBuf::from("/home/yard/.config/yardmaster/config.toml")
        );
        assert_eq!(
            state_dir(),
            PathBuf::from("/home/yard/.local/state/yardmaster")
        );
        unsafe {
            match prev_home {
                Some(h) => env::set_var("HOME", h),
                None => env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn state_file_helpers_join_names() {
        let dir = Path::new("/var/state");
        assert_eq!(db_path(dir), PathBuf::from("/var/state/yardmaster.db"));
        assert_eq!(socket_path(dir), PathBuf::from("/var/state/daemon.sock"));
        assert_eq!(lock_path(dir), PathBuf::from("/var/state/daemon.lock"));
    }

    #[test]
    fn tilde_expansion() {
        let _g = lock();
        let prev_home = env::var_os("HOME");
        unsafe {
            env::set_var("HOME", "/home/yard");
        }
        assert_eq!(expand_tilde("~"), PathBuf::from("/home/yard"));
        assert_eq!(expand_tilde("~/dev/x"), PathBuf::from("/home/yard/dev/x"));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde("rel/path"), PathBuf::from("rel/path"));
        // `~user` is unsupported: kept verbatim, never mangled.
        assert_eq!(expand_tilde("~bob/x"), PathBuf::from("~bob/x"));
        unsafe {
            match prev_home {
                Some(h) => env::set_var("HOME", h),
                None => env::remove_var("HOME"),
            }
        }
    }
}
