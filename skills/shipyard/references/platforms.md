# Shipyard Platform Notes

## macOS

- Tailscale App Store builds can support Funnel. Do not classify the App Store
  build itself as incompatible.
- Prefer `/Applications/Tailscale.app/Contents/MacOS/Tailscale` when probing
  Tailscale on Daniel's Mac. `/usr/local/bin/tailscale` may be a symlink to
  that app binary and can crash with `BundleIdentifiers.swift:41` before command
  dispatch.
- Shipyard's daemon launches Tailscale with `TERM=dumb`. The app-bundle CLI can
  otherwise try to start its GUI and emit non-JSON when inherited service
  environments omit both `TERM` and `TERM_PROGRAM`. Keep this behavior on every
  noninteractive status, Serve, and Funnel invocation; do not rely on an
  interactive SSH shell masking the fleet failure.
- `scripts/validate_webhook_tunnel_live.py --json` is safe and non-mutating.
  It should prove Shipyard binary presence, `gh` auth, GitHub hook-read access,
  Tailscale readiness, and Funnel permission without starting a daemon.
- `scripts/validate_webhook_tunnel_live.py --apply --allow-funnel-reset --json`
  starts the Rust daemon, creates a transient GitHub hook, pings it, observes
  IPC delivery, cleans up, and resets local Tailscale Funnel. Run only in an
  approved window.
- macOS release signing is local-first. Preserve Developer ID signing,
  notarization, stapling, and arm64-only macOS artifact policy unless the
  current migration packet says otherwise.
- Credential checks may read `/Users/danielraffel/Code/PlunderTube/.env`.
  Accepted aliases include `APPLE_ID`, `TEAM_ID`,
  `APP_SPECIFIC_PASSWORD` / `APP_PASSWORD`, and `APP_CERT`.
- GUI validation belongs in `/Users/danielraffel/Code/shipyard-macos-gui`.
  Use isolated selected-CLI paths or signed rehearsal artifacts until cutover.

## Linux

- Namespace is the fast default for Rust CI and sandbox E2E unless workflow
  dispatch inputs or repo variables override runner selection.
- Sandbox E2E must run with isolated `HOME`, `PATH`, state, and install roots.
  It must not read or mutate the daily Python Shipyard install.
- Linux is the easiest place to prove command-surface, JSON, installer dry-run,
  and cloud-dispatch behavior, but macOS signing and GUI gates still remain
  separate go/no-go inputs.

## Windows

- Preserve PowerShell quoting and path behavior. Windows SSH validation is
  sensitive to CLIXML decoding, slash/UNC path handling, remote upload paths,
  and UTF-8/code-page behavior.
- Do not assume POSIX shell semantics for Windows targets. Keep command
  construction and diagnostics explicit.
- Windows CI may use Namespace or GitHub-hosted runners depending on the
  current resolver settings. Check `scripts/ci_matrix.py` before claiming
  runner behavior.

## GitHub And Cloud

- Prefer Shipyard's `wait`, `cloud status`, `cloud run`, `cloud retarget`,
  `cloud add-lane`, and `cloud handoff` surfaces over hand-rolled polling when
  the target repo is opted into Shipyard.
- Keep `gh auth status` scoped to `github.com` behavior when auditing parity.
- Webhook registration is repo-scoped and must clean up transient hooks after
  live validation.
