//! Central configuration: TOML schema, loading, validation, and the small
//! templating/slug helpers used to derive branch names.
//!
//! Schema (see SPEC.md §Configuration): `[profile]`, `[providers.<name>]`,
//! `[repos.<name>]`, `[budgets]`. Unknown keys are rejected so typos surface
//! as precise parse errors instead of silently-ignored settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::paths;

/// Provider kinds this build knows how to drive. `kind` stays a plain string
/// in the schema so future kinds only extend this list.
pub const KNOWN_PROVIDER_KINDS: &[&str] = &["jira", "github", "local"];

/// Placeholders allowed in `branch_template`.
pub const BRANCH_TEMPLATE_VARS: &[&str] = &["type", "ticket", "slug"];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid config: {msg}")]
    Validation { msg: String },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub profile: Profile,
    pub providers: BTreeMap<String, Provider>,
    pub repos: BTreeMap<String, Repo>,
    pub budgets: Budgets,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Profile {
    /// When true, 🔴 public-side-effect gates can never be auto-approved.
    pub lock_public_gate: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            lock_public_gate: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// One of [`KNOWN_PROVIDER_KINDS`]; validated at load, kept as a string
    /// for forward compatibility.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    /// Repo clone location; `~` is expanded at load time.
    pub path: PathBuf,
    pub forge: String,
    #[serde(default = "default_base")]
    pub base: String,
    #[serde(default = "default_branch_template")]
    pub branch_template: String,
    #[serde(default)]
    pub check_command: Vec<String>,
    #[serde(default = "default_true")]
    pub rebase: bool,
    #[serde(default = "default_ci_retry_cap")]
    pub ci_retry_cap: u32,
    #[serde(default)]
    pub merge_policy: MergePolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MergePolicy {
    pub ci: String,
    pub approvals: u32,
    pub no_changes_requested: bool,
}

impl Default for MergePolicy {
    fn default() -> Self {
        MergePolicy {
            ci: "green".to_owned(),
            approvals: 1,
            no_changes_requested: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Budgets {
    /// 0 = unlimited (usage is tracked regardless).
    pub default_tokens_per_item: u64,
}

fn default_base() -> String {
    "main".to_owned()
}

fn default_branch_template() -> String {
    "{type}/{ticket}-{slug}".to_owned()
}

fn default_true() -> bool {
    true
}

fn default_ci_retry_cap() -> u32 {
    3
}

impl Config {
    /// Loads and validates the configuration.
    ///
    /// `None` resolves via [`paths::config_path`]; a *missing* file at that
    /// default location is not an error and yields `Config::default()`.
    /// An explicit `Some(path)` that does not exist is an [`ConfigError::Io`].
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let (path, missing_ok) = match path {
            Some(p) => (p.to_path_buf(), false),
            None => (paths::config_path(), true),
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if missing_ok && e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no config file, using defaults");
                return Ok(Config::default());
            }
            Err(source) => return Err(ConfigError::Io { path, source }),
        };
        let mut config: Config = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// Post-parse canonicalization: tilde-expand repo paths.
    fn normalize(&mut self) {
        for repo in self.repos.values_mut() {
            // TOML strings are valid UTF-8, so the lossy round-trip is exact.
            repo.path = paths::expand_tilde(&repo.path.to_string_lossy());
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, provider) in &self.providers {
            if !KNOWN_PROVIDER_KINDS.contains(&provider.kind.as_str()) {
                return Err(ConfigError::Validation {
                    msg: format!(
                        "provider '{name}': unknown kind \"{}\" (known: {})",
                        provider.kind,
                        KNOWN_PROVIDER_KINDS.join(", ")
                    ),
                });
            }
        }
        for (name, repo) in &self.repos {
            for segment in segments(&repo.branch_template)? {
                if let Segment::Var(var) = segment
                    && !BRANCH_TEMPLATE_VARS.contains(&var)
                {
                    return Err(ConfigError::Validation {
                        msg: format!(
                            "repo '{name}': branch_template placeholder {{{var}}} \
                             is not one of {{type}}, {{ticket}}, {{slug}}"
                        ),
                    });
                }
            }
        }
        Ok(())
    }
}

enum Segment<'a> {
    Lit(&'a str),
    Var(&'a str),
}

/// Splits a `{name}`-style template into literal and placeholder segments.
/// An unclosed `{` is a validation error; a bare `}` is treated literally.
fn segments(tpl: &str) -> Result<Vec<Segment<'_>>, ConfigError> {
    let mut out = Vec::new();
    let mut rest = tpl;
    while let Some(open) = rest.find('{') {
        if open > 0 {
            out.push(Segment::Lit(&rest[..open]));
        }
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or_else(|| ConfigError::Validation {
            msg: format!("unclosed '{{' in template \"{tpl}\""),
        })?;
        out.push(Segment::Var(&after[..close]));
        rest = &after[close + 1..];
    }
    if !rest.is_empty() {
        out.push(Segment::Lit(rest));
    }
    Ok(out)
}

/// Renders a `{name}` template against `vars`. A placeholder absent from
/// `vars` is a [`ConfigError::Validation`].
pub fn render_template(tpl: &str, vars: &BTreeMap<&str, &str>) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(tpl.len());
    for segment in segments(tpl)? {
        match segment {
            Segment::Lit(lit) => out.push_str(lit),
            Segment::Var(var) => match vars.get(var) {
                Some(value) => out.push_str(value),
                None => {
                    return Err(ConfigError::Validation {
                        msg: format!("unknown placeholder {{{var}}} in template \"{tpl}\""),
                    });
                }
            },
        }
    }
    Ok(out)
}

/// Branch-name slug: ASCII lowercase, alphanumeric runs joined by `-`,
/// common Latin accents folded, truncated to 40 chars (never ending in `-`).
pub fn slugify(input: &str) -> String {
    const MAX_LEN: usize = 40;
    let mut out = String::with_capacity(input.len().min(MAX_LEN));
    let mut pending_sep = false;
    for c in input.chars() {
        match fold_char(c) {
            Some(c) => {
                if pending_sep && !out.is_empty() {
                    out.push('-');
                }
                pending_sep = false;
                out.push(c);
            }
            None => pending_sep = true,
        }
    }
    // Output is pure ASCII, so byte truncation is char-safe.
    out.truncate(MAX_LEN);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Maps a char to its slug representation: ASCII alphanumerics pass through
/// lowercased, common Latin accents fold to their base letter, everything
/// else (symbols, spaces, ligatures, non-Latin scripts) acts as a separator.
fn fold_char(c: char) -> Option<char> {
    let c = c.to_lowercase().next().unwrap_or(c);
    match c {
        'a'..='z' | '0'..='9' => Some(c),
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => Some('a'),
        'ç' => Some('c'),
        'è' | 'é' | 'ê' | 'ë' => Some('e'),
        'ì' | 'í' | 'î' | 'ï' => Some('i'),
        'ñ' => Some('n'),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => Some('o'),
        'ù' | 'ú' | 'û' | 'ü' => Some('u'),
        'ý' | 'ÿ' => Some('y'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::ENV_LOCK;
    use std::env;

    const FULL: &str = r#"
        [profile]
        lock_public_gate = false

        [providers.work]
        kind = "jira"
        url = "https://work.example.com"
        user = "jeremy"

        [providers.oss]
        kind = "github"

        [repos.backend]
        path = "~/dev/backend"
        forge = "github"
        base = "develop"
        branch_template = "{type}/{ticket}-{slug}"
        check_command = ["make", "test"]
        rebase = false
        ci_retry_cap = 5

        [repos.backend.merge_policy]
        ci = "green"
        approvals = 2
        no_changes_requested = false

        [budgets]
        default_tokens_per_item = 100000
    "#;

    #[test]
    fn full_config_round_trip() {
        let config: Config = toml::from_str(FULL).expect("parse");
        assert!(!config.profile.lock_public_gate);
        let work = &config.providers["work"];
        assert_eq!(work.kind, "jira");
        assert_eq!(work.url.as_deref(), Some("https://work.example.com"));
        assert_eq!(work.user.as_deref(), Some("jeremy"));
        assert_eq!(config.providers["oss"].kind, "github");
        let backend = &config.repos["backend"];
        assert_eq!(backend.base, "develop");
        assert_eq!(backend.check_command, ["make", "test"]);
        assert!(!backend.rebase);
        assert_eq!(backend.ci_retry_cap, 5);
        assert_eq!(backend.merge_policy.approvals, 2);
        assert!(!backend.merge_policy.no_changes_requested);
        assert_eq!(config.budgets.default_tokens_per_item, 100_000);

        // serialize -> reparse must be lossless
        let rendered = toml::to_string(&config).expect("serialize");
        let reparsed: Config = toml::from_str(&rendered).expect("reparse");
        assert_eq!(reparsed, config);
    }

    #[test]
    fn defaults_applied_when_absent() {
        let config: Config = toml::from_str(
            r#"
            [repos.app]
            path = "/opt/app"
            forge = "github"
            "#,
        )
        .expect("parse");
        assert!(config.profile.lock_public_gate);
        assert!(config.providers.is_empty());
        let app = &config.repos["app"];
        assert_eq!(app.base, "main");
        assert_eq!(app.branch_template, "{type}/{ticket}-{slug}");
        assert!(app.check_command.is_empty());
        assert!(app.rebase);
        assert_eq!(app.ci_retry_cap, 3);
        assert_eq!(app.merge_policy, MergePolicy::default());
        assert_eq!(app.merge_policy.ci, "green");
        assert_eq!(app.merge_policy.approvals, 1);
        assert!(app.merge_policy.no_changes_requested);
        assert_eq!(config.budgets.default_tokens_per_item, 0);

        let empty: Config = toml::from_str("").expect("parse empty");
        assert_eq!(empty, Config::default());
    }

    #[test]
    fn unknown_key_is_precise_parse_error() {
        let err = toml::from_str::<Config>("[profile]\nlock_publik_gate = true\n")
            .expect_err("must reject unknown key");
        assert!(
            err.to_string().contains("lock_publik_gate"),
            "error should name the offending key: {err}"
        );
    }

    #[test]
    fn unknown_provider_kind_is_validation_error() {
        let config: Config = toml::from_str(
            r#"
            [providers.x]
            kind = "gitlab"
            "#,
        )
        .expect("parse");
        let err = config.validate().expect_err("must reject unknown kind");
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::Validation { .. }), "{msg}");
        assert!(msg.contains("gitlab") && msg.contains("'x'"), "{msg}");
    }

    #[test]
    fn bad_branch_placeholder_is_validation_error() {
        let config: Config = toml::from_str(
            r#"
            [repos.app]
            path = "/opt/app"
            forge = "github"
            branch_template = "{type}/{tycket}"
            "#,
        )
        .expect("parse");
        let err = config.validate().expect_err("must reject bad placeholder");
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::Validation { .. }), "{msg}");
        assert!(msg.contains("{tycket}") && msg.contains("'app'"), "{msg}");
    }

    #[test]
    fn load_missing_default_yields_default_but_explicit_missing_errors() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("config.toml");

        unsafe {
            env::set_var(paths::CONFIG_ENV, &missing);
        }
        let config = Config::load(None).expect("missing default file is fine");
        assert_eq!(config, Config::default());
        unsafe {
            env::remove_var(paths::CONFIG_ENV);
        }

        let err = Config::load(Some(&missing)).expect_err("explicit path must exist");
        assert!(matches!(err, ConfigError::Io { .. }), "{err}");
    }

    #[test]
    fn load_expands_repo_tilde() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("config.toml");
        std::fs::write(
            &file,
            "[repos.app]\npath = \"~/dev/app\"\nforge = \"github\"\n",
        )
        .expect("write");

        let prev_home = env::var_os("HOME");
        unsafe {
            env::set_var("HOME", "/home/yard");
        }
        let config = Config::load(Some(&file)).expect("load");
        unsafe {
            match prev_home {
                Some(h) => env::set_var("HOME", h),
                None => env::remove_var("HOME"),
            }
        }
        assert_eq!(
            config.repos["app"].path,
            PathBuf::from("/home/yard/dev/app")
        );
    }

    #[test]
    fn render_template_substitutes_and_rejects_unknowns() {
        let vars = BTreeMap::from([("type", "feat"), ("ticket", "ZEE-42"), ("slug", "login")]);
        assert_eq!(
            render_template("{type}/{ticket}-{slug}", &vars).expect("render"),
            "feat/ZEE-42-login"
        );
        assert_eq!(
            render_template("no-placeholders", &vars).expect("render"),
            "no-placeholders"
        );

        let err = render_template("{type}/{oops}", &vars).expect_err("unknown var");
        assert!(err.to_string().contains("{oops}"), "{err}");

        let err = render_template("{type", &vars).expect_err("unclosed brace");
        assert!(matches!(err, ConfigError::Validation { .. }), "{err}");
    }

    #[test]
    fn slugify_edge_cases() {
        assert_eq!(
            slugify("Fix the Café: naïve re-encoding!!"),
            "fix-the-cafe-naive-re-encoding"
        );
        assert_eq!(slugify("  Ça marche à 100%  "), "ca-marche-a-100");
        assert_eq!(slugify("___"), "");
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("UPPER lower 123"), "upper-lower-123");

        // truncated to 40 chars, never ending in '-'
        assert_eq!(slugify(&"a".repeat(50)), "a".repeat(40));
        let boundary = format!("{} bb", "a".repeat(39)); // '-' lands on char 40
        assert_eq!(slugify(&boundary), "a".repeat(39));
    }
}
