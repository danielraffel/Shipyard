---
name: upgrade
description: Upgrade the Shipyard CLI to the latest (or a specific) version
---

Upgrade the `shipyard` CLI binary in place. Uses the same installer
the Claude Code plugin's SessionStart hook calls — drops the new
binary at `~/.local/bin/shipyard` (the canonical location), no
other files touched.

## Default: latest release

```bash
curl -fsSL https://raw.githubusercontent.com/danielraffel/Shipyard/main/install.sh | bash
shipyard --version
```

## Pin a specific version

If the user asks for a specific version (e.g. "downgrade to 0.21.2",
"stay on 0.22.0 for now"), pass `SHIPYARD_VERSION`:

```bash
SHIPYARD_VERSION="v0.22.1" bash <(curl -fsSL https://raw.githubusercontent.com/danielraffel/Shipyard/main/install.sh)
shipyard --version
```

Accepts `v0.22.1`, `0.22.1`, or `latest`.

## When to reach for this

- The user asks for "upgrade", "update shipyard", "get the latest
  CLI", "install the new version".
- Plugin features depend on a newer CLI (e.g. a new command or
  schema field) and the user is running an older binary.
- After a release that fixes a bug affecting the current session
  (e.g. the 0.22.1 daemon-spawn fix).

## Responding to the SessionStart staleness signal

The SessionStart hook now emits a JSON response with two fields
when the CLI is stale:

- `systemMessage` — renders as a **user-visible banner** at session
  start (independent of the agent). Example text:
  *"Shipyard CLI is on 0.22.0; plugin expects ≥ 0.22.1. Run
  /shipyard:upgrade to update."*
- `hookSpecificOutput.additionalContext` — added to your context
  so you can speak to it if the user asks.

Behavior to follow:

- **Do not proactively prompt** on your own. The banner already told
  the user. If they don't ask about it, don't mention it.
- **If the user responds to the banner** ("yes upgrade", "go ahead"),
  run the install command in this file and verify with
  `shipyard --version`.
- **If the user says their install is project-pinned** (e.g. pulp's
  `tools/install-shipyard.sh`), don't upgrade — persist the
  dismissal (see below) so the banner stops on future sessions, and
  suggest bumping their pin file instead.

### Persisting a "don't ask again" choice

When the user picks option 3, write a tiny JSON file under the
shipyard state dir so the hook stays silent on future sessions
(until the plugin raises its `min_shipyard_version` past what was
dismissed — a new release that matters, new decision).

The hook prints the exact paths in its marker; use those. On macOS
that's `~/Library/Application Support/shipyard/plugin-upgrade-dismissed.json`;
on Linux, `${XDG_STATE_HOME:-~/.local/state}/shipyard/plugin-upgrade-dismissed.json`.

```bash
# macOS example:
mkdir -p "$HOME/Library/Application Support/shipyard"
printf '%s\n' '{"dismissed_for_min":"0.22.1"}' \
  > "$HOME/Library/Application Support/shipyard/plugin-upgrade-dismissed.json"
```

The `dismissed_for_min` value should match the `min_expected` from
the staleness marker. Next session the hook reads the file; if the
current plugin's `min_shipyard_version` is ≤ the dismissed version,
it stays silent. If a newer plugin release bumps the minimum past
that, the hook prompts again — by design.

## When NOT to auto-run this

- **Project-pinned installs.** If a project pins a specific CLI
  version via its own installer (e.g. pulp's `tools/install-shipyard.sh`
  reading `tools/shipyard.toml`), defer to the pin — upgrading the
  CLI bypasses the project's intentional pin and can break
  reproducibility. Warn the user and suggest bumping the pin file
  instead.
- **Unattended agent sessions.** Upgrading a running CLI mid-session
  can produce surprising mismatches with the plugin. Prefer: finish
  the current task, then upgrade.

## After upgrade

```bash
shipyard --version    # confirm the new version
shipyard doctor       # re-run the environment check
```

If the user has the macOS menu-bar app running in live mode,
restart the daemon so it picks up the new binary:

```bash
shipyard daemon stop
# Auto/On mode in the GUI will spawn the new one automatically,
# or manually:
shipyard daemon start
```
