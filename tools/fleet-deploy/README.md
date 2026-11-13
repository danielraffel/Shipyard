# fleet-deploy

Installs merged fleet scripts on a host so that a fix on `main` reaches the
machine that runs it, and a host that falls behind says so.

The steward tick starts `~/.local/libexec/shipyard-fleet-deploy.sh` at its start, detached in its own session so the tick never waits for it.
At most every 10 minutes it fetches `main` of Shipyard and fleet-handbook into
private clones under `~/.local/share/shipyard-steward/deploy/` and installs:

| Unit | Source | Installed to | Validation |
| --- | --- | --- | --- |
| `tick` | `tools/steward-tick/shipyard-steward-tick.sh` | `~/.local/libexec/shipyard-steward-tick.sh` | `bash -n` |
| `deploy` | `tools/fleet-deploy/fleet-deploy.sh` | `~/.local/libexec/shipyard-fleet-deploy.sh` | `bash -n` |
| `pass-runner` | fleet-handbook `scripts/pass-runner` | the live pass-runner directory | fleet-handbook's `install-pass-runner.sh`, which self-tests the source first and backs up what it replaces |

A unit is installed only when its source changed. Files are replaced through a
temporary file and `mv`, so a running script keeps reading the old file, and
the previous file is kept as `<dest>.bak-<stamp>` (five per file). A unit
that fails validation or install is left as it was and logged as `refused` on
every run until `main` is fixed.

Every action is one line in the steward tick's `summary.log`:

```text
<stamp> fleet-deploy unit=pass-runner installed rev=<sha> from=<sha>
<stamp> fleet-deploy unit=tick refused rev=<sha> reason=syntax
```

The script exits 0 in every case.

## Settings

| Variable | Default |
| --- | --- |
| `SHIPYARD_FLEET_DEPLOY` | `on`; `off` disables it |
| `FLEET_DEPLOY_INTERVAL_S` | `600` |
| `FLEET_DEPLOY_ROOT` | `~/.local/share/shipyard-steward/deploy` |
| `FLEET_DEPLOY_KEEP_BACKUPS` | `5` |

## First install on a host

Once per host, copy the two scripts into place; after that the tick keeps them
current:

```bash
install -m 0755 tools/fleet-deploy/fleet-deploy.sh ~/.local/libexec/shipyard-fleet-deploy.sh
install -m 0755 tools/steward-tick/shipyard-steward-tick.sh ~/.local/libexec/shipyard-steward-tick.sh
```

## Tests

```bash
python3 -m unittest tools/fleet-deploy/test_fleet_deploy.py
```
