//! `SecretStore` implementations (issue #12).
//!
//! Backends, in the precedence order used by [`default_chain`]:
//! 1. [`EnvStore`] — read-only env vars, for headless/CI.
//! 2. [`KeyringStore`] — macOS Keychain (macOS only; the Linux
//!    secret-service backend needs libdbus and cannot link into static
//!    musl releases, so Linux relies on the file store).
//! 3. [`FileStore`] — flat TOML map, `chmod 600` enforced.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::{env, fs};

use yard_core::adapters::{AdapterError, AdapterResult, Secret, SecretStore};

// ---------------------------------------------------------------------------
// EnvStore

const ENV_PREFIX: &str = "YARDMASTER_SECRET_";

/// Read-only store backed by process environment variables.
///
/// Key mapping: prefix `YARDMASTER_SECRET_`, key uppercased, `.` and `-`
/// replaced by `_`. E.g. `providers.jira-acme` reads
/// `YARDMASTER_SECRET_PROVIDERS_JIRA_ACME`.
pub struct EnvStore;

fn env_var_name(key: &str) -> String {
    let mut name = String::with_capacity(ENV_PREFIX.len() + key.len());
    name.push_str(ENV_PREFIX);
    for c in key.chars() {
        match c {
            '.' | '-' => name.push('_'),
            c => name.extend(c.to_uppercase()),
        }
    }
    name
}

impl SecretStore for EnvStore {
    fn get(&self, key: &str) -> AdapterResult<Option<Secret>> {
        let var = env_var_name(key);
        match env::var(&var) {
            Ok(value) => Ok(Some(Secret::new(value))),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => Err(AdapterError::Permanent(format!(
                "env var {var} is not valid unicode"
            ))),
        }
    }

    fn set(&self, _key: &str, _value: Secret) -> AdapterResult<()> {
        Err(AdapterError::Permanent("env store is read-only".into()))
    }
}

// ---------------------------------------------------------------------------
// FileStore

const SECRETS_MODE: u32 = 0o600;

/// Store backed by a flat TOML map (`"key" = "value"`) on disk.
///
/// The file is created with mode 0600 and re-tightened on every write.
/// Reads *reject* a file readable by group/others instead of silently using
/// it, so a leaked secrets file fails loudly.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Load the map; `None` when the file does not exist yet.
    /// Rejects group/other-accessible files.
    fn load(&self) -> AdapterResult<Option<BTreeMap<String, String>>> {
        let path = self.path.display();
        let meta = match fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(AdapterError::Permanent(format!(
                    "cannot stat secrets file {path}: {e}"
                )));
            }
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(AdapterError::Permanent(format!(
                "secrets file {path} has mode {mode:03o}, accessible to group/others; \
                 run `chmod 600 {path}`"
            )));
        }
        let raw = fs::read_to_string(&self.path).map_err(|e| {
            AdapterError::Permanent(format!("cannot read secrets file {path}: {e}"))
        })?;
        let map = toml::from_str(&raw).map_err(|e| {
            AdapterError::Permanent(format!("secrets file {path} is not a flat TOML map: {e}"))
        })?;
        Ok(Some(map))
    }
}

impl SecretStore for FileStore {
    fn get(&self, key: &str) -> AdapterResult<Option<Secret>> {
        Ok(self
            .load()?
            .and_then(|mut map| map.remove(key))
            .map(Secret::new))
    }

    fn set(&self, key: &str, value: Secret) -> AdapterResult<()> {
        let path = self.path.display();
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                AdapterError::Permanent(format!(
                    "cannot create secrets dir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        // Writes *enforce* 0600 (a pre-existing loose file gets tightened);
        // only the read path rejects. Tighten before load() so it passes.
        if fs::metadata(&self.path).is_ok() {
            fs::set_permissions(&self.path, fs::Permissions::from_mode(SECRETS_MODE)).map_err(
                |e| AdapterError::Permanent(format!("cannot chmod secrets file {path}: {e}")),
            )?;
        }
        let mut map = self.load()?.unwrap_or_default();
        map.insert(key.to_owned(), value.expose().to_owned());
        let rendered = toml::to_string(&map)
            .map_err(|e| AdapterError::Permanent(format!("cannot serialize secrets: {e}")))?;
        let mut opts = fs::OpenOptions::new();
        opts.write(true)
            .create(true)
            .truncate(true)
            .mode(SECRETS_MODE);
        let mut file = opts.open(&self.path).map_err(|e| {
            AdapterError::Permanent(format!("cannot open secrets file {path}: {e}"))
        })?;
        io::Write::write_all(&mut file, rendered.as_bytes()).map_err(|e| {
            AdapterError::Permanent(format!("cannot write secrets file {path}: {e}"))
        })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// KeyringStore (macOS Keychain)

/// macOS Keychain store: service `"yardmaster"`, account = secret key.
#[cfg(target_os = "macos")]
pub struct KeyringStore {
    service: String,
}

#[cfg(target_os = "macos")]
impl KeyringStore {
    pub fn new() -> Self {
        Self {
            service: "yardmaster".into(),
        }
    }

    fn entry(&self, key: &str) -> AdapterResult<keyring::Entry> {
        keyring::Entry::new(&self.service, key).map_err(|e| {
            AdapterError::Permanent(format!("cannot address keychain entry for {key}: {e}"))
        })
    }
}

#[cfg(target_os = "macos")]
impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
impl SecretStore for KeyringStore {
    fn get(&self, key: &str) -> AdapterResult<Option<Secret>> {
        match self.entry(key)?.get_password() {
            Ok(value) => Ok(Some(Secret::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AdapterError::Permanent(format!(
                "keychain read for {key} failed: {e}"
            ))),
        }
    }

    fn set(&self, key: &str, value: Secret) -> AdapterResult<()> {
        self.entry(key)?
            .set_password(value.expose())
            .map_err(|e| AdapterError::Permanent(format!("keychain write for {key} failed: {e}")))
    }
}

// ---------------------------------------------------------------------------
// ChainStore

/// Ordered chain of stores.
///
/// `get`: first store returning `Some` wins; errors propagate immediately
/// (a broken store is a misconfiguration, not a miss).
/// `set`: first store that accepts the write wins — [`EnvStore`] always
/// rejects, so it falls through to the next store.
///
/// Ordering convention: Env > Keyring > File (see [`default_chain`]).
pub struct ChainStore(pub Vec<Box<dyn SecretStore>>);

impl SecretStore for ChainStore {
    fn get(&self, key: &str) -> AdapterResult<Option<Secret>> {
        for store in &self.0 {
            if let Some(secret) = store.get(key)? {
                return Ok(Some(secret));
            }
        }
        Ok(None)
    }

    fn set(&self, key: &str, value: Secret) -> AdapterResult<()> {
        let mut last_err = None;
        for store in &self.0 {
            match store.set(key, value.clone()) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| AdapterError::Permanent("secret store chain is empty".into())))
    }
}

/// Default chain: Env > Keyring (macOS) > File at `{state_dir}/secrets.toml`.
pub fn default_chain(state_dir: &Path) -> ChainStore {
    let mut stores: Vec<Box<dyn SecretStore>> = vec![Box::new(EnvStore)];
    #[cfg(target_os = "macos")]
    stores.push(Box::new(KeyringStore::new()));
    stores.push(Box::new(FileStore::new(state_dir.join("secrets.toml"))));
    ChainStore(stores)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn file_mode(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    // Each test uses its own secret key so the env-mutating tests never
    // collide under the parallel test runner.

    #[test]
    fn env_maps_dots_and_dashes_and_is_read_only() {
        assert_eq!(
            env_var_name("providers.jira-acme"),
            "YARDMASTER_SECRET_PROVIDERS_JIRA_ACME"
        );
        // SAFETY: test-only; the var name is unique to this test.
        unsafe { env::set_var("YARDMASTER_SECRET_PROVIDERS_JIRA_ACME", "tok-1") };
        let got = EnvStore.get("providers.jira-acme").expect("get");
        assert_eq!(got.expect("some").expose(), "tok-1");
        assert!(EnvStore.get("providers.absent").expect("get").is_none());
        assert!(matches!(
            EnvStore.set("x", Secret::new("v".into())),
            Err(AdapterError::Permanent(_))
        ));
    }

    #[test]
    fn file_round_trip_creates_0600_and_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/secrets.toml");
        let store = FileStore::new(&path);
        store
            .set("providers.jira-acme", Secret::new("tok-2".into()))
            .expect("set");
        assert_eq!(file_mode(&path), 0o600);
        let got = store
            .get("providers.jira-acme")
            .expect("get")
            .expect("some");
        assert_eq!(got.expose(), "tok-2");
        // Store boundary never leaks the value through Debug.
        assert_eq!(format!("{got:?}"), "Secret(***)");
        assert!(!format!("{got:?}").contains("tok-2"));
    }

    #[test]
    fn file_missing_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileStore::new(dir.path().join("secrets.toml"));
        assert!(store.get("anything").expect("get").is_none());
    }

    #[test]
    fn file_loose_permissions_rejected_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets.toml");
        let store = FileStore::new(&path);
        store.set("k", Secret::new("v".into())).expect("set");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        match store.get("k") {
            Err(AdapterError::Permanent(msg)) => {
                assert!(msg.contains("chmod 600"), "actionable message, got: {msg}");
            }
            other => panic!("expected Permanent error, got {other:?}"),
        }
    }

    #[test]
    fn file_write_tightens_loose_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets.toml");
        let store = FileStore::new(&path);
        store.set("k", Secret::new("v1".into())).expect("set");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        store
            .set("k2", Secret::new("v2".into()))
            .expect("set enforces mode");
        assert_eq!(file_mode(&path), 0o600);
        assert_eq!(store.get("k").expect("get").expect("some").expose(), "v1");
    }

    #[test]
    fn chain_env_wins_over_file_and_set_falls_through_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chain = ChainStore(vec![
            Box::new(EnvStore),
            Box::new(FileStore::new(dir.path().join("secrets.toml"))),
        ]);

        // set: EnvStore rejects, FileStore accepts.
        chain
            .set("chain.test-key", Secret::new("from-file".into()))
            .expect("set");
        assert_eq!(
            chain
                .get("chain.test-key")
                .expect("get")
                .expect("some")
                .expose(),
            "from-file"
        );

        // precedence: once the env var exists it shadows the file value.
        // SAFETY: test-only; the var name is unique to this test.
        unsafe { env::set_var("YARDMASTER_SECRET_CHAIN_TEST_KEY", "from-env") };
        assert_eq!(
            chain
                .get("chain.test-key")
                .expect("get")
                .expect("some")
                .expose(),
            "from-env"
        );
    }

    #[test]
    fn empty_chain_set_is_permanent() {
        let chain = ChainStore(Vec::new());
        assert!(matches!(
            chain.set("k", Secret::new("v".into())),
            Err(AdapterError::Permanent(_))
        ));
        assert!(chain.get("k").expect("get").is_none());
    }

    /// Touches the real login Keychain: run manually with
    /// `cargo test -p yard-daemon -- --ignored keychain`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "talks to the real macOS Keychain"]
    fn keychain_smoke_round_trip() {
        let store = KeyringStore::new();
        let key = "yardmaster.test-smoke";
        store.set(key, Secret::new("smoke".into())).expect("set");
        assert_eq!(
            store.get(key).expect("get").expect("some").expose(),
            "smoke"
        );
        keyring::Entry::new("yardmaster", key)
            .expect("entry")
            .delete_credential()
            .expect("cleanup");
    }
}
