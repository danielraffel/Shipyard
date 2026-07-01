# GitHub Auth Boundary Plan

Status: implementation
Last updated: 2026-05-26
Owner: unassigned
Master status: `planning/phase-handoff-status.md`

## Goal

Consolidate Shipyard's GitHub CLI subprocess calls behind one auth-aware boundary, then add opt-in token support for higher-limit or more portable GitHub identities.

The default behavior must remain unchanged: with no new config, Shipyard keeps using ambient `gh` auth. Users who need a separate quota bucket or a portable setup can opt into env-backed tokens or command-backed token helpers, including helpers that mint GitHub App installation tokens.

## Current Status

| Area | Status | Notes |
|---|---|---|
| RepoPrompt discovery | done | Selected context covered config loading, supervision, PR/wait/merge, cloud Actions, doctor, registrar, release-bot, and current auth docs. |
| External docs check | done | Verified GitHub App installation token rate-limit shape, one-hour token lifetime, private-key rotation guidance, and `gh` `GH_TOKEN` precedence against current docs on 2026-05-26. |
| Claude review | done | First pass findings are incorporated. Strict follow-up used RepoPrompt and found no blocking design issues. |
| Implementation | done for first slice | P1 shared boundary, Q2 operational `gh` migration, Q3 source-aware diagnostics/docs, and Q4 auth portability CLI are done for the first implementation slice; manual real-credential smoke remains. |

## Review Notes

Claude first-pass review completed on 2026-05-26 and recommended tightening the plan before implementation. Incorporated changes:

- Added missing call sites in `src/governance.rs` and `src/app/pr_cmd.rs`.
- Added daemon/token-cache expiry rules.
- Clarified that `GitHubActions::auth_status()` should be replaced or deprecated in favor of source-aware auth summaries.
- Added command-preparation constraints for caller-owned stdio.
- Enumerated error categories needed by doctor rendering.
- Added tracked-config helper security warnings.
- Added `gh` version-skew and daemon-expiry risks.
- Clarified P3 vs P4 diagnostics/export scope.

Claude strict follow-up completed on 2026-05-26 using RepoPrompt. It found no remaining blockers, then recommended pre-listing more raw `gh` sites from the audit. Those additional sites are now in the call-site inventory and P2 migration scope.

## Locked Decisions

- Keep `gh` as the transport. Do not rewrite Shipyard into a typed REST/GraphQL client for this work.
- Add a shared internal boundary, likely `src/gh.rs`, that prepares `gh` commands with auth, supervision, binary override, and token cache behavior.
- Use child-process `GH_TOKEN` injection for configured tokens. Do not mutate global `gh auth` state.
- Supported auth sources for the first implementation: ambient `gh-cli`, env var, and command helper.
- Do not store raw tokens, GitHub App private keys, keychain blobs, or token caches in Shipyard config, state, binaries, or release artifacts.
- Keep release-bot setup/status on operator ambient auth by default. `[github.auth]` is for Shipyard operational GitHub calls, not for changing the `RELEASE_BOT_TOKEN` workflow contract.
- Treat Mac-to-Mac portability as non-secret config portability plus external credential reprovisioning, not as Shipyard moving secrets.
- `GhClient` token caches are process-local and expiry-aware. Long-lived daemon code must either refresh through the same expiry logic or construct a fresh client for each operation; no daemon path may assume a one-hour App token remains valid indefinitely.

## Non-Goals

- No built-in GitHub App JWT signing in the first slice.
- No disk token cache.
- No shell-string token helper parsing; helper commands are argv arrays.
- No automatic auth injection into arbitrary custom commands like a user-provided merge command.
- No GitHub Enterprise host-generalization unless an existing call site already requires it.

## Current Call-Site Inventory

RepoPrompt found these primary `gh` surfaces:

| File | Current boundary | Supervised today | Auth today | Migration policy |
|---|---|---:|---|---|
| `src/pr.rs` | shared `GhClient` helper | yes | `Default` via `GhClient` | done |
| `src/wait_transport.rs` | shared `GhClient` helper | yes | `Default` via `GhClient` | done |
| `src/app/auto_merge_cmd.rs` | shared `GhClient` helper for built-in merge paths | yes | `Default` via `GhClient` | done for built-in merge paths; custom merge command remains caller-owned |
| `src/cloud.rs` | `GitHubActions` owns shared `GhClient` and uses `run_gh` | no | `Default` via `GhClient` | done |
| `src/registrar.rs` | custom `run_gh` preserving stdin/timeout/output classification | no | `Default` via `GhClient` | done; explicit fake-`gh` overrides still work for tests |
| `src/governance.rs` | shared `GovernanceGh` wrapper for branch-protection API calls | no | `Default` via `GhClient` | done |
| `src/reconcile.rs` | shared `GhClient` helper for `statusCheckRollup` | no | `Default` via `GhClient` | done; active pass reuses one client across state files; daemon fallback uses process cwd when no cwd is passed |
| `src/app/cloud_cmd.rs` | `GitHubActions::run_gh` for `remote_ref_sha` | no | `Default` via `GhClient` | done |
| `src/diagnostics.rs` | `GhDiagnosticsFetcher` owns shared `GhClient` | no | `Default` via `GhClient` | done |
| `src/app/runner_cmd.rs` | `GitHubActions::run_gh` for runner metadata | no | `Default` via `GhClient` | done |
| `src/app/rescue_cmd.rs` | `GitHubActions` cloud helper paths | no | `Default` via `GhClient` | done |
| `src/app/cleanup_cmd.rs` | shared `GhClient` helper for PR closed checks | no | `Default` via `GhClient` | done |
| `src/app/ship_state_cmd.rs` | shared `GhClient` reconcile fetch with command cwd | no | `Default` via `GhClient` | done |
| `src/pin.rs` | shared `GhClient` helper for latest Shipyard release lookup | no | `Default` via `GhClient` | done |
| `src/app/pin_cmd.rs` | shared `GhClient` helper for pin-update PR flow | no | `Default` via `GhClient` | done |
| `src/app/doctor_cmd.rs` | shared `GhClient` helper for `doctor --rate-limit` | no | `Default` via `GhClient` | done for rate-limit probe |
| `src/doctor.rs` | release-chain/default-branch/secret-listing helpers use `GhClient`; auth diagnostics use `GhClient::auth_summary` | no | `Default` via `GhClient` for migrated helpers | done for Q3 source-aware diagnostics |
| `src/app/pr_cmd.rs` | `warn_missing_release_bot_token` uses shared `GhClient` | yes | `Default` via `GhClient` | done |
| `src/app/release_bot_cmd.rs` | file-local `GhClient::ambient()` helper | no | `AmbientOnly` via `GhClient` | done; preserves operator ambient auth policy |
| `src/branch.rs`, `src/app/branch_cmd.rs`, `src/app/governance_cmd.rs` | shared `GovernanceGh` wrapper | no | `Default` via `GhClient` | done |
| `src/daemon_runtime.rs` | constructs registrar with runtime mode and cwd | no | `Default` via `Registrar`/`GhClient` | done |

For audits, re-run:

```bash
rg -n 'Command::new\("gh"\)|gh_supervised\(|fn gh\(|run_gh\(' src
```

At the last handoff update, direct `Command::new("gh")` appeared only in the
central factories `src/gh.rs` and `src/supervised.rs`. Helper names such as
`fn gh` and `run_gh` still exist in several files, but they now route through
`GhClient`, `GitHubActions`, or supervision.

## Proposed Design

Add `src/gh.rs` with these responsibilities:

- Parse `[github.auth]` from `LoadedConfig`.
- Resolve configured tokens from env or command helpers.
- Cache command-helper tokens in memory until expiry or configured TTL.
- Prepare `std::process::Command` instances for callers, while preserving caller-owned args, cwd, stdio, timeout, and output handling.
- Compose with `src/supervised.rs` so `SHIPYARD_PR_RUNNING=1` remains the source-of-truth supervised marker.
- Own shared GitHub helpers currently scattered in `src/pr.rs`, especially GraphQL rate-limit classification and rate-limit reset probing.
- Report a sanitized auth summary for `doctor` and future export/import UX.

Suggested types:

```rust
pub struct GhClient {
    auth: GhAuthConfig,
    cache: Arc<Mutex<GhTokenCache>>,
}

pub enum GhAuthPolicy {
    Default,
    AmbientOnly,
}

pub enum GhSupervision {
    Supervised,
    Unsupervised,
}
```

Suggested entry points:

```rust
impl GhClient {
    pub fn from_cwd(mode: RuntimeMode, cwd: &Path) -> Result<Self, GhConfigError>;
    pub fn from_loaded_config(config: &LoadedConfig) -> Result<Self, GhConfigError>;
    pub fn ambient() -> Self;

    pub fn prepare_command(
        &self,
        cwd: &Path,
        binary_override: Option<&Path>,
        supervision: GhSupervision,
        auth_policy: GhAuthPolicy,
    ) -> Result<Command, GhPrepareError>;

    pub fn auth_summary(
        &self,
        cwd: &Path,
        auth_policy: GhAuthPolicy,
    ) -> Result<GhAuthSummary, GhPrepareError>;
}
```

`from_loaded_config` should be the canonical constructor. `from_cwd` should be a thin wrapper that loads `LoadedConfig` and delegates, so config parsing has one owner.

Command preparation rules:

1. Use the binary override if supplied, otherwise `gh`.
2. If supervised, start from `crate::supervised::gh_supervised(...)`; otherwise use raw `Command::new(...)`.
3. If auth policy is `AmbientOnly`, inject nothing.
4. If auth policy is `Default` and auth source is ambient, inject nothing.
5. If auth policy is `Default` and auth source is env or command, resolve the token and set child env `GH_TOKEN=<token>`.
6. Never write to the parent env, `gh auth` state, keychain, config files, or Shipyard state.
7. Never take ownership of caller stdio. Callers such as release-bot secret setup and registrar still need to pipe stdin, set stdout/stderr, apply timeouts, and classify output themselves.
8. GraphQL fallback reset probes must run through the same `GhClient`, auth policy, and effective identity as the failed call. Otherwise the reset time can describe the wrong quota bucket.

Suggested error categories:

- config parse error
- unsupported auth source
- missing `token_env`
- configured env var not set
- empty `token_command`
- helper executable not found
- helper exited non-zero
- helper stdout empty
- helper stdout malformed
- token expired before use
- cached token stale and refresh failed
- repo slug required for placeholder expansion but unavailable

## Config Schema

Use the existing merged config layers from `src/config.rs`.

```toml
[github.auth]
source = "gh-cli" # "gh-cli" | "env" | "command"
token_env = "SHIPYARD_GITHUB_TOKEN"
token_command = ["op", "read", "op://Private/shipyard/github-token"]
cache_ttl_seconds = 300
refresh_skew_seconds = 60
```

Rules:

- Missing section or `source = "gh-cli"` means ambient `gh` auth.
- `source = "env"` requires `token_env`.
- `source = "command"` requires non-empty `token_command`.
- Direct token literals are unsupported.
- `refresh_skew_seconds` defaults to `60`.
- Invalid configured auth fails closed; do not silently fall back to ambient auth.

Config placement guidance:

- `.shipyard/config.toml`: only non-secret, team-shared conventions such as env var names or repo-standard helper commands.
- `.shipyard.local/config.toml`: preferred for machine-local helper paths, personal vault paths, or Keychain item names.
- Global config: good for one user applying the same auth setup across many repos.

Layering note: `LoadedConfig` deep-merges tables and replaces leaf values. A local overlay can replace `token_env` or the whole `token_command` from a global or tracked config.

Security note: a tracked `.shipyard/config.toml` can define a `token_command`. That is convenient for team-standard helpers but should be treated as trusted repo code. `doctor` should report which config layer supplied the helper and warn when a tracked helper is present, especially if it points outside the repo or uses an absolute path.

## Token Helper Contract

`token_command` is an argv array. Supported placeholders:

- `{repo_slug}` -> `owner/repo`
- `{repo_owner}` -> `owner`
- `{repo_name}` -> `repo`
- `{cwd}` -> current working directory

If a placeholder needs the repo slug and `origin` cannot be resolved to a GitHub remote, fail closed.

Preferred JSON stdout for expiring tokens:

```json
{
  "token": "ghs_...",
  "expires_at": "2026-05-26T20:12:00Z",
  "kind": "github-app-installation"
}
```

Plain token stdout is also allowed. Cache it only if `cache_ttl_seconds` is configured.

Redaction rules:

- Never print resolved tokens.
- On helper failure, surface stderr only.
- Treat empty stdout as an error.
- Treat malformed JSON-like stdout as an error without echoing token-bearing content.
- Avoid echoing full helper argv in errors when any argument looks token-like. Prefer sanitized argv with secrets redacted.
- If helper refresh fails but a cached token is still valid, continue using the cached token. Once expired, fail closed.

## GitHub App Support

Phase 1 should support GitHub Apps through the command helper source rather than by implementing JWT signing in Shipyard.

Recommended shape:

```toml
[github.auth]
source = "command"
token_command = ["shipyard-github-app-token", "--repo", "{repo_slug}"]
refresh_skew_seconds = 60
```

The helper owns:

- GitHub App id/client id.
- Installation id lookup or explicit installation id.
- Private key retrieval from Keychain, 1Password, env, or another external store.
- JWT signing.
- `POST /app/installations/{installation_id}/access_tokens`.
- JSON stdout with `token`, `expires_at`, and `kind = "github-app-installation"`.

Shipyard owns:

- Calling the helper.
- Caching the returned token until `expires_at - refresh_skew_seconds`.
- Injecting the token into child `gh` commands as `GH_TOKEN`.
- Showing sanitized diagnostics and rate limits.

Why this split:

- GitHub App installation tokens expire after one hour.
- Private keys are long-lived high-value secrets.
- GitHub supports multiple App private keys, which makes per-machine keys and rotation practical.
- Keeping signing out of phase 1 avoids new crypto/key-management dependencies while still enabling higher-limit App identities.

## Mac-To-Mac Portability

Portability is a core requirement. The signed Shipyard binary and `.dmg` remain credential-free. Moving to another Mac should mean moving config and re-establishing credentials externally.

Export/import covers config only. It must not try to relocate daemon sockets, queues, ship state, local runner state, `gh auth` state, Keychain items, 1Password sessions, private keys, or any other mutable runtime state.

Supported portable patterns:

Env-backed token:

```toml
[github.auth]
source = "env"
token_env = "SHIPYARD_GITHUB_TOKEN"
```

On the new Mac, set the same env var through the user's shell, launch agent, direnv, or secret manager.

macOS Keychain helper:

```toml
[github.auth]
source = "command"
token_command = ["security", "find-generic-password", "-w", "-s", "shipyard-github-token"]
cache_ttl_seconds = 300
```

On the new Mac, create the Keychain item separately. Shipyard does not export it.

1Password CLI helper:

```toml
[github.auth]
source = "command"
token_command = ["op", "read", "op://Private/shipyard/github-token"]
cache_ttl_seconds = 300
```

On the new Mac, install/sign in to `op`.

GitHub App helper:

```toml
[github.auth]
source = "command"
token_command = ["shipyard-github-app-token", "--repo", "{repo_slug}"]
refresh_skew_seconds = 60
```

On the new Mac, install the helper and re-provision the App private key through Keychain, 1Password, env, or a machine-local file outside the repo.

Implemented first-slice CLI UX:

- `shipyard auth doctor`: focused auth diagnostics, helper validation, effective rate-limit probe, portability warnings.
- `shipyard auth export`: write a sanitized bundle containing config, required env var names, helper argv, and notes. Never include secrets.
- `shipyard auth import`: write sanitized config into global, tracked repo, or local overlay config, then optionally validate helper/env availability.

Export bundle sketch:

```toml
version = 1

[github.auth]
source = "command"
token_command = ["op", "read", "op://Private/shipyard/github-token"]
cache_ttl_seconds = 300
refresh_skew_seconds = 60

[requirements]
commands = ["op"]
env_vars = []
notes = ["Run `op signin` on the destination Mac before using Shipyard."]
```

## Doctor And Rate-Limit UX

`shipyard doctor --rate-limit` should probe rate limits through the same `GhClient` and auth policy that operational commands use.

Add or expand a GitHub auth section with:

- effective auth source: `gh-cli`, `env`, or `command`
- token resolution status
- token kind and expiry when known
- whether scopes/permissions are inspectable locally
- portability warnings, such as missing helper command or absolute helper path
- effective REST and GraphQL remaining/reset data

Doctor wording must distinguish:

- ambient classic token with visible scopes
- fine-grained PAT or App token where classic scopes are not inspectable
- broken env var or helper config
- App token expiry

For configured env/command tokens, `gh auth status` may not provide useful classic-scope text. That should not be treated as a failure if the token resolves and API probes succeed.

`GitHubActions::auth_status()` should not remain a boolean wrapper around ambient `gh auth status`. Replace or deprecate it in favor of `GhClient::auth_summary(...)`, so callers can distinguish ambient auth, resolved configured token auth, and broken configured auth.

The trait seam is `src/executor/cloud.rs::CloudActionsClient::auth_status`. Migrating `GitHubActions::auth_status()` means updating that trait and its fake implementation, not just `src/cloud.rs`.

## Release-Bot Interaction

Keep `shipyard release-bot setup/status` on ambient operator auth by default.

Reasoning:

- It manages repository secrets and dispatches verification workflows as an operator action.
- A repo-configured App token may be intentionally narrower and may not have secret-management access.
- The existing workflow template using `${{ secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN }}` remains unchanged.

Implementation policy:

- Migrate local `gh` construction in `src/app/release_bot_cmd.rs` to the shared boundary.
- Use `GhAuthPolicy::AmbientOnly`.
- Preserve optional `gh_command` binary override.

## Rollout Phases

### P0: Validation Spike

- Verify `gh` honors child `GH_TOKEN` for `gh api rate_limit`.
- Verify App installation token behavior for one `gh api` read, one `gh pr view`, and one REST mutation Shipyard uses.
- Verify at least one explicit GraphQL-backed read under an App installation token, since PR/check status paths can use GraphQL even when REST works.
- Check `gh auth status` output under injected env token and App token.
- Observe whether `gh` emits stderr warnings when `GH_TOKEN` is set alongside an ambient keychain login, and decide whether to pass them through or suppress them in specific probes.
- Verify command preparation preserves stdin by smoke-testing a command shape equivalent to `gh secret set --body -`.
- Finalize doctor wording based on observed behavior.

### P1: Shared Boundary

- Add `src/gh.rs`.
- Parse config.
- Resolve env and command tokens.
- Cache command tokens.
- Prepare supervised and unsupervised commands.
- Move shared GraphQL rate-limit helpers and GitHub remote parsing helpers where appropriate.
- Add unit tests.

### P2: Operational Call-Site Migration

- Migrate `src/pr.rs`.
- Migrate `src/wait_transport.rs`.
- Migrate `src/app/auto_merge_cmd.rs`.
- Migrate `src/cloud.rs` and all `GitHubActions::new(...)` callers.
- Migrate `src/reconcile.rs`.
- Migrate `src/app/cloud_cmd.rs`.
- Migrate `src/app/rescue_cmd.rs`.
- Migrate `src/diagnostics.rs`.
- Migrate `src/app/runner_cmd.rs`.
- Migrate `src/app/cleanup_cmd.rs`.
- Migrate `src/app/ship_state_cmd.rs`.
- Classify and migrate `src/pin.rs` and `src/app/pin_cmd.rs`.
- Migrate `src/governance.rs`.
- Migrate `src/app/pr_cmd.rs`.
- Migrate `src/registrar.rs`.
- Migrate `src/app/release_bot_cmd.rs` with `AmbientOnly`.
- Migrate `src/app/doctor_cmd.rs` rate-limit probe.

### P3: Diagnostics And Docs

- Update `src/app/doctor_cmd.rs`.
- Update `src/doctor.rs`.
- Add effective auth source and rate-limit diagnostics.
- Update `docs/install.md`, `RELEASING.md`, and `skills/shipyard/SKILL.md`.

### P4: Auth Portability CLI

- `shipyard auth doctor` added as a focused namespace after the general P3
  doctor behavior stabilized.
- `shipyard auth export` added for sanitized config-only bundles.
- `shipyard auth import` added for sanitized config-only import into global,
  project, or local config.
- Keep export/import strictly non-secret.

## Test Plan

New tests in `src/gh.rs`:

- config parsing for all source variants
- invalid config failures
- env token injection into child command
- command helper plain-token parsing
- command helper JSON-token parsing
- expiry, TTL, skew, refresh, and stale-cache behavior
- placeholder expansion
- repo slug resolution failure
- token redaction in errors
- supervised command contains both `SHIPYARD_PR_RUNNING=1` and injected `GH_TOKEN`

Migration tests:

- Move `is_graphql_rate_limited` tests from `src/pr.rs` to `src/gh.rs`.
- Preserve PR parsing and REST fallback tests.
- Add fake-`gh` tests proving fallback reset probes use the configured auth source.
- Update `src/cloud.rs` tests for `GitHubActions { gh }`.
- Preserve cloud/reconcile command tests after routing through `GhClient`.
- Add doctor rendering tests for ambient, env, command, missing env, broken helper, and App expiry cases.
- Preserve release-bot workflow rendering tests.

Manual validation matrix:

| Auth shape | Required smoke tests |
|---|---|
| ambient `gh auth login` | `doctor`, `doctor --rate-limit`, PR read, cloud read |
| env-backed fine-grained token | same plus one mutation if permissions allow |
| Keychain command helper | same |
| 1Password command helper | same |
| GitHub App installation helper | same plus expiry/cache behavior |

## Docs To Update

`docs/install.md`:

- Optional GitHub auth override.
- Env source example.
- Keychain helper example.
- 1Password helper example.
- GitHub App helper example.
- Mac-to-Mac checklist.
- Warning that Shipyard exports/imports config only, not secrets.

`RELEASING.md`:

- Clarify `[github.auth]` is separate from `RELEASE_BOT_TOKEN`.
- Release-bot setup continues to use operator ambient `gh` auth.
- Moving Macs does not require changing release artifacts.

`skills/shipyard/SKILL.md`:

- Check `[github.auth]` when debugging GitHub behavior.
- `doctor --rate-limit` reflects effective auth.
- Portability rule: config only; secrets stay external.

## Risk Register

| Risk | Mitigation |
|---|---|
| `gh pr` behaves differently under App installation tokens | P0 validation; keep REST fallbacks; fail closed on configured auth failure. |
| Doctor classic-scope checks mislead fine-grained/App users | Source-aware `GhAuthSummary`; explicit “permissions not inspectable locally” wording. |
| Helper paths are machine-local and break after Mac migration | Doctor/export portability warnings; docs recommend stable command names or repo-local helpers. |
| Token leaks through errors or tests | Redaction rules and explicit tests. |
| Broken configured auth silently falls back to ambient auth | Explicitly disallow fallback for env/command sources. |
| Release-bot users expect `[github.auth]` to affect workflow secrets | Keep `AmbientOnly`; document separation. |
| Users expect Shipyard to move App private keys | Define export/import as config-only; recommend per-machine keys or external secret-manager sync. |
| Long-lived daemon code reuses an expired App token | Make cache expiry mandatory and test refresh paths; avoid static global clients. |
| `gh` version skew breaks doctor text parsing | Keep `gh auth status` parsing source-aware and backed by API probes; record validated `gh` version in the handoff status. |
| Team-shared `token_command` becomes a supply-chain footgun | Warn when helper config comes from the tracked layer; recommend local overlays for personal secrets and helper paths. |

## Acceptance Criteria

- With no `[github.auth]`, behavior is unchanged.
- Every built-in `gh` subprocess in migrated surfaces goes through the shared boundary, except documented bypasses.
- Env-backed and command-backed tokens work without changing global `gh auth` state.
- Command-backed App installation tokens can be reused until expiry within one process.
- `doctor --rate-limit` probes the same effective auth context as operational commands.
- Doctor clearly distinguishes ambient auth, configured token auth, non-inspectable permissions, broken helper/env config, and App token expiry.
- Release-bot setup/status still use ambient operator auth by default.
- No raw token or private key is written to config, state, logs, tests, or release artifacts.
- Docs cover Mac-to-Mac portability through non-secret config plus external credential reprovisioning.
- Repo-wide grep confirms no accidental raw `gh` spawn remains in migrated surfaces.
- A CI or test gate covers the raw-`gh` audit for migrated surfaces, rather than relying only on manual review.
- GraphQL fallback reset probes use the same `GhClient` and auth policy as the failed command.

## Agent Handoff Checklist

Use this block as the live status record while implementing:

```md
### Active Phase
- Phase: quota/auth first slice complete; local Mac pool P1 is next unless the user wants more auth polish
- Branch: main
- Last validated commit: 5ca2c0b
- `gh` version validated: gh version 2.92.0 (2026-04-28)

### Decisions
- [x] Shared boundary in `src/gh.rs`
- [x] `gh` remains subprocess transport
- [x] Child `GH_TOKEN` injection
- [x] Auth sources: `gh-cli`, `env`, `command`
- [x] No disk token cache
- [x] Release-bot uses `AmbientOnly`
- [x] Portability exports config only

### Call-Site Migration
| Area | Files | Status | Notes |
|---|---|---|---|
| PR / wait / auto-merge | `src/pr.rs`, `src/wait_transport.rs`, `src/app/auto_merge_cmd.rs` | done | Custom auto-merge commands still bypass configured auth by design. |
| Cloud | `src/cloud.rs` | done | `GitHubActions` owns a process-local `GhClient`; command-helper token cache can span one cloud operation. |
| Reconcile | `src/reconcile.rs` | done | CLI path passes mode/cwd; daemon fallback preserves prior process-cwd behavior. |
| Cloud command helpers | `src/app/cloud_cmd.rs`, `src/app/rescue_cmd.rs`, `src/app/ship_state_cmd.rs` | done | Constructors use already loaded config where available. |
| Diagnostics | `src/diagnostics.rs`, `src/app/ship_cmd.rs` | done | Failure diagnostics use a config-aware `GhDiagnosticsFetcher`. |
| Runner / cleanup | `src/app/runner_cmd.rs`, `src/app/cleanup_cmd.rs` | done | Runner metadata and ship-state cleanup PR checks route through `GhClient`. |
| Pin/update | `src/pin.rs`, `src/app/pin_cmd.rs` | done | Uses `Default` auth for latest-release lookup and PR creation. |
| Governance | `src/governance.rs`, `src/branch.rs`, `src/app/branch_cmd.rs`, `src/app/governance_cmd.rs` | done | Shared `GovernanceGh` wrapper preserves explicit binary overrides for tests. |
| PR command helpers | `src/app/pr_cmd.rs` | done | `warn_missing_release_bot_token` uses the already loaded config and `GhClient`. |
| Registrar | `src/registrar.rs`, `src/daemon_runtime.rs` | done | Webhook create/update/delete preserve stdin, timeout, and output classification. |
| Doctor | `src/app/doctor_cmd.rs`, `src/doctor.rs` | done for Q3 | `doctor --rate-limit` and legacy release-chain/default-branch/secret-listing helpers route through `GhClient`; doctor renders configured auth source, helper kind/expiry, and manual-verification rows for non-inspectable permission states. |
| Release-bot | `src/app/release_bot_cmd.rs` | done | Uses `AmbientOnly` by design. |

### Portability
- [x] Env source documented
- [x] Keychain helper documented
- [x] 1Password helper documented
- [x] GitHub App helper documented
- [x] Mac-to-Mac checklist documented
- [x] Future `shipyard auth` UX scoped
- [x] `shipyard auth doctor`
- [x] `shipyard auth export`
- [x] `shipyard auth import`

### Validation
- [x] `src/gh.rs` unit tests (`cargo test gh::`, 13 tests)
- [x] config parsing tests in `src/gh.rs`
- [x] `src/pr.rs` focused tests (`cargo test pr::`)
- [x] `src/wait_transport.rs` focused tests (`cargo test wait_transport::`)
- [x] built-in auto-merge focused tests (`cargo test app::auto_merge_cmd::`)
- [x] PR command helper focused tests (`cargo test app::pr_cmd::`)
- [x] governance focused tests (`cargo test governance::`, `cargo test app::governance_cmd::`)
- [x] branch focused tests (`cargo test branch::`, `cargo test app::branch_cmd::`)
- [x] registrar focused tests (`cargo test registrar::`, `cargo test daemon_runtime::`)
- [x] legacy doctor focused tests (`cargo test doctor::`, `cargo test app::doctor_cmd::`)
- [x] doctor rendering tests for configured auth summary
- [x] helper stderr token-prefix redaction test
- [x] auth CLI focused tests (`cargo test app::auth_cmd::`, 5 tests)
- [x] auth CLI smoke tests (`auth export`, `--json auth doctor`)
- [x] supervised env + `GH_TOKEN` coexistence test
- [ ] full Rust tests green (`cargo test` has two unrelated auto-merge failures: `app::tests::auto_merge_failure_preserves_state`, `app::ship_cmd::tests::ship_command_green_merge_failure_keeps_active_state_and_exits_success`)
- [ ] ambient auth manual smoke test
- [ ] env token manual smoke test
- [ ] Keychain/op helper manual smoke test
- [ ] App installation token manual smoke test

### Open Questions / Blockers
- Run manual smoke tests for ambient, env, Keychain/1Password command helper,
  and GitHub App helper auth.
- Decide whether Q4 should expand the `GhClient` helper cache from one slot to
  a keyed map before multi-repo helper-heavy workflows rely on it.
```

## References

- RepoPrompt context used for this plan:
  - `src/config.rs`
  - `src/supervised.rs`
  - `src/pr.rs`
  - `src/wait_transport.rs`
  - `src/app/auto_merge_cmd.rs`
  - `src/cloud.rs`
  - `src/registrar.rs`
  - `src/app/doctor_cmd.rs`
  - `src/doctor.rs`
  - `src/app/release_bot_cmd.rs`
  - `docs/install.md`
  - `RELEASING.md`
  - `skills/shipyard/SKILL.md`
- GitHub docs checked on 2026-05-26:
  - REST API rate limits: https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api
  - GitHub App installation authentication: https://docs.github.com/enterprise-cloud@latest/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation
  - GitHub App private keys: https://docs.github.com/en/enterprise-cloud@latest/apps/creating-github-apps/authenticating-with-a-github-app/managing-private-keys-for-github-apps
  - GitHub CLI environment variables: https://cli.github.com/manual/gh_help_environment
