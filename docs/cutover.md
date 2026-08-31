# Rust Shipyard Release And Rollback

Shipyard is Rust-backed by default as of `v0.51.0`. This runbook is
release-agnostic; substitute the exact candidate and rollback tags being
validated rather than relying on a version copied from an earlier rollout.

This page is now an operator runbook: how to validate the installed
binary, how release artifacts are produced, how to test live webhook
delivery safely, and how to roll back if the Rust CLI or daemon
regresses.

## Current Release Shape

- `shipyard` and `sy` install to `~/.local/bin`.
- Current macOS releases are Apple Silicon only:
  `shipyard-macos-arm64.dmg`.
- Linux x64, Linux arm64, and Windows x64 ship as native standalone
  binaries.
- macOS artifacts must be Developer-ID signed, notarized, stapled into
  a DMG, mounted, extracted, and launch-tested before the GitHub release
  is published.
- The release stays draft until all expected public assets and
  `checksums.sha256` are present and install E2E has passed.

## Validate The Installed CLI

Run these after install, upgrade, rollback, or daemon refresh:

```sh
shipyard --version
sy --version
shipyard --json doctor
shipyard --json doctor --release-chain
shipyard wait release vX.Y.Z --repo danielraffel/Shipyard --timeout 60 --json
codesign --verify --deep --strict "$(command -v shipyard)"
```

Expected healthy state:

- `shipyard --version` and `sy --version` report the same release.
- `doctor.ready` is `true`.
- `daemon-version` says the daemon and CLI versions match.
- `doctor --release-chain` reports `release_chain.version:
  checkout-ok` when `RELEASE_BOT_TOKEN` is configured.
- `wait release` observes the expected release assets from
  `danielraffel/Shipyard`, not from the current checkout's inferred
  repo. Pass `--repo` explicitly when running from a sidecar repo.

## Release Gates

Before publishing a new Shipyard CLI release, run the local gates that
match the change:

```sh
python3 -m unittest discover -s scripts -p 'test_*.py'
cargo fmt -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo llvm-cov --locked --summary-only --fail-under-lines 75
python3 -m pytest tools/sandbox-e2e/ -q
```

The sandbox gate serializes its protected-path snapshot and contamination
assertion against legitimate production Shipyard writes with the host-global
writer-domain lease under the production state directory. Each protected
filesystem mutation holds a shared lease only for its critical section; an idle
daemon, a read-only command, and a session-independent worker between writes do
not own it. This gate holds the exclusive lease from snapshot through assertion.
A fair-entry turnstile prevents a stream of short writers from starving the
audit while still allowing unrelated work to continue.
`sandbox_writer_domain_overlap` is a bounded, proven-overlap deferral/failure:
wait for the active writer or audit and rerun. Do not allowlist the reported
protected path or delete either lock file. During a mixed-version rollout,
restart every v0.108.1 daemon because it retains the obsolete lifetime lease;
also drain pre-v0.108.1 processes, which do not participate in the domain.
Prove exact-binary fleet convergence before treating a sandbox result as
authoritative.

The protected production-writer inventory covers queue requests/outcomes and
supervisor receipts; ship, merge-queue, recovery, registrar, warm-pool,
host-pool, cloud, metrics, evidence, and selector state; machine-global config,
auth, target, pin, install, and runner-recovery paths; and daemon plus
local/SSH/Windows streamed logs. Repository/worktree mutations and remote-host
files are outside the audited home roots. Recovery-model HOME/TMP scratch is
kept in an identity-keyed OS temporary directory because the opaque child may
outlive its launching session; only validated receipts enter durable state.
The default-off `artifact_transport::ArtifactStore` remains explicitly outside
this inventory because no production path constructs it; any future production
activation under an audited root must join this writer domain before wiring.

For a release that changes merge-queue behavior, add these fleet gates:

1. Set and verify a distinct runner tag on every machine.
2. Configure one `[merge_queue].mutation_machine` in each host's trusted
   machine-global `config.toml` reported by `shipyard paths`; prove every
   other host refuses a mutation before invoking the GitHub client.
3. Prove `shipyard merge-queue hold` blocks enqueue and dequeue, survives a
   process restart, and reports the stored reason.
4. Run concurrent same-repo/base mutation fixtures and verify only one writer
   enters while `merge_queue/mutations.jsonl` records the winning correlation.
5. Replay the incident timelines for stale head/base, ambiguous enqueue,
   manual removal, `failed_checks`, `invalid_merge_commit`, and base advance.
6. Install the same release on every fleet host and verify both
   `shipyard --version` and the artifact checksum before activating the sole
   authority.
7. For a generation-aware GitHub App rollout, record the wrapper selector,
   machine-auth generation and authority IDs, manifest/member digests,
   generation count and bytes, and free space on every host. Prove the loaded
   daemon uses that same generation. Retain the prior generation for rollback;
   cleanup is a separate reader-aware dry-run and must preserve every selected,
   journal-referenced, rollback/rollforward-referenced, or open/ambiguous
   generation.

For release candidates, also validate the package/install path in an
isolated sandbox:

```sh
SHIPYARD_BINARY_FOR_TEST="$PWD/target/release/shipyard" \
  python3 -m pytest tools/sandbox-e2e/ -q
```

The Rust cutover rehearsal passed CLI surface parity, 82%+ line
coverage, local unit/script gates, sandbox E2E, signed macOS rehearsal,
GUI validation, release-chain doctor, and live webhook validation before
`v0.51.0` was merged.

## macOS Release

The default tag push creates a draft release with non-macOS artifacts.
The macOS DMG is then produced by either the local maintainer path or
the optional CI signing path.

Local signing remains the primary release path:

```sh
scripts/release-macos-local.sh \
  --tag vX.Y.Z \
  --upload \
  --rollback-tag vPREVIOUS \
  --env-file /path/to/release.env
```

Required environment:

- `SHIPYARD_NOTARIZE_APPLE_ID`
- `SHIPYARD_NOTARIZE_TEAM_ID`
- `SHIPYARD_NOTARIZE_APP_PASSWORD`
- `SHIPYARD_SIGNING_IDENTITY`

The script builds or accepts a supplied binary, signs the Mach-O,
packages a DMG, signs and notarizes the DMG, staples the ticket,
mounts the DMG, runs `shipyard --version`, uploads the macOS artifact,
merges checksums, verifies public asset visibility, and runs
install/upgrade/rollback E2E when a rollback tag is provided.

The optional CI path is gated by `CI_MACOS_SIGNING_ENABLED=true` and
requires the `MACOS_SIGN_*` / `MACOS_NOTARIZE_*` secret set. CI signing
uses an ephemeral keychain and the same `release-macos-local.sh
--ci-mode` orchestration. If CI signing is not enabled, the macOS job is
build-health-only and does not upload an unsigned artifact.

## Webhook And Funnel Validation

### Recover missing local webhook provenance

Do not edit `daemon/registrations.json` or delete a remote hook to repair a
missing local registration. Starting with the registrar reconciliation change,
an explicit refresh first lists every remote hook page and adopts exactly one
`web` hook whose callback URL exactly matches Shipyard's current callback. It
creates a hook only when no exact match exists and fails closed without remote
mutation when more than one exact match exists.

For the M3 Pulp/Forge daemon, after installing the reconciliating binary, use
the complete configured authority explicitly:

```sh
shipyard daemon refresh \
  --repo Generous-Corp/forge \
  --repo Generous-Corp/pulp
shipyard --json daemon status
```

The terminal status must report both repositories under `configured_repos`.
`registered_repos` is only the subset whose remote hook registration has
succeeded; it is not the configured watch authority. If refresh reports
ambiguous exact hooks, stop and resolve the named remote hook IDs with a human
who has repository-hook administration authority. Do not choose or delete one
automatically.

Non-mutating preflight is safe anytime:

```sh
python3 scripts/validate_webhook_tunnel_live.py \
  --repo danielraffel/Shipyard \
  --binary "$(command -v shipyard)" \
  --json
```

Full live validation creates a temporary GitHub webhook and may reset the
machine-global Tailscale Serve/Funnel route. Run it only when that short
interruption is acceptable:

```sh
python3 scripts/validate_webhook_tunnel_live.py \
  --repo danielraffel/Shipyard \
  --binary "$(command -v shipyard)" \
  --apply \
  --allow-funnel-reset \
  --json
```

The validator understands the App Store Tailscale build and probes
`/Applications/Tailscale.app/Contents/MacOS/Tailscale` when PATH shims
are unavailable. A healthy non-mutating pass proves `gh`, `curl`,
GitHub hook read access, DNS, Funnel permission, and current Funnel
status without changing the route.

## GUI And Consumers

The macOS GUI should use the selected CLI path as the source of truth.
When the selected CLI supports `shipyard --json paths`, the GUI can
derive daemon socket, pid, and state paths from that response. Older
Python binaries without `paths` need the legacy production socket
fallback.

Do not update Pulp or other consumer pins as part of a Shipyard release
PR. After the release is published and stable, update consumers through:

```sh
shipyard pin show
shipyard pin bump --to vX.Y.Z
```

Keep consumer pin PRs isolated from unrelated docs or source changes.

## Rollback

Rollback if the new release fails doctor, daemon IPC, GUI selected-CLI
resolution, webhook/Funnel validation, installer E2E, or a consumer
gate.

1. Stop the daemon:

   ```sh
   shipyard daemon stop
   ```

2. Reinstall the last known-good release, or restore a preserved local
   backup binary:

   ```sh
   SHIPYARD_VERSION=vPREVIOUS \
     curl -fsSL https://generouscorp.com/Shipyard/install.sh | bash
   ```

3. Confirm the restored binary:

   ```sh
   shipyard --version
   shipyard --json doctor
   ```

4. Restart the daemon only after the restored CLI is healthy.
5. Point the GUI selected CLI path back to the restored binary if needed.
6. Revert or supersede any consumer pin PRs that targeted the bad
   release.
7. If a just-published GitHub release failed install E2E, return it to
   draft or delete the bad asset before users can install it.

## Deferred Features

Issue `#265` shipped in `v0.51.1`. Its additive-dispatch extension is
still intentionally not implemented because a standalone dispatch may
not satisfy the same stale PR-event required check context.

Issue `#266` remains the next deferred post-cutover candidate:
`SHIPYARD_PR_RUNNING=1` for supervised `shipyard pr` child processes and
clean GraphQL rate-limit backoff. Keep it separate from release
stability work.
