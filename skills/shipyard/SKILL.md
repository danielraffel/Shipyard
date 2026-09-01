---
name: shipyard
description: Shipyard operations guardrails. Use when working in /Users/danielraffel/Code/shipyard, /Users/danielraffel/Code/shipyard-rust, or /Users/danielraffel/Code/shipyard-macos-gui on parity checks, drift checks, sandbox validation, live Tailscale/GitHub webhook validation, release signing, GUI validation, Pulp/consumer pin cutover, or any go/no-go migration work.
---

# Shipyard

## Core Rule

Preserve the user's active Shipyard install and rollback path. Rust Shipyard is
the daily implementation as of `v0.51.0` / `v0.51.1`, but do not replace
`/Users/danielraffel/.local/bin/shipyard`, remove preserved backups, change
Pulp pins, reset Tailscale Funnel, or merge GUI cutover support without a clear
go/no-go for that operation.

## Durable work handoff

Do not spend an agent session polling a pull request, build, benchmark, release,
or other bounded script after its inputs and completion contract are fixed.
Hand that work to Shipyard when the installed version supports the required
stewardship mode. The handoff must bind immutable inputs (repository and exact
head when applicable), command or controller identity, owner/coordinator,
deadline, heartbeat/stall policy, artifact and log locations, success criteria,
the success continuation, and the failure conditions that require agent
judgment. A handoff with only a success trigger is invalid: terminal failure,
head drift, cancellation, timeout, and ambiguous process loss each need an
explicit disposition or wake owner.

Until the receipt reports `monitoring_transferred=true`, the originating agent
retains the last monitoring obligation. Durable publication creates a
zero-wake daemon obligation; it does not launch a provider process. Stop
monitor-only children after transfer, but continue independent runnable work by
default. Park only when the exact machine-readable tuple is
`monitoring_transferred=true`, `agent_disposition=pause`, and
`pause_required=true`, backed by `--after-handoff pause --task-graph`.

Shipyard records terminal provenance using explicit terminal contracts. A
HerdR handoff requires `HERDR_ENV=1` plus workspace, tab, and pane identity;
the optional `HERDR_SESSION` name defaults to `default`. The provider session
comes from Shipyard's resolved agent provenance, because HerdR does not export
one. Partial or conflicting metadata fails closed. Existing cmux handoffs
preserve their legacy route hash, while an ordinary terminal remains
terminal-agnostic. Raw terminal
and provider-session identifiers stay in Shipyard's private ledger and are not
published to GitHub. Typed provenance is only a wake address: until a trusted
consumer advertises availability, it does not authorize pausing, resuming, or
transferring monitoring.

When an exact wrapper or provider-specific resume command must survive the
handoff, pass a private `LaunchProfileV1` JSON file with `--launch-profile`.
Preserve launch and resume argv as exact arrays; never translate provider flags
or put credentials in the profile. The contract also binds opaque
provider/account/model metadata, checkpoint generation/digest, and exact
repository/worktree/head/lineage provenance. Shipyard never executes the
stored argv directly. It validates the prompt-free native grammar, projects
typed model/reasoning options into the pinned provider adapter, and reports
transfer only after durable publication. See `docs/launch-profile.md`.

After an acknowledged handoff whose receipt proves monitoring is transferred:

- Shipyard owns process lifetime, monitoring, bounded deterministic retries,
  transition-only logs, terminal receipts, and recovery after its own restart.
- The agent stops monitor-only children and continues independent runnable work
  by default. It parks only when the handed-off work is its remaining blocker.
- A swarm child never becomes a permanent monitor. Shipyard wakes the logical
  coordinator, or one explicitly promoted owner, when interpretation or repair
  is required.
- A normal success should flow directly to the next deterministic action. An
  actionable failure should wake exactly one owner with the exact failure,
  inputs, logs, and smallest legal next action; ownership returns to Shipyard
  after a repaired immutable input is published.
- A provider timeout with an uncertain dispatch outcome is recorded and is not
  blindly repeated. Session death, quota exhaustion, host reboot, and
  offline/rejoin must not erase an acknowledged obligation.

See `docs/post-handoff-disposition.md` for the task-graph schema, crash windows,
and Pulp/Forge Modular/Forge Sequencer/Vellum workflow rules.

This contract applies to bounded jobs as well as PRs: CMake/CTest proofs,
artifact builds, release publication, notarization, benchmark matrices, cache
prewarming, and fleet canaries are examples. Do not hand Shipyard open-ended
design or debugging. A job without a deterministic completion condition,
bounded timeout, durable log/receipt, and safe cancellation rule remains
agent-owned until those are defined.

Shipyard's protected local ledger and live provider/GitHub state are execution
authority. Linear may receive an asynchronous, idempotent projection for human
visibility and planning, but Linear failure must never block execution, wake,
repair routing, queue admission, or merge. Never project provider session IDs,
credentials, private paths, or raw prompts.

## Custody carrier setup and teardown

Cross-machine custody stays default-off until the owner supplies the complete
private host contract. Start with the no-write plan, apply only that exact
manifest, and retain the returned policy digest:

```bash
shipyard --json custody provision --input /owner/private/custody.toml
shipyard --json custody provision --input /owner/private/custody.toml --apply
shipyard --json custody doctor
```

The manifest must be owner-only and contain both top-level
`schema_version = 1` and `custody_transport.setup_contract_version = 1`.
Doctor validates bounded include-expanded `/usr/sbin/sshd -T` output rather
than scanning one config file: it requires `ExposeAuthInfo yes`, the exact
`Subsystem shipyard-custody-v1 <receiver> --mode shipyard work-ledger
custody-receive` argv, and an effective `AuthorizedKeysFile` that resolves to
the configured path. Authorized-key checks accept OpenSSH options such as
`restrict` but still bind the exact peer key. Each outbound private identity
and inbound public key is directional and digest-pinned.

SSH provisioning is not sufficient readiness. The same policy must bind valid
owner-only destination-bootstrap, native-publication, and private-profile
receipts for the exact machine, incarnation, route, and authority. Shipyard
does not create or infer those prerequisites. Missing, unknown, legacy, stale,
or mismatched setup evidence remains not-ready; migrate a legacy policy through
supported exact-digest removal and reprovisioning, never manual TOML edits.

Return the carrier to default-off through the supported dry-run/apply pair:

```bash
shipyard --json custody disable --policy-digest <doctor-policy-digest>
shipyard --json custody disable --policy-digest <doctor-policy-digest> --apply
shipyard --json custody doctor
```

Disable rereads the exact generation under the writer-domain lease, refuses
while custody state is active or indeterminate, removes only the matching
`[custody_transport]` table, preserves unrelated machine configuration and all
append-only ledger history, and proves the disabled readback. It never deletes
SSH keys, `authorized_keys`, `known_hosts`, or terminal custody receipts. See
[`docs/durable-custody-transport.md`](../../docs/durable-custody-transport.md)
for the full manifest and host-runbook boundary.

## First Steps

1. Confirm the active repo and dirty state with `git status --short`.
2. Use RepoPrompt for code analysis across Shipyard, historical shipyard-rust,
   and the macOS GUI before declaring parity or implementation gaps.
3. Read the current planning packet before making release/cutover claims:
   `planning/post-cutover-status.md`, `planning/go-no-go-completion-audit.md`,
   `planning/upstream-drift.md`, `planning/documentation-backlog.md`, and
   `docs/plan/README.md`.
4. Use `--mode isolated`, temporary install directories, and sandbox HOME/PATH
   roots for rehearsals that must not touch the active production state.
5. For contributor-controlled revisions or `/shipyard review`, use
   `skills/review-external-contributions/SKILL.md`. The dedicated disposable-VM
   controller is intentionally separate from normal Shipyard targets and must
   fail closed rather than use local, SSH, host-pool, self-hosted, or fallback
   execution. Its untrusted job VM has no virtual NIC; only the trusted
   controller talks to GitHub. Timer activation remains an operator decision.

## Sandbox writer-domain lease

The sandbox E2E contamination audit and production Shipyard share one
host-global OS lock at `.sandbox-writer-domain.lock` under the production state
directory reported by `shipyard paths`, plus a fair-entry
`.sandbox-writer-domain.turnstile.lock`. Production acquires a shared lease only
around each protected filesystem mutation; streamed logs reacquire per append,
while an external child that can write a protected transaction keeps the lease
for that bounded transaction. Idle daemons, read-only commands, and durable
workers between writes own no lease. Each sandbox fixture keeps both exclusive
locks from before snapshotting through the final contamination assertion. Do
not add filename-, PID-, queue-ID-, log-, or evidence-path exemptions: a
protected write is either fenced by the lease or it remains contamination.

The standalone `scripts/shipyard-github-app-token` helper participates in this
same domain when its optional disk cache is under a real Shipyard protected
root. Cache reads and GitHub requests remain outside the lease; cache directory
creation and the atomic token replacement acquire the fair turnstile and shared
domain lease on Unix/macOS. Contention keeps the cache untouched and exits `75`
with `sandbox_writer_domain_overlap`. Windows disk caching remains fail-closed
until private ACL validation and an equivalent writer-domain implementation
exist.

The audit waits up to five seconds for an active mutation. A production
mutation arriving during an audit waits up to 30 seconds, then propagates exit
`75` with the stable `sandbox_writer_domain_overlap` classification. That result
proves overlap: defer and retry after the other side finishes; do not convert it
into a pass or code/test failure. During rollout, restart every v0.108.1 daemon
because it holds the obsolete process-lifetime lease. Pre-v0.108.1 binaries do
not participate at all; drain them and prove exact-binary fleet convergence
before trusting the audit.

If the guardian refuses before any transition because production workers are
active, the workflow may classify that result as a safe INFRA deferral only by
passing the complete receipt through `scripts/sandbox_admission_deferral.py`.
The resulting marker remains valid only while the installed hash, exact
mutation-probe path, absent lease, daemon PID, and live process start identity
still match at both workflow checkpoints. Never treat the deferral as a test
failure, rerun the production work, or accept a missing/null receipt field.

On macOS, advisory-lock contention can briefly appear before `lsof` reports its
holder, and a corrected daemon can briefly be the sole reported holder while it
finishes one mutation-scoped write. The guardian may observe only those exact
no-holder or production-PID-only contended states for a bounded stable-idle
window while continuously fencing the production PID/start identity, binary
hash, configuration, and active-worker set. Stable idle selects the corrected
preserve path. Only uninterrupted exact production-PID ownership through the
bound plus exact IPC-reported running-daemon version `0.108.1` selects the legacy
quiesce/restore path, and the current installed artifact must independently
report that same exact version before a destructive stop so restoration is
possible. The post-observation peer, disk-version, identity, and final-holder
proofs share one aggregate deadline so guardian readiness remains within the
workflow receipt budget. Holder observation has its own bound: every exact
production-identity revalidation receives a fresh bounded budget that a slow
`lsof` call cannot consume, while both holder attempts and that identity proof
remain inside one 30-second composite maximum and any shorter caller deadline.
A retained corrected-path lease from the historical
double-`lsof`/near-zero-`ps` timeout may be reconciled only when the failure has
that exact typed shape and the existing lease generation, dead candidate,
mutation fence, unchanged production identity, stable queue idle, and final
writer-lock fence all revalidate. Do not delete such a lease manually or admit
other timeout shapes. Known-corrected, unknown, or running/disk-mismatched versions fail closed under
persistent contention and are never stopped on duration alone. Any
foreign/additional or missing holder sample, identity drift, new worker,
changing or ambiguous ownership, or deadline that does not prove one of those
two states remains a hard failure. Production stop/control must run from stable
`/` with explicit production state
authority, never from a repository checkout that can be unavailable.
Restore retries must adopt an already-live
exact matching production daemon before spawning, but only after bounded
same-connection macOS `LOCAL_PEERPID`-fenced IPC status response and original
stdio identity proof, with repository, version, and active-worker probes all
bound to the explicit production state root rather than candidate or HOME
defaults, so a
post-spawn verification error cannot launch a duplicate or accept a
hung/misdirected process. Retain a live/uncertain restore child across cleanup
retries, but clear a child only after its exit is authoritative so the next
retry can replace it. Retain the machine-wide canary lease from any production
stop request across partial quiesce or restore failure and in the final failure
receipt; release it only after exact
production identity is verified, so another canary cannot overlap an uncertain
detached restore child. Before adopting a different exact production daemon, terminate and
authoritatively reap Shipyard's own retained restore child so it cannot later
acquire the lifetime lock and replace the verified owner. A restored v0.108.1
daemon must report that exact version over same-peer IPC and additionally prove
stable PID-only kernel contention on its lifetime lock before the guardian
releases the host lease, with repository/worker authority and exact process
identity revalidated after the lock proof. IPC readiness or an open descriptor
alone is not enough. After any stop request, never adopt the original PID/start
generation as restored while it remains alive; wait boundedly for it to exit,
then spawn or adopt only a different exact generation. If the corrected daemon
was never stopped and the canary fails before its mutation-fence audit, recover
by proving the exact unchanged production identity and stable idle state without
misreporting the mutation fence as passed, then release the lease. Sandbox failure
artifacts must stay inside the explicit canary or runner temp root; never glob
protected system temp trees.

After cleanup, treat a single daemon IPC status miss as an observation rather
than proof of death: use a bounded status window that continuously rechecks the
exact production PID and requires the configured repository set from the
guardian receipt before accepting liveness. A PID change, process loss,
repository drift, or deadline still fails closed.

Each macOS sandbox guardian also owns its unique launchd registration. It must
publish an fsynced terminal receipt before unloading its exact canonical label;
an active retained-lease reconciliation remains loaded until it reaches that
terminal state. A later canary may recover at most four inert registrations per
run, and only from a private receipt/plist pair that binds the exact label and
canary root. PID incarnation, receipt identity, and a successful full launchd
inventory must agree. Missing, malformed, live, ambiguous, or over-limit
entries are skipped or deferred, never removed by prefix alone. Terminal paths
poll for label disappearance and fail after bounded fallback cleanup, so an
unload regression cannot silently repopulate Background Items.

Opaque recovery-model HOME/TMP scratch lives outside durable Shipyard state in
an identity-keyed OS temporary directory, so a session-independent model child
cannot contaminate the audit. Only its validated durable receipts are published
under the protected state root.

## Merge-queue ownership

When GitHub's live branch queue or evaluated rules require a merge queue,
Shipyard is a validator and queue supervisor, not a second merge authority. A
passing `shipyard ship` calls GitHub's queue mutation with
`expectedHeadOid=<validated SHA>` and waits for GitHub to merge it. The
one-shot `shipyard auto-merge` returns exit 3 while queued and retains
ship-state for later ticks.

For private-free repositories, GitHub may return an explicit null live queue
and its exact plan-entitlement 403 from evaluated rules. That combination can
classify the branch as non-queue-governed, but Shipyard still refuses automatic
classic direct merge. `admin=false` does not prove the authenticated mutation
credential is excluded from every admin, custom-role, ruleset, or GitHub App
bypass path, so a client-side check snapshot cannot become mechanical merge
authority. Use the native merge queue for automatic merging. A manual
maintainer exact-head merge remains outside Shipyard. Generic 401/403 responses,
malformed authority, and a non-null queue still stop or select the queue path.

Never treat queue absence alone as permission to rearm. The PR must first have
been observed in the queue (durably recorded across restarts), and only an
`invalid_merge_commit` removal may be re-enqueued automatically. Failed checks,
manual/unknown removal, head drift, malformed authority data, or a
403/rate-limit response stop fail-closed. The sole reviewed exception is the
default-off steward flag `--recover-hosted-setup-eviction-priority`: it may use
one write-ahead-audited `jump: true` enqueue for an absent same-head managed PR
only from a durable queue-front pre-removal queue/base-revision witness whose speculative commit
has that base as a parent, plus an exact `failed_checks` timeline event no more than two hours old
and one linked failed required GitHub Actions CheckRun whose sole failure is a
GitHub-hosted setup-only provider-internal DNS outage. Generic setup failures,
self-hosted jobs, incomplete or ambiguous histories, intervening admissions,
base/governance drift, and prior receipts
never authorize the jump.

Use `shipyard --json queue-observe --repo <owner/repo> [--follow]` when an
agent needs a durable, read-only feed of base SHA, open PR heads/checks,
server-owned queue order and merge-group checks, mutation ownership, and
`HOLD` state. Each live tick is one bounded GraphQL query; unchanged polls emit
nothing and back off through 15/30/60/120/300 seconds. The observer writes an
atomic canonical cursor plus an append-only transition log under the Shipyard
state root and takes an exclusive per-cursor lock. It exposes no mutation flag,
does not acquire a mutation lease, and needs no write credential. See
[`docs/queue-observer.md`](../../docs/queue-observer.md).

Use `shipyard --json changed-surface-plan --repo <owner/repo> --pr <n>
--target <name>` for the shadow-only exact-head selector. Policy must come from
the authenticated protected base and may contain only reviewed path globs plus
literal baseline/family tests. Never substitute caller regexes, caller-selected
base/head SHAs, `skip-target`, `resume-from`, or diff-cover selectors. A local
head/tree mismatch hard-fails with no receipt; every later ambiguity selects the
full suite. The receipt is telemetry and cannot replace full target evidence.
Schema v2 classifies bounded candidates as mandatory, affected, or extended;
reviewed high-risk families and `full_required_paths` select full. Medium-risk
families add only protected-base-declared literal integration/co-failure tests.
Schema v1 remains accepted as affected-only. Never infer risk from mutable
head-side policy or use an unknown path to narrow the suite.
Release-only families under a Debug target require fresh, non-reused,
same-exact-head evidence no more than 24 hours old from their base-declared
Release target. The evidence must carry that target's own
`validation_build_type`, and neither direct nor active-profile advisory targets
qualify. The evidence must bind a clean pre-execution checkout at the
authenticated head and tree. Use a concrete local secondary target; remote,
cloud, host-pool, and fallback targets are rejected until their source-tree
provenance is captured, and prepared-state reuse must be disabled. Without a
fresh, non-resumed execution of the full contract the plan is policy-blocked,
not redirected into a known-incompatible full Debug run.
See [`docs/changed-surface-selection.md`](../../docs/changed-surface-selection.md).

Metadata/docs-only authority is a separate trusted machine-global fast path.
It may clear native targets only for a complete exact base/head/tree/path
closure covered by narrow repository globs and exact required contexts bound
to their GitHub App or status-actor database identity. Only terminal
`SUCCESS` counts. Shipyard reobserves the protected base, PR head, and hosted
checks at worker start, carries an immutable crash-consistent receipt, and
runs the controller outside native-worker capacity. Unknown paths, incomplete
pagination, stale or replaced checks, producer/policy/SHA drift, malformed
config, or receipt uncertainty retain ordinary full validation.

The first build-once/shard-many surface is also shadow-only and default-off.
Its pure canary admission accepts only Pulp's exact numeric repository identity
and slug plus its mac target, with an M3 builder and a freshly observed M1 LAN
worker. Preserve the bound exhaustive inventory,
fixtures, dependencies, `RUN_SERIAL` fleet exclusivity, and resource locks;
require exact host-local cache generations and authenticated persistent,
normalized, non-temporary host-declared staging roots.
Reject stale/offline/Tailnet-only observations, mismatched capabilities or
cache identity, timing evidence from another exact manifest or target, savings that do
not meet both 120 seconds and 10 percent, or transfer/dispatch overhead above
15 percent of shard work. M5 remains optional and excluded until roaming
recovery is proven. No v1 canary decision dispatches work or satisfies merge
readiness.

The shadow inventory producer accepts only bounded CTest JSON-v1 observations.
It requires controller-owned target/per-test capability declarations and an
explicit host/fleet scope for every observed `RESOURCE_LOCK`; unused
declarations, a filtered set that differs from the independent expected count
or sorted-ID digest, a test whose emitted configuration differs from the exact
controller-owned `CTest -C` configuration, ambiguous property types, duplicate
properties, unsupported versions, disabled or non-executable tests, and
nonempty `REQUIRED_FILES` without controller filesystem attestation, and
`RESOURCE_GROUPS` fail closed. Capability expansion is bounded before per-test
cloning. It is a pure translation boundary: it does not invoke CTest, dispatch
work, or satisfy merge readiness.

Bounded execution remains machine-global default-off. For a controlled canary,
use `changed_surface_execution.mode = "shadow_compare"` in trusted global
config; the repository cannot enable it, the queued command is exact-bound,
and the repository adapter must keep the full result authoritative while
writing selected-vs-full receipts. Do not use `authoritative` without reviewed
graduation evidence and a machine-global
`changed_surface_execution.accepted_shadow_policy_digests."<owner/repo>".<target>`
entry that exactly matches the protected plan. Repository and target scope are
both authorization boundaries. The legacy scalar remains compatible only by
itself; replace it atomically when migrating because mixed scalar/scoped policy
is ambiguous and fails closed.

The one-host build-once bridge remains machine-global default-off. The
`parallel-proof-canary` command exposes a one-shot plan/apply seam; apply also
requires a trusted digest-pinned native adapter and complete reviewed
invocation-authority digest. Core contains no Pulp commands or personal host
defaults. Bind one successful configure and
one successful build receipt to the exact source, toolchain, canonical CTest
inventory, proof manifest, and compact artifact content/layout/size identity.
Consumption must remain in the same authenticated M3 session, verify that
exact content address, observe zero configure/build invocations, and reconcile
the sorted unique executed IDs exactly to the canonical inventory. It cannot
run commands, dispatch cross-host work, publish a check, or replace the full
authoritative gate.

Schema v3 also binds nonempty baseline and per-family `CMake` producer-target
lists. It may replace only the exact protected `build` and `test` stages as one
`build_and_test` transaction. In `shadow_compare`, the repository adapter must
build selected targets and run selected tests first, then execute the original
full build and test path as authority. Missing or ambiguous target projection,
or a partial-stage substitution, fails closed.
Do not resume schema-v3 changed-surface execution from `test`; Shipyard first
authenticates every eligible plan in a read-only preflight, then refuses before
persisting activation or substituting any target because the request could
skip producer builds and test stale warm artifacts. If every plan is schema v2
or ineligible, Shipyard preserves the original stages and resumes the ordinary
test stage without a second observation or activation. Restart schema v3 from
`build` or start a fresh validation.

After a shadow comparison completes, use `shipyard --json
changed-surface-trial-status --repo <owner/repo> --pr <n> --target <name>
--head <sha>` to read its immutable activation and single adapter result. The
command reports `collecting` (exit 3), `ready` (exit 0), or `rejected` (exit 1)
and fails closed on ambiguous files, identity/digest drift, nonzero results, or
invalid timing. For schema-v3 build-and-test trials, its selected total includes
receipt verification and it reconstructs the full-build estimate from selected
build plus the warm incremental remainder before reporting savings or speedup.
The command is evidence inspection only: it never changes machine mode,
accepted policy digests, queue state, or merge readiness.

When a PR base is stale in `shadow_compare`, keep `stale_base` as the
authoritative full-suite reason. Shipyard may still compute a shadow-only
selection from the complete old-to-live base delta and an exact conflict-free
integration tree. A bounded result executes selected-versus-full only inside a
content-addressed, fenced checkout of that integration commit; activation,
tracked content, submodules, cleanup intent, restart recovery, and the terminal
receipt must all preserve the same repository/PR/head/tree, old and live base,
merge base, policy/workflow/validation-contract, changed-path, and integration
identity. Reuse is exact-identity only. Any drift, conflict, incomplete
observation, unmapped/full-required path, replay, or semantic disagreement
invalidates or selects full. The result always remains
`blocked_until_current_merge_tree`: it is comparison telemetry, never merge
authority, an automatic rebase, or permission to mutate a runner or PR.

Prospective pre-push selection is only a transport optimization and is also
machine-global default-off. Shipyard permits one non-delete branch update and
authenticates the actual `core.hooksPath/pre-push` against the protected base:
it must be a tracked regular file with the platform-valid Git tree mode
(executable on POSIX), not a symlink, and its bytes must remain unchanged
across the push. Shipyard owns the private result directory,
nonce, and receipt identity; the hook must bind the exact head, tree, changed
paths, selected tests, and hook digest. Any ambiguity or identity drift falls
back to the full authoritative validation contract.

The selector library also understands schema-v2/v3 protected-base
`execution` declaration for a future controlled promotion. It remains inert
unless orchestration explicitly supplies the exact planner input again, the
unaltered validation and workflow contract digests, a proven POSIX command
transport, and a trusted machine-global enable bit. Shadow policy, an unset
machine switch, unsupported transports, full/high-risk plans, stale identity,
or any malformed digest retain the ordinary full suite. The bounded command
contains only a size-limited authenticated receipt payload; it never embeds a
regex or caller-selected test expression. Defining this policy is not itself
activation: until the ship/queue path durably snapshots and substitutes the
plan, full validation remains authoritative.

Formal GitHub stacked pull requests are a separate merge lifecycle. Shipyard
reads the protected base's top-level `stacked_pr_mode = "off" | "observe" |
"apply"` together with `PullRequest.headRefOid`, `stack`, and `stackEntry` at
each merge or enqueue mutation boundary, including `shipyard runner steward`.
The default is `off`; detection remains active and refuses the unstacked path.
`observe` emits a deterministic `stacked-pr-plan=<json>` receipt bound to the
full PR head, repository, PR, stack number/size/position, and stack base. The
receipt always says `github_mutation=false` and
`required_checks_suppressed=false`: it is telemetry after normal validation,
not evidence and not a check waiver. `apply` parses so configuration drift is
visible, but is structurally unavailable (NO-GO) until Shipyard durably models
GitHub's asynchronous request UUID and result polling. A trusted machine-global
top-level `stacked_pr_mode = "off"` is the conservative fleet override; global
`observe`/`apply` is rejected so a machine cannot broaden repository policy.
Invalid values, unreadable policy, partial stack metadata, or an observed head
different from the validated head fail closed before GitHub mutation. Ordinary
unstacked PRs retain the existing merge path in all three modes. Validate each
stack layer, then use `gh stack merge <pr> --merge` for the manual pilot. A
classic-boundary GraphQL exhaustion permits read-only REST identity recovery,
but never REST merge mutation; automatic classic merge returns exit 10. GitHub
requires the asynchronous endpoint for formal stacks, and queue admission plus
re-enqueue inspections remain fail-closed.

For fleets, configure exactly one mutation host in the trusted machine-global
`config.toml` reported by `shipyard paths` (never in tracked project config):

```toml
[merge_queue]
mutation_machine = "studio"
```

Each box must have its stable `shipyard runner tag`. Validation can execute on
any host, but a non-authority host must fail before queue mutation. Use
`shipyard merge-queue hold --reason "<incident>"` on the configured mutation
machine as the authority stop and `shipyard merge-queue status` there to verify
it. Propagate the hold to other hosts when consistent fleet status matters. Do
not resume until the incident owner has restored the intended queue order.
Queue writes are serialized process-wide and recorded in machine-global
`merge_queue/mutations.jsonl`.

For release rollout, configure absolute sibling `shipyard_bin` and `github_cli`
paths named `shipyard` and `ghapp`. Machine-global command auth must be exactly
wrapper + `token --app-id VALUE --private-key ABS --repo {repo_slug}`. Direct
`ghapp` resolves only that shape through its sibling Shipyard after grammar and
repo validation; the wrapper pins API, cache, and resolved repo arguments.
Also configure explicit `shipyard_mode`, `shipyard_global_dir`, and `shipyard_state_dir`
on every remote `[host_class.<name>]`. Review `shipyard runner fleet-update
--to vX.Y.Z --host-class <class>` and then use the same command with `--apply`;
do not assemble per-host SSH/install pipelines. Repeat `--host-class` only for
a reviewed ordered subset, or use explicit `--all-hosts`. Missing, unknown, and
duplicate selection fails closed; apply stops before later hosts after the
first failure.

For targets v0.134.0 and newer, the fleet transaction stages the exact
release-matched CLI, helper, wrapper, and typed context in a private,
content-addressed auth generation. Starting with v0.137.0, that generation
also contains the mandatory release-matched `pr-close-guard` and advertises
the mutually exclusive `auth-selector-v2` contract. A v0.134-v0.136 fleet
client must reject that target wrapper before publication; first install a
v0.137-or-newer controller through the supported exact-release path, then run
the governed fleet transaction. The first v0.137 activation publishes a
digest-bound, regular-file public trampoline only after legacy readers drain.
Later updates never replace that public file: they atomically move an
owner-private generation selector instead. The trampoline reads the selector
once and executes the selected immutable wrapper; that wrapper resolves the
CLI, context, helper, and close guard from its own generation. The v4 journal
binds the trampoline, selector, and complete cohort while retaining bounded
v2/v3 recovery compatibility. Crash recovery rolls back before validation and
rolls forward after validation or commit. Do not remove a published generation
during rollout because an already-open reader may still require it. Preserve
generation count, total bytes, and free disk in each host receipt; any future
reader-aware garbage collector remains a separately reviewed, dry-run-only
operation.

Machine-global `token_command[0]` must name the stable public `ghapp`
trampoline, not a path inside `auth-generations`. To repair a v0.137-era direct
generation binding, run `shipyard auth helper-argv --wrapper <public-ghapp>
--repo <owner/repo>`. The repair is deliberately narrow: it validates the full
manifest and every installed generation member, acquires the writer-domain
lease, proves the live selector and configured generation still agree, re-reads
the command, then atomically replaces only its first argument while preserving
the owner-private 0600 TOML. Any selector/config disagreement, unmanifested or
tampered member, public-trampoline mismatch, or concurrent command change
refuses without mutation.

Before any host can mutate, Shipyard resolves the exact annotated tag object,
commit, tree, release ID, checksum-manifest asset, and macOS DMG asset. It
streams both assets by immutable ID into owner-private hard-capped staging,
verifies the exact declared byte count, GitHub SHA-256 values, and manifest
entry, and requires DMG `gh attestation verify` proof from
`danielraffel/Shipyard/.github/workflows/release.yml` at the exact tag ref and
source commit. A tag string or locally asserted receipt is never source
authority. A missing DMG attestation keeps the release ineligible. Shipyard
uses one process-tree deadline for the producer and bounded nonblocking capture
so overflow, partial files, noisy diagnostics, or escaped pipe holders cannot
grow disk use or wedge cleanup. Shipyard
closes the authority-mint window by re-reading the tag, release ID, and asset
inventory, then freezes that exact authority for the rollout. Each host must
return the frozen authority and platform-asset digests; Shipyard never remints
authority between hosts, and any receipt or cross-host pair-hash mismatch stops
every later host.

A successful typed host receipt includes the full authority plus before/after
primary and `shipyard-workstream-provider` path, version, and double-observed
SHA-256, CLI/daemon identity, configured runtime paths, and repository
preservation. Pre-install provenance is explicitly unverified; only the
post-install pair is bound to the verified authority digest. Older
fleet-update releases starting at v0.127.0 required both pair members, while
rollback tags from v0.100.0 through v0.126.x required the companion absent;
the current client does not target those releases. Fleet rollout requires a self-contained machine-global command auth
helper because inherited secrets are deliberately stripped. The updater stages
the entire exact-tag installer before execution. The first transition from
v0.130.x or older to a resolver-capable target requires an ordinary exact-tag
update on each host, then migration to the exact machine-global wrapper command;
without that predeployment the governed fleet update refuses before download or
mutation. It refreshes daemons only after
the staged binary passes its smoke, then runs the installed binary's resolver
probe at the exact configured mode/global directory before transaction commit.
During that first replacement only, the resolver also accepts the legacy
repository-routed wrapper's single newline-terminated `ghs_` token shape; it
accepts the token body as opaque ASCII within a hard size bound, and
rejects other prefixes, missing or extra lines, controls, and malformed output.
The accepted token is used only to fetch the exact frozen release assets before
the transaction installs and proves the current JSON wrapper.
The committed transaction streams the typed daemon-refresh receipt directly;
`INT` and `TERM` always remain nonzero, so a refreshed daemon without a complete
receipt cannot be reported as a successful fleet update.
The wrapper's mandatory strict 0600 non-symlink context preserves those runtime
arguments for direct calls; manual installs must provision the typed default
context. The current fleet creates and probes it for targets v0.134.0 and
newer. Probe failure rolls back helper, wrapper, context, CLI, and companion.
Older fleet-update releases v0.100.0-v0.130.x used the compatible four-target
transaction and nine-line journal without the unavailable probe; the current
client does not target them. It terminates any host attempt that exceeds
ten minutes. A minimal non-login PATH that hides
Homebrew is launch-environment drift, not evidence that Tart or Shipyard is
missing.

After changing `mutation_machine` or upgrading the release bot, regenerate the
post-tag workflow with `shipyard release-bot hook install`. Its tag extraction
must remain literal Bash, `tag="${GITHUB_REF#refs/tags/}"`; the double-braced
`${{GITHUB_REF#refs/tags/}}` form is not a valid GitHub expression. Because the
workflow checks out the release tag in detached-HEAD state, branch pushes must
first attach the deterministic local PR branch and then use Shipyard's
supervised push with the fully qualified `HEAD:refs/heads/<branch>` refspec.
The full refspec alone is insufficient in repositories whose pre-push hook
rejects detached HEAD or requires `SHIPYARD_PR_RUNNING=1`.
Generated consumer workflows install the exact generating CLI by default. A
repository whose post-tag hook runs while its own new release is still draft
may explicitly set `release.post_tag_hook.workflow_shipyard_version =
"latest"` to use the latest already-published CLI. Only exact stable
optional-`v` semver or `latest` is valid; invalid configuration must refuse
before overwriting the workflow, and an explicit CLI override has precedence.
Repositories that require signed bot commits should set
`release.post_tag_hook.ssh_signing_setup_script` to a safe repository-relative
helper path. Hook installation then regenerates the required secret-backed SSH
signing step instead of relying on edits to the owned workflow.

## Local/SSH VM Watch

Use `shipyard watch local` for long target-backed jobs that are not GitHub
Actions runs, such as following a build inside a Tart Linux VM:

```sh
shipyard watch local \
  --target linux-vm \
  --command './build-v8.py --target linux-x64 --seal --audit' \
  --milestone-regex '\[[0-9]+/[0-9]+\]' \
  --terminal-regex 'AUDIT FAIL|ld\.lld: error'
```

This mode supports `backend = "local"` and POSIX `backend = "ssh"` targets. It
streams target output, emits milestone lines for matching regexes, emits exactly
one terminal line on process exit or terminal-regex match, and exits with the
process status unless a terminal regex stops it early.

## Target Command Evidence

Use `shipyard run command` when a local or POSIX SSH target should run a single
workload-specific command, assert its exit code, pull declared artifact globs
back to the host, and store a typed evidence bundle that is separate from
merge-ready validation evidence:

```sh
shipyard run command \
  --target linux-vm \
  --name v8-linux-x64-seal \
  --expect-code 0 \
  --artifact 'build/linux-x64/lib/libv8.so' \
  --artifact 'logs/v8-audit.log' \
  -- bash -lc './build-v8.py --target linux-x64 --seal --audit'
```

Query the newest bundle with `shipyard evidence command --json` or list stored
bundles with `shipyard evidence command --list`. Artifact globs are relative to
the target working directory (`cwd` for local targets, `repo_path` for SSH
targets, or `--target-cwd` when overridden).

## Runner Metrics

Use `shipyard metrics` when an agent needs historical runner timing, queue, and
health context before recommending a routing, cache, or monitoring change. The
metrics store is optional and local to Shipyard state; projects do not need
tartci to participate. GitHub-hosted workflows, local commands, SSH targets, and
other VM managers can all record or import rows.

`shipyard run command` writes a best-effort metrics row alongside command
evidence. Import cloud and VM history explicitly when comparing local hardware
with GitHub-hosted runners:

```sh
shipyard metrics import github --repo Generous-Corp/pulp --limit 50 --json
tartci runtime export --repo Generous-Corp/pulp |
  shipyard metrics import tartci --json
shipyard metrics summary --project pulp --json
shipyard metrics scorecard --project pulp --since 30d --json
shipyard metrics watch --project pulp --since 14d --json
shipyard metrics advise --project pulp --json
shipyard metrics compare --project pulp --baseline github-hosted --candidate macstudio --json
```

The agent-facing commands return conservative JSON findings. Use `scorecard`
for a compact project-level view of outcomes, worker-minutes, queue time,
distinct-PR throughput, and cache reuse. Fields without durable source data are
explicitly `unavailable`; never reinterpret them as zero. Low sample counts are
a collection gap, not a regression. Prefer filing issues or changing profiles
only when `watch`, `advise`, or `compare` reports enough samples and a material
delta relative to that repo's baseline.

When fixing GitHub importer bugs, keep Actions list endpoints absolute
(`/repos/<owner>/<repo>/...`) and force `gh api -X GET` whenever `-f` supplies
query parameters. `gh api -f` defaults to POST, which can turn a valid list
endpoint into a misleading 404.

## Trusted Project Environment

For a non-secret machine path that every fresh worktree of one project needs,
prefer the trusted project-environment contract over a copied
`.shipyard.local` file. The tracked validation declares
`machine_environment = ["NAME"]` plus an exact `[project].repository` slug;
names must end in `_DIR`, `_FILE`, `_HOME`, `_PATH`, or `_ROOT`, and
secret-like names remain rejected even with one of those suffixes. Each host
supplies `[repository_environment."OWNER/REPO"] NAME = "/absolute/host/path"`
in the machine-global `config.toml` reported by `shipyard paths`. Shipyard
rejects missing, malformed, aliased, or case-confused identities before
execution, ignores tracked attempts to supply values, and snapshots resolved
values only under its protected machine state for daemon-owned work. Never use
this table for credentials or signing material.

## GitHub Auth And Quota

Shipyard's operational GitHub calls can be configured with `[github.auth]`.
Default behavior is ambient `gh` auth. Configured env or command-helper tokens
are injected only into child `gh` commands as `GH_TOKEN`; Shipyard must never
write raw tokens, GitHub App private keys, Keychain items, 1Password sessions,
or token caches to config, state, logs, or release artifacts.

`shipyard update` reads public release metadata, so it works with no auth — but
unauthenticated GitHub API calls are capped at 60/hr. As of v0.68.0 `update`
opportunistically authenticates: it uses `SHIPYARD_GITHUB_TOKEN` / `GH_TOKEN` /
`GITHUB_TOKEN` if set, else falls back to `gh auth token`, and threads that token
into both its own `releases/latest` query and the `install.sh` it invokes. No
token is ever required (the repo is public). If you see *"GitHub API rate limit
exceeded"* from `update`/`install.sh`, that is the 60/hr unauthenticated cap, not
a missing macOS `.dmg` — run `gh auth login` (or export `GITHUB_TOKEN`) and retry.

To raise the quota above the ambient `gh` user token's 5,000/hr, point
`[github.auth]` at a GitHub App installation token helper
(`scripts/shipyard-github-app-token`) for the 12,500/hr installation bucket.
Put the block in the **global** config dir (find it with `shipyard paths`;
macOS `~/Library/Application Support/shipyard/config.toml`) to cover every repo
on the machine — not the tracked project config. The same App private key works
across multiple Macs (M1/Studio/M5). Full setup, permissions, and the
additional-client steps: [`docs/github-app-quota.md`](../../docs/github-app-quota.md).

For multi-account App installations, `{repo_slug}` is the authority boundary:
the helper must look up that repository's installation and must not let a fixed
environment installation id override it. Shipyard partitions its in-memory
token cache by the fully expanded command, preserving separate repo/installation
entries. Keep the fleet's absolute `ghapp` wrapper as `token_command[0]` when
tartci policy pins it; use its reviewed `token --repo {repo_slug}` mode for
Shipyard while the audited `shipyard-v1` CLI surface remains available to
tartci. Remove the old fixed installation id. Missing repo provenance fails
closed. The privileged wrapper is not a
general native-`gh` drop-in, and unknown commands or flags fail before minting.
The one bounded asset-publication exception is `ghapp release upload`: require
an explicit exact repository, tag, and existing non-symlink regular files. It
does not admit `--clobber`, release creation/deletion, or tag retargeting.
The wrapper snapshots each operand through no-follow descriptors into a private
staging directory before token mint; native `gh` must never reopen the original
workspace path.
The wrapper/helper must resolve trusted Python, `openssl`, and native `gh` through
absolute paths, preserve the installed merge/queue/close guards, and never put
the token in process argv. Guards that query GitHub must receive the same
repo-routed token and exact native binary as the guarded command and still run
before native `gh`; the PR-close guard inspects every command shape itself.

The standalone App helper must remain usable from stripped SSH/daemon
environments without weakening TLS. It first uses Python's default trust store.
Only after a real certificate-verification failure and proof that there is no
explicit, loaded, or on-disk default CA source may it augment that same context
with known platform CA files. Never broaden pinned, directory-backed, or private
enterprise roots. It fails closed when neither source works. Do not disable
certificate verification; set `SSL_CERT_FILE`, repair that interpreter, or use
the helper's verified platform fallback.

When debugging GitHub behavior:

- Run `shipyard doctor --rate-limit --json` to see the effective auth source
  and REST/GraphQL buckets. This actively resolves configured auth, so command
  helpers may run and GitHub App helpers may mint installation tokens.
- When checkout remotes are ambiguous, pass `--repo OWNER/REPO` to
  `shipyard doctor --rate-limit` or `shipyard auth doctor`. The canonical slug
  is an exact token-helper repository override; malformed slugs fail closed,
  and omitting it preserves the default ambiguity refusal.
- Optional-provider rows stay green when unused: `nsc` reads "not configured
  (optional)" unless a Namespace provider is configured, and a `{repo_slug}`
  `token_command` that can't resolve in a repo-less context (doctor) is
  green with a "pin `--repo`" hint rather than a red "misconfigured". The
  **daemon** no longer hits this: webhook registration passes the served
  `--repo` as a `{repo_slug}` hint (`GhClient::with_repo_hint`), so a
  `{repo_slug}` `token_command` mints a token from the daemon's repo-less CWD
  instead of looping on "placeholder requires remote.origin.url" with live mode
  stuck on "updates paused". The
  `gh-scope` row is also green-informational for App/Env/helper tokens (scopes
  not inspectable locally), keeping the "verify Actions: Read/write" reminder.
- Check `.shipyard/config.toml`, `.shipyard.local/config.toml`, and global
  config for `[github.auth]` before assuming ambient `gh auth status` explains
  the operation.
- Treat GitHub App installation tokens and fine-grained tokens as permissions
  that may not be locally inspectable through `gh auth status`; verify App or
  token permissions in GitHub when cloud retarget/handoff needs Actions: Read
  and write.
- Keep workflow and runner permissions distinct. `Actions: Read-only` covers
  workflow/run/job inspection; repository `/actions/runners` inventory needs
  `Administration: Read-only`, and organization runner-group verification
  separately needs organization `Self-hosted runners: Read-only`. Add write
  access only for explicit cancel/dispatch, registration mint/reclaim, or group
  configuration operations. An unreadable runner inventory is unknown/defer,
  never proof that capacity is idle or safe to reset.
- Keep `RELEASE_BOT_TOKEN` separate. `shipyard release-bot setup/status` are
  operator actions and intentionally use ambient `gh` auth.
- Keep high-volume GitHub inspection on the configured Shipyard auth source.
  Ambient auth is permitted only for documented low-volume mutations after an
  exact App integration-permission denial (PR creation after both GraphQL and
  REST fail, and steward handoff writes). Shipyard removes both GitHub token
  variables and selects a direct native `gh`, skipping script/wrapper shims.
  Set `github.auth.ambient_gh_binary` to an absolute native GitHub CLI path when
  PATH discovery is not appropriate; never point it at a `ghapp` wrapper.
- Mac-to-Mac portability is config-only. Reprovision env vars, Keychain items,
  1Password sign-in, or App private keys outside Shipyard on the destination
  Mac.
- Use `shipyard auth export` and `shipyard auth import --scope local` only for
  sanitized config movement. The bundle must not contain tokens, private keys,
  Keychain exports, 1Password sessions, queue state, daemon sockets, or token
  caches. Export/import preserves the complete typed GitHub auth table,
  including absolute `ambient_gh_binary`, `privileged_gh_binary`, and
  `privileged_git_binary` authority. Import replaces that auth table while
  preserving unrelated configuration, so review the bundle before applying it
  at machine-global scope.

## Drift And Parity

Run drift checks whenever Python Shipyard may have changed:

```sh
python3 scripts/update_drift_tracker.py
```

Only advance the baseline with `--mark-reviewed` after the new upstream changes
have been audited and reflected in Rust or explicitly risk-accepted.

Compare command surfaces safely:

```sh
python3 scripts/compare_cli_surface.py \
  --python-bin /Users/danielraffel/Code/shipyard/.venv/bin/shipyard \
  --rust-bin target/release/shipyard \
  --allow-rust-only paths
```

Run the finish-line credential gate before signing or release claims:

```sh
python3 scripts/finish_line_status.py \
  --env-file /Users/danielraffel/Code/PlunderTube/.env \
  --json
```

## Runner Watchdog (self-hosted runner recovery)

Shipyard ships a `runner` subcommand family for detecting and recovering from
stuck self-hosted GitHub Actions runner state. Built after the 2026-05-12
incident where a UBSan job from a closed branch wedged Pulp's local runner for
>75 min while 17 stale queued runs piled up behind it, blocking PR #1859 for
hours.

### When to reach for it

- Runner reports `busy=true` to GitHub but no Worker process running locally
- Worker process running >90 min on a job that should take ~20-30 min
- Queue depth growing while runner appears stalled
- Stale queued runs from closed/rebased branches monopolizing the runner

### Safe commands (read-only or advisory)

- `shipyard runner status` — one-shot health check, exit 0/1/2, `--json` supported
- `shipyard runner cleanup --dry-run` — list stale queued runs without cancelling
- `shipyard runner watch` — advisory daemon mode, polls every 5 min
- `shipyard runner zero-job-recover --pr <n> --source-run-id <id>` — exact
  Pulp zero-job selector for a REST `queued` or `pending` run, dry-run by
  default. It is not the stale-run reaper or merge-steward coalescing path.

### Mutating commands (require explicit flags)

- `shipyard runner cleanup --fix` — cancel stale queued PR/merge-group runs;
  release, push, schedule, tag, and dispatch runs are protected
- `shipyard runner watch --fix` — auto-recovery loop (cron-friendly)
- `shipyard runner zero-job-recover --pr <n> --source-run-id <id> --apply` —
  persist one remote at-most-once receipt, re-read every selector fact, then
  dispatch only protected-main `build-macos.yml`. It never cancels, reruns,
  enqueues, labels, pushes, merges, coalesces, or changes TartCI state. Any
  receipt on the exact head suppresses every later attempt; a lost dispatch
  response is terminal for that head. Apply is authorized only from Pulp's
  live serialized protected-main `Shipyard merge steward` workflow; its exact
  workflow ref, event, SHA, run, and attempt are re-read from GitHub and bound
  with the candidate fingerprint in the remote receipt.
- `shipyard runner admission-clean --repo <owner/repo> --base main --labels self-hosted,<exact-labels> --apply --json` — TartCI's last pre-JIT correctness gate. It emits a flat schema-v1 verdict with exit 0 `admit`, 3 `defer`, 1 operational error, or 2 invalid configuration. It inspects only managed PR/merge-group runs: a compatible queued job inside a superseded queued workflow may authorize cancellation, while one inside an `in_progress` workflow blocks admission but is never cancelled. Every GitHub call made while holding the exact-key observation lock shares one 120-second absolute budget. Observation or stewardship contention returns typed `observation_in_progress` or `stewardship_in_progress` defer; a timeout or other non-contention observation or lock failure releases the lock and returns a typed error with bounded underlying detail. A non-authority machine returns `mutation_authority_required` instead of mutating. TartCI must treat every result except typed `admit` as fail-closed and discard the still-unregistered VM.
- `shipyard runner kill --pid X --reason "..."` — kill a specific Worker; requires typed `KILL` confirmation

### `runner kill` recovery sequence

10 steps, all reversible. **Nothing is destroyed.**

1. Snapshot kill event to `~/.shipyard/kill-recovery.jsonl`
2. Typed `KILL` confirmation (skip with `--yes`)
3. SIGTERM with 10s grace (configurable via `--grace-secs`)
4. SIGKILL only if still alive
5. Reap orphaned children (`cmake|ninja|make|ctest|build`)
6. **Move** (not delete) partial `build*` dirs to `/tmp/shipyard-killed-builds/<event-id>/`
7. Verify `Runner.Listener` health via `pgrep`
8. Poll GitHub for status flip to `completed`/`failure`
9. Optional `--retrigger` re-queues the killed PR's CI
10. Print recovery summary with `--recover` invocation hint

A misclick costs ~2 min of cmake re-configure. To recover:
`shipyard runner kill --recover <event-id>` walks the quarantined `build*` dir
back to `_work/<repo>/` and re-queues the killed run.

### Gotchas

- The watchdog's `busy=true but no Worker process` check has a brief 1-5 min
  false-positive window after `cleanup --fix` cancels a run — the runner needs
  time to gracefully exit. Don't double-cancel.
- `runner kill --pid` REFUSES non-Runner.Worker PIDs as a safety check. Override
  via `--runner-dir` only if your install path is non-standard.
- The `concurrency: cancel-in-progress: true` workflow setting SHOULD auto-cancel
  on force-push but doesn't always (Pulp issue #1884). The watchdog's stale-queue
  detection catches the consequences.

### Config

Per-machine overrides in `.shipyard.local/config.toml`:

```toml
[runner.watchdog]
runner_id = 1763
runner_dir = "/Users/me/actions-runner"
max_job_min = 90
max_queue_age_hours = 2
watch_interval_seconds = 300
auto_fix = false
# Stale-run reaper thresholds (minutes) for `runner watch --reap-stale-runs`:
reap_in_progress_max_min = 300
reap_queued_max_min = 480
```

## Host-Health Pre-Dispatch Gate (optional)

For self-hosted runners *co-located with heavy interactive work*: read a shared
`host_vitals` signal during ship/run preflight and surface — or, opt-in,
hard-stop on — a saturated host before a ship runs into a jetsam/reboot failure
that reds the required gate for an *infra* reason.

**Off by default, fails open.** No `[host_health]` block or no signal file → no
read, no change. Missing/unreadable signal → treated as "no opinion", the ship
proceeds. (Inverse of backend-reachability preflight, which fails closed:
reachability gates correctness, host-health gates only crash-avoidance.)

```toml
[host_health]
gate = true                     # master opt-in (pre-dispatch gate)
block_on_critical = false       # true → `critical` hard-stops preflight (exit 4); default warns
classify_local_failures = false # true → relabel a local TEST failure as INFRA when a host
                                #        jetsam/WindowServer crash overlapped the leg window
# file = "/custom/host_vitals.json"   # default: ~/.local/state/pulp/host_vitals.json
```

`classify_local_failures` is an independent opt-in: it relabels a **local** leg's
`TEST` failure to `INFRA` (with an honest note) when the signal shows a host
incident during the leg. Conservative — only `TEST` is eligible, only local legs,
and it's a pure label (never flips `TargetStatus`, so a failed leg still blocks
merge). Fails open. Full behavior: `docs/local-mac-pool.md` § Infra-vs-code
failure labelling.

Signal contract: JSON with numeric `code` (0/10/20) and/or string `level`
(green/warn/critical) + optional `reason`; `code` wins. Shipyard ships no
producer — bring your own (Pulp's `tools/scripts/host_vitals.sh` + its launchd
sensor writes the default path). `SHIPYARD_HOST_VITALS_FILE` overrides the path.
Behavior when `gate=true`: green/absent → silent; warn → warn + proceed;
critical → warn + proceed, or fail (exit 4) when `block_on_critical`. Full table:
`docs/local-mac-pool.md` § Host-Health Pre-Dispatch Gate.

**Same-backend transient retry (`[ship] transient_local_retries`).** Off by
default (`0`, clamped `0..=2`). When set, a **local** leg that fails with a
transient `INFRA` blip is re-run once (or up to the bound) on the same backend
before the failure is recorded. Deliberately stricter than the global
`is_retryable` taxonomy: **`INFRA` only** — a local `TIMEOUT` would just re-burn
its wall-clock budget, and `CONTRACT`/`TEST`/`TREE_DRIFT` are authoritative.
Remote legs already have next-backend failover, so same-leg retry is local-only.
Each retry uses a distinct `<log>.retry<N>` path (attempt-0 evidence preserved);
a recovered leg is noted in `phase`, an exhausted one in its error message. With
the default `0`, execution is byte-identical to no retry. Composes with
`classify_local_failures` (a relabelled-`INFRA` failure becomes retry-eligible).
Full behavior: `docs/local-mac-pool.md` § Same-backend transient retry.

## Durable Queue: daemon-owned execution

The detached daemon must own a trusted temporary root independent of the
launching shell. `shipyard daemon start` creates a real owner-private
`<state>/daemon/tmp` directory, rejects a symlink at that path, sets mode 0700
on Unix, and exports it as `TMPDIR` before detaching. Preserve that invariant:
launchd and minimal SSH environments may omit `TMPDIR`, while macOS `/tmp` is a
symlink that hardened consumers correctly reject under `O_NOFOLLOW`. A shell
profile export is not a durable fix for daemon-owned work.

Self-update crosses a process boundary before daemon refresh: after verifying
the exact installed version, the old updater invokes that installed binary's
`daemon refresh` with explicit mode/global/state paths and requires its typed
refresh receipt. This ensures a newly shipped daemon-spawn invariant (including
the private `TMPDIR`) takes effect during the same rollout even when the command
began in an older Shipyard process.

The daemon keeps that protected `TMPDIR` for its own runtime files. A local
validation subprocess that merely inherited it receives a separate
owner-private ephemeral root outside the production writer domain, so test
fixtures cannot become protected writes accidentally. Do not relax the writer
domain predicate to accommodate fixtures, and do not replace an explicitly
configured trusted validation `TMPDIR`.

When local validation is required, normal `shipyard run`, `shipyard ship`, and
`shipyard pr` submissions persist their resolved request and exact
checkout/configuration provenance, ensure the matching-version daemon is live,
and return after the daemon accepts durable ownership. (`shipyard pr` may
instead complete an enabled terminal steward handoff before local validation.)
Ending the submitting terminal, agent session, or model allocation must not
terminate the validation worker. Use `--foreground` only for explicit
interactive debugging where terminal lifetime should own execution.
This first replacement is deliberately Unix-only (macOS and Unix hosts) and
single-worker. On Windows and other platforms where the Unix process-group
contract is unavailable, these commands retain foreground execution instead of
pretending the job has durable daemon ownership. Parallel proof, sharding, and
multi-worker admission are separate work and are not implied by this feature.

The separate artifact transport proof is also default-off. It validates typed
manifests, exact source/toolchain/cache generations, authenticated chunk-prefix
resume with opaque manifest/session-bound plans, space watermarks, same-root
atomic publication, and safe exact-layout `tar.zst` extraction. Archive
consumption rejects traversal, links, duplicates, undeclared/missing members,
case-insensitive/Windows path aliases, unbounded decoder or layout metadata,
and type/mode/size/digest drift before atomically exposing an extracted tree.
Extraction reserves allocation overhead, rechecks live space around every
directory/file allocation, and reports a distinct published-but-parent-sync-
pending outcome after the atomic commit point; never blindly retry that outcome
as though no destination exists.
The proof does not select a host or dispatch a shard. A roaming/offline worker
is additive only and must be excluded or reassigned without blocking the
minimum completion set.
The `parallel_proof_canary_receipt` schema records exact proof/artifact,
exact admitted host observations/session generations, route, transfer/resume/
object-reuse bytes, exact cache-generation use, worker-minutes, wall-time evidence,
and a separately validated same-proof control-receipt digest. It requires zero model calls,
reports the measured speed/overhead gate, and never authorizes a merge. A
receipt digest alone is not durable publication. Use the default-off
`parallel_proof_canary_driver` to run the control strictly before distributed
work, recheck exact authenticated M3/M1 session and storage fences, bind actual
transport/resume counters and prefix digests, force avoided-cache-byte claims
and model calls to zero, and publish exact bytes through its crash-durable
immutable evidence store. Retain the complete fence observations, and durably
record distributed-started before mutation plus terminal failure on every
post-start error; never retry an unreconciled correlation. No production host adapter exists yet: do not use
ad-hoc SSH/rsync or treat policy enablement as authority to mutate hosts.
Follow [`docs/artifact-transport.md`](../../docs/artifact-transport.md) before
integrating it with a scheduler.

Each worker runs in a separate process group with an unpredictable generation
receipt. A restarted daemon adopts only a live process whose PID, job id, and
generation all match the durable receipt. A `Running` job without that exact
live identity becomes terminal `UNCERTAIN`; Shipyard never blindly replays work
that may already have produced side effects. Pending work is replay-safe only
when its canonical cwd, repository root/origin, HEAD, tree signature, and
resolved layered configuration still match. Legacy or malformed pending
requests are cancelled with an explicit reason and cannot block unrelated
valid jobs. `shipyard cancel` terminates the complete daemon-owned process
group, including descendants. A terminal worker receipt continues to reserve
the single execution slot until process-group death is proven; a failed kill
cannot release capacity into overlapping side effects.

The daemon also observes queued and running ship PRs. It cancels only when the
authoritative PR state is merged and GitHub reports the exact head SHA stored in
the durable request. For running work, the authenticated repository/PR/head
proof drives a durable, restart-idempotent termination transaction: freeze the
recursive process tree, prove it dead, then release leases and publish the typed
terminal outcome. Deferred work returns to pending only after the same proof.
Worker-receipt cleanup is generation-CAS fenced by job id, worker generation,
and root PID, so recovery cannot delete a replacement worker's receipt. Open
PRs, a different merged head, auth failure, timeout, legacy reason strings, and
malformed or ambiguous responses are no-ops. Terminal queue records are
immutable against stale worker progress, and a later daemon tick repairs a
missing typed outcome from the winning terminal record.

Daemon-owned GitHub work requires `[github.auth] source = "command"`, so an
existing daemon can refresh credentials independently of the submitting shell;
`env` and ambient interactive `gh` auth are intentionally rejected. A
running daemon from another Shipyard version must be refreshed before a new
job is persisted. Adding a new repository to a same-version daemon refreshes
its registration set while exact live workers remain independently owned.

## Legacy Queue Recovery: killed-worker stale-running reaping

A `shipyard ship` / `shipyard pr` worker that is killed (SIGTERM, crash,
`kill <pid>`) leaves its job `status: running` in the durable queue
(`queue.json`). Before v0.68.0 this wedged the PR: every later same-PR ship was
refused with `SamePrShipRunning`, and there was no clean way out — `shipyard
cancel <id>` only handled pending jobs, `shipyard ship-state discard <pr>` left
the queue job intact, and the startup reaper
(`recover_stale_running_jobs_for_drain`) only fires on daemon restart, so a
long-lived daemon never recovered. The only fix was hand-editing `queue.json`.

For foreground and pre-daemon-owned jobs, the v0.68.0 queue recovery remains:
a `Running` job whose freshest heartbeat
is older than `DEFAULT_RUNNING_JOB_STALE_SECONDS` (180s) is treated as a dead
worker and reaped to `Cancelled` — at ship-submit time
(`refuse_same_pr_running_ship` reaps the stale job, then proceeds) and on every
drain admission pass (`apply_admit_pass_for_drain`). The reap re-checks
staleness under the queue lock, so a worker merely between heartbeats is never
killed; a "stale" job that revived between plan and apply defers conflicting
starts to the next pass rather than double-running the PR.

Pending and running ship jobs whose PR merged are cancelled only when GitHub
reports the same exact head SHA recorded in the durable request. The observer
batches by `(repository, PR)`, reuses one result for duplicate jobs, and
throttles re-observation for 30 seconds. GitHub reads must use the effective
`GhClient`, an explicit `--repo`, and the shared 15-second credential-plus-child
budget; auth, timeout, malformed JSON, and head drift all fail closed.

### Gotchas

- Recovery is heartbeat-age based, so a retry waits up to ~180s after the
  worker dies before it goes through. That is intentional — it must not reap a
  slow-but-live worker. Don't shorten it below the ~15s heartbeat interval's
  safety margin.
- Streamed output silence is not a dead-worker signal. A live subprocess may
  remain output-quiet well past 90 seconds while its durable heartbeat stays
  fresh; report that state as `quiet` and never promote it to `stuck`, infra,
  or fallback eligibility without a stale heartbeat or dead ownership proof.
- Do NOT launch a second `shipyard pr` for the same PR while the first is still
  alive. That is what strands a `running` job in the first place — one ship per
  PR at a time.
- On a pre-0.68.0 binary the manual recovery is still: `shipyard ship-state
  discard <pr>`, then mark the stuck `queue.json` job terminal (or restart the
  daemon to trigger startup recovery).

## Orphaned ship-state reporting

The queue reaper above recovers the *queue* `Job` when a worker dies. The
durable *ship-state* (`<state>/ship/<pr>.json`) is a separate store with no
orphan lifecycle: when the owning process dies mid-validation (host reboot from
a jetsam kill, daemon crash, `cmux` relaunch), the ship-state freezes in an
in-flight verdict forever. `ship_terminal_verdict` returns `None`, so auto-merge
reports `InFlight` and refuses to merge — the PR silently stalls with no signal.

Both `shipyard ship-state list` and `shipyard status` flag these. A state is
reported orphaned when it is still in flight (the same `ship_terminal_verdict`
predicate the auto-merge gate uses, so the two never drift) **and** the queue
confirms — or fails to disprove — a dead worker. The signal is **source-aware**,
established by cross-referencing a single queue snapshot (module
`src/ship_liveness.rs`), strongest to weakest:

| Evidence | Meaning | When flagged |
|----------|---------|--------------|
| `queue_stale` | matching *running* job whose heartbeat is dead past the reaper's 180s window (`is_stale_running`) — a provably gone worker | immediately (no time gate) |
| `queue_terminal` | matching job already terminal while the ship-state never finalized | immediately |
| `queue_absent` | queue consulted, no matching job (ship-state stores no job id → absence is *inferred*) | time-gated |
| `time_fallback` | queue unavailable — pure `updated_at` staleness | time-gated |

A live worker (running with a fresh heartbeat) or a *pending* (queued,
not-yet-started) job is never flagged, however old `updated_at` looks. The
`queue_stale`/`queue_terminal` signals surface a genuinely dead ship in ~3
minutes; the weak (`queue_absent`/`time_fallback`) signals require the staleness
threshold. Human output prints `ORPHANED? [<evidence>]: ...`; JSON gains
`orphaned: [{pr, stalled_minutes, evidence}]` (`ship-state list`) /
`orphaned_ship_states: [...]` (`status`).

The threshold defaults to 45 minutes and is configurable:

```toml
[ship_state]
orphan_stale_minutes = 45    # clamped to [1, 525600]
auto_resume = false          # opt-in daemon abandon sweep (default off)
```

Detection (`shipyard ship-state list` / `status`) is **report-only** — it never
mutates anything and cannot affect merge readiness (a flagged state is in flight,
which auto-merge already refuses; the harm it surfaces is the *inverse* — a PR
that silently never merges).

**Opt-in abandon sweep (`auto_resume`, default off).** When enabled, the daemon's
periodic reconcile pass runs `ship_resume::sweep_orphaned_ship_states`: for a
strongly-orphaned in-flight state it sets a terminal `abandoned` marker
(`ship_terminal_verdict` → `Some(false)`), so the wait/auto-merge path stops
blocking and a human re-ships. Deliberately conservative — marking a *live* ship
failed is the one catastrophic error, so it abandons **only** on `queue_stale`
evidence (a provably dead owning worker) and fails **closed** (an unavailable
queue never abandons). The sweep snapshot only *selects candidates*; the abandon
decision is re-made per PR under the per-PR lock, re-checking in-flight status
**and re-reading the queue live** — never the sweep snapshot — so a worker that
starts or resumes during the sweep (e.g. an operator re-shipping the moment they
see the orphan) shows live and is spared. It never auto-re-dispatches (no
resume→die→resume loop), and abandonment is idempotent (an abandoned state is
terminal). Emits a `ship_state_abandoned` daemon IPC event. With the default
`false`, the sweep opens no queue and mutates nothing.

**Recovery clears the marker.** Re-shipping an abandoned PR (`shipyard ship
<pr>`) clears `abandoned` when the ship execution begins — on both the
reuse-existing-state and the archive-and-replace fresh-attempt paths — so the
re-validated PR is no longer short-circuited to failure. `shipyard ship-state
discard <pr>` remains the manual alternative. The config load is scoped to the
daemon's own runtime mode, so an isolated daemon reads its own overlay.

### Gotchas

- Only the strong signals (`queue_stale`/`queue_terminal`) are proof of a dead
  worker; the weak ones are staleness heuristics. The time threshold only gates
  the weak signals, so don't shorten it toward normal leg durations — a false
  positive is harmless (it just invites an operator to look; it cannot merge or
  cancel anything), but the weak signals are the noisy ones.
- Classification is lazy and read-only: computed at `ship-state list` / `status`
  time from one queue snapshot, with no daemon startup sweep and no write-back.
  The module opens the queue/ship stores only when they already exist, so
  running a diagnostic in a fresh directory materializes nothing.
- A state stops being reported the moment a live worker touches it or it reaches
  a verdict.

## Runner Provisioning (register / list / remove / tag)

The `runner` family also *provisions* self-hosted GitHub Actions runners, not
just recovers them. This is the generic, repo-agnostic path for bringing a Mac
into a repo's CI fleet — used to stand up the Mac Studio's pulp runners. Pure
naming/index/label/table logic lives in `src/runner_provision.rs`; the shell
side (gh, `config.sh`, `svc.sh`, local `~/actions-runner-*` dirs) is
`src/app/runner_provision_cmd.rs`. See `docs/runner-provisioning.md`.

Pinned runner upgrades are fail-closed: never automatically stop or upgrade a
service-installed runner. Retain it unchanged when it already matches the pin;
otherwise report it deferred with exit 3 and leave its job eligibility
unchanged. Upgrade a configured service-less runner only after a
fresh GitHub observation proves it offline and idle, then repeat that check at
the final rename boundary after staging. Clone-stage and verify the replacement
before activation, keep the intact original until the new service starts, and
use the runner's compound `svc.sh uninstall` to stop/remove a partially started
replacement before restoring the original directory. Never re-run fresh
`config.sh` over a configured upgrade.
Toolchain readiness checks are silent probes; never let their stdout/stderr
contaminate normal or JSON output.

### Machine tag (load-bearing for multi-Mac fleets)

Runners are named `<repo>-<machine-tag>-NN` (e.g. `pulp-studio-01`). The tag is
an explicit per-box value stored at `<state_dir>/machine-tag`, **never derived
from the hostname** — two MacBook Pros can share a hostname, so a
hostname-derived tag would collide. Set it once per machine:

```bash
shipyard runner tag --set studio   # or m1, m5, …
shipyard runner tag                # prints the stored tag
```

### Register

```bash
# Host must already have the toolchain/caches (repo-specific bootstrap).
# This step only registers runners and points their .env at the shared caches.
shipyard runner register --repo Generous-Corp/pulp --count 3 \
  --ci-root /Volumes/Workshop/ci/pulp [--dry-run]
```

- Names continue from the highest existing `<repo>-<tag>-NN` (any machine), so
  re-running appends exactly `--count` capacity without collisions. Existing
  configured runners are reconciled separately. Reserve every existing
  runner's unchanged `.env` allocation, including other repos and old tags on
  the same host, before dividing remaining cores across
  additive runners; fail closed rather than let a late activation overcommit
  the host.
- Default labels: `self-hosted,macos,arm64,<repo>-build,<repo>-build-<tag>`.
  `<repo>-build` is what a repo's workflow selects for normal routing;
  `<repo>-build-<tag>` pins work to one machine. Override with `--labels`.
- Per-runner `_work` is `<ci-root>/work/<name>`; the `.env` points ccache and
  FetchContent at `<ci-root>/cache/*`. Cache *size* is owned by the host's
  `ccache.conf`, not this command.
- Runner registration is deliberately fleet-pinned and uses `--disableupdate`.
  Its `.path` is system-first so `/usr/bin/tar` resolves before Homebrew, and
  Rust lives under that runner's own `_toolcache/{rustup,cargo}` on local disk.
  Never point those homes through a symlink to a shared/external build volume;
  an offline filesystem can otherwise wedge `Runner.Worker` inside native
  `open` before Shipyard receives a useful failure.
- Existing-runner reconciliation parses `.runner` and requires its `agentName`
  and repository URL to match the requested runner before any mutation.
  Service-installed runners are never auto-stopped or upgraded; matching pins
  are retained, while outdated services are deferred unchanged (exit 3). A service-less runner is eligible only after a
  fresh GitHub observation proves it offline and idle, with a second check at
  the final rename boundary after staging. Eligible upgrades clone-stage and
  verify the full replacement before activation, retain the intact original
  until the replacement service starts, and uninstall failed service setup
  before rollback. Never activate a partial extraction.
  If an existing runner becomes busy, online, or service-managed during
  staging, defer that reconciliation and continue the additive registrations.

### List and remove

```bash
shipyard runner list --repo Generous-Corp/pulp   # live pool, grouped by machine
shipyard runner remove --name pulp-studio-03 --yes [--purge-dir]
```

Removal uses the runner's compound `svc.sh uninstall` before GitHub
deregistration. Do not replace it with `svc.sh stop`: stopping alone preserves
the LaunchAgent plist and leaves a stale `runsvc.sh` Background Item after the
runner directory disappears. If uninstall fails, removal fails closed before
requesting or consuming a GitHub removal token.

`list` aggregates across machines straight from GitHub (no controller needed)
and reconciles local `~/actions-runner-*` dirs against GitHub to flag orphans.

### Audit (host-class drift)

```bash
shipyard runner audit --repo danielraffel/Shipyard   # exit 1 on any drift
```

`audit` checks every runner against the host-class scheme — a conforming
`<repo>-<class>-NN` name, the shared `<repo>-build` routing label, the
`<repo>-build-<class>` pin label, and agreement between the class in the name
and the class in the labels. It flags non-conforming names (e.g. a hand-named
`daniels-macbook-shipyard`) and missing labels (e.g. a runner registered with a
bespoke `--labels` that dropped `<repo>-build-<class>`), exiting non-zero so CI
or a cron can gate on a clean fleet. For configured runners on the current
machine, it also requires `.path` to match Shipyard's generated system-first
value exactly. This rejects interactive/session PATH capture before a missing
or ephemeral executable directory can wedge `Runner.Worker`. New and
service-less registration runs `config.sh` under that canonical PATH and
rewrites `.path` before service start. A service-installed runner remains
fail-closed until it is drained, stopped/uninstalled with its own `svc.sh`, and
reconciled by `runner register`; Shipyard never stops a live runner implicitly.
This is the foundation for M5 joining
by class with zero bespoke setup. Pure naming/label logic; physically
confirming a `*-studio-*` runner is on the Studio is `runner capacity`'s job
(reads the host machine tag over SSH). Full design:
`planning/2026-06-01-multi-mac-controller.md` (Shipyard #316).

### Capacity (VM-slot accounting)

```bash
shipyard runner capacity --json   # exit 1 if any host unreadable
```

macOS caps **2 running VMs per host** (XNU kernel quota; Pulp plan Appendix D).
`runner capacity` reads each `[host_class.<name>]`'s running Tart VMs (locally
for the controller's own box, over SSH otherwise), enriches each running VM with
`tart get <name> --format json`, and counts only macOS/darwin VMs as consuming
the macOS quota. Set `tart_home` when launchd supervisors use a non-default Tart
store; the probe then runs with `TART_HOME=<absolute-path>` and reads the same
store. Linux/Windows Tart VMs do not reduce this free-slot count.
**Fail-closed:** an unreadable host or VM OS counts the host as 0 free and the
command exits non-zero — a silent host must never read as spare capacity.
Configure host classes (operator-specific, so keep these in
`~/.config/shipyard/config.toml` or `.shipyard.local/`, not the committed repo
config):

```toml
[host_class.studio]
# ssh omitted → the controller's own box, read locally
cap = 2                                    # Studio may raise via Appendix-D override
tart_bin = "/opt/homebrew/bin/tart"        # if tart isn't on the SSH PATH
tartci_bin = "/Users/ci/.local/bin/tartci" # for fleet-status doctor probes
tart_home = "/Users/ci/VMs"                # absolute path; no shell/tilde expansion
labels = ["self-hosted", "macos", "arm64", "shipyard-build-studio"]

[host_class.m1]
ssh = "m1-ci.local"
cap = 2
tart_bin = "/opt/homebrew/bin/tart"
tartci_bin = "/Users/ci/.local/bin/tartci"
tart_home = "/Users/ci/VMs"

# [host_class.m5] arrives later — same shape, inherits cap = 2.
```

This free-slot count is what the cloud→local reroute watcher (#316 Part C)
gates on: drain a still-queued cloud macOS job to local only when `free > 0`.

Use `shipyard runner fleet-status --repo <owner/repo> --target macos --json`
for the operator view that answers "can queued jobs actually drain?" It combines
capacity with host-local `tartci doctor --reap --json`, supervisor heartbeat
freshness, Tart storage admission headroom, ccache size versus its configured
maximum, the repository's complete registered-runner inventory, per-host
routability, and oldest queued macOS age. Registered-runner labels make metal
pools such as MacPro Linux and Mac Mini Intel visible without adding them as
Tart host classes. Declare machines that should exist even before registration
under `[runner.fleet.expected_host.<name>]`; matching is an extensible,
case-insensitive required-label subset rather than hard-coded machine names:

```toml
[runner.fleet.expected_host.macpro]
labels = ["self-hosted", "Linux", "X64", "pulp-host-macpro"]
min_online = 2

[runner.fleet.expected_host.macmini]
labels = ["self-hosted", "macOS", "X64", "pulp-host-macmini"]

[runner.fleet.expected_host.macbook_air]
active = false # planned inventory: visible, but does not alert yet
labels = ["self-hosted", "Linux", "ARM64", "pulp-host-macbook-air"]
```

Active expected hosts default to `min_online = 1`; absent or offline matches are
reported as `expected_host_unavailable`. Inactive hosts remain visible without
making the command unhealthy. It is read-only and exits non-zero when a host is
unreadable/unhealthy, a merge-group Linux build requests `ubuntu-latest` while
an online idle self-hosted Linux x64 runner exists, or queued macOS work is older
than `--queued-age-threshold-secs` while routable capacity exists. Use
`--queue-run-limit N` to keep live debugging snappy on a large queued backlog.
That limit does not cover merge-queue, enrollment, or release calls, so every
GitHub read in one fleet tick also shares a 30-second absolute deadline. An
expired tick renders a fail-closed partial assessment; do not wrap the command
in a longer retry loop or treat missing observations as idle capacity.

The report retains optional workflows, finds queued jobs inside `in_progress`
workflows, and compares exact merge-group SHAs. A tick spends at most two
active-run list requests plus 50 per-run job requests; larger observations
fail visibly with `OBSERVATION_TRUNCATED` instead of exhausting the monitor's
own API budget. Its local snapshot detects cleared auto-merge enrollment with
a separate 25-PR reconciliation cap and the same truncation signal.
Release staleness uses the oldest releasable commit when available and
conservatively falls back to the release publication age when a bounded commit
scan proves releasable work exists but cannot recover that timestamp.
Consume the stable reason codes rather than chat-turn counts. With host classes
configured, `runner watch` invokes this observer by default. This path never
uses Orchard and never mutates GitHub.
The watcher resolves the repository default branch; use `--fleet-base` or
`runner.watchdog.fleet_base` for a different merge target.

Use `shipyard runner steward` for agent-neutral merge-on-green reconciliation.
It observes by default; `--apply` enables exact-head guarded queue admission,
reruns, and narrowly policy-scoped capacity-preemption mutations. Client-side
REST direct merge is refused because GitHub cannot atomically bind both complete
check materialization and the validated base revision; use a server-owned merge
queue or manual merge instead. Apply mode is restricted by central mutation
authority, durable write-ahead intent, and live revalidation immediately before
every GitHub write. Transient retry limits are also fenced against GitHub's
durable `run_attempt`, so losing a controller cache after an accepted rerun
cannot reset the budget. The built-in capacity preemption preset applies only to
explicitly advisory Pulp workflows; required workflows and unknown repositories
are disabled because GitHub cannot bind a cancellation to an atomic job-state
snapshot.

The case-insensitive `5·unresolved` label is a fail-closed provenance blocker
by default. A matching current PR reports `provenance_blocked` and receives no
mutation. Repeat `--provenance-blocking-label <label>` for another repository
vocabulary; live revalidation must observe the blocker absent before authority
returns. This decision precedes opt-out, and both same-process and restarted
force-cancel terminalization re-read the current PR's blocker, opt-out, and
exact-head management authority before making the final POST.

Stewardship is opt-in per immutable head. Prefer making the receipt atomic with
PR creation: set `[merge_steward].auto_handoff = true` on the protected base
branch and run `shipyard pr`, optionally adding `--workstream-id ID
--context-url URL --launch-profile PRIVATE_JSON`. Shipyard never trusts the PR
branch to enable that default.
Immediately after the PR exists and before validation starts, Shipyard writes
the receipt; without explicit values it uses `OWNER/REPO#PR` and the PR URL.
Use `--no-steward-handoff` only as an explicit project-default override.

When submitter provenance must survive the invoking agent disappearing after
GitHub accepts the PR, configure an argv-only hook in project config:

```toml
[pr.provenance]
command = ["whence", "--pr", "{pr}", "--auto"]
required = true
```

`shipyard pr` expands `{pr}`, `{repo}`, `{head}`, `{branch}`, `{base}`, and
`{url}`, exports the same facts as `SHIPYARD_PR_*`, and runs the hook before the
steward status/label receipt or validation dispatch. It inherits the submitting
session's Whence/cmux/router environment. A configured hook defaults to
required and fails closed. Explicit recovery through `shipyard ship --pr`
never invokes the hook, because a later recovery agent must not overwrite the
original submitter's provenance.

Run that recovery command only from the exact live PR worktree. Shipyard
compares the current GitHub origin, branch, and full HEAD with the authenticated
PR repository, head branch, and head SHA before it writes queue, ship-state, or
validation state. A detached, stale, fork-origin, or unrelated checkout is
rejected; switch to the exact PR worktree instead of using `--pr` as a retarget
override. A verified intentional head/base change still requires explicit
`--adopt-head`, and known drift is rejected before queue insertion so it cannot
wait behind unrelated work only to fail at worker start. Never auto-adopt.

For an already-created PR, the submitting agent must run
`shipyard runner steward-handoff --repo OWNER/REPO --pr N --head SHA
--workstream-id ID [--context-url URL] [--launch-profile PRIVATE_JSON] --apply`.
That command writes a
successful `shipyard/steward-handoff` status on the expected head, re-reads the
PR, and only then adds `shipyard:managed` and removes `shipyard:unmanaged`.
Shipyard persists a stable private machine identity on first use; later host
renames, launcher environments, or machine-tag changes cannot strand replay.
If the original provider session expires, a replacement agent must explicitly
take the same immutable work item with `--transfer-agent-owner` plus explicit
`--agent-provider` and `--agent-session-id`. The transfer cannot change the
head, workstream, context, or disposition and increments the durable ownership
generation; ordinary invocations never adopt another session implicitly.
The apply-mode steward inventories all other open PRs as `unmanaged`, adds the
explanatory `shipyard:unmanaged` label, but never enqueues, reruns, cancels, or
signals recovery for them. A managed semantic blocker receives one deduplicated
`shipyard:needs-agent` label plus failed `shipyard/steward-recovery` status;
healthy deterministic progress clears the signal. This lets a cheap recovery
agent handle exceptions without spending model tokens on polling.

For phase-1 automated triage, run `shipyard runner recovery-worker` to inspect
and revalidate one durable pending request's target base, exact head, and
complete failed-required-check set or recorded merge state, or add `--apply` to launch its single
bounded worker attempt. `--drain --apply` handles only a bounded initial
snapshot. Apply-mode pre-claim failures preserve the unused attempt but
durably rotate behind untouched pending work, so one unavailable repository
cannot starve the queue. The policy comes exclusively from trusted
machine-global `[merge_steward.recovery_worker]`; Shipyard constructs the exact
fail-closed `codex exec` read-only/ephemeral argv, defaults to
`gpt-5.3-codex-spark`, disables tool surfaces, receives request JSON on stdin,
and runs from scratch with an empty, minimal allow-listed environment.
Alternate runtime modes and global/state path overrides are rejected so the
policy and one-attempt ledger cannot split.
Outputs are strict JSON and remain advisory. Phase 1 rejects repair and
`no_change` verdicts, evidence, paths, and test suggestions; every accepted
result explicitly escalates, with classification used only for routing. A
global lease, overall deadline, and pre/post policy-evidence witnesses prevent
parallel calls and stale authorization. This phase cannot edit, rerun, enqueue,
merge, or release; quota/provider failures become typed terminal failures
rather than blocking unrelated stewardship.
See [references/merge-steward.md](references/merge-steward.md) for the config,
schema, limits, and exact authority boundary.

Use `shipyard work-ledger status --json` to inspect whether the canonical
lifecycle shadow exists without creating it. Authentic schema v11 is inspected
through the same immutable snapshot boundary as inventory, without migration or
WAL creation. Native publication dry-run returns a typed disposition for every
bound v11 row; apply revalidates the exact snapshot under the exclusive writer
domain, migrates and binds only the authenticated work ID/repository/PR/head/
workstream tuple, then requires exact reread/replay. Foreign lineage, ambiguity,
TOCTOU drift, or any unrelated unbound work row refuses. Use `shipyard work-ledger
inventory --json` for a bounded, deterministically ordered immutable view of
local work. Inventory opens only existing storage, never creates or migrates a
database or takes writer custody. A valid migrated legacy `NULL,NULL`
repository identity makes `complete=false`; malformed or half-bound identity
and a noncanonical `GEN-N` handle refuse the whole snapshot.

Use `shipyard work-ledger reconcile-terminal --json` to inventory native
`terminal_handoff` rows that lack their immutable projection binding. The
bounded, redacted, no-write result separates exact already-terminal repair
targets from clean publication precursors, managed-unbound rows, and blocked
rows; bounded related-state counts and explicit blockers prevent incomplete
rows from disappearing or becoming repair authority. Select
one exact repository/PR/head for a dry-run; add `--apply` only after reviewing
the authenticated terminal-head/base proof. Terminal authority is typed: a
merged PR requires its exact merge commit and merge timestamp, while a PR
closed without merging requires an exact close timestamp and the absence of
merge evidence. The latter is recorded as `closed_unmerged`; it never mints or
implies merge authority. An exact dispatching row is eligible
only when one exact wake/delivery is durably uncertain and its activation epoch
is released; dry-run projects the terminal receipt without writing. Apply
re-reads the same typed GitHub outcome inside the writer transaction, atomically
terminalizes that fenced row with evidence bound to the GitHub/wake/delivery identities, then
continues the ordinary terminal projection repair. Apply may add only the immutable provider receipt, projection
binding, its schema-required inert ownership-root identity, and the
dispatch-to-terminal and terminal-to-terminal audit events. The root is not ownership authority: apply
creates no agent ownership, holder material, bootstrap eligibility, or lease.
It never creates or changes a route, wake, continuation, custody record,
activation epoch, or projection intent. Historical unrelated wakes remain unchanged. Ambiguous
targets, active activation/work, incomplete local authority, GitHub movement, an orphan ownership root,
or any receipt/binding/event disagreement refuse. The same exact targeted
command after success is a write-free replay; do not use direct SQL.

Use `shipyard work-ledger import
--json` for the deterministic no-write plan, then `--apply` only when a shadow
import is intended. Import is idempotent, redacted, and fail-closed: it selects
canonical fields and opaque digests from legacy stores, leaves those stores
authoritative and untouched, and cannot schedule, wake, call a model, mutate
GitHub, or project to Linear. Both activation and dispatch remain disabled.

### Successor ownership recovery

Normal context acknowledgement atomically mints the ledger-owned root, first
lease, and holder material. `work-ledger ownership bootstrap` is only for an
acknowledged pre-v13 ownership. Holder material is secret mutation authority:
read it from an owner-only file or strict stdin and write every bootstrap,
renew, or adoption result to a new owner-only `--holder-output`; never put the
material in argv, public inventory, status, or logs.

- `shipyard work-ledger ownership renew --ownership <ao_id>
  --expected-generation <n> --expires-at <RFC3339> --holder <file-or-stdin>
  --holder-output <new-file>` rotates both the lease generation and holder.
  Replace the old holder; it cannot authorize later mutations.
- `shipyard work-ledger ownership release --ownership <ao_id>
  --expected-generation <n> --holder <file-or-stdin>` records the explicit durable
  release while leaving the acknowledged ownership adoptable.
- `shipyard work-ledger ownership adopt --ownership <ao_id>
  --expected-generation <n> --expires-at <RFC3339> --proof <file-or-stdin>
  --holder-output <new-file>` accepts only exact JSON proof
  `{"kind":"expired","expected_expires_at":"<RFC3339>"}` or
  `{"kind":"explicit_release","release_digest":"<64hex>"}`. Adoption is
  atomic, increments `owner_generation`, and safely replays the same successor
  generation and material into a new output file. Supplying `--holder
  <private-file>` authenticates attachment to that exact already-active holder;
  it cannot authorize a different successor.

There is no confirmed-dead mode: do not invent issuer receipts, infer death
from a missing session, or use Linear/correlation identifiers as the root UUID.
For remote custody, run `work-ledger ownership custody-prepare` with the exact
current lease generation and holder, then allow the daemon reconciler to drive
prepare/acknowledge/finalize or authenticated abort. Adoption rotates the
holder/session identity but does not rotate the static transport-daemon
endpoint policy. Custody authorization binds repository provider/id, PR, head,
workstream, root UUID, lease ID/generation/expiry, and re-reads the live tuple
at each commit; never bypass the reconciler with direct database edits.

Use `shipyard --json work-ledger custody-inventory --message wm_<64hex>` to
query only the protected destination selected by the source ledger's exact
active custody rebind. Accepted or processed custody returns a fully
revalidated bounded `complete` or `partial` inventory. Pending or claimed
custody returns `uncertain` without SSH; cancelled, superseded, missing, or
contradictory custody returns `refused`. The request binds the complete
source/target/rebind/transfer tuple and travels through the existing forced SSH
subsystem with no host or shell argument. Optional `--correlation-hints` reads
an owner-only no-follow file containing immutable Linear workspace/root UUID
and provider repository IDs; hints appear only in local output and never cross
the wire, affect the digest, or select authority.
The daemon does run a subscriber-independent **read-only shadow observer** over
policy-covered native nonterminal exact PR heads; inert `shadow_imported`
history is never scheduled. Relevant webhooks debounce for two
seconds with a ten-second maximum burst age; overflow is requeued. A missed-
event catch-up samples at most eight exact targets every five minutes in
deterministic round-robin order. The same target has a 30-second webhook
cooldown, no more than four reads run concurrently, and a rolling-hour
240-request ceiling reserves worst-case pagination before target selection.
Worst-case cost is reserved durably before a pass and reconciled to actual cost
afterward; in-flight and recent usage is conservatively restored on restart. A
shared one-minute deadline covers auth preparation and reads so a slow endpoint
cannot starve later triggers.
Multiple ledger records for one
exact `(repo, PR, head)` cost one bounded paginated observation. Provenance is
exhaustive through 1,000 contexts and fails closed beyond that bound; API cost
counts every GraphQL page.
The daemon loads only its trusted machine-global configuration, selects exact-
repository App auth, and bounds token-helper preparation separately; repository
and local overlays cannot replace unattended auth. Non-App command credentials
fail closed, and one repository-scoped App installation token is pinned per
target observation. The token reaches only the
configured validated native privileged `gh` under a cleared child environment;
a preparation failure
does not count as a GitHub request. Baselines and unchanged snapshots stay
silent; a changed snapshot publishes `shadow_observation_transition` to IPC and
the retained supervised daemon stderr log with the
exact-head fence, policy revision, API-request count, elapsed milliseconds, and
`model_calls=0`. This path has no GitHub write, ledger write, outbox wake,
Linear projection, model, activation, or dispatch capability. Fetch failure and
recovery emit once per state change with a stable redacted class; repeated
failures do not spam logs or expose command output.
With trusted stewardship activation, a separate durable detector can escalate
an assigned-capacity `dispatch_wedge`: two stable observations must prove the
exact current merge-queue job is still queued without a runner while a
compatible runner is online and idle. The receipt and final authority reread
bind repository identity, PR/head, merge-group head, run attempt, job, labels,
queue position, and policy. Restart preserves the first-seen deadline and any
pending publication; transient or capacity-less observations schedule one
bounded follow-up instead of being forgotten. Head movement, queue regeneration
or removal, incomplete pagination, label mismatch, busy/offline capacity, and
ambiguous evidence refuse. The resulting actionable wake is diagnosis only:
it never cancels or requeues work, changes selectors or runners, reorders the
GitHub queue, or authorizes a retry. The wake consumer must revalidate live
state before recommending recovery.
Treat `failed_checks` as observation only, never as permission to use
`gh run rerun --failed`. Active rerun planning must first carry exact failed job
IDs, preview the dependency closure, estimate worker-minutes against a
revision-fenced per-repository ceiling, and refuse when closure is unknown or
exceeds the classified failed-job set. Shadow observation must not invent that
graph or cost evidence from check names.
The transactional wake API derives a deterministic wake ID from the
complete work/owner generation and delivery fence; a caller-supplied identity
that does not match that derivation is rejected before commit.
The route must also resolve to a protected, integrity-valid record for the same
exact head and generations. Terminal runtime (cmux or opted-in HerdR), agent
session, and provider routing (explicit Direct, Subrouter, or CLIProxyAPI) are
separate. Missing provenance fails closed without direct-provider or fresh-agent
fallback. Protected session-header material has both a resolvable opaque
reference and a digest, and the native session and launch profile must agree on
one wrapper reference. Registered future terminal/provider adapters preserve
the same versioned lifecycle boundary and require an active protected adapter
record with exact generation, revision, and implementation/configuration/
capability digests. Imported records remain inert until both continuation outcomes exist
and a legal typed transition records its audit event transactionally.
The internal wake-consumer seam holds a host-local exclusive lease, records
append-only ownership epochs, binds the protected launch-profile and provider
identities, and durably claims and finalizes exact-array provider launches. No
CLI, daemon, or schedule can activate it. Restart may reconcile an idempotent
claim only after the prior live lease is gone; ambiguous non-idempotent delivery
stays `uncertain` and is never blindly repeated.
Use `shipyard work-ledger policy set` to plan a per-repository platform policy;
apply requires the exact current revision. The primary platform is explicit
(use macOS for Pulp, Forge, and Vellum), with a complete repeatable
`--compatibility-lane` inventory and independent compatibility scheduling.
Repository identity is canonical lowercase across daemon arguments, handoffs,
and policy lookup, even when the daemon is launched with GitHub's display-case
spelling.
Repeat `--declared-dependency-lane` only for an inventoried lane with a real
artifact dependency; unknown lanes fail closed and other
cross-lane blocking requires evidenced shared-integrity fault. A policy row
enrolls that repository in shadow observation and is attached to evidence, but
cannot influence GitHub or queue state in the current phase. Keep separate
revision-fenced rows for `generous-corp/pulp`, `generous-corp/forge`, and
`generous-corp/vellum`; change one row when a repository needs a different
platform or dependency rule rather than changing a fleet-wide default.

The preferred unattended credential has Commit statuses and Issues read/write.
A local read-oriented GitHub App that receives the exact integration-permission
403 falls back, with a visible warning, to ambient `gh` for these low-volume
steward status/label writes only; normal observation remains on configured auth.

Read [references/merge-steward.md](references/merge-steward.md) before operating
the steward, changing its policy, or recovering a pending cancellation.

### Pulp disposable Linux health lease

Use `shipyard runner local-linux-lease --repo Generous-Corp/pulp` to inspect the
checked-in `normal-local-fast` Linux lane and decide whether its disposable Mac
Pro pool may serve new unprivileged jobs. Dry-run is the default. `--apply`
renews `PULP_LOCAL_LINUX_LEASE_UNTIL` only when the exact profile-derived labels
have enough unreserved online idle runners in the exact controller-owned
`pulp-ci-ephemeral-` name namespace after matching queued jobs reserve their
slots. It reads `main`'s live merge-queue rule and
requires idle capacity for the full declared `max_entries_to_build` admission
burst; otherwise it deletes the variable. With Pulp's five-entry queue and
two-runner fleet, automatic routing intentionally remains disarmed.
`--apply --watch --interval-secs 60` is the durable controller form.
The profile TTL must be 60–900 seconds, so a dead controller expires safely.
The default accepted namespace is exactly `merge_group`, and the first target
must include `pulp-auto-linux-x64`.

An independently provisioned PR-safe pool is selected explicitly with
`--context pr --lane linux`. It must use the complete tuple
`PULP_PR_SAFE_LINUX_LEASE_UNTIL`, `pull_request`,
`pulp-pr-safe-ephemeral-`, and target label `pulp-pr-safe-linux-x64`.
The trusted and PR-safe capability labels are mutually exclusive in a target;
mixed tuples or selectors fail closed. The live inventory must also show that
every selector-eligible registration uses the approved prefix and lacks the
opposite capability. The PR-safe admission burst is a
reviewed capacity budget because GitHub exposes no repository-wide PR
materialization cap, so this lane must remain advisory and cannot promise
hosted fallback after assignment.

This is a routing lease, not merge or runner mutation authority. It never
changes the selector variable, dispatches or cancels a workflow, or touches the
protected queue. Never use the generic lease for `pull_request_target`, Vellum
trusted, WebCLAP/deploy, signing, release, or other secret-bearing work. See
[`docs/pulp-local-linux-lease.md`](../../docs/pulp-local-linux-lease.md).
Each fleet observation has one 20-second total budget across credential
resolution and all GitHub pages. A slow read must emit a fail-closed
`fleet_unreadable` decision instead of leaving the invoking controller silent;
applied lease mutation has a separate 10-second total budget.

### Reroute watcher (cloud→local drain)

```bash
shipyard runner reroute-watch --repo danielraffel/Shipyard            # observe
shipyard runner reroute-watch --apply --interval 30 --flap-window 300 # act
```

Ports Pulp's `macos_reroute_watcher.py` (task #22), generalized to multi-host
VM-slot accounting. Each tick: read free slots (`runner capacity`), list the
repo's cloud-queued macOS jobs (`gh` runs+jobs, cloud markers `macos-15` /
`nscloud-` / `namespace-profile-`), and — when `free > 0` and a job is still
waiting on cloud — drain **one** PR back to local. Safety properties (pure logic
in `src/reroute.rs`): **slot-safe/fail-closed** (unreadable hosts count as 0
free, so an all-unreadable fleet does nothing), **flap-guard** (skip a PR
rerouted within `--flap-window`), **one reroute per tick** (natural pacing), and
**deterministic** oldest-run-first choice. **Observe by default** — without
`--apply` it logs each decision, per-host capacity, and the candidate list but
acts on nothing. `--apply` shells `shipyard
cloud retarget … --provider local --apply`, which works for PRs Shipyard is
shipping (ship-state-backed). `cloud retarget` has no `--repo` flag — it
resolves the repo from the current checkout — so run `reroute-watch --apply`
inside the target repo's checkout (its `--repo` only scopes which queued runs
are listed, not where the reroute acts). To prevent retargeting the wrong repo,
`--apply` **fails fast** unless the monitored `--repo` matches the repo
`cloud retarget` will dispatch to — the `[cloud].repository` override if set,
otherwise the checkout (so a configured cross-repo controller setup is allowed);
observe mode may monitor any repo. **Follow-up (Part C.2):** rerouting a PR with no
ship-state, and spinning an ephemeral JIT VM runner on a free-slot host (drive
Pulp's `tart-run-job.sh` equivalent) — until then a persistent host-class runner
handles pickup. Full design: `planning/2026-06-01-multi-mac-controller.md`.

### Gotchas

- These four subcommands are newer than the watchdog set; an older installed
  binary will not have them. Verify with `shipyard runner register --help`.
- `register` does **not** provision the host toolchain (Xcode, Homebrew deps,
  Skia, ccache sizing). Run the repo's own host bootstrap first; this command
  assumes a buildable host and only wires up runners + caches.
- A fresh python.org Python with no CA certs breaks asset downloads in repo
  bootstraps (`SSL: CERTIFICATE_VERIFY_FAILED`) — run the bundled
  `Install Certificates.command`.

### Routing Shipyard CI to a registered Mac (the `local` provider)

Registering a runner only stands up the machine; it does not move any job
onto it. Shipyard's own workflows pick a runner via `scripts/ci_matrix.py`,
which now understands a `local` provider in addition to `github-hosted` and
`namespace`. Set repo variable `DEFAULT_RUNNER_PROVIDER=local` (or dispatch
with `-f runner_provider=local`) and the **macOS ARM64** leg resolves to the
label set `["self-hosted","local-mac"]`; Linux/Windows have no local box and
fall back to GitHub-hosted. So to send Shipyard's macOS **release** build to
the Mac Studio, register a Studio runner that carries those labels —

```bash
shipyard runner tag --set studio
shipyard runner register --repo danielraffel/Shipyard --count 1 \
  --labels self-hosted,macos,arm64,local-mac \
  --ci-root /Volumes/Workshop/ci/shipyard
```

— then flip `DEFAULT_RUNNER_PROVIDER=local`. The signing identity already
lives in the Studio keychain, so the signed/notarized dmg build skips
GitHub's hosted-macOS queue. Full provider semantics:
`skills/ci/SKILL.md` → "Runner Provider Defaults" → "The `local` provider".

When the release workflow imports an ephemeral signing identity, preserve the
runner owner's exact user keychain default and search-list order by never
mutating either: pass the ephemeral keychain explicitly to `codesign`.
Cleanup must run under `always()`, verify both settings stayed unchanged, and
delete the ephemeral keychain even when signing setup fails.
Because `codesign --keychain` does not bypass its search-list eligibility
requirement, the workflow gives signing an isolated temporary `HOME` whose
search list contains only the ephemeral keychain.

Local M5 releases use a separate fail-closed contract. Run
`./scripts/release-macos-local.sh --check-auth`; it auto-discovers the standard
file-backed P12 and App Store Connect P8 environment under
`~/.config/pulp/secrets`, creates a disposable signing keychain, installs the
full `apple-tool:,apple:,codesign:` partition list, temporarily makes it first
in the user search list, and performs a real timestamped hardened-runtime
signing probe. A full local release repeats that probe and uses P8 API-key
notarization. Do not use a persistent or login keychain when this preparation
fails, do not continue after a failed probe, and never ask for a password.
Restore the exact search-list snapshot before deleting the disposable
keychain; preserve it and fail if restoration cannot be proven.

The v0.127.0+ installer manages `shipyard` and
`shipyard-workstream-provider` as one version-matched pair. A partial install
must be reported without an implicit plugin upgrade, and rollback to an older
tag must remove the provider introduced by the newer release.
The pair is valid only when both launch from the same install directory and
report the exact same parsed semantic version. Installer replacement and
pre-provider rollback are transactions: preserve and restore both old binaries
and the alias on any move, remove, or post-move smoke failure.
If automatic restoration itself fails, never delete its backup. Preserve the
pair/alias backups and `.shipyard-install-recovery.*` journal, print their exact
source and destination paths, and fail for manual recovery.

### CI routing profiles

Use `shipyard ci profile show <name>` and
`shipyard ci profile plan <name> --repo owner/repo` to inspect repo-owned CI
routing profiles without requiring Tart or any provider-specific CLI. The
planner reads `.tartci/<name>.toml`, `.shipyard/ci-profiles/<name>.toml`, or
`ci-profiles/<name>.toml`, then prints the ordered target chain and the GitHub
variables that would route each lane. It is intentionally read-only; live
capacity resolution and variable writes happen outside this command.

When one routing PR needs an admission hold, first remove any existing queue
entry or native auto-merge request and confirm that removal, then add the
configured opt-out label. The label prevents the steward from re-admitting the
PR but does not disarm admission that was already active. Do not serialize
unrelated PRs with a repository-wide hold for this case.

## Supervised Subprocess Marker (issue #266)

Every `git` / `gh` child process spawned by the supervised
`pr` / `ship` / `auto-merge` / `overflow` / `wait` flows is launched
with `SHIPYARD_PR_RUNNING=1` in its environment. Downstream tooling
(notably Pulp's pre-push hook in `danielraffel/pulp#1406`) uses this
to distinguish a Shipyard-orchestrated push from a raw `git push`.

When adding a new subprocess spawn site inside one of those flows,
route through the helpers in `src/supervised.rs`:

- `crate::supervised::gh_supervised(gh_command)` instead of
  `Command::new("gh")` (mirrors the existing `gh(gh_command)`
  helper in `src/pr.rs`).
- `crate::supervised::git_supervised()` instead of
  `Command::new("git")`.
- `crate::supervised::git_push_supervised()` for a supervised push. It adds
  OpenSSH server-alive probes when `GIT_SSH_COMMAND` is unset, preventing a
  long pre-push gate from losing an otherwise idle GitHub SSH connection while
  preserving caller-supplied SSH identity/proxy commands.
- `crate::supervised::supervised(cmd)` when wrapping an
  injection-style `git_command.map_or_else(..., Command::new)`
  pattern (see `src/branch.rs` for the precedent).

Diagnostic subcommands (`doctor`, `pin`, `runner`, `cleanup`,
`cloud`, `governance`, `release_bot`, `reconcile`) deliberately
skip the marker — they are not "supervised pushes" per the
audit-log use case. If you add a brand new orchestrated flow,
extend the scope deliberately rather than blanket-supervising
everything.

### Log retention operations

Run `shipyard cleanup` first: dry-run is the default and reports each planned
`compress`/`delete`, every protected active/failure/audit directory, and the
high/low byte-watermark projection. Apply with `shipyard cleanup --apply` only
after reviewing that receipt. Use `shipyard cleanup --pin <job-id>` to create
the indefinite `.shipyard-retain` incident/audit pin under the cleanup lock;
do not raw-`touch` it while cleanup may run. Failure and unclassified legacy
evidence is never pressure-deleted. Current Phase 1 rotation is lossless at
writer reopen boundaries; do not use external `copytruncate` against active
Shipyard writers. Full continuously-active writer rotation remains Phase 2.

## GraphQL And GitHub App Fallback Behaviour

Raw queue removal is not a queue-steward operation. Install
`scripts/ghapp_queue_removal_guard.py` at the App-authenticated `ghapp`
chokepoint and run it against the wrapper argv before invoking the real `gh`.
It refuses `pr merge --disable-auto`, `dequeuePullRequest`, and
`disablePullRequestAutoMerge` unless the call comes from Shipyard's
machine-authorized, exact-head, write-ahead-audited mutation path. A deliberate
manual authority action requires the loud `GHAPP_ALLOW_QUEUE_REMOVAL=1`
override. Long-running or pending advisory/self-hosted checks are never queue
removal authority.

Raw PR closure is protected at the same chokepoint by
`scripts/ghapp_pr_close_guard.py`. The guard resolves the live base commit and
always calls GitHub compare as `current-base...PR-head`; in that direction,
`ahead` means the PR still owns unique commits and must remain open, while
`behind` or `identical` proves ancestry containment. Diverged or ahead history
may close only when every changed path's exact blob (including rename/delete
semantics) is already present on the pinned base. Missing, contradictory, or
truncated compare evidence fails closed. A deliberate abandonment or temporary
sequence lock requires the loud `GHAPP_ALLOW_UNINTEGRATED_PR_CLOSE=1` override.
Never infer integration from `ahead_by=0` obtained from the reversed
`PR-head...current-base` endpoint.

The installed `ghapp` wrapper requires an executable `pr-close-guard` for every
non-token command and refuses before native `gh` when it is missing. Install or
update the guard before publishing the wrapper at the same guards directory
(`~/.config/shipyard/guards` by default); `token --repo` remains available for
credential bootstrap without invoking guards.

Five operations detect `is_graphql_rate_limited` in `gh` stderr and
fall through to a REST equivalent: PR list, PR create, PR view, PR
snapshot (in `wait_transport`), and PR merge (in
`app/auto_merge_cmd`). When that happens, `pr::report_rate_limit_fallback(operation, cwd)`
prints a one-line user-visible notice on stderr, including the
GraphQL reset time when a best-effort `gh api rate_limit` probe
succeeds. Add this call to any new REST-fallback dispatch site so
the operator-visible signal stays consistent.

The PR snapshot's raw `statusCheckRollup` does not carry `isRequired`, and
`gh pr checks --required --json` returns only required checks that have already
materialized. `wait pr --state green` must therefore read the complete policy
from classic branch protection plus evaluated repository rulesets, then use
the `gh pr checks` result only for state and producer-aware classification.
Materialize every policy-required context absent from that result as
`PENDING`. A failed policy lookup is not permission to use `mergeable` as a
green proxy: mark the snapshot unknown and make the evaluator fail closed.
State-only waits such as `--state merged` remain usable from the same snapshot.

Long-lived `shipyard wait` ownership reconciles authoritative snapshots on
`--poll-interval` while the daemon remains connected, so a missed webhook
cannot strand the wait or force polling fallback. It must also survive brief
token-helper and network preparation outages. Snapshot preparation retries only
errors that are classified as transient, uses bounded backoff within the
caller's existing overall timeout, and reports the retry count as
`transient_errors`. Permanent credential, helper-output, and configuration
failures still fail immediately; do not paper them over with an outer unbounded
polling loop.

The overall timeout also bounds credential preparation and each `gh` snapshot
subprocess; no retry begins after the deadline. `wait run --success` preserves
the accumulated retry count and transport metadata when a recovered snapshot
ends in a terminal non-success conclusion (exit 4).

`wait pr --state green` likewise exits 4 once every required check for the
observed exact head is terminal and at least one failed. Before that terminal
decision, re-read the live PR head through REST and retry the bounded snapshot
when it moved or cannot be revalidated; never combine one head's identity with
another head's check observations. Keep this extra exact-head fence scoped to
the green/check path: `--state merged` and `--state closed` already have their
answer in the authoritative PR snapshot and must not depend on another REST
request. Human output names the failed head and checks; JSON retains the full
observation with `matched:false`.

Durable queue jobs use `shipyard wait job <sy-id> [--success]`, backed only by
the machine-global queue state. Never pass a `sy-*` ID to `wait run`; the CLI
rejects that class before GitHub lookup. A pending/running queue job plus a
missing run or log observation is still pending/unknown, never success.
`shipyard --json logs <sy-id>` emits typed job lifecycle and log-availability
metadata without raw log content; nonterminal observations exit 3.

GitHub App installation tokens can also be rejected by GitHub's GraphQL
`createPullRequest` / `mergePullRequest` mutations even when the App token is
otherwise the right auth source for inspection. PR creation first tries the
existing GraphQL path, then REST with the same configured token. If both are
blocked with `Resource not accessible by integration`, Shipyard prints a second
explicit notice and falls back to ambient `gh` auth for PR creation only.
Automatic PR merge uses the native merge queue; classic direct merge and its
former REST mutation fallback are disabled with `automatic-merge-refused`
(exit 10). Do not apply ambient-auth fallback to polling, watch,
retarget, diagnostics, merge, or other high-volume operations.

`GitHubActions::pr_head_ref` also falls back from `gh pr view` to
`GET /repos/:owner/:repo/pulls/:number` when GraphQL is rate-limited; both
attempts must use the same configured `GhClient` so GitHub App quota is
preserved.

The legacy REST merge implementation remains non-authoritative code;
automatic classic direct merge never reaches it. A classic branch returns the
typed nonretryable `automatic-merge-refused` outcome (exit 10), retains ship
state, and directs the operator to a native merge queue or manual maintainer
exact-head merge.

Before that merge ever runs, `execute_auto_merge` does a **client-side
superseded-SHA preflight** (#321): it fetches the live PR head via
`fetch_live_head_sha` (which accepts either `headRefOid` or `head.sha`
from a snapshot or a fresh `gh`/REST read) and compares it with
`shas_match` against the `state.head_sha` Shipyard actually validated.
If they differ, it returns `AutoMergeOutcome::SupersededSha { validated,
current }` and **refuses to merge** rather than landing a SHA whose
green evidence is stale — `ship_cmd::post_validation::post_run_merge_state` maps that
outcome to `GreenNotMerged`. This is fail-closed: if the live head
cannot be read, the preflight does not assume safety. It is a belt-and-
suspenders layer in front of the server-side `--match-head-commit`/`sha=`
guard above, because GraphQL auto-merge can otherwise land a commit
pushed *after* validation completed (the bug that merged pulp #3128 at a
pre-fix SHA).

### GraphQL selection sets are unverified until something runs them

`gh api graphql` failures arrive as prose on stderr with no machine-readable
code, and a malformed *document* is indistinguishable at the call site from a
legitimate rejection. Two rules follow.

**Verify a new or edited selection set against the live schema.** Nothing in
`cargo build` checks that a field exists; a wrong guess fails only when the
query runs, in front of an operator. Shipyard ≤0.80.1 selected
`autoMergeRequest{id}` in the merge-queue poll query — `AutoMergeRequest` is a
plain OBJECT implementing no interfaces, so it is not a `Node` and has never had
an `id`. GitHub rejected the entire document. Since `queue_admission` issues that
query *before* any mutation, merge-queue admission failed outright for every
queue-governed repository: PRs validated green and were never enqueued.

```sh
gh api graphql -f query='{__type(name:"AutoMergeRequest"){fields{name}}}'
```

The poll query is now built by `queue_poll_query()` from a named probe constant,
with the type's real field list pinned in `AUTO_MERGE_REQUEST_FIELDS` and
asserted by tests — so a stale selection fails in `cargo test`, not at merge
time. Follow that shape for any query whose selection set could drift: build it
from constants and assert the shape, rather than inlining a string literal. Note
that GraphQL forbids an empty selection set, so a mere presence probe still has
to name one real field.

**Never let a client defect render as a PR problem.**
`is_graphql_malformed_query_error` classifies malformed-document stderr, and
`ship_cmd::post_validation::post_run_merge_state` routes it to
`ShipRenderState::GreenNotMergedClientDefect`
*before* any PR-inspecting classification runs. That state renders a diagnostic
naming Shipyard as the fault, reports `status:"green_not_merged_client_defect"`
in `--json`, and exits `8` — distinct from `1` (validation genuinely failed) and
from the `0` the other green-but-unmerged states keep. The classifier fails
closed: unrecognized stderr stays an ordinary merge rejection, so genuine blocks
never lose their branch-protection guidance. Extend
`GRAPHQL_MALFORMED_QUERY_SIGNATURES` when GitHub adds a phrasing, and keep the
"must NOT match" test cases — a false positive would tell an operator to report a
Shipyard bug when their PR really was blocked.

**Never convert passed validation into failed merge readiness.** Once the
queued validation job is complete and passed, a later `InFlight` or
`TargetFailed` merge-readiness observation is `green_pending_merge_readiness`,
not a worker failure. The completed target
results remain immutable pass evidence, deterministic stewardship owns the
readiness transition, and the operator should use
`shipyard wait pr <N> --state green` rather than rerun validation.
`PrNotFound` instead means the durable scoped ship state is missing: report
`green_validation_state_missing` with exit `9`, preserve the validation proof,
forbid an automatic rerun, and recover state before waiting or merging.

The inverse boundary is equally strict: GitHub's check-rollup reconciler may
heal only dispatched runs carrying a numeric GitHub Actions workflow-run ID.
Local and SSH targets retain Shipyard `sy-*` job IDs, and their terminal
evidence is authoritative. Never fuzzy-match a local target such as `mac` to a
hosted check and replace a local failure with that check's green conclusion.

### Flaky-required-leg wedge → rescue hand-off (`auto_rescue`)

When Shipyard validated every target green but `gh pr merge` is *rejected*
(`post_run_merge_state` sees `AutoMergeOutcome::MergeFailed`), the usual cause
is a GitHub branch-protection **required check that is RED on the exact SHA
Shipyard just validated** — a *flaky* required leg, not a real regression.
`classify_merge_failure` (`src/app/ship_cmd/post_validation.rs`, backed by the pure
`auto_rescue::classify_wedge`) decides whether that's the case and, if so,
renders `GreenNotMergedFlakyRequired` — a hand-back that hands the operator the
one-liner `shipyard rescue <PR> --rerun-failed` instead of the generic message.
It never mutates the merge path (only guidance text + one additive JSON field,
`flaky_required_recovery`).

It is deliberately fail-closed — it falls back to the plain `GreenNotMerged`
hand-back on **any** ambiguity:
- a red *or pending* check with absent `isRequired` (ruleset / merge-queue
  governance, older `gh`, REST-synthesized rollup that lacks requiredness);
- a red required check whose name doesn't map to a Shipyard-validated-green
  target;
- an unreadable ship-state, a failed rollup fetch, or a live `headRefOid` that
  no longer matches the validated `head_sha` (same head-advance guard as the
  superseded-SHA preflight above).

**Mapping is exact or explicitly configured — never fuzzy.** A red required
check maps to a validated-green target only when the check context name equals
the target name (case-insensitive) *or* the target declares
`required_check_context = "<check name>"` in `[targets.<t>]`. This bridges the
common case where the Shipyard target is `mac` but the GitHub required check is
`macos`: without the config line the classifier fail-closes to a no-op, so a
consuming repo must add `required_check_context` to activate the hint. Do NOT
reach for the fuzzy reconcile matcher here — a wrong mapping would tell the
operator to "just rerun" a genuinely-failing required check.

The destructive counterpart (actually invoking `rescue --rerun-failed` + arming
auto-merge automatically, behind a default-off flag) is a planned follow-up; the
current behavior is diagnostic-only.

The standalone `shipyard rescue` operator is replacement-first. Before it can
cancel a queued PR/merge-group run, local workflow discovery must prove that the
workflow declares `workflow_dispatch` and that every required input is either
resolved by the dispatch plan or safely synthesized from the PR number
(`pr`, `pr_number`, or `pull_request_number`). Shipyard submits the replacement
first and cancels only after GitHub accepts that dispatch. A workflow without a
dispatch trigger, an unknown required input, or a rejected dispatch leaves the
original run untouched. If the later cancellation fails, report the duplicate
work as an error; never roll back by cancelling the accepted replacement.
For a completed cancelled/failed/timed-out candidate selected by
`--rerun-failed`, the accepted replacement is the entire transaction: leave the
terminal original untouched. Re-arming it merely to cancel it races GitHub's
queued transition, can return HTTP 409, and can create duplicate work.

Manual `shipyard cancel <job> --reason <why>` records the supplied reason on
the durable job. If omitted, Shipyard still records command source, host, agent,
and PID; a reasonless terminal cancellation is never valid evidence.
When an active local or SSH validation observes that durable cancellation, its
progress callback returns a termination action and Shipyard kills the supervised
process tree, including descendants. Preserve the process-tree and integration
regressions; changing the ledger without stopping active work is not cancellation.

Dry-run must use the same static dispatchability/input checks as apply. Preserve
the negative control that a rejected dispatch produces no cancel call and the
positive control that the Vellum-style PR input is sent before cancellation.

## Validation Gates

**Before `shipyard pr` / `shipyard ship`, run the *exact* chain the `mac`
target enforces** (`.shipyard/config.toml` `[targets.mac]`). `--lib`-scoped
checks are NOT enough — `--all-targets -- -D warnings` and `cargo fmt` catch
things the lib build won't, and a miss costs a full ship round-trip (the
2026-06-01 runner-provisioning PR failed mac validation twice this way):

```sh
cargo fmt --all --check \
  && cargo clippy --all-targets --locked -- -D warnings \
  && cargo test --all-targets --locked
```

**`Cargo.lock` after a version bump — now automatic.**
`version_bump_check.py --mode=apply` used to rewrite `Cargo.toml` and leave
`Cargo.lock` on the old version, so the `--locked` steps above failed with

```
error: cannot update the lock file … because --locked was passed
```

before compiling anything. Every self-ship that bumped the version hit it. The
apply step now rewrites the lockfile's workspace-member entry and stages it with
the bump (`refresh_cargo_lock`), so no manual step is needed.

It is deliberately narrow, and stays silent when it cannot be sure: it rewrites
only the `[[package]]` block whose `name` matches and which has **no** `source`
key, because a registry crate sharing the name does have one. Zero matches or
more than one means it writes nothing and the `--locked` failure returns — that
is the intended behaviour, not a regression to patch over. If you see that error
after a bump, check `Cargo.lock` actually contains a source-less entry for the
crate rather than reaching for `cargo update`. The rewrite is textual, so it
works in the Python-only version-skill-check job where no `cargo` exists, and it
is idempotent across a re-applied bump.

(`cargo fmt --all` on new modules is the other easy miss.)

**Ship-state SHA drift recovery (`--adopt-head`, #346):** if you amend or
force-push a PR's tip after Shipyard recorded ship-state (e.g. adding a
required `Version-Bump: skip` trailer), the next `shipyard ship`/`pr` aborts
with `ship state SHA drift: existing <old>, current <new>`. Re-run with
`--adopt-head` (`shipyard ship --adopt-head` / `shipyard pr --adopt-head`): it
adopts the current head and **clears the recorded remote runs + evidence** so
the new head re-validates from scratch — it never blesses stale validation for
a possibly-different tree. The policy-signature guard still applies (a changed
merge policy is still refused). Without the flag the old dead-end (manual `gh pr
merge`) stands.

Other non-mutating checks:

```sh
cargo test --all-targets --locked
python3 -m unittest discover -s scripts -p 'test_*.py'
python3 scripts/update_drift_tracker.py
python3 scripts/compare_cli_surface.py --allow-rust-only paths
scripts/validate_webhook_tunnel_live.py --json
```

The live webhook gate is intentionally dangerous because it resets the local
Funnel config:

```sh
scripts/validate_webhook_tunnel_live.py \
  --repo danielraffel/Shipyard \
  --binary "$(command -v shipyard)" \
  --apply \
  --allow-funnel-reset \
  --json
```

Run that only in an approved window where briefly taking over the
machine-global Tailscale Serve/Funnel route is acceptable. The validator knows
about the App Store Tailscale binary at
`/Applications/Tailscale.app/Contents/MacOS/Tailscale`; do not assume a
`tailscale` PATH shim exists.

Shipyard itself launches every noninteractive Tailscale status, Serve, and
Funnel command with `TERM=dumb`. Preserve that environment override: the macOS
app-bundle CLI can otherwise attempt to start its GUI and emit non-JSON under a
stripped LaunchAgent or SSH environment even while Tailscale is healthy. An
interactive-shell success is not sufficient fleet proof.

## macOS GUI

The GUI lives at `/Users/danielraffel/Code/shipyard-macos-gui`. Validate it
against a sandboxed or signed rehearsal artifact before replacing the active
production `shipyard`. Update GUI docs during migration/release work, not
after the fact.

## Platform Notes

Read `references/platforms.md` when work touches Tailscale, live mode,
signing, packaging, Namespace/GitHub Actions runners, Windows SSH/PowerShell,
or cross-platform sandbox E2E behavior.

Namespace is optional and account-dependent. When Namespace is unavailable,
Shipyard should default to GitHub-hosted Linux/macOS/Windows runners or explicit
self-hosted GitHub Actions labels. Do not assume `nsc` access, and do not route
new Shipyard CI to Namespace unless the user explicitly confirms active access.
Do not add hidden repo-variable fallbacks to local/self-hosted macOS runners:
local runner use should be explicit via workflow-dispatch selector inputs so
default GitHub-hosted runs cannot be stolen by stale local runner variables.

For local capacity, keep GitHub Actions as the dispatch layer and use SSH only
to manage the runner hosts. Stable labels such as `shipyard-macos-arm64`,
`shipyard-linux-arm64`, and `shipyard-windows-x64` are preferable to raw host
names in workflow `runs-on` selectors.

For a simple Mac Studio setup, use explicit Shipyard fallback config rather
than hidden self-hosted runner state:

```toml
[targets.mac]
backend = "ssh"
host = "mac-studio"
platform = "macos-arm64"
repo_path = "/Users/shipyard/work/shipyard"
warm_keepalive_seconds = 1800

fallback = [
  { type = "local", cwd = "/Users/danielraffel/Code/shipyard" },
]
```

For named members and lease visibility, use `backend = "host-pool"`:

```toml
[host_pools.local_macs]
strategy = "ordered"

[[host_pools.local_macs.members]]
id = "mac-studio"
type = "ssh"
host = "mac-studio"
repo_path = "/Users/shipyard/work/shipyard"
capabilities = ["macos", "arm64"]

[[host_pools.local_macs.members]]
id = "local"
type = "local"
cwd = "/Users/danielraffel/Code/shipyard"
capabilities = ["macos", "arm64"]

[targets.mac]
backend = "host-pool"
pool = "local_macs"
platform = "macos-arm64"
requires = ["macos", "arm64"]
```

Host-pool targets acquire/release local leases, show state through
`shipyard targets pool status`, and prune stale lease records with
`shipyard targets pool cleanup --fix`. They can drain multiple
non-conflicting queued jobs across available members under one local drain
owner, but they still do not interrupt running GitHub-hosted macOS jobs. Jobs
serialize when they claim the same checkout, PR state, evidence lane, or
exhausted pool capacity. See `docs/local-mac-pool.md` before claiming
multi-Mac throughput.

The durable drain refills capacity per worker completion, not per admitted
batch: when one worker finishes, Shipyard replans immediately and may start the
next eligible job while slower siblings continue. A scheduler-deferred job must
respect `scheduler_defer_until` and wait for a later paced pass; it must never
hot-loop back into the slot it just released. If refill admission itself fails,
Shipyard preserves that error but still drains active worker completions and
durably requeues any deferred jobs before returning.

Durable agent resume has independent terminal and provider-routing axes. cmux
is the current default terminal; HerdR is an optional terminal adapter, while
Subrouter is provider-routing provenance carried by the exact launch/resume
profile. Preserve both dimensions concurrently. Never infer one from the other
or silently translate a missing Subrouter route into direct `codex`. During the
inert projection phase, reconcile legacy terminal handoffs even on a no-op
transition, atomically publish/roll back terminal and resume maps together, and
verify that every record keeps dispatch disabled. Activation additionally
requires a generation-fenced transactional outbox, durable acknowledgment,
uncertain-delivery refusal, and physical original/fresh-owner canaries.

For Pulp/tartci macOS VM work, prefer local queueing over hosted overflow: a
full local fleet should leave jobs queued on the self-hosted VM labels until a
controller/secondary Mac slot opens. Add GitHub-hosted macOS only as an
explicit operator fallback when fleet status says the local Macs are
offline/unhealthy, or when the workflow intentionally asks for hosted coverage.

For a host-wide CI transition, wrap the exact transition command with
`shipyard queue-hold exec` and bind the normalized host, service, repository,
and runner scope. Include every applicable identity; a provider-only host may
have no repository-scoped persistent runner. Shipyard assigns and exports the
positive monotonic generation; it is never a caller-selected `exec` argument. The live child
inherits the held queue-lock open-file description; `queue-hold verify` accepts
only that exact owner PID/start identity, FD, lock inode, scope digest, generation, ledger
revision, non-revoked state, and independent contention proof. Re-verify under
the downstream transition lock before every exact service mutation and before
final participation-state publication. Never treat `queue-hold.json`, a PID,
an inode name, or a prior successful read as authority. Exit `3` is refusal,
`124` is bounded contention/no child, and `125` is setup/observation failure.
If authority is revoked between batches, preserve completed safe shutdown,
refuse the remaining services, and keep the transition nonterminal. The hold
fences only Shipyard's local pending-to-running queue admission; it does not
drain GitHub persistent runners, prove zero leases/VMs, or override the host
resource governor.

For the default-off Pulp M3/M1 performance canary, cache readiness requires an
immutable content manifest produced by the read-only no-follow tree observer.
Observe every required M3 generation before probing M1, preserve exact policy
generation and freshness fences, record `model_calls=0`, and publish only
crash-durable no-overwrite paired receipts. M1 observation must use the strict
digest-pinned companion protocol over `StrictSshRemoteM1CacheTransport`:
explicit pinned identity and known-host authority only, with cleared ambient
SSH configuration. Probe direct LAN first; an independently pinned Tailnet
target may carry diagnostic cache observation only after a transport-class
failure and can never close the LAN/session gate. Bind the exact host receipt,
session generation, route, capabilities, persistent staging reserve, verified
terminal instance, companion executable, immutable manifest, request/response
digests and bytes, route RTTs, fallback class, and exchange RTT. Never reroute
after the request crosses the companion boundary. This can close only the exact
remote M1 gates it proves; M3 and execution authority remain separate. See
`docs/pulp-mac-cache-readiness.md`.

For Vellum's repository-scoped disposable lanes, treat an `offline + busy`
runner as an ownership mismatch until TartCI proves otherwise. Run the bounded
two-snapshot check, correlate the exact VM/lease/supervisor and in-progress job,
and preserve the protected queue while the result is live or uncertain. The
current `offline_busy_wait_for_github` result never authorizes recovery; preserve
and escalate until a future pinned TartCI version supplies a machine-checked
orphan verdict. Never bulk-cancel busy runners, reset shared names, broaden a
trusted group, or use a fresh worktree as a reason to register another runner.

## Cloud Retargeting

`shipyard cloud retarget --apply` is intentionally fail-closed. It cancels
matching GitHub Actions jobs first, uses whole-run cancellation only when every
active job in the run matches the target, and does not dispatch a replacement
if cancellation cannot be proven complete. When handling `event=cancel_failed`,
preserve the classification (`auth`, `scope`, `not_found`, `unsupported`,
`transient`, `unknown`), run/job URLs, manual recovery steps, and
branch-protection warning; do not collapse HTTP 404/not-found into an
`actions:write` scope hint unless the raw error also indicates auth or
permission trouble.

## Immutable Pulp dependency pins

Repositories opt in through a reviewed `[dependencies.pulp]` table; there is no
implicit channel for unrelated users. Use the `latest-qualified` template for
active first-party repositories, an explicit reviewed `stable_tag` for
production, or an exact `fixed_tag` plus peeled `fixed_commit` for frozen or
reviewed downgrade cases. Floating refs such as `main` are never valid. Full
templates and the lock schema are in `docs/dependency-channels.md`.

Run `shipyard dependency pulp update` to select and qualify a release. It must
use trusted machine-global GitHub App command auth for GitHub reads, the HTTPS
push, and REST PR creation. Do not bypass its draft/prerelease, complete asset
set, checksum manifest, immutable-release attestation, SLSA workflow/tag/commit,
same-version rewrite, or downgrade guards. Cached qualification receipts are
only a large-download optimization and are keyed by the complete immutable
release identity. Reuse them only to reproduce an existing tracked proof;
untracked candidates require fresh verification, and latest-qualified scans
every GitHub release page plus every candidate's paginated asset inventory.
Only deterministic rejection may advance to an older release; operational
failures abort. Build qualification binds the exact source tag ref and peeled
commit in the GitHub-issued certificate, not only in workflow-authored predicate
fields. The writer pins the exact validated App-helper token.
It also resolves the exact App bot identity. Privileged commit/push operations
require the machine-global trusted absolute `privileged_gh_binary` and
`privileged_git_binary`. Token-bearing Git runs only in a Shipyard-initialized
isolated repository, ignores inherited/system/global Git configuration, and
releases its credential only to exact HTTPS `github.com`; hooks and other
helpers remain disabled. Privileged `gh` and Git children use a minimal
allowlisted environment, excluding inherited loader, proxy, CA, trace, and
tool-routing overrides. App-authenticated `--delete-branch` must preflight its
trusted Git path before any merge mutation. Verify the exact lock-only commit and recheck the
consumer base SHA before publication. Branch
identity includes the base SHA and complete lock digest; first push uses an
absence lease, and reuse
requires the exact commit/tree and App-authored PR envelope. Never adopt an
orphan or foreign branch. Verification retains the exact recorded attestation
when multiple valid proofs exist.

Consumer CI must independently run `shipyard dependency pulp verify`, which
bypasses the cache. The consumer build remains responsible for verifying the
exact SDK bytes it consumes and matching extracted `sdk-provenance.json` source
and distribution eligibility to the lock.

## Cutover Discipline

For native continuation delivery, require fresh exact PR head/base SHA and the
numeric repository-scoped GitHub App installation identity. Treat cmux labels
as provenance only: delivery authority is a unique local process/surface plus
the exact live native checkpoint, rechecked immediately before provider I/O.
HerdR remains typed-refused until it exposes equivalent independent evidence.
Never replace a refused Subrouter route with direct provider execution.

Release/cutover is a human decision, not an implementation side effect. Before
asking for go/no-go, ensure:

- Drift tracker has no untriaged upstream changes.
- CLI surface comparison is clean.
- CI, coverage, sandbox E2E, and GUI validation are green on the current Rust
  commit.
- Tailscale/GitHub live delivery is either passed in an approved reset window
  or explicitly risk-accepted.
- Signing/notarization and rollback paths are validated.
- Documentation changes for Shipyard, GUI, and Pulp/consumer pins are tracked.
