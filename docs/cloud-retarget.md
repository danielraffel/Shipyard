# Mid-flight runner retargeting

`shipyard cloud retarget` switches one target's runner provider on an
in-flight PR without tearing down the other targets' jobs. Useful mid-
batch when you discover one provider is faster/cheaper than another
and want to flip that lane across open PRs without re-running
everything.

Available since Shipyard v0.8.0.

## The use case

You start a ten-PR drain with the topology:

- `mac` — GitHub-hosted macOS
- `linux` — Namespace
- `windows` — Namespace

Ten minutes in, you notice Namespace macOS is two-thirds the runtime
of GitHub-hosted macOS. You'd like to flip the mac lane *now*, without
cancelling the Linux and Windows jobs that have already been running
for eight minutes.

Historically the only options were:

- Accept the slow drain (and the 10× 15-minute macOS builds you're
  paying for).
- Push a workflow change + re-dispatch every in-flight PR — tearing
  down hours of already-spent CI time and restarting from zero.

Neither is great. `shipyard cloud retarget` is the middle ground.

## What it does

```sh
shipyard cloud retarget --pr 224 --target mac --provider namespace
```

Dry-run by default. Prints a plan:

```
Retarget plan for PR #224 (danielraffel/pulp):
  workflow:    build
  ref:         feature/foo
  prior run:   24458654321
  target:      mac
  new provider: namespace
  matching jobs (1):
    - macOS (ARM64) [github-hosted] (job id 71460714958)

Dry-run. Re-run with --apply to cancel + redispatch.
```

When the plan looks right:

```sh
shipyard cloud retarget --pr 224 --target mac --provider namespace --apply
```

1. Cancels the **one** matching in-progress job (`macOS (ARM64) [github-
   hosted]`). Other targets on the same run (Linux, Windows) keep
   running.
2. Dispatches a fresh workflow_dispatch with `runner_provider=namespace`.

`--target` uses substring + case-insensitive matching against the job
name, so `--target mac` matches "macOS (ARM64) [github-hosted]".

## Known limitation: step 2 starts a new workflow run

GitHub doesn't natively support per-job re-dispatch with a different
runner selector. Step 2 therefore kicks off a **new** workflow run,
which starts *all* that workflow's jobs — including Linux and Windows,
which you didn't want re-run.

**This is less bad than it looks**, thanks to two things:

1. **Prior statuses persist on the PR's check rollup.** The old run's
   Linux/Windows jobs already reported SUCCESS to the PR. Those checks
   stay; the new run just adds another set of entries.
2. **Workflows with `resolve-provider` matrix steps** (pulp-style) reuse
   caches per provider, so re-running Linux on Namespace is much cheaper
   than the original build. In our testing with Pulp, the "collateral"
   Linux/Windows re-runs finish in ~2 minutes vs. ~15 for the macOS
   lane that's being retargeted.

**If you truly want full per-target isolation**, add a per-target
filter input to your workflow (e.g., `inputs.only_target`). When the
workflow respects it, `shipyard cloud retarget` could forward the
filter so only one lane re-runs. This isn't built yet; file an issue
if you need it.

## Multi-PR fleet

For drains: just loop.

```sh
for pr in $(gh pr list --state open --json number --jq '.[].number'); do
    shipyard cloud retarget --pr $pr --target mac --provider namespace --apply
done
```

Or run in parallel with `xargs -P` for faster propagation.

## Authentication

Cancelling a workflow run uses `gh api -X POST
/repos/:owner/:repo/actions/runs/:id/cancel` (the run-level endpoint
GitHub exposes; `gh run cancel` wraps the same path). It needs the
**`workflow` scope** on the PAT `gh` is authenticated with — GitHub's
short name for `actions:write` on classic PATs, or **Actions: Read
and write** on fine-grained tokens.

If your gh token lacks that scope, `cloud retarget` reports a clear
message and exits 1 without attempting the dispatch half:

```
error: Couldn't cancel the matching job(s). Your gh token may lack
`actions:write` scope.
```

The fix is a one-liner for the common case: `gh auth refresh
-h github.com -s workflow`. For fine-grained tokens and
GitHub-App-backed identities (bots, `RELEASE_BOT_TOKEN`,
`pulp-release-bot`, etc.), see [docs/install.md § First-run
auth](install.md#first-run-auth) for where the scope actually lives
per identity shape.

`shipyard doctor` probes for the scope under the `Core` section
(`gh-scope`); if it shows `✗`, you know to run the refresh before
the next retarget.

## JSON mode

`--json` emits a single envelope per invocation — `event: plan` on
dry-run, `event: applied` on `--apply`. Suitable for piping into
`jq` or an agent's stdin. Schema includes `matching_jobs`,
`cancelled_job_ids` (apply only), and the new `dispatch` plan
summary.

## Adding a lane mid-flight

`shipyard cloud add-lane` is retarget's sibling. Retarget *swaps* an
existing lane's provider; add-lane *appends* a new lane entirely. The
use case: you started shipping with `[macos, linux]` and ten minutes in
you realize you want windows too. Without add-lane the only option is
to cancel the ship and re-dispatch the full matrix, throwing away the
macOS and Linux work already done.

```sh
# Preview (dry-run by default):
shipyard cloud add-lane --pr 224 --target windows

# Apply — dispatches one workflow and appends it to the PR's ShipState:
shipyard cloud add-lane --pr 224 --target windows --provider namespace --apply
```

### Behaviour

1. Loads the PR's `ShipState` from `<state_dir>/ship/<pr>.json`. If no
   state exists (no `shipyard ship` ever ran for this PR) the command
   refuses with exit 1 — there's no in-flight ship to add a lane to.
2. Refuses if the ship is already past dispatch phase. "Past dispatch"
   means every entry in `evidence_snapshot` has terminated (pass or
   fail) and the merge decision has been made. Adding a lane after the
   verdict has been rendered is nonsensical.
3. **Idempotent.** If the target is already present in
   `dispatched_runs`, the command reports a `noop` event and does not
   dispatch anything. Safe to re-run from cron/retry loops.
4. Dispatches the single workflow for that target/provider using the
   same `resolve_cloud_dispatch_plan` path that the initial matrix
   dispatch uses — so `--provider` overrides behave identically to
   `shipyard cloud run`.
5. Appends a new `DispatchedRun` entry to the ShipState (atomic write)
   so `shipyard watch` and `shipyard auto-merge` pick up the new lane
   on their next poll.

### JSON schema

`--json` emits one of three events:

- `event: plan` — dry-run preview.
- `event: applied` — lane dispatched and appended.
- `event: noop` — target already tracked; nothing done.

Keys common to all: `pr`, `target`, `dry_run`. `plan`/`applied`
additionally expose `branch`, `repo`, `workflow_key`, `provider`, and
`dispatch_fields`. `applied` carries `run_id` + `run_url`; `noop`
carries `existing_run` with the already-tracked `DispatchedRun` dump.

### When to reach for add-lane vs. retarget

| Situation | Command |
|---|---|
| "This macOS lane is too slow, move it to Namespace" | `retarget` |
| "I realized I should also validate on windows" | `add-lane` |
| "My whole matrix is wrong, start over" | Cancel the ship, fix config, re-ship |
