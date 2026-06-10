# Multi-Mac controller — host-class routing, VM-slot capacity, cloud→local drain

**Date:** 2026-06-01 · **Owner:** daniel.raffel
**Tracking:** Shipyard #316 · **Builds on:** #325 / v0.62.0 (`local` runner provider,
`DEFAULT_RUNNER_PROVIDER=local`, self-hosted macOS release).
**Behavioral references:** Pulp `tools/scripts/macos_reroute_watcher.py` (task #22),
Pulp `planning/2026-06-01-macos-ci-isolation-plan.md` Appendix D (2-VM/host kernel cap),
Pulp `tools/ci/tart-*.sh` (golden image + ephemeral runner).

## Why

The operating model is now multi-Mac: an always-on **Mac Studio** (primary capacity +
Tart VM pool under `/Volumes/Workshop/VMs`), an **M1 MacBook Pro** (dev machine that
sometimes contributes capacity), and a **future M5** that must inherit policy with zero
bespoke setup. GitHub-hosted macOS is overflow. Today Shipyard can route the macOS lane
to a `local` self-hosted runner (#325) but has **no notion of how much local macOS
capacity exists** across hosts, and no automatic way to claw a cloud-queued macOS job
back to local when a slot frees up. This plan adds three primitives, smallest-first.

## Host inventory (2026-06-01)

| Host class | Hostname | Runs | VM pool | cap (macOS VMs) |
|---|---|---|---|---|
| `studio` | controller / local host | pulp-studio-01/02/03, `Shipyard-studio-01`, Tart pool | yes (`/Volumes/Workshop/VMs`) | 2 (kernel quota; raisable per Appendix D, Studio-only) |
| `m1` | operator-local SSH alias | pulp-m1-01/02, Shipyard runner | dev | 2 |
| `m5` | operator-local SSH alias | project-specific runners | host-backed Tart store | 2 (inherits) |

The 2-VM cap is the XNU kernel `hv_apple_isa_vm_quota` (Appendix D), **not** Tart or a
license limit. Default `cap = 2` everywhere; only the dedicated Studio may override
higher (dev-kernel boot-arg, manual re-apply after macOS updates). M5 inherits the
default.

## Part A — host-class naming/label policy (done first; M5 depends on it)

Shipyard's `src/runner_provision.rs` **already** models the generic scheme:

- Runner name: `<project>-<class>-NN` (`runner_name()`), e.g. `pulp-studio-01`,
  `shipyard-m1-01`, future `*-m5-01`. The class is the per-box **machine tag**
  (`shipyard runner tag --set <class>`), never hostname-derived.
- Default labels (`default_labels()`): `self-hosted, macos, arm64, <project>-build,
  <project>-build-<class>`. `<project>-build` = shared routing; `<project>-build-<class>`
  = host-class pin. This matches Pulp's `pulp-build` / `pulp-build-studio|m1|m5`.

**Gap:** no drift detection, and the live fleet has drifted:
- `Shipyard-studio-01` was registered (#325) with `--labels self-hosted,macos,arm64,local-mac`
  — it carries the release.yml `local` selector but **not** `Shipyard-build-studio`.
- `daniels-macbook-shipyard` has a non-conforming name (no `<repo>-<class>-NN`), so its
  class can only be guessed from labels.

**Deliverable:** pure-logic `audit_runners(repo_short, runners)` + `shipyard runner audit`
that flags, per runner: non-conforming name, missing `<project>-build` / `<project>-build-<class>`
label, and name-class vs label-class mismatch. Exit non-zero on drift (CI-friendly).
Physical host verification ("is `*-studio-*` actually on the Studio?") is a *hint* in Part A
and becomes authoritative once Part B can SSH the host and read its machine tag.

The `local-mac` label (#325) and the host-class labels coexist: `local-mac` is the broad
"any local Mac" selector release.yml uses today; `<project>-build-<class>` is the pin the
reroute watcher (Part C) uses to target a *specific* free host.

## Part B — VM-slot-aware capacity accounting

New config section, parsed like `parse_host_pools` (`src/host_pool.rs`):

```toml
[host_class.studio]
ssh = "studio-ci.local"            # or user@host; omit for the controller's own box
cap = 2                            # macOS VM slots (kernel quota); Studio may raise
tart_bin = "/opt/homebrew/bin/tart"
tartci_bin = "/Users/ci/.local/bin/tartci"
tart_home = "/Users/ci/VMs"         # absolute path; no shell/tilde expansion
labels = ["self-hosted", "macos", "arm64", "shipyard-build-studio"]

[host_class.m1]
ssh = "m1-ci.local"
cap = 2
tart_bin = "/opt/homebrew/bin/tart"
tartci_bin = "/Users/ci/.local/bin/tartci"
tart_home = "/Users/ci/VMs"
labels = ["self-hosted", "macos", "arm64", "shipyard-build-m1"]

# [host_class.m5] added when it arrives — same shape, inherits cap = 2.
```

`shipyard runner capacity [--json]`:
1. For each configured host class, read running VM names by SSH'ing the host and
   running `tart list`, then enrich each running VM with `tart get <name> --format
   json` and count only OS `darwin`/macOS VMs as `running_macos_vms`. The
   controller's own box is read locally (no SSH). When `tart_home` is set, the
   probe runs with `TART_HOME=<absolute-path>` so it reads the same home-backed
   store the launchd supervisors use.
2. `free_host = max(0, cap_host − running_macos_vms_host)`; `free = Σ free_host`.
3. **Fail-closed:** an unreadable host (SSH/`tart` error, unparseable output) contributes
   `free_host = 0` and is flagged `readable = false` — never counted as free capacity.
4. Emit per-host `{class, ssh, cap, running, free, readable, source}` and the total.
   **Log every capacity decision** (host, cap, running, free) — silence must not read as
   success.

Pure-logic core (`compute_free_slots`, running-name parsing, and `tart get` OS parsing) is
unit-tested with injected Tart JSON output; SSH and `tart get` enrichment are the impure
edge. Note the Studio also hosts the long-lived pulp/Shipyard runner agents and any
ephemeral macOS builders — those consume its slots, so the OS-enriched live count is the
truth, not a static assumption. Linux/Windows Tart VMs must not reduce macOS free slots.

`shipyard runner fleet-status --repo <owner/repo> --target macos [--json]` is the
operator-level visibility command. It aggregates `runner capacity`, host-local
`tartci doctor --reap --json` via `tartci_bin`, supervisor heartbeat freshness,
and queued macOS job age. A host is routable only when capacity is readable,
free slots exist, `tartci doctor` is readable/clean, and at least one supervisor
heartbeat is fresh. The command exits non-zero on unreadable/problem hosts or
`queued_age_with_capacity`, separating "no slot exists" from "slots exist but
queued macOS jobs are not draining."

## Part C — cloud→local queue-drain watcher

`shipyard runner reroute-watch [--interval 30] [--flap-window 300] [--json]`, modeled on
the `runner watch` loop and Pulp's `macos_reroute_watcher.py`, generalized to multi-host
slot accounting:

Each tick:
1. `free = compute_free_slots()` (Part B). If `free <= 0`, log + skip (fail-closed if any
   host unreadable lowers free).
2. List repo `queued` workflow runs whose macOS job is dispatched to a **cloud** target
   (`macos-15` / `nscloud-` / `namespace-profile-`) and not yet picked up. (`_macos_job_targets_cloud`
   logic ported to Rust over the jobs API.)
3. For the first eligible PR not in the flap-guard window: retarget its macOS lane to a
   **specific free host** via the existing `cloud retarget` path, re-firing on that host's
   `<project>-build-<class>` label, and ensure an ephemeral VM runner is available on a
   host with a free slot.
4. Preserve the four safety properties: **flap-guard** (one PR per `--flap-window`),
   **one reroute per tick** (natural pacing), **idle/slot-safe** (only when `free > 0`),
   **fail-closed** on unreadable host state.

**Ephemeral VM runner** (avoid double-pickup): Shipyard drives Pulp's
`tart-run-job.sh`-equivalent (mint a JIT runner via `gh ... generate-jitconfig`, clone the
golden, boot, run one job, destroy). First slice may target an already-registered
persistent host-class runner and treat ephemeral spin-up as a follow-up — flagged below.

## Sequencing & PR slices

1. **PR 1 (this):** design doc + Part A (`runner audit`) + reconcile the drifted
   `Shipyard-studio-01` label set.
2. **PR 2:** Part B (`[host_class.*]` config + `runner capacity`).
3. **PR 3:** Part C (`runner reroute-watch`) on top of B, persistent-runner target first.
4. **PR 4 (follow-up):** ephemeral JIT VM runner spin-up + Studio kernel-cap override
   runbook.

## Non-goals (first slices)

Full controller/client RPC (the broader #316 product goal), distributed locking across
Macs, Tart/Tartelet isolation changes, replacing GitHub Actions as the required-check
source of truth. The Studio remains the canonical state owner; laptops use GitHub-backed
status/dispatch/retarget.
