//! Git worktree management: one worktree per work item (SPEC.md §Architecture).
//!
//! `git` is always executed as an external binary (`std::process::Command`),
//! never through a library — the one hard rule from the spec. Every
//! invocation captures stdout/stderr and surfaces failures as typed
//! [`WorktreeError`]s.
//!
//! Timeout note: git calls use `Command::output` with **no watchdog**, a
//! deliberate boring choice. All operations here act on local repositories
//! (fetch is skipped when no remote is configured), so a hang would indicate
//! a broken environment rather than a slow operation; a 60s poll-and-kill
//! loop would add machinery without a realistic failure mode to defend
//! against. Revisit if fetch against slow remotes ever becomes a problem.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Static description of where and how worktrees are managed for one repo.
///
/// Hooks are argv vectors (`hook[0]` = program, rest = arguments); an empty
/// vector means "no hook". Config wiring happens in the E2E slice (#13);
/// this type is constructed directly.
#[derive(Debug, Clone)]
pub struct WorktreeSpec {
    /// Path to the main repository clone.
    pub repo_path: PathBuf,
    /// Directory under which worktrees are created (one child per branch).
    pub worktrees_root: PathBuf,
    /// Optional per-repo `setup-worktree` hook, run after `worktree add`.
    pub setup_hook: Vec<String>,
    /// Optional per-repo `teardown-worktree` hook, run before removal.
    pub teardown_hook: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// A git invocation exited nonzero.
    #[error("git command failed: `{cmd}`: {stderr}")]
    Git { cmd: String, stderr: String },
    /// The setup hook exited nonzero (teardown hook failures only warn).
    #[error("setup hook failed: {stderr}")]
    Hook { stderr: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Creates, removes, and inventories git worktrees for one repository.
pub struct WorktreeManager {
    spec: WorktreeSpec,
}

impl WorktreeManager {
    pub fn new(spec: WorktreeSpec) -> Self {
        Self { spec }
    }

    /// Creates a worktree for `branch` (new branch off `base`) and runs the
    /// setup hook inside it. Returns the worktree path.
    ///
    /// A failing setup hook rolls the worktree and branch back before
    /// returning: no half-created state survives.
    pub fn create(&self, branch: &str, base: &str) -> Result<PathBuf, WorktreeError> {
        // Refresh remote refs so `base` can be an up-to-date remote branch;
        // a local-only repo (no remotes) simply skips this.
        if !self.run_git(&["remote"])?.trim().is_empty() {
            self.run_git(&["fetch", "--quiet"])?;
        }

        fs::create_dir_all(&self.spec.worktrees_root)?;
        let path = self.worktree_path(branch);
        let path_str = path.to_string_lossy();
        self.run_git(&["worktree", "add", &path_str, "-b", branch, base])?;

        if let Err(err) = self.run_hook(&self.spec.setup_hook, &path, branch) {
            // Roll back so a retry starts clean; rollback failures are
            // secondary to the hook error we are about to report.
            if let Err(cleanup) = self.remove_worktree_and_branch(&path, branch) {
                tracing::warn!(%branch, error = %cleanup, "rollback after failed setup hook incomplete");
            }
            return Err(err);
        }
        Ok(path)
    }

    /// Removes the worktree and deletes the branch. The teardown hook runs
    /// first, best-effort: its failure is logged, never fatal.
    pub fn remove(&self, branch: &str) -> Result<(), WorktreeError> {
        let path = self.worktree_path(branch);
        if let Err(err) = self.run_hook(&self.spec.teardown_hook, &path, branch) {
            tracing::warn!(%branch, error = %err, "teardown hook failed; removing worktree anyway");
        }
        self.remove_worktree_and_branch(&path, branch)
    }

    /// All worktrees of the repo as `(path, branch)` pairs, parsed from
    /// `git worktree list --porcelain`. Includes the main checkout; entries
    /// without a branch (detached HEAD, bare) are skipped. This is the
    /// daemon's primitive for detecting leftover worktrees at startup.
    pub fn list(&self) -> Result<Vec<(PathBuf, String)>, WorktreeError> {
        let out = self.run_git(&["worktree", "list", "--porcelain"])?;
        let mut entries = Vec::new();
        let mut path: Option<PathBuf> = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(rest) = line.strip_prefix("branch ") {
                if let Some(p) = path.take() {
                    let branch = rest.strip_prefix("refs/heads/").unwrap_or(rest);
                    entries.push((p, branch.to_string()));
                }
            } else if line.is_empty() {
                path = None;
            }
        }
        Ok(entries)
    }

    /// Commit sha currently checked out in `worktree`.
    pub fn current_head(&self, worktree: &Path) -> Result<String, WorktreeError> {
        let out = run_git_in(worktree, &["rev-parse", "HEAD"])?;
        Ok(out.trim().to_string())
    }

    /// Worktree directory for a branch: `/` is not a valid path segment, so
    /// `feature/foo` lands in `<root>/feature-foo` (the branch name itself
    /// keeps its slashes).
    fn worktree_path(&self, branch: &str) -> PathBuf {
        self.spec.worktrees_root.join(branch.replace('/', "-"))
    }

    fn remove_worktree_and_branch(&self, path: &Path, branch: &str) -> Result<(), WorktreeError> {
        let path_str = path.to_string_lossy();
        self.run_git(&["worktree", "remove", "--force", &path_str])?;
        self.run_git(&["branch", "-D", branch])?;
        Ok(())
    }

    /// Runs `argv` with cwd = the worktree and the `YARD_*` environment;
    /// empty argv is a no-op. Nonzero exit → [`WorktreeError::Hook`].
    fn run_hook(
        &self,
        argv: &[String],
        worktree: &Path,
        branch: &str,
    ) -> Result<(), WorktreeError> {
        let Some((program, args)) = argv.split_first() else {
            return Ok(());
        };
        let output = Command::new(program)
            .args(args)
            .current_dir(worktree)
            .env("YARD_WORKTREE", worktree)
            .env("YARD_BRANCH", branch)
            .env("YARD_REPO", &self.spec.repo_path)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(WorktreeError::Hook {
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    /// Runs git against the managed repo (`git -C <repo> ...`).
    fn run_git(&self, args: &[&str]) -> Result<String, WorktreeError> {
        run_git_in(&self.spec.repo_path, args)
    }
}

/// Runs `git -C <dir> <args>`, returning stdout on success and a typed
/// [`WorktreeError::Git`] (command line + captured stderr) on nonzero exit.
fn run_git_in(dir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(WorktreeError::Git {
            cmd: format!("git -C {} {}", dir.display(), args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Runs git in `dir`, panicking on failure (test scaffolding only).
    fn git(dir: &Path, args: &[&str]) -> String {
        run_git_in(dir, args).expect("test git command failed")
    }

    /// Fresh local-only repo (no remote) with one commit on `main`.
    fn init_repo(tmp: &TempDir) -> PathBuf {
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        fs::write(repo.join("README"), "hello\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);
        repo
    }

    fn manager(tmp: &TempDir, setup: Vec<String>, teardown: Vec<String>) -> WorktreeManager {
        WorktreeManager::new(WorktreeSpec {
            repo_path: init_repo(tmp),
            worktrees_root: tmp.path().join("worktrees"),
            setup_hook: setup,
            teardown_hook: teardown,
        })
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn create_checks_out_branch_from_base() {
        let tmp = TempDir::new().unwrap();
        let mgr = manager(&tmp, vec![], vec![]);
        // Second commit on a side branch so `base` is distinguishable from main's tip.
        let repo = &mgr.spec.repo_path;
        git(repo, &["branch", "base-branch"]);
        fs::write(repo.join("extra"), "x\n").unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-m", "second"]);

        let wt = mgr.create("feature-x", "base-branch").unwrap();
        assert!(wt.is_dir());
        assert_eq!(
            git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "feature-x"
        );
        let base_sha = git(repo, &["rev-parse", "base-branch"]).trim().to_string();
        assert_eq!(mgr.current_head(&wt).unwrap(), base_sha);
        // Local-only repo: fetch was tolerated (no remote configured).
    }

    #[test]
    fn create_twice_is_a_typed_git_error() {
        let tmp = TempDir::new().unwrap();
        let mgr = manager(&tmp, vec![], vec![]);
        mgr.create("dup", "main").unwrap();
        let err = mgr.create("dup", "main").unwrap_err();
        assert!(matches!(err, WorktreeError::Git { .. }), "got {err:?}");
    }

    #[test]
    fn setup_hook_sees_env_and_runs_in_worktree() {
        let tmp = TempDir::new().unwrap();
        let mgr = manager(
            &tmp,
            sh(
                "printf '%s\\n%s\\n%s\\n' \"$YARD_WORKTREE\" \"$YARD_BRANCH\" \"$YARD_REPO\" > hookout",
            ),
            vec![],
        );
        let wt = mgr.create("hooked", "main").unwrap();
        // `> hookout` is cwd-relative: proves the hook ran inside the worktree.
        let lines: Vec<String> = fs::read_to_string(wt.join("hookout"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            lines,
            vec![
                wt.to_string_lossy().into_owned(),
                "hooked".to_string(),
                mgr.spec.repo_path.to_string_lossy().into_owned(),
            ]
        );
    }

    #[test]
    fn failing_setup_hook_rolls_back_worktree_and_branch() {
        let tmp = TempDir::new().unwrap();
        let mgr = manager(&tmp, sh("echo boom >&2; exit 1"), vec![]);
        let err = mgr.create("doomed", "main").unwrap_err();
        match &err {
            WorktreeError::Hook { stderr } => assert!(stderr.contains("boom"), "stderr: {stderr}"),
            other => panic!("expected Hook error, got {other:?}"),
        }
        assert!(!tmp.path().join("worktrees").join("doomed").exists());
        let branches = git(&mgr.spec.repo_path, &["branch", "--list", "doomed"]);
        assert!(branches.trim().is_empty(), "branch survived: {branches}");
        // And a retry starts clean.
        let mgr_ok = WorktreeManager::new(WorktreeSpec {
            setup_hook: vec![],
            ..mgr.spec.clone()
        });
        mgr_ok.create("doomed", "main").unwrap();
    }

    #[test]
    fn remove_runs_teardown_then_deletes_worktree_and_branch() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("teardown-ran");
        let mgr = manager(
            &tmp,
            vec![],
            sh(&format!(
                "printf '%s' \"$YARD_BRANCH\" > {}",
                marker.display()
            )),
        );
        let wt = mgr.create("gone", "main").unwrap();
        mgr.remove("gone").unwrap();
        assert_eq!(fs::read_to_string(&marker).unwrap(), "gone");
        assert!(!wt.exists());
        let branches = git(&mgr.spec.repo_path, &["branch", "--list", "gone"]);
        assert!(branches.trim().is_empty(), "branch survived: {branches}");
    }

    #[test]
    fn failing_teardown_hook_does_not_block_removal() {
        let tmp = TempDir::new().unwrap();
        let mgr = manager(&tmp, vec![], sh("exit 1"));
        let wt = mgr.create("stubborn", "main").unwrap();
        mgr.remove("stubborn").unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn list_round_trips_created_worktrees() {
        let tmp = TempDir::new().unwrap();
        let mgr = manager(&tmp, vec![], vec![]);
        let wt_a = mgr.create("alpha", "main").unwrap();
        let wt_b = mgr.create("beta", "main").unwrap();

        // git prints realpaths (macOS: /var -> /private/var), so canonicalize
        // both sides before comparing.
        let listed: Vec<(PathBuf, String)> = mgr
            .list()
            .unwrap()
            .into_iter()
            .map(|(p, b)| (fs::canonicalize(&p).unwrap_or(p), b))
            .collect();
        for (wt, branch) in [(&wt_a, "alpha"), (&wt_b, "beta")] {
            let canon = fs::canonicalize(wt).unwrap();
            assert!(
                listed.contains(&(canon.clone(), branch.to_string())),
                "missing ({}, {branch}) in {listed:?}",
                canon.display()
            );
        }
        // Main checkout is listed too, on its own branch.
        assert!(listed.iter().any(|(_, b)| b == "main"));
    }

    #[test]
    fn branch_slashes_are_sanitized_in_path_only() {
        let tmp = TempDir::new().unwrap();
        let mgr = manager(&tmp, vec![], vec![]);
        let wt = mgr.create("feature/deep/thing", "main").unwrap();
        assert_eq!(wt, tmp.path().join("worktrees").join("feature-deep-thing"));
        assert!(wt.is_dir());
        assert_eq!(
            git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "feature/deep/thing"
        );
        // remove() resolves the same sanitized path.
        mgr.remove("feature/deep/thing").unwrap();
        assert!(!wt.exists());
    }
}
