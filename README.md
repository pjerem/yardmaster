# yardmaster

> A yardmaster coordinates every track of a rail yard. They never drive a train.

`yardmaster` orchestrates N coding agents working on N tickets, from ticket
intake to merge. It is a single compiled binary (`yard`) — a scriptable CLI on
top of a self-spawned daemon — for macOS and Linux. The core is deterministic:
the orchestrator never calls an LLM; all intelligence lives in agent backends
(omp first). Every public side effect (PR creation, comments, merges, tracker
transitions) requires explicit human approval through a gate queue, every
single time. See [SPEC.md](SPEC.md) for the full design.

## Install

Download a prebuilt binary from
[GitHub Releases](https://github.com/pjerem/yardmaster/releases). Four targets
are published per release, plus a `SHA256SUMS` file:

| target | platform |
|---|---|
| `aarch64-apple-darwin` | macOS, Apple Silicon |
| `x86_64-apple-darwin` | macOS, Intel |
| `x86_64-unknown-linux-musl` | Linux x86_64 (static) |
| `aarch64-unknown-linux-musl` | Linux ARM64 (static) |

```sh
VERSION=v0.0.1                  # pick the latest from the releases page
TARGET=aarch64-apple-darwin     # pick yours from the table above

curl -fsSLO "https://github.com/pjerem/yardmaster/releases/download/${VERSION}/yard-${VERSION}-${TARGET}.tar.gz"
curl -fsSLO "https://github.com/pjerem/yardmaster/releases/download/${VERSION}/SHA256SUMS"

# Verify — macOS:
grep "${TARGET}" SHA256SUMS | shasum -a 256 -c -
# Verify — Linux:
sha256sum -c --ignore-missing SHA256SUMS

tar -xzf "yard-${VERSION}-${TARGET}.tar.gz"
install -m 755 yard ~/.local/bin/yard    # or any directory on your PATH
```

Check the install:

```sh
yard --version
```

## Quickstart

### 1. Configure

Configuration lives in `~/.config/yardmaster/config.toml` (override with
`$YARDMASTER_CONFIG`). Minimal setup — one GitHub provider, one repo, the
built-in omp agent:

```toml
[providers.gh]
kind = "github"                    # "jira", "github", or "local"
user = "your-github-login"
# url defaults to https://api.github.com; token: see Secrets below

[repos.myrepo]
path = "~/dev/myrepo"              # existing local clone
forge = "github"
provider = "gh"                    # the [providers.*] entry above
remote_repo = "your-org/myrepo"    # owner/name on the forge
base = "main"                      # default
# agent = "omp"                    # default: built-in omp backend
```

The default agent backend runs the `omp` binary, which must be on the
daemon's `PATH`. Override its command lines with an `[agents.omp]` section,
or add other backends as `[agents.<name>]` argv templates.

**Secrets.** The provider token is looked up under the key
`providers.<name>` (here `providers.gh`), in order:

1. Environment variable `YARDMASTER_SECRET_PROVIDERS_GH`
   (prefix `YARDMASTER_SECRET_`, key uppercased, `.`/`-` → `_`).
2. macOS Keychain (service `yardmaster`, account `providers.gh`).
3. Flat TOML file `~/.local/state/yardmaster/secrets.toml`, mode 600:
   `"providers.gh" = "<API_TOKEN>"`.

### 2. Run

```sh
yard add gh:your-org/myrepo#23
```

This tracks the ticket, creates a dedicated git worktree and branch, and
starts the agent. The `gh:` prefix may be omitted when a single provider is
configured. The daemon is spawned on demand and survives terminal closure —
no service to set up.

```sh
yard status          # daemon health + work-item table (--json for scripts)
yard gates           # pending 🔴 approvals, with the exact payload each one implies
yard approve 1       # the human keypress: executes gate 1 (e.g. creates the PR)
```

Nothing public happens without that keypress: worktree edits and local
commits are automatic (🟢), pushes to the feature branch are automatic with
notification (🟠), and PR creation, comments, merges, and tracker
transitions wait in the gate queue (🔴).

Other commands:

```sh
yard reject 1 --reason "wrong ticket"   # refuse a gate; the item escalates
yard logs 1 --tail 50                   # agent transcript for work item 1
yard daemon run                         # run the daemon in the foreground
yard daemon stop                        # clean shutdown (auto-spawned otherwise)
```

## Design

The full engineering contract — principles, architecture, domain model,
workflow state machine, adapter traits, and roadmap — is in
[SPEC.md](SPEC.md). Product rationale (French) is in
[docs/pm-notes.md](docs/pm-notes.md).

## License

MIT
