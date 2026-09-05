---
name: ci
description: Cross-platform CI coordination with Shipyard — validates, ships, manages queue, and runs cloud workflows
---

# CI Operations with Shipyard

## Webhook repository identity

Webhook registrar repository keys are canonical lowercase `owner/name` values
(surrounding whitespace is ignored). Mixed-case callers therefore reuse the
same persisted registration and can unregister through any casing. Registrar
changes must be committed and pushed to the same PR branch so Shipyard validates
one exact head; do not create a parallel PR or mutate webhooks manually.

## Metrics authority

GitHub job `created_at` is the provider-authoritative queue timestamp. Metrics
imports may persist queue latency only when both `created_at` and `started_at`
parse successfully; never infer submit-to-receipt, cache reuse, or model-token
counts from wall-clock duration or logs. Those scorecard dimensions must remain
explicitly unavailable until authenticated event fields exist.

## Version-only release bumps

The CLI version is part of the CI surface map. Even a release-only
`Cargo.toml`/`Cargo.lock` bump must carry a CI skill note (or an explicit
`Skill-Update: skip` trailer), so the version and skill-sync gate remains
truthful on release PRs.

Shipyard coordinates validation across local, SSH, and cloud targets.

## Quick reference

| Task | Command |
|------|---------|
| Validate current branch | `shipyard run --json` (Unix/macOS: queues to the single-worker daemon and returns after durable acceptance; Windows remains foreground) |
| Validate specific targets | `shipyard run --targets mac,ubuntu --json` |
| Iterate on one platform's CI failure | `shipyard run --skip-target <others>` (see [Iterating on a single-platform failure](#iterating-on-a-single-platform-failure)) |
| Fast smoke check | `shipyard run --smoke --json` |
| Run one target command and store typed evidence/artifacts | `shipyard run command --target <name> --artifact '<glob>' -- <argv...>` |
| Start the live-mode webhook daemon | `shipyard daemon start` |
| Inspect the daemon | `shipyard daemon status --json` |
| Stop the daemon | `shipyard daemon stop` |
| Full ship (PR + validate + merge) | `shipyard ship --json` (Unix/macOS: queues to the single-worker daemon and returns after durable acceptance; Windows remains foreground) |
| Debug validation in this terminal | `shipyard run --foreground` / `shipyard ship --foreground` |
| Ship to develop instead of main | `shipyard ship --base develop --json` |
| Resume an interrupted ship | `shipyard ship --resume --json` (auto when state exists) |
| Force-restart a stale ship | `shipyard ship --no-resume --json` |
| List in-flight ship states | `shipyard ship-state list --json` |
| Inspect one PR's ship state | `shipyard ship-state show <pr> --json` |
| Live-tail the active ship | `shipyard watch` (or `shipyard watch --pr <n>`) |
| One-shot snapshot | `shipyard watch --no-follow --json` |
| Watch a long local/SSH VM build | `shipyard watch local --target <name> --command '<cmd>' --milestone-regex '<re>' --terminal-regex '<re>'` |
| Merge on green (cron-safe one-shot) | `shipyard auto-merge <pr>` (0=merged, 1=fail, 2=not-found, 3=in-flight, 10=automatic classic merge refused) |
| Diagnose RELEASE_BOT_TOKEN | `shipyard release-bot status --json` |
| Configure RELEASE_BOT_TOKEN | `shipyard release-bot setup` (guided) |
| Re-paste token after rotation | `shipyard release-bot setup --paste` |
| Opt in to post-release docs sync | `shipyard changelog init` then `shipyard release-bot hook install` |
| Regenerate CHANGELOG.md from tags | `shipyard changelog regenerate` |
| CI drift gate for CHANGELOG.md | `shipyard changelog check` |
| Run the post-tag hook locally | `shipyard release-bot hook run --tag v0.9.0` |
| Audit a generated post-tag hook | The run step must contain literal Bash `tag="${GITHUB_REF#refs/tags/}"`; `${{GITHUB_REF#refs/tags/}}` is invalid GitHub expression syntax. PR pushes from the detached tag checkout must first attach the deterministic local branch, then use Shipyard's supervised push and target `HEAD:refs/heads/<branch>` so repository hooks see both a branch and `SHIPYARD_PR_RUNNING=1`. Repositories requiring signed bot commits set `release.post_tag_hook.ssh_signing_setup_script`; never hand-edit the owned workflow. Consumer workflows install the exact generating CLI by default; a repository that must avoid racing its own draft release may explicitly set `release.post_tag_hook.workflow_shipyard_version = "latest"`. Only exact stable optional-`v` semver or `latest` is accepted, and the CLI override wins lower config. |
| Live-probe the release chain | `shipyard doctor --release-chain` (dispatches + waits) |
| Show queue and status | `shipyard status --json` |
| Show all queued jobs | `shipyard queue --json` |
| Experimental authority schema v5 | No operational command exists. Official builds are v4-only; an explicit source test build may validate the reserved request shape only to return `ExperimentalAuthorityRefused`, with no writer, queue mutation, outcome, backend, execution, or authority. |
| Observe GitHub queue and PR transitions without mutation | `shipyard --json queue-observe --repo <owner/repo> [--follow]` (one bounded GraphQL query per tick; unchanged polls are silent and back off adaptively) |
| Remove an exact queue entry | Do not use raw `ghapp pr merge --disable-auto` or `dequeuePullRequest`; use Shipyard's audited exact-head path. The ghapp queue-removal guard refuses unaudited removal, with `GHAPP_ALLOW_QUEUE_REMOVAL=1` reserved for an explicit authority action. |
| Shadow-plan changed-surface tests for an exact PR head | `shipyard --json changed-surface-plan --repo <owner/repo> --pr <n> --target <name>` (base-owned literal tests only; full suite remains authoritative; identity mismatch hard-fails, ambiguity falls back full) |
| Authorize an exact metadata-only PR without a native worker | Configure trusted machine-global `[metadata_authority]` plus one repository entry containing a narrow path allowlist and exact required hosted checks. `shipyard pr` emits an immutable exact base/head/tree/path/check/policy receipt and queues zero native targets only when every observation agrees; unknown paths, stale/pending checks, SHA drift, or policy ambiguity preserve full validation or refuse execution. Project config cannot activate or widen this tier. |
| Read one exact shadow-comparison result | `shipyard --json changed-surface-trial-status --repo <owner/repo> --pr <n> --target <name> --head <sha>` (read-only; exit 3 collecting, 0 ready/safe terminal, 1 rejected/invalidated; stale-base results are typed and permanently shadow-only; validates exact receipt identity plus conservative timing/savings and never promotes policy or merge readiness) |
| Assess a macOS sharding canary | Default-off: `shipyard parallel-proof-canary --request <absolute-private-json>` is a no-execution/no-mutation plan unless `--apply` is explicit. Apply requires trusted machine-global policy pinning a native adapter digest, full invocation-authority digest, bounds, repository/target, and exact builder/worker identities. The controller completes the proof-bound same-host control before distributed work, rechecks session/storage fences, binds actual transfer and interrupted-resume counters, forces avoided-byte claims and model calls to zero, records distributed-started before mutation, and no-overwrite publishes evidence. Core has no Pulp commands or personal host defaults; installed adapter configuration and physical fleet proof remain required. Neither surface authorizes a merge. |
| Produce a canonical shadow CTest inventory | Library-only: translate bounded CTest JSON-v1 with an independent exhaustive count/sorted-ID digest, exact configuration, bounded controller-owned capabilities, and explicit host/fleet scope for every resource lock. Filtered, ambiguous, disabled, non-executable, `REQUIRED_FILES`-dependent, or unsupported graphs fail closed. This does not invoke CTest, dispatch work, or authorize a merge. |
| Assess one-host Pulp M3 build-once consumption | Library-only and default-off: bind exactly one successful configure/build receipt to the exact source, toolchain, canonical CTest inventory, proof manifest, and compact artifact content address; require same-session M3 consumption with zero configure/build invocations and exact sorted executed-set reconciliation. The existing full gate remains authoritative, and this cannot run commands, dispatch cross-host work, publish a check, or authorize a merge. |
| Show run logs | `shipyard logs <job_id> --json` |
| **Runner watchdog: health check** | `shipyard runner status --repo <r> --runner-id <id>` |
| **Recover an exact Pulp zero-job Actions wedge** | `shipyard runner zero-job-recover --pr <n> --source-run-id <id>` is read-only by default; review the exact candidate, then add `--apply` once. This is independent of coalescing and stale cancellation. |
| **Runner watchdog: list stale queued runs (dry-run)** | `shipyard runner cleanup --dry-run` |
| **Runner watchdog: cancel stale queued runs** | `shipyard runner cleanup --fix` |
| **Runner watchdog: daemon mode** | `shipyard runner watch --fix` |
| **Runner watchdog: auto-kill hung workers (full recovery)** | `shipyard runner watch --kill-hung-workers` (implies `--fix`) |
| **Runner provisioning: set this box's machine tag** | `shipyard runner tag --set <studio\|m1\|m5>` (stored per-box; never hostname-derived) |
| **Runner provisioning: register N runners for a repo** | `shipyard runner register --repo <owner/repo> --count <N> [--ci-root <dir>]` (additive: appends N names `<repo>-<tag>-NN`; separately reconciles existing local runners) |
| **Runner provisioning environment contract** | Registration uses one fleet-pinned Actions runner with auto-update disabled, a system-first `.path`, and runner-private `_toolcache/{rustup,cargo}` homes; never symlink toolchain homes to a shared/external build volume. Discover configured local runners across repositories and old tags, reserve every existing runner's unchanged `.env` allocation, then divide remaining host cores across additive runners; fail closed instead of overcommitting. Parse `.runner` and require exact agent/repo ownership before any mutation. Never auto-stop or upgrade a service-installed runner: retain it when already pinned, otherwise defer it unchanged with exit 3. Upgrade a configured service-less runner only after fresh GitHub evidence proves it offline and idle immediately before clone-staging and again before rename activation, with a final local service-marker check after that network refresh. Pinned upgrades clone-stage and verify before activation, retain the intact old directory until the new service starts, use the runner's single compound `svc.sh uninstall` to stop/remove a partially started replacement before rollback, and never activate a partially extracted replacement. Readiness probes must not leak child output. |
| **Runner provisioning: dry-run the registration plan** | `shipyard runner register --repo <owner/repo> --count <N> --dry-run` |
| **Runner provisioning: live cross-repo pool view** | `shipyard runner list [--repo <owner/repo>]` (groups by machine; flags orphaned local dirs) |
| **Runner provisioning: audit host-class and local PATH drift** | `shipyard runner audit [--repo <owner/repo>]` (paginated; flags non-conforming names + missing `<repo>-build` / `<repo>-build-<class>` labels, fatally rejects advisory/required label overlap, and strictly checks every in-scope local runner's `.path` against Shipyard's canonical system-first value; malformed/unreadable local inventory fails closed; exit 1 on drift). New/service-less registration preserves that PATH through `config.sh`; drain and stop/uninstall a service-installed runner before reconciliation because Shipyard never stops live capacity implicitly. |
| **Runner provisioning: VM-slot-aware free macOS capacity** | `shipyard runner capacity [--json]` (reads `tart list` + `tart get` per `[host_class.*]`, using configured `tart_home` as `TART_HOME`; counts only running macOS/darwin VMs; `free = Σ max(0, cap − running_macos)`; fail-closed, exit 1 if any host/VM OS unreadable) |
| **Runner fleet visibility: exact-head queue/release liveness** | `shipyard runner fleet-status --repo <owner/repo> --target macos [--json]` (bounded pagination plus one shared 30-second GitHub deadline; deadline expiry renders a fail-closed partial assessment instead of hanging; stable auth/rate/truncation reasons; detects optional/superseded capacity owners and cleared enrollment) |
| **Roll one exact Shipyard release across the fleet** | Configure absolute `host_class.<name>.shipyard_bin` and remote `github_cli` paths, explicit `shipyard_mode`/`shipyard_global_dir`/`shipyard_state_dir`, plus self-contained machine-global command auth. Review `shipyard runner fleet-update --to vX.Y.Z --host-class <class> --json`, then add `--apply`; repeat the selector for an ordered subset or use explicit `--all-hosts`. Before any host, Shipyard binds the annotated tag's full tag-object/commit/tree OIDs, release ID, exact checksum-manifest and macOS DMG SHA-256, and `release.yml` build-provenance attestations, closes the mint window, and freezes that authority for every selected host. Release assets stream by immutable asset ID into owner-private hard-capped staging; exact declared size and SHA are verified before attestation, while partial files and escaped pipe holders are bounded by the same process-tree deadline. Missing attestations, manifest/source drift, implicit/duplicate hosts, authority/asset receipt mismatch, cross-host pair-hash mismatch, mixed CLI/provider pairs, or a companion retained by legacy rollback fail closed. Receipts preserve complete authority plus before/after paths, versions, double hashes, daemon identity, and configured repositories; rollout stops at the first failure. |

The first governed replacement of a deployed pre-release-matched legacy auth
wrapper may consume its repository-routed, single-line `ghs_` token response.
That bootstrap is JSON-first, preserves exact typed credential and repository
binding, accepts only tightly framed opaque ASCII, and exists only to fetch the
frozen release assets that install and prove the current JSON wrapper.
After commit, the typed daemon-refresh receipt is streamed without an
intermediate shell variable; interrupt and termination signals remain nonzero.
| **Supply non-secret machine paths to every fresh worktree** | Track exact `[project].repository` plus `validation.<name>.machine_environment = ["NAME"]`; put each host's absolute path under machine-global `[repository_environment."OWNER/REPO"]`. Names must end in `_DIR`, `_FILE`, `_HOME`, `_PATH`, or `_ROOT`; secret-like names remain rejected even with one of those suffixes. Shipyard binds the case-sensitive GitHub origin, ignores repo/local attempts to supply values, rejects relative values, and snapshots resolved values under owner-protected queue state for session-independent execution. |
| **Maintain Pulp's expiring disposable-Linux route** | `shipyard runner local-linux-lease --repo Generous-Corp/pulp [--apply] [--watch --interval-secs 60] [--json]` (dry-run by default; profile-derived exact labels; queued matching jobs reserve idle slots; renews only for unreserved online idle ephemeral capacity; unhealthy/unreadable clears; 15-minute maximum TTL; no workflow or MQ mutation) |
| **Gate TartCI JIT registration on an exact stale-run census** | `shipyard runner admission-clean --repo <owner/repo> --base main --labels self-hosted,<exact-labels> --apply --json` (flat versioned TartCI contract: 0=`admit`, 3=`defer`, 1=operational error, 2=invalid configuration; managed queued PR/merge-group runs with superseded immutable heads may be cancelled only when a queued job's labels are a subset of the prospective runner, while the same compatible queued job inside an `in_progress` workflow blocks admission but is never cancelled; every GitHub call under the exact-key observation lock shares one 120-second absolute budget; observation or stewardship contention returns typed `observation_in_progress` or `stewardship_in_progress` defer, and timeouts or other non-contention observation/lock failures release the lock and return a typed error with bounded detail; a non-authority host never mutates) |
| **Cross-repo merge-on-green stewardship** | Prefer atomic submission: configure `[merge_steward].auto_handoff = true` on the protected base branch and use `shipyard pr [--workstream-id <id>] [--context-url <url>]` (a PR branch cannot opt itself in); otherwise hand off one immutable head with `shipyard runner steward-handoff --repo <owner/repo> --pr <n> --head <sha> --workstream-id <id> [--context-url <url>] --apply`, then reconcile with `shipyard runner steward --repo <owner/pulp> --repo <owner/forge> --repo <owner/vellum> [--json]` (dry-run by default; only PRs carrying both the `shipyard:managed` label and a successful `shipyard/steward-handoff` status on their current head may be mutated, so apply mode explicitly labels old backlog `shipyard:unmanaged` without adopting it and exact handoff removes that explanatory label; `--apply` requires the trusted machine-global mutation authority, obeys central `HOLD`, serializes and write-ahead audits every GitHub mutation, emits one deduplicated `shipyard:needs-agent` plus `shipyard/steward-recovery` failure signal for semantic blockers, resumes durable exact-run pending cancellations before planning, re-enrolls only the current exact green head, and preserves native queue order unless the separately default-off `--recover-hosted-setup-eviction-priority` flag has a durable pre-removal witness plus exact linked required GitHub-hosted setup-only provider-DNS failure proof for one once-per-removal/run `jump: true`; it refuses mutation without authoritative required-check governance and refuses client-side direct merge when GitHub cannot atomically bind the validated base revision, bounds transient reruns with both write-ahead intent and GitHub's durable `run_attempt`, cancels only queued runs whose immutable PR/merge-group head is provably superseded, and may preempt one exact allow-listed advisory Pulp workflow holding `pulp-preamble` after a 15-minute exact-front pool wait; same-head duplicates, required workflows, pushes, unknown work, and unmanaged PRs are never cancelled; opt out with `shipyard:no-auto-merge` or disable preemption with `--no-preempt-capacity`. Automatic classic direct merge is disabled: `admin=false` cannot prove the authenticated credential is excluded from every admin, custom-role, ruleset, or GitHub App bypass path. Use the native merge queue for automatic merging; a manual maintainer exact-head merge remains outside Shipyard.) |
| **Publish an exact agent continuation** | Add `--launch-profile <private-json>` to `shipyard pr` or `runner steward-handoff`. `LaunchProfileV1` binds exact prompt-free launch/resume argv plus typed provider/account/model/reasoning effort, checkpoint, bootstrap, and worktree provenance. With trusted machine-global activation, Shipyard transactionally publishes a zero-wake daemon obligation and reports `monitoring_transferred=true`; any refusal leaves monitoring with the origin. `continue` is the default; `pause` also requires `--task-graph` proof. Private fields are never projected to GitHub/Linear. See `docs/launch-profile.md` and `docs/post-handoff-disposition.md`. |

Fleet GitHub App rollout additionally requires `github_cli` to be the `ghapp`
sibling of `shipyard_bin`. Machine-global command auth must be exactly wrapper
+ `token --app-id VALUE --private-key ABS --repo {repo_slug}`. Direct `ghapp`
resolves only that shape through the sibling Shipyard after grammar and repo
validation; the wrapper pins API, cache, and resolved repo arguments. A
strict 0600 non-symlink wrapper context is mandatory for direct mode and
preserves the exact configured mode/global directory. Fleet creates it for
targets v0.131.0 and newer; manual installs must provision the typed default
context. A post-install resolver probe runs before transaction commit, and
failure rolls back helper, wrapper, context, CLI, and companion. The first
transition from v0.130.x or older requires an ordinary exact-tag update on each
host and migration to the exact machine-global wrapper command before the
governed fleet pass; otherwise it refuses before download or mutation. Older
fleet-update releases v0.100.0-v0.130.x used the compatible four-target
transaction and nine-line journal without that unavailable probe; the current
client does not target them.

Targets v0.134.0 and newer publish machine-global auth as a private,
content-addressed generation containing the release-matched Shipyard binary,
helper, wrapper, and typed context. Starting with v0.137.0, the generation also
contains the mandatory release-matched `pr-close-guard` and uses the mutually
exclusive `auth-selector-v2` contract. Fleet clients from v0.134-v0.136 must
reject that target before publication; upgrade the controlling client through
the supported exact-release path before the governed fleet transaction. The
first v0.137 activation publishes a digest-bound regular-file trampoline only
after legacy readers drain. Later updates leave that public file untouched and
atomically move a separate owner-private generation selector. The trampoline
reads that selector once and executes the immutable selected wrapper, which
resolves every sibling from the same generation. The v4 journal binds the
trampoline, selector, and complete cohort while retaining bounded v2/v3
recovery compatibility. Crash recovery rolls back an unvalidated generation
and rolls forward a validated or committed one. A rollout must not delete a
published generation because an already-open wrapper may still need its
siblings. Record generation count, generation bytes, and free disk for every
host; reader-aware generation garbage collection is a separate dry-run-only
follow-up.

Keep machine-global `token_command[0]` on that stable public `ghapp`
trampoline, never on an immutable generation member. If a v0.137-era install
left the command pointing directly at a generation wrapper, use `shipyard auth
helper-argv --wrapper <public-ghapp> --repo <owner/repo>` for the bounded
repair. It rewrites only that command argument after validating the complete
generation manifest and installed member digests, proving the live selector
still names the configured generation under the writer-domain lease, and
re-reading the configuration before publication. Selector drift, an
unmanifested or tampered member, or an invalid public trampoline refuses with
no mutation. The repaired TOML remains an owner-private regular file at mode
0600.

Native delivery also requires fresh exact-head/base-SHA GitHub App installation
authority and a live terminal checkpoint. cmux workspace moves preserve surface
identity; static labels and HerdR environment metadata do not grant authority.

Terminal reconciliation keeps terminal outcomes typed. A merged PR requires
its exact merge commit and merge timestamp. A PR closed without merging
requires the exact close timestamp and absent merge evidence; Shipyard records
that as `closed_unmerged` and never treats it as successful integration. Both
paths retain the exact head/base check and the second authenticated read under
writer custody before mutation.

| **Triage a steward exception without a resident agent** | `shipyard runner recovery-worker` inspects/revalidates one durable exact-head request without launching a model; add `--apply` for one bounded phase-1 classification attempt, or `--drain --apply` for the bounded current snapshot. Policy is machine-global only; Shipyard constructs a tool-disabled argv, clears the inherited environment, uses a global model lease and overall deadline, and accepts strict JSON that can classify/escalate but cannot authorize repairs, paths, or tests. Provider/quota failures terminalize; unsafe findings escalate; neither blocks unrelated PRs. |
| **Inspect or narrowly repair the canonical lifecycle ledger** | `shipyard work-ledger status --json` never creates storage and immutably supports the released authentic schema v11. `shipyard work-ledger inventory --json` returns a bounded, deterministically ordered identity view without creating, migrating, or taking writer custody of storage. A valid migrated legacy `NULL,NULL` repository identity makes `complete=false`; malformed or half-bound identity refuses the snapshot. Native `work-ledger publish` on authentic v11 emits typed per-row dry-run dispositions; apply revalidates the same snapshot under the exclusive writer domain, migrates, binds only the exact authenticated work ID/repo/PR/head/workstream, and requires exact replay. `work-ledger reconcile-terminal` inventories exact already-terminal repair targets plus every other unbound native terminal handoff as a clean publication precursor, managed-unbound row, or blocked row with bounded related-state counts and explicit blockers. An exact dispatching target is repairable only when one exact wake/delivery is durably uncertain and its activation is released; dry-run is write-free, while apply re-reads GitHub inside the writer transaction before atomically terminalizing and recording bound evidence. Only an exact terminal target can then be bound after reviewing the authenticated merged-head/base proof. Apply may add only the immutable provider receipt, binding, and terminal audit events; it preserves routes, wakes, continuations, ownership, custody, activation, and intents byte-for-byte, and exact replay is write-free. Foreign lineage, ambiguity, active work, drift, or incomplete authority refuses without mutation. `work-ledger import --json` remains a deterministic, redacted, no-write plan; add `--apply` only to populate the SQLite/WAL shadow idempotently. Legacy stores remain authoritative and untouched; activation and dispatch remain false. |
| **Query work at its exact custody host** | `shipyard --json work-ledger custody-inventory --message wm_<64hex> [--correlation-hints /owner-only/private.json]` selects only the source ledger's active custody rebind and reuses the protected forced-SSH subsystem. Accepted/processed custody returns `complete` or bounded `partial`; pending/claimed is `uncertain` without SSH; cancelled/superseded/missing/contradictory is `refused`. Optional owner-only no-follow Linear workspace/root UUID/provider-repository hints are output-only and never cross the wire or select authority. |
| **Provision, diagnose, or remove the custody carrier** | Prepare an owner-only contract-v1 manifest, then run `shipyard --json custody provision --input <private.toml>` before repeating with `--apply`; prove the installed generation with `shipyard --json custody doctor`. Doctor requires bounded include-expanded `/usr/sbin/sshd -T`, the exact `shipyard-custody-v1` receiver argv, the effective `AuthorizedKeysFile`, options-aware peer keys, directional identities, and all destination-bootstrap/native-publication/private-profile receipts. SSH setup alone is incomplete. Return default-off only with `shipyard --json custody disable --policy-digest <doctor-digest>` and then `--apply`; exact-digest removal holds a noncreating exclusive dry-run or exclusive apply snapshot, commits intent before config publication and an immutable receipt after exact config/custody-schema/history readback, and resumes only that globally chained generation after interruption. An unresolved intent blocks every provision until recovery; a supported identical reprovision receives a new chained disable generation. Active state, live/orphan SQLite sidecars, drift, schema topology mismatch, ambiguity, or malformed/replayed receipts refuse while unrelated config and all custody history remain intact. Never hand-edit the policy. See `docs/durable-custody-transport.md`. |
| **Plan macOS-first policy per repository** | `shipyard work-ledger policy set --repo owner/repo --primary-platform macos --compatibility-mode independent --compatibility-lane linux --compatibility-lane windows --blocking-rule declared_dependency_or_shared_integrity [--declared-dependency-lane linux] --expected-revision N [--apply]`. The primary platform and complete compatibility inventory are explicit; repeat the dependency flag only for an inventoried lane with a real artifact dependency. Repository identity is canonical lowercase across daemon arguments, handoffs, and policy lookup, even when the daemon is launched with GitHub's display-case spelling. Dry-run is the default; stale revisions refuse. Policy remains shadow-only until scheduler activation gates pass. |
| **Understand daemon shadow observation** | With an existing work ledger, the daemon observes policy-covered native nonterminal exact PR heads even with zero IPC subscribers; inert imported history is never scheduled. Relevant webhooks debounce for 2s with a 10s maximum burst age; overflow is requeued. An 8-target round-robin catch-up runs every 5m; exact-target cooldown, four-read concurrency, and a rolling-hour request ceiling with worst-case page reservation bound cost. Each exact target uses producer-provenanced pagination through exact-repository auth loaded only from trusted machine-global configuration, exhaustive through 1,000 contexts and fail-closed beyond; request evidence counts every page. Token-helper preparation is separately bounded and cached, and pre-command failures do not count as requests. Only changed snapshots and redacted fetch failure/recovery transitions emit `shadow_observation_transition` to IPC and the retained supervised daemon stderr log, including exact-head, API-cost, latency, policy-revision, and zero-model evidence. Repeated failures stay quiet. This phase cannot write GitHub, the ledger, or Linear; publish a wake; activate; or dispatch. |
| **Recognize an assigned-capacity dispatch wedge** | The daemon can publish one durable `dispatch_wedge` actionable wake after two stable observations prove that the exact current merge-queue job remains queued with no assigned runner while a compatible runner is online and idle. Authority binds repository identity, PR/head, merge-group head, run/attempt/job, labels, queue position, policy, and the final authenticated PR/queue reread. Head movement, regenerated/dequeued groups, incomplete pagination, label mismatch, busy/offline capacity, or ambiguous evidence refuses publication. Restart preserves the observation deadline and exact pending publication; a failed or capacity-less observation gets a bounded durable follow-up. This is diagnosis and escalation only: it does not cancel, requeue, mutate selectors or runners, reorder GitHub's queue, or retry work. |
| **Run sandbox E2E beside production Shipyard safely** | Each protected queue/supervisor/ship mutation holds the shared host-global writer-domain lease only for its critical section; streamed logs reacquire per append, while idle daemons and read-only commands own no lease. Sandbox E2E keeps the fair-entry turnstile and data-domain lease exclusive from snapshot through contamination assertion. A production mutation waits boundedly, then exits `75` with `sandbox_writer_domain_overlap` instead of racing evidence. If the guardian refuses before transition because production workers are active, classify the canary as safely deferred only through `scripts/sandbox_admission_deferral.py`; its exact receipt, installed hash, mutation-probe path, lease absence, daemon PID, and live process start identity must all agree at both workflow checkpoints. That admission deferral is INFRA/retryable and does not justify rerunning the production job. The macOS guardian observes only the exact no-holder/contended ambiguity through a bounded stable-idle window while continuously fencing production identity, binary, config, and active workers; any holder or drift still fails closed. If a corrected-path cleanup retains the host lease solely because workers appeared after its mutation proof, the launchd-owned guardian may reconcile it without a model: authenticate the unique prior ready/mutation/final receipts, preserve exact production identity and configuration while workers drain, prove stable idle again immediately before removing the exact device/inode/ctime plus cryptographically random lease generation, and publish a durable pending/terminal receipt. Removal first atomically detaches that generation under a unique tombstone name, then deletes only the revalidated detached identity; crash debris cannot authorize or block a later fixed-name lease and is retained for evidence rather than guessed away. Pre-generation legacy leases require explicit operator reconciliation; ambiguous, unexpectedly populated, live-owner, quiesced, identity-drifted, or foreign-holder cases remain untouched. A pending/deferred retained-lease result is only INFRA/retryable evidence, never physical-canary or release acceptance. After its terminal receipt proves the exact lease generation was removed, rerun only the targeted macOS Sandbox job; do not rerun the whole workflow or rebuild unchanged binaries. After cleanup, confirm daemon IPC liveness through a bounded exact-PID-fenced window and require the receipt's configured repositories rather than treating one status miss as death. The guardian must fsync its terminal receipt before self-unloading its exact launchd label; later runs may recover only a bounded set of inert, receipt-authenticated registrations, and terminal workflow paths must fail closed if a successful full inventory does not prove the current label disappeared. Keep failure artifacts inside explicit canary/runner temp roots. Do not add filename/PID/job-ID exemptions or delete either lock file. Restart v0.108.1 daemons during rollout because they hold the obsolete lifetime lease. |
| **Drain cloud-queued macOS jobs to local when a slot frees** | `shipyard runner reroute-watch [--apply] [--once] [--interval N] [--flap-window N]` (observe-only without `--apply`; logs per-host capacity + candidate list; flap-guard, one-reroute-per-tick, slot/fail-closed) |
| **Runner provisioning: deregister a runner** | `shipyard runner remove --name <repo>-<tag>-NN --yes [--purge-dir]`; removal must use the compound `svc.sh uninstall`, not `stop`, so the LaunchAgent registration is removed before GitHub deregistration |
| **Self-update: check if a new release is available** | `shipyard update --check --json` |
| **Self-update: apply latest stable** | `shipyard update` (governed machine-global auth; downloads the tag-matched installer completely before execution) |
| **Self-update and refresh daemon after verification** | `shipyard update --to vX.Y.Z --refresh-daemon` |
| **Self-update: pin / rollback to a specific tag** | `shipyard update --to v0.53.0` |
| **Self-update hits "rate limit exceeded"** | v0.68.0+ auto-uses `gh`/`GITHUB_TOKEN` auth; if still rate-limited (60/hr unauth, no `gh` login), run `gh auth login` or export `GITHUB_TOKEN` and retry. Not a missing-`.dmg` error. |
| **Stuck-runner: kill specific worker (with recovery)** | `shipyard runner kill --pid <pid> --reason "..." [--retrigger]` |
| **Stuck-runner: review past kills** | `shipyard runner kill --history` |
| **Stuck-runner: restore quarantined build after a misclick** | `shipyard runner kill --recover <event-id>` |
| Show logs for one target | `shipyard logs <job_id> --target windows` |
| Check merge readiness | `shipyard evidence --json` |
| Show latest command-evidence bundle | `shipyard evidence command --json` |
| Import recent GitHub Actions timing into runner metrics | `shipyard metrics import github --repo <owner/repo> --limit 20 --json` |
| Import tartci VM timing into runner metrics | `tartci runtime export --repo <owner/repo> | shipyard metrics import tartci --json` |
| Summarize runner timing history | `shipyard metrics summary --project <name> --json` |
| Show one bounded stewardship scorecard | `shipyard metrics scorecard --project <name> --since 30d --json` |
| Ask for agent-readable runner health findings | `shipyard metrics watch --project <name> --since 14d --json` |
| Compare local vs GitHub runner timing | `shipyard metrics compare --project <name> --baseline github-hosted --candidate macstudio --json` |
| Bump job priority | `shipyard bump <job_id> high` |
| Cancel a job | `shipyard cancel <job_id>` |
| List cloud workflows | `shipyard cloud workflows --json` |
| Show cloud defaults | `shipyard cloud defaults --json` |
| Dispatch a cloud workflow | `shipyard cloud run build --json` |
| Dispatch only if remote matches HEAD | `shipyard cloud run build --require-sha HEAD --json` |
| Opt a target into cross-PR reuse | set `reuse_if_paths_unchanged = ["src/backend/**"]` under `[targets.<name>]` |
| Opt a target into warm-pool reuse | set `warm_keepalive_seconds = 600` under `[targets.<name>]` (see "Warm-pool reuse" below) |
| Inspect warm-pool entries | `shipyard targets warm status --json` |
| Drain the warm-pool (force cold-start everywhere) | `shipyard targets warm drain --yes` |
| Force cold-start for one ship only | `shipyard ship --no-warm` (or `shipyard run --no-warm`) |
| Global warm-pool kill switch | `SHIPYARD_NO_WARM_POOL=1` in the environment |
| Retarget one lane on an in-flight PR | `shipyard cloud retarget --pr <n> --target macos --provider github-hosted` (dry-run; add `--apply`) |
| Add a new lane to an in-flight PR | `shipyard cloud add-lane --pr <n> --target windows [--provider github-hosted]` (dry-run; add `--apply`) |
| Rescue a PR whose runs are wedged on a self-hosted runner | `shipyard rescue <pr>` (preflights + dispatches a replacement before cancelling the old run; add `--dry-run` to preview, `--rerun-failed` for completed cancelled/failed/timed-out runs; omit `--to` to re-resolve a failed leg local-first, or pass `--to <provider>` to force) |
| Rescue every stuck run repo-wide | `shipyard rescue --all-stuck` |
| Same-PR ship refused by a killed worker (`SamePrShipRunning`) | v0.68.0+ auto-reaps the stale `running` queue job after ~180s — just retry `shipyard pr`. See the `shipyard` skill's "Legacy Queue Recovery: killed-worker stale-running reaping". Don't run two `shipyard pr`s for one PR concurrently. |
| PR stuck in-flight forever (never auto-merges after a host reboot / daemon crash) | `shipyard ship-state list` or `shipyard status` flags it `ORPHANED? [<evidence>]` — cross-referencing the queue: `queue_stale` (dead worker heartbeat) / `queue_terminal` (worker ended without finalizing) surface in ~3m; `queue_absent` / `time_fallback` are time-gated (default 45m, `[ship_state] orphan_stale_minutes`). A live or pending worker is never flagged. Re-run `shipyard ship <pr>` to re-validate (this clears any `abandoned` marker), or `shipyard ship-state discard <pr>` if truly dead. Detection is report-only; the daemon can optionally abandon a `queue_stale` orphan (so auto-merge stops waiting) via `[ship_state] auto_resume = true` (default off, fail-closed, never re-dispatches, re-reads the queue live under the per-PR lock so a concurrent re-ship is spared). See the `shipyard` skill's "Orphaned ship-state reporting". |
| Skip a version-bump gate | `shipyard pr --skip-bump sdk --bump-reason "docs only"` |
| Skip a skill-sync gate | `shipyard pr --skip-skill-update ci --skill-reason "mechanical"` |
| Deliberately skip one lane | `shipyard run --skip-target windows` (repeatable; no probe run) |
| Proceed with unreachable lanes (VALIDATION GAP) | `shipyard run --allow-unreachable-targets` (prints a loud warning; exits 3 without the flag) |
| Inspect tracked cloud runs | `shipyard cloud status --json` |
| Environment check | `shipyard doctor --json` |
| Probe SSH runner reachability | `shipyard doctor --runners --json` |
| Inspect GitHub REST + GraphQL rate-limit buckets (both separately) | `shipyard doctor --rate-limit --json` (`--repo OWNER/REPO` when remotes are ambiguous) |
| Inspect effective GitHub auth only | `shipyard auth doctor --json` (`--repo OWNER/REPO` when remotes are ambiguous) |
| Export/import GitHub auth config only | `shipyard auth export --output shipyard-auth.toml` / `shipyard auth import shipyard-auth.toml --scope local` (preserves typed ambient/privileged binary authority; import replaces only the auth table) |
| Explain log/artifact retention without mutation | `shipyard cleanup` (dry-run default; includes action reasons, protected evidence, and byte watermarks) |
| Apply bounded terminal-log retention | `shipyard cleanup --apply` (gzip closed logs; pressure-deletes successful terminal jobs only; honors `.shipyard-retain`) |
| Serialize an indefinite incident/audit pin | `shipyard cleanup --pin <job-id>` (do not raw-`touch` the marker while cleanup can run) |
| Wait for a release to fully upload | `shipyard wait release v0.23.0 --timeout 900 --json` |
| Wait for a PR's required checks to go green | `shipyard wait pr 151 --state green --timeout 1800 --json` |
| Wait for a workflow run to finish | `shipyard wait run 223344 --success --timeout 1200 --json` |
| Wait for a durable Shipyard job to pass | `shipyard wait job sy-20260901-example --success --timeout 1200 --json` |
| Mark a target advisory | `[targets.<n>] advisory = true` in `.shipyard/config.toml` (see "Advisory lanes" below) |
| Flip lane policy for one PR | `Lane-Policy: <target>=required\|advisory` trailer on the tip commit |
| List quarantined targets | `shipyard quarantine list --json` |
| Quarantine a flaky target | `shipyard quarantine add <target> --reason "..."` |
| Remove from quarantine | `shipyard quarantine remove <target>` |

The steward defaults to treating case-insensitive `5·unresolved` as an
unresolved-provenance authority blocker. It reports `provenance_blocked` and
makes no mutation until a current-head revalidation sees the label absent.
Repeat `--provenance-blocking-label <label>` for another explicit vocabulary.
The blocker precedes opt-out, and the final force-cancel boundary revalidates
current PR provenance and management authority even after a restart.

### Pulp zero-job recovery

Use `runner zero-job-recover` only when Pulp's exact `Build and Test`
`pull_request` run remains REST `queued` or `pending` with no conclusion for at least 45 minutes and exhaustive
`filter=all` inspection proves that it materialized zero jobs. Supply the exact
PR and run IDs; dry-run is the default. Apply mode first persists the
non-required `shipyard/zero-job-redispatch` status on the immutable head, then
re-reads the complete selector before its only second write: dispatching
protected-main `.github/workflows/build-macos.yml` with the PR, ref, exact head,
source run/attempt, recovery marker, and `github-hosted` runner input.

Apply is accepted only inside the live serialized protected-main `Shipyard
merge steward` Actions workflow. Shipyard verifies its exact workflow ref,
event, head, run ID, and attempt against GitHub both before the receipt and
immediately before dispatch. The receipt binds that controller run/attempt and
the full candidate fingerprint.

The command is Pulp-only and requires the same-repository open non-draft PR,
the managed label plus successful exact-head steward handoff, main's GitHub
Actions-owned required `macos` context, the exact workflow identity, exactly
one active same-head run, and no existing `macos` check or recovery receipt.
Forks, provenance/opt-out labels, truncated observations, head or governance
drift, and ambiguous writes fail closed. A receipt is spent before dispatch;
if the dispatch response is lost or fails, never retry that head. This command
does not cancel, rerun, enqueue, label, push, merge, coalesce, or mutate TartCI.

## tartci local VM routing profiles

When a repo uses tartci-backed local VM lanes, inspect the profile before
changing GitHub variables or dispatch inputs:

```sh
tartci profile explain normal-local-fast --repo Generous-Corp/pulp --json
tartci profile plan normal-local-fast --repo Generous-Corp/pulp --json
tartci status --json
```

tartci owns host-local facts: Tart/QEMU providers, capacity, golden/cache
state, and target-to-`runs-on` mappings. Shipyard owns fleet routing: read each
host's tartci status, choose one concrete target from the ordered fallback chain,
then apply that selector through repo variables or `workflow_dispatch`.

Do not pass a fallback chain into GitHub Actions. GitHub cannot change `runs-on`
after a job queues. Pulp workflows should receive one concrete selector per run.

For a routing PR whose external proof is incomplete, use the configured
per-PR opt-out label only after explicitly dequeuing the PR or disabling its
already-armed native auto-merge and confirming admission is gone. The label
prevents future steward admission; it does not disarm existing admission. A
repository-wide merge-queue hold is for incidents, not one routing PR.

For Pulp's normal fast profile, local ARM64 PR lanes are fast feedback and
GitHub-hosted nightly Intel Linux/Windows lanes are compatibility surveillance.
Windows QEMU on Apple Silicon is Windows ARM64; x64 MSVC/Prism execution is
smoke/debug until proven and should not replace `windows-latest` authority.
Coverage must use dedicated ephemeral labels, not warm bare-metal build pools.

Keep non-macOS failure work bounded when the active objective is macOS delivery.
Capture the exact head, run/job identity, failing test, and a short causal log
excerpt; allow one focused reproduction or targeted retry. If the result is
unrelated or intermittent, record it in the durable workstream and continue the
macOS-critical path instead of broadening the active PR. Keep the failure and
its repair lane-scoped: it may block only the artifact or release contract that
explicitly depends on that lane. Globally block unrelated lanes only for rare,
proven shared-integrity risks such as artifact corruption, schema
incompatibility, or false-green merge authority. Routine status monitoring is
model-free.

Make platform focus repository-scoped rather than a global Shipyard
assumption. Pulp, Forge, and Vellum currently treat macOS as the primary
delivery lane and Linux/Windows as independently repaired compatibility lanes.
Other repositories may select different primary, artifact-required, advisory,
or globally coupled platforms through trusted repository policy. Do not infer a
cross-lane block when that policy is missing or ambiguous; fail closed only for
the affected artifact contract and surface the configuration gap.

For local x64 Linux, keep selector policy in the checked-in `normal-local-fast`
profile and run the external Shipyard health operator documented in
`docs/pulp-local-linux-lease.md`. The trusted merge-group namespace renews
`PULP_LOCAL_LINUX_LEASE_UNTIL` only while the exact disposable Mac Pro selector
has idle capacity for the full live merge-queue admission burst after queued
reservations; all other observations clear the variable and new jobs fall back
hosted. Its first target must carry `pulp-auto-linux-x64` and its runner prefix
must be exactly `pulp-ci-ephemeral-`.

A future PR route must use the fully separate PR-safe tuple selected with
`--context pr`: `PULP_PR_SAFE_LINUX_LEASE_UNTIL`,
`pulp-pr-safe-ephemeral-`, and `pulp-pr-safe-linux-x64`. Shipyard rejects target
selectors that carry both capability labels. The PR-safe lane must remain
advisory because its declared burst is a reviewed capacity budget, not an
atomic or GitHub-enforced PR admission cap. Broad/near-miss prefixes and mixed
control tuples fail closed. Renewal also refuses any inventory where a
selector-eligible runner sits outside its approved prefix or carries the
opposite capability. Never reuse either lease for secret-bearing or
`pull_request_target` jobs.

### Survivability: will the lane still be there after an ordinary exit?

`fleet_service` answers *is this lane being served*. It cannot answer *will it
survive*, and the two come apart badly.

On 2026-09-05 this repository's only macOS runner was `online` and `busy` —
`Served`, correctly, by every existing assertion. A routine force-push tripped
`cancel-in-progress`, the cancel arrived at the runner as a signal
(`Runner will be shutdown for UserCancelled`), and it exited. It never came
back: not ephemeral, `RunAtLoad` with **no `KeepAlive`**, and not loaded in
launchd at all. The lane went from `Served` to permanently `Unserved` with no
state in between, while the host stayed up and kept serving three other
runners. The precondition was statically visible the whole time.

`fleet_supervision` reads that precondition. `Restartability` per runner
(`Supervised` / `SelfReplacing` / `Unsupervised` / `Unknown`), rolled up per
lane into `Survivability` (`Survivable` / `Fragile` / `SinglePointOfFailure` /
`Unknown`).

Two traps it is built around, both of which produce a confident false fault:

- **A LaunchAgent lives in the per-user GUI domain.** `launchctl list` over SSH
  prints nothing for a job that is loaded and running, so "not loaded" from an
  SSH session is not a fact. Query `launchctl print gui/$(id -u)/<label>`, and
  **always run it against a job you know is loaded first** — without that
  control an empty result is indistinguishable from a broken query. An
  unreadable domain is `Unknown { boundary: Scope }`, never a fault.
- **An empty runner census is a scope error until proven otherwise.** Zero
  runners yields `Unknown`, not `SinglePointOfFailure`; the org-vs-repo scope
  mistake already misled this fleet once.

`RunAtLoad` is not supervision. It starts a job once and says nothing about
what happens when it exits — which is exactly the plist that took the lane out.

## Runner Metrics For Agents

Runner metrics are optional and provider-neutral. Use them when an agent needs
historical context before changing CI routing, cache policy, or monitoring
cadence. Shipyard owns the local SQLite store and query surface; tartci, GitHub
Actions, local commands, SSH targets, or other VM managers can feed the store.

For GitHub-hosted history, import recent job timings:

```sh
shipyard metrics import github --repo Generous-Corp/pulp --limit 50 --json
shipyard metrics watch --project pulp --since 14d --json
```

For tartci VM history, export runtime records from tartci and import them into
Shipyard:

```sh
tartci runtime export --repo Generous-Corp/pulp |
  shipyard metrics import tartci --json
shipyard metrics summary --project pulp --json
shipyard metrics scorecard --project pulp --since 30d --json
```

The `summary`, `scorecard`, `watch`, `advise`, and `compare` commands return
structured JSON intended for agents. `scorecard` is the concise project-level
view; it reports telemetry that Shipyard does not collect as `unavailable`
rather than inventing a value. Treat insufficient-sample findings as "keep
collecting", not as proof of a regression. Escalate only when the finding
includes enough samples and a material delta for that repo/lane.

When debugging GitHub imports, remember that Shipyard invokes `gh api` with
absolute `/repos/...` paths and forces `-X GET` when query parameters are passed
with `-f`; without `-X GET`, `gh api -f` can POST and produce misleading 404s.

## GitHub Auth Diagnostics

Before blaming ambient `gh auth status`, check whether the repo config has
`[github.auth]`. Shipyard can inject env or command-helper tokens into its
built-in `gh` subprocesses as `GH_TOKEN`, including helpers that mint GitHub
App installation tokens. `shipyard doctor --rate-limit --json` reports the
effective source and rate-limit buckets. For GitHub App or fine-grained tokens,
permissions may not be locally inspectable, so verify Actions: Read and write
on the token/App when cloud retarget or handoff fails with auth/scope errors.
That doctor command actively resolves configured auth, so command helpers may
run and GitHub App helpers may mint installation tokens.

The `github-auth` doctor row distinguishes a context-dependent placeholder from
a genuinely broken source (presentation only — operational auth still never
silently falls back). A `token_command` using `{repo_slug}`/`{repo_name}` that
can't resolve in a repo-less context (`doctor`) reads as **green** with a
hint to pin `--repo <owner>/<name>` for account-wide Apps, because it resolves
normally inside a repo. The **daemon** resolves `{repo_slug}` from its served
`--repo` (the registrar hints it), so live-mode webhook registration mints a
token from a repo-less CWD instead of failing on "placeholder requires
remote.origin.url" (which left live mode stuck on "updates paused"). Any other
resolution failure stays **red** and now tells
gh-only users they can simply drop `[github.auth]` to use ambient `gh`. The
`nsc` row is likewise optional: green "not configured (optional)" unless a
Namespace provider is configured (`cloud.provider` or a per-target `provider`).
The `gh-scope` row is green-informational for configured Env/App/helper tokens
(whose scopes can't be inspected locally) — same treatment as a fine-grained/app
token under ambient `gh` — keeping the "verify Actions: Read/write" reminder in
detail rather than showing a red ✗ that only the rare configured-token user sees.

GitHub App installation tokens are the preferred path for high-volume
inspection because Shipyard injects them into its built-in `gh` subprocesses
and REST/GraphQL fallback paths. Do not silently fall back to ambient user auth
for polling, watch, retarget, or diagnostics. Ambient auth is restricted to
documented low-volume mutations after the exact App integration-permission
denial: pull-request creation after both GraphQL and REST fail, and steward
handoff writes. Shipyard removes `GH_TOKEN` and `GITHUB_TOKEN` and selects a
direct native `gh`, skipping script/wrapper shims. If PATH discovery is not
appropriate, configure an absolute native `github.auth.ambient_gh_binary`;
never point it at a `ghapp` wrapper.
PR merge stays on the configured token and native merge queue. A GraphQL probe
failure may use REST for read-only identity recovery, but never as automatic
classic merge authority.

When one App has installations on multiple accounts, require an exact
`{repo_slug}` in the configured token command. The helper must resolve that
repository's installation; a fleet-wide fixed installation id is not valid
routing. Shipyard caches helper results under the expanded repo-specific argv,
and absent repo provenance must fail closed. Preserve an absolute policy-pinned
`ghapp` wrapper as `token_command[0]` and update its implementation in place.
Shipyard's remaining argv must select the wrapper's `token --repo {repo_slug}`
mode; the audited `shipyard-v1` tartci CLI commands and guards remain intact.
The wrapper is not a general `gh` drop-in; use ambient native `gh` for an
operator command outside its explicit command/subcommand/flag grammar. Keep
the App key and cache current-user-owned at `0600` inside current-user-owned
`0700` directories, and keep token material in environment/stdin rather than
process argv. Run live-evidence guards with that exact repo-routed App token,
after mint/cache resolution but before native `gh` executes the command. Bind
their `GHAPP_REAL_GH`/merge probe to the wrapper's selected native binary, and
dispatch the PR-close guard for every command so its REST/GraphQL/issue aliases
cannot bypass inspection.

The bounded App-authenticated publication exception is
`ghapp release upload <tag> <files>... --repo OWNER/REPO`. It accepts only an
existing release, stable private snapshots of non-symlink regular files, and
an explicit repository; `--clobber`, lifecycle creation/deletion, and tag
retargeting remain outside the privileged grammar.

## Supervised-Push Signal (`SHIPYARD_PR_RUNNING=1`)

Every `git` / `gh` subprocess spawned by `shipyard pr` / `ship` /
`auto-merge` / `overflow` / `wait` runs with `SHIPYARD_PR_RUNNING=1`
in its environment. Consumer-side pre-push hooks (notably
[`danielraffel/pulp#1406`](https://github.com/danielraffel/pulp/pull/1406))
use this to differentiate a Shipyard-orchestrated push (full local
validation, version-bump gate, etc.) from a raw `git push` that
bypasses those gates and turns CI into the discovery channel.

Quick smoke from a checkout that wants to verify the hook side:

```sh
SHIPYARD_PR_RUNNING=1 git push --dry-run    # what shipyard pr looks like to the hook
unset SHIPYARD_PR_RUNNING ; git push --dry-run    # what a raw push looks like
```

The marker is set inside `src/supervised.rs` and routed through
every supervised spawn site. Diagnostic subcommands (`doctor`,
`pin`, `runner`, `cleanup`) intentionally do not set it. See
`skills/shipyard/SKILL.md` → "Supervised Subprocess Marker" for the
helper API.

Supervised pushes also use an OpenSSH server-alive probe when the caller has
not supplied `GIT_SSH_COMMAND`. Git opens its transport before invoking the
consumer's pre-push hook; without keepalive traffic, an hour-long local gate can
finish successfully only to find GitHub closed the idle connection. Preserve a
caller's explicit SSH command rather than replacing its identity/proxy policy.

## Runner Provider Defaults

Shipyard's own workflows default to GitHub-hosted runners for Linux, macOS, and
Windows. Namespace is optional and account-dependent; do not assume `nsc` or
Namespace capacity is available. If a workflow or repo variable still points at
Namespace during an outage/account-expired period, set
`DEFAULT_RUNNER_PROVIDER=github-hosted` or pass `-f runner_provider=github-hosted`.

Explicit `*_runner_selector_json` workflow-dispatch inputs can still route
trusted jobs to self-hosted GitHub Actions runners, such as a local Mac or SSH
VM fleet. Do not add hidden repo-variable fallbacks that silently override the
GitHub-hosted default; a trusted self-hosted run should be an explicit per-run
choice. GitHub dispatches by `runs-on` labels; SSH is only the management layer
for those machines.

### The `local` provider (self-hosted Mac)

`scripts/ci_matrix.py` recognizes a third provider, `local`, alongside
`namespace` and `github-hosted`. Set it the same way — repo variable
`DEFAULT_RUNNER_PROVIDER=local` or per-dispatch `-f runner_provider=local`.
It routes the **macOS ARM64** leg to the maintainer's self-hosted Mac via the
built-in label set `["self-hosted","local-mac"]`; Linux and Windows have no
local box, so they transparently degrade to their GitHub-hosted labels (the
resolved `provider` for those rows reports `github-hosted`). Override the macOS
selector with repo var `LOCAL_MACOS_ARM64_RUNS_ON_JSON` if a different label set
is needed. An explicit `*_runner_selector_json` input still wins over the
provider default. This is *not* a hidden fallback — `local` only takes effect
when explicitly requested, and the default remains GitHub-hosted.

To land jobs on the Mac, register a runner carrying the matching labels with
`shipyard runner register --repo <owner/repo> --labels self-hosted,macos,arm64,local-mac`
(see the runner-provisioning rows above). This is the mechanism behind routing
macOS **release** builds to the Mac Studio so they skip GitHub's hosted-macOS
queue — the Studio's keychain already holds the Developer ID signing identity.
Use `local` only on private repos / the owner's own machine, never a public repo
with untrusted PRs.

The tag release's CI signing step may temporarily add an imported Developer ID
keychain. It must snapshot and verify the user-domain default keychain and
complete search list without mutating either; pass the ephemeral keychain
directly to `codesign`. Its `always()` cleanup deletes the ephemeral keychain.
Never parse `security list-keychains` with line-based quote stripping or make a
CI signing keychain the persistent runner user's default.

`codesign --keychain` still requires that identity keychain to appear in the
calling process's user search list. Configure that list under the release
step's isolated temporary `HOME` and pass the same `HOME` only to `codesign`;
never add the ephemeral keychain to the persistent runner user's search list.

For an unattended local macOS release, use
`./scripts/release-macos-local.sh --check-auth` before the real release. It
auto-loads the standard `~/.config/pulp/secrets/{keychain,notary}.env` files,
imports the file-backed P12 into a disposable keychain, applies the full
`apple-tool:,apple:,codesign:` partition list, temporarily places that
keychain first, and proves a hardened-runtime timestamped signing operation.
The normal release runs the same gate automatically and notarizes with the
App Store Connect P8. Never continue to `codesign` after this gate fails, use
a persistent/login keychain as a local fallback, or ask for a keychain
password. Cleanup restores the exact prior search list before deleting the
disposable keychain; if restoration cannot be proven, fail and preserve the
keychain file rather than leave a dangling search-list reference.

Shipyard releases from v0.127.0 onward are a binary pair. Keep the historical
`shipyard-<platform>` asset names, add matching
`shipyard-workstream-provider-<platform>` assets on Linux and Windows, and put
both signed binaries in the single macOS DMG. Packaging and mounted-DMG smoke
must run both `--version` probes before publication.
Publication also requires exact pair-version equality, every Linux/Windows
companion asset and checksum entry, plus a public installer E2E; `--ci-mode`
cannot bypass these closure gates, and failure must re-draft the release.
On Windows the asset filenames retain `.exe`, but version output uses the
logical names `shipyard` and `shipyard-workstream-provider`; pair comparison
must not treat the suffix as part of the semantic-version authority.

### External contribution execution

Never route contributor-controlled revisions through Shipyard's normal local,
SSH, host-pool, cloud/self-hosted, or fallback dispatchers. Those are execution
providers, not isolation boundaries. Use the dedicated external-contribution
review workflow in `skills/review-external-contributions/SKILL.md`; if its
disposable VM lane is unavailable, the request blocks and does not fall back.
Treat Git hooks as execution too: an external-derived branch must not trigger a
maintainer-workstation configure, build, generator, or test hook.

## Live mode (`shipyard daemon`) — when it helps and when to ignore it

Shipyard has a long-running webhook receiver that converts GitHub
Actions events into a push-based event stream. When it's running,
`shipyard watch` can subscribe to the daemon instead of polling —
near-realtime updates with zero GitHub API budget spent on the watch
itself.

| You're here | Does live mode matter? |
|---|---|
| Solo macOS dev with Tailscale + Funnel enabled | **Yes, big win.** `shipyard daemon start` registers webhooks on tracked repos and streams events; the macOS menu-bar app and any `shipyard watch` invocation in a terminal both consume the same stream. |
| CI / headless server / someone without Tailscale | **Ignore it.** The daemon needs a public tunnel (Tailscale Funnel in v1) to receive webhooks. Without that, `shipyard watch` and everything else fall back to polling — behavior is unchanged from the pre-daemon CLI. |
| Agent running one-shot `shipyard ship` + `watch --follow` | **Probably doesn't matter.** The daemon helps most when multiple sessions or the GUI are tracking the same state concurrently; a single session blocking on `watch --follow` already has its own connection. |

On macOS, keep Shipyard's noninteractive Tailscale subprocess environment intact.
The app-bundle CLI needs `TERM=dumb` when a LaunchAgent or stripped SSH shell
provides neither `TERM` nor `TERM_PROGRAM`; otherwise it can attempt to launch
the GUI and return non-JSON even though Tailscale itself is online. Diagnose the
daemon environment, not an interactive shell that may mask this fleet failure.

**When in doubt, don't start the daemon.** The daemon is an
optimization, not a requirement. Polling is the correct fallback
for everything it doesn't cover and is always safe. The `run` /
`ship` / `watch` / `auto-merge` commands don't require the daemon
to be running.

`shipyard daemon status` is free (no `gh api` calls, just reads
the local socket) and cheap to probe from an agent — use it if
you want to know whether the user has live mode on before
deciding whether to rely on webhook-speed updates vs polling
cadence.

**Idle behavior (v0.56.0+):** when no IPC subscriber is attached
(no `shipyard watch` running, no GUI), the daemon skips the
periodic `gh` reconcile poll. Webhooks still update state in real
time, so correctness is unchanged — the daemon just doesn't burn
GitHub REST budget for ticks no one is watching. The reconcile
resumes the moment a subscriber attaches. Webhook registration
also retries on a 5-minute backoff after failure rather than every
loop iteration.

Reconciliation is transport-scoped: GitHub rollups may update only runs with
numeric GitHub Actions workflow-run IDs. Local and SSH `sy-*` runs keep their
own terminal evidence even when a similarly named hosted check is green. If a
local failure and hosted success disagree, classify the local log; do not
expect daemon reconciliation to turn it green.

See [`docs/live-mode.md`](../../docs/live-mode.md) for setup (≈1
click on a Tailscale-ready Mac) and troubleshooting. The macOS
menu-bar app (`shipyard-macos-gui`) is a thin subscriber to this
same daemon.

## When to use `watch` (agent decision guide)

After dispatching a ship (`shipyard ship`), agents have four ways to
track it to completion. Pick by **session posture**, not by how long you
think the build takes:

| Posture | Command | Why |
|---|---|---|
| You can hold the session open until merge | `shipyard watch --follow --json` | Blocks; exits `0` pass, `1` fail, `130` SIGINT. Zero polling logic needed. |
| You want to release the session, re-check later | `shipyard watch --no-follow --json` + `ScheduleWakeup` | One-shot snapshot is cheap. Re-check on wakeup; exits `3` while in-flight. |
| The agent is stepping away entirely | `shipyard auto-merge <pr>` on cron / GitHub schedule | Idempotent one-shot. Exits `0` merged, `1` fail, `2` not-found, `3` in-flight or natively enqueued. |
| You just want a status peek right now | `shipyard watch --no-follow --json` | Same as a `ship-state show` but uses the live event schema. |

**Rules of thumb for agents:**

- If you just ran `shipyard ship` in the same turn and the user is
  waiting, `shipyard watch --follow --json` is almost always right —
  you already own the session.
- If you'll need more than ~5 minutes and want to yield back to the
  user, prefer `--no-follow` + `ScheduleWakeup`. Don't `sleep` inside
  the session.
- **Never poll with `watch --follow` in a tight loop.** `--follow`
  already blocks; calling it repeatedly is wasted cache and clock.
- `auto-merge` is for out-of-session automation (cron, systemd timer,
  GitHub Actions schedule). Not a substitute for `watch` within a live
  agent session.
- `auto-merge` and `wait pr` use REST for read-only identity when GraphQL is
  rate-limited. `gh pr view --json` may fall back to
  `GET /repos/:r/pulls/:n` + `GET /repos/:r/commits/:sha/check-runs`.
  Shipyard never uses `PUT /pulls/:n/merge` as a classic automatic fallback;
  that path returns `automatic-merge-refused` (exit 10). REST
  has its own 5000/hr bucket, separate from GraphQL. Agents do not
  need to hand-roll `gh api` calls anymore. Check both buckets with
  `shipyard doctor --rate-limit --json`. A green verdict additionally
  requires a successful `gh pr checks --required --json` classification;
  `statusCheckRollup` alone does not expose requiredness. If that
  classification is unavailable, including on the REST snapshot path,
  `wait pr --state green` fails closed with exit 7 rather than guessing.
  Snapshot output carries `_rest_fallback: true` when the fallback path
  served the value.

Example — agent blocks until merge in-session:

```sh
shipyard ship --json
shipyard watch --follow --json   # exits when ship completes
```

Example — agent yields, re-checks later via `ScheduleWakeup`:

```sh
shipyard ship --json
shipyard watch --no-follow --json | jq '.state'
# → "in_flight" → ScheduleWakeup 20m, re-run the same snapshot
# → "passed"    → done
# → "failed"    → inspect logs
```

### Reading rich watch output

`shipyard watch` (human mode) shows per-run elapsed time, heartbeat
age (`last_seen=12s_ago`, tagged `stale` when > `WATCH_STALE_SECS`,
default 90s), a progress summary (`2/3 targets complete`), color +
symbols (`✓`/`✗`/`⋯`), and a timestamp separator between snapshots.
Honors `NO_COLOR=1` (XDG) for piped output. JSON mode adds
`last_heartbeat_at`, `phase`, and `elapsed_seconds` fields to each
dispatched-run emission; existing consumers keep working.

When a runner's durable heartbeat goes stale past the configured threshold,
`FallbackChain` auto-demotes it to UNREACHABLE and continues with the next
provider. A process that remains alive and heartbeating but emits no output is
reported as `quiet`; output silence alone never authorizes failover or an infra
classification.
Use `shipyard doctor --runners` to probe SSH targets without running
a ship.

## Mid-flight runner retargeting

When a provider change would be valuable *during* an in-flight PR drain — e.g., you need to move a lane from an unavailable paid pool back to GitHub-hosted — use `shipyard cloud retarget`:

```sh
# Preview first (dry-run by default):
shipyard cloud retarget --pr 224 --target macos --provider github-hosted

# Apply when the plan looks right:
shipyard cloud retarget --pr 224 --target macos --provider github-hosted --apply
```

What it does:
1. Finds the PR's latest workflow run.
2. Cancels the **one job** matching `--target` on the old provider (substring match on the job name, e.g. `macos` matches `macOS (ARM64) [github-hosted]`). If every active job in the run matches that target, Shipyard can safely fall back to cancelling the whole run.
3. Dispatches a fresh workflow run with the new provider.

Cancellation failures are fail-closed. If GitHub denies or cannot find the
job/run, Shipyard does **not** dispatch a replacement. It reports
`event=cancel_failed`, classifies the failure (`auth`, `scope`, `not_found`,
`unsupported`, `transient`, `unknown`), includes the run/job URLs, and prints
manual recovery steps. Do not treat a standalone `workflow_dispatch` as an
equivalent fallback unless the workflow/check integration is known to satisfy
the same required PR check context.

**Known limitation (read before running):** step 3 starts a new workflow run, so targets other than the one you retargeted will also re-run in that new run. Their *prior* pass/fail statuses persist on the PR's check rollup, and pulp-style `resolve-provider` matrix workflows reuse caches — so the net effect is "flip the lane" without losing ground on the other lanes, even though they technically re-execute.

## Mid-flight lane addition

Sibling to retarget. Use when a ship is already in flight and you realize you want to validate against an *additional* platform without cancelling and re-dispatching the whole matrix — e.g., you started with `[macos, linux]` and want to add `windows`:

```sh
# Preview (dry-run by default):
shipyard cloud add-lane --pr 224 --target windows

# Apply when the plan looks right:
shipyard cloud add-lane --pr 224 --target windows --provider github-hosted --apply
```

What it does:
1. Loads the PR's ShipState. Refuses if absent (no in-flight ship) or terminal (merge already issued).
2. Idempotent: if the target is already in `dispatched_runs`, reports a no-op and does nothing.
3. Dispatches the single workflow for that target/provider.
4. Appends a new `DispatchedRun` to the ShipState so the watch loop joins it into the overall verdict.

See `docs/cloud-retarget.md` for full context; add-lane complements retarget.

## Before blaming a lane: a runner census needs both scopes and a control

A job stuck `queued` on self-hosted labels has three very different causes that
produce the *same* empty runner census, so read the census correctly before
concluding anything:

- **Query both scopes.** `repos/{owner}/{repo}/actions/runners` omits
  org-registered runners entirely. If a lane's runners register at the org, the
  repo census returns empty whether the lane is healthy or dead — and a session
  has already reported "this lane has no server" from exactly that reading. Pair
  it with `orgs/{org}/actions/runners`.
- **An empty census is not a fault on its own.** A just-in-time pool at rest
  registers nothing. Only queued demand distinguishes *unserved* (aged demand,
  nothing advertises the labels — unschedulable, it will wait forever) from
  *idle* (nothing registered, nothing asking). An issue was filed on this
  confusion and closed as wrong.
- **Pair the absence with a control that must return non-zero.** Run the same
  query against a label you know is served; if the control also comes back
  empty, the instrument is broken and the finding is worthless.

If runners *do* advertise the labels and jobs still sit, that is starvation
(scheduling or capacity), not routing — `shipyard rescue` below, not a variable
edit. Background: `docs/runner-watchdog.md` § Fleet service assertions.

## Rescuing wedged runners (`shipyard rescue`)

Use this when a self-hosted runner has wedged — orphaned `Runner.Worker`
process, queued runs sitting >30m, repo PRs all in
`mergeable_state=blocked` — and you need to move the work to a different
provider in one shot:

```sh
# Most common case: one PR is stuck. Rescue it (omit --to → provider is
# resolved per candidate; see below):
shipyard rescue 286

# Preview without acting:
shipyard rescue 286 --dry-run

# Also re-dispatch completed runs that ended cancelled / FAILED / timed-out
# (e.g. a flaky required leg, or a watchdog-cancelled run):
shipyard rescue 286 --rerun-failed

# Repo-wide: rescue every queued run older than 30m:
shipyard rescue --all-stuck

# Force a specific destination provider (e.g. pin a re-run to local):
shipyard rescue 286 --rerun-failed --to local
```

Rescue is fail-closed to `pull_request` and `merge_group` runs, including the
PR-targeted form: branch equality alone is not cancellation authority.
`Release CLI` and `Sign and Release` are protected by workflow name and
filename in both repo-wide and PR-targeted rescue. Use an exact-run release
operation for push, schedule, tag, or `workflow_dispatch` runs.

What it does:
1. Resolves the PR's head branch (skipped under `--all-stuck`).
2. Lists queued workflow runs and filters to (a) the PR's branch and (b) ones older than `--threshold` (default `30m`).
3. With `--rerun-failed`, additionally pulls `status=completed` runs whose conclusion is `cancelled`, `failure`, or `timed_out` on that branch (#345 — previously cancelled-only, so a plain failed leg was never a candidate). Once a replacement dispatch is accepted, the terminal original remains untouched. Never re-arm a terminal run merely to cancel it: GitHub can accept the rerun before it becomes cancellable, producing HTTP 409 and duplicate work.
4. For each candidate, proves that its workflow declares `workflow_dispatch`, resolves every required dispatch input, and submits the replacement **before** cancelling a still-queued old run. Terminal originals are not mutated. Known PR-number inputs (`pr`, `pr_number`, and `pull_request_number`) are filled from the PR argument. A workflow with no dispatch trigger, an unknown required input, or a rejected dispatch is reported as `skipped-no-plan`/`failed` and its original run is preserved. **Provider resolution is kind-aware when `--to` is omitted (#345):** a wedged *stuck-queued* run falls back to `github-hosted` (move off the stuck local runner), while a re-run *failed* run RE-RESOLVES the provider (config/default — local-first with overflow) so a leg that overflowed to a GPU-less hosted runner can return to a real local runner. An explicit `--to <provider>` forces the destination for any candidate.
5. Emits a per-run summary (`applied`, `replacement-applied`, `planned`, `skipped-completed`, `skipped-no-plan`, `failed`) with a top-level `event=cloud.rescue` JSON envelope under `--json`.

**Do not reach for `runner-watchdog.sh --fix` instead of `shipyard rescue`.**
The watchdog's cancellation registers as required-check `failure` on the PR
without redispatching — it makes the wedge look terminal to branch
protection. `shipyard rescue` is the safe primitive because it fail-closes
before cancellation and uses a replacement-first transaction. A rejected or
unconstructable dispatch leaves the original run untouched; a queued-run
cancellation failure after an accepted replacement can create duplicate work,
but never zero work. Terminal originals are never re-armed. There are no
destructive ops on the runner host itself.

`shipyard rescue` is the discoverable surface for what was previously a
5-step recipe (`gh api` + `cloud handoff list-stuck` + per-run
`cloud handoff run --apply`). Both `cloud handoff list-stuck` and
`cloud handoff run` remain available for cases where you need to operate
on a specific run ID outside the PR-scoped flow.

### Preventing wedges: `runner watch --kill-hung-workers`

`shipyard rescue` recovers from a wedge after the fact. The companion
preventive surface is the auto-kill mode of `runner watch`:

With `[host_class.*]` configured, `runner watch` also runs read-only fleet
liveness by default. Consume its stable reason codes: `NORMAL_SERIAL_WAIT`
means a follower is not blocked; cleared enrollment and optional/superseded
capacity theft require attention. Never infer a wedge solely from an unchanged
follower queue position.
Fleet liveness also reports every registered runner, Tart disk admission
headroom, ccache actual versus configured maximum, and merge-group Linux jobs
left on `ubuntu-latest` while compatible self-hosted capacity is idle. Declare
metal or planned machines under `[runner.fleet.expected_host.<name>]` with a
required `labels` array, optional `min_online` (default 1), and `active = false`
for visible future inventory that should not alert yet. Active absent/offline
machines fail visibly as `expected_host_unavailable`, including machines that
have not completed runner registration.
The watcher resolves the repository default branch. For a different merge
target, pass `--fleet-base <branch>` or configure
`runner.watchdog.fleet_base`.

```sh
# Daemon mode that auto-cancels stale queued runs AND auto-kills hung Workers
# whose etime exceeds the watchdog threshold (default 90 min):
shipyard runner watch --kill-hung-workers

# Adjust the threshold (e.g. for long-running iOS builds):
shipyard runner watch --kill-hung-workers --interval 300
```

What it does on every tick (default every 5 min):

1. Calls the same `assess_runner` logic `runner status` uses.
2. If `Symptom::HungWorker` fires, enumerates local `Runner.Worker`
   processes via `ps`, finds those whose etime exceeds the
   `runner.watchdog.max_job_min` threshold, and invokes the same
   recovery sequence as `shipyard runner kill --pid <pid> --yes`:
   snapshot → SIGTERM → grace → SIGKILL → reap children → quarantine
   partial builds → verify `Runner.Listener` → optionally wait for
   GitHub status to flip.
3. `--fix` is implied — stale queued runs are cancelled in the same
   tick so neither the host process nor the Actions side is left
   wedged.
4. Emits `runner.watch` JSON envelopes with `event=auto_kill_worker`
   and per-PID `phase` ∈ {`attempt`, `killed`, `failed`,
   `no-pid-found`} under `--json`.

Run it as a launchd/systemd service for prevention; pair with
`shipyard rescue <pr>` for the after-the-fact PR rescue path. Together
they replace the legacy `runner-watchdog.sh --fix` workflow that today
masks wedges as required-check failures.

### Reaping stale workflow runs: `runner watch --reap-stale-runs`

`--kill-hung-workers` reaps hung *processes* on the runner host.
`--reap-stale-runs` is the **run-level** complement: on every tick it
lists the repo's GitHub Actions runs and cancels genuinely-stale ones
repo-wide — including runs on **GitHub-hosted** runners, which the
process-level reaper cannot see.

Its cancellation authority is limited to `pull_request` and `merge_group`
runs. `Release CLI` and `Sign and Release` are never reaper candidates; push,
schedule, tag, and `workflow_dispatch` runs require an exact-run operation.
They are still emitted as protected `skipped` observations by the stale-run
reaper, including outside dry-run mode.
Protected stale runs remain visible in status/dry-run output even though the
mutating command skips them.
Human output labels their policy state; JSON exposes `cancellation_safe` and
`protected_run_ids` for automation.

```sh
# Auto-cancel stale workflow runs on every tick:
shipyard runner watch --reap-stale-runs

# Preview only — log what would be cancelled, cancel nothing:
shipyard runner watch --reap-stale-runs --dry-run --json

# Override thresholds (minutes):
shipyard runner watch --reap-stale-runs \
  --reap-in-progress-max-min 240 --reap-queued-max-min 360
```

What it cancels on every tick:

1. Runs stuck `in_progress` longer than `--reap-in-progress-max-min`
   (default ~5h) — hung runs squatting until GitHub's 6h timeout.
   Age is measured from `run_started_at` (execution start), **not**
   `created_at`, so a run that sat queued for hours before starting is
   not mistaken for hung; when GitHub omits `run_started_at` the
   computation falls back to `created_at`.
2. Runs stuck `queued` longer than `--reap-queued-max-min` (default
   ~8h) — orphaned runs waiting on a runner label/branch that no longer
   exists, which never hit any `timeout-minutes`. A queued run never
   started, so its age is measured from `created_at`.

Both status queries are paginated (`per_page=100`, up to 5 pages each),
so busy repos with more than one page of `queued` / `in_progress` runs
are fully scanned and the oldest entries are never missed.

Thresholds are deliberately well past any healthy run, so an in-flight
Shipyard validation run is never touched. Configure persistent defaults
in `[runner.watchdog]` (`reap_in_progress_max_min` /
`reap_queued_max_min`). Emits `runner.watch` JSON envelopes with
`event=reap_stale_run` and `phase` ∈ {`attempt`, `cancelled`, `failed`,
`skipped`} (`skipped` only under `--dry-run`).

## Waiting on conditions (`shipyard wait`)

Whenever you'd otherwise write a polling loop around `gh` — wait for a release to upload, wait for a PR's required checks to go green, wait for a dispatched workflow run to finish — reach for `shipyard wait` instead. It opens a daemon subscription first (if one's running), takes one authoritative `gh` snapshot, and either exits 0 immediately or keeps re-evaluating immediately on relevant webhook events. While the daemon remains connected, it also reconciles an authoritative snapshot on `--poll-interval` so a missed event cannot strand the wait; this remains daemon transport, not fallback. When the daemon isn't running or disconnects, it falls back to polling transparently — safe to use on headless CI too.

The waiter does not drop ownership on a brief token-helper or network
preparation failure. It retries only classified transient failures with bounded
backoff inside the existing `--timeout` and reports the count as
`transient_errors`; permanent credential/configuration failures still exit
immediately.

For `--state green`, Shipyard reads the authoritative required-check policy from
both classic branch protection and evaluated repository rulesets, then uses
`gh pr checks --required` only to observe which policy entries have actually
materialized and their state. A policy-required context that has not appeared
is emitted as `PENDING`; it is never silently omitted. Never infer completeness
from the raw `gh pr view --json statusCheckRollup` payload or from the subset
returned by `gh pr checks --required`. If the policy cannot be read, Shipyard
exits 7 and does not report green.

When every materialized required check for the observed exact head is terminal
and at least one failed, `wait pr --state green` exits 4 immediately with that
head and its check observations. It does not spend the rest of the timeout
waiting for an external rerun. A still-active or unknown required check keeps
the subscription open, and a moved PR head is evaluated only from its fresh
authoritative snapshot.

### Before/after

| Before | After |
|---|---|
| `for i in {1..60}; do status=$(gh run view 22345 --json status -q .status); [ "$status" = "completed" ] && break; sleep 20; done` | `shipyard wait run 22345 --success --timeout 1200 --json` |
| Treating a missing queue-job log or GitHub run as completion | `shipyard wait job sy-20260901-example --success --timeout 1200 --json` |
| `while ! gh release view v0.23.0 --json assets -q '.assets\|length' \| grep -q '^5$'; do sleep 10; done` | `shipyard wait release v0.23.0 --timeout 900 --json` |
| `gh pr checks 151 --watch` (blocking; no structured output) | `shipyard wait pr 151 --state green --timeout 1800 --json` |

### Detection gate (when to use it vs hand-rolled `gh`)

Only use `shipyard wait` when:

1. `command -v shipyard` succeeds (binary is installed).
2. The project has `.shipyard/config.toml` **or** `tools/shipyard.toml` (i.e. opted in to Shipyard).

If either check fails, fall back to `gh run watch` / `gh pr checks --watch`.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | condition matched |
| 1 | `--timeout` elapsed |
| 4 | A requested success became impossible: `wait run/job --success` reached a terminal failed conclusion, or `wait pr --state green` observed all exact-head required checks terminal with at least one failure |
| 5 | invalid input (PR/release/run/job not found, bad tag, wrong ID class) |
| 6 | daemon unreachable + snapshot didn't match + `--no-fallback` |
| 7 | unsupported scope — rulesets / merge-queue governance detected; switch lanes or do it manually |
| 130 | SIGINT / SIGTERM |

Transient snapshot retries do not extend `--timeout`: credential preparation
and the `gh` subprocess are bounded by the remaining overall budget, and no new
attempt starts after the deadline. JSON `transient_errors` remains accurate
when a run or PR wait stops early with exit 4 on a terminal failed result.

For a `sy-*` queue identity, use `wait job`, never `wait run`. Durable queue
state is authoritative: pending/running plus a missing run or log observation
remains pending/unknown and must never be summarized as terminal success.
`shipyard --json logs <sy-id>` reports typed lifecycle and availability only;
run `shipyard logs <sy-id>` without `--json` when raw log content is needed.

### JSON shape

```json
{
  "schema_version": 1,
  "command": "wait:pr",
  "matched": true,
  "condition": {"type": "pr_green", "pr": 151, "repo": "owner/repo", "head_sha": "f521fa9b"},
  "observed": {
    "checks": [{"name": "Linux", "conclusion": "SUCCESS", "required": true}],
    "advisory": []
  },
  "transport": "daemon",
  "fallback_used": false,
  "events_received": 3,
  "transient_errors": 1,
  "elapsed_seconds": 12.4
}
```

Branch on `matched` and the condition-specific `observed` fields.
`transport == "daemon"` records that the daemon subscription remained
available; it does not prove an event caused the match. Use
`events_received > 0` only as evidence that at least one relevant event
triggered a refresh. Zero events can still match through the initial or
periodic authoritative snapshot. `transport == "polling"` means the daemon was
unavailable or disconnected and polling fallback was used.

### Always set `--timeout`

Unbounded waits in an agent workflow hang sessions. Pick a realistic ceiling (10–30 minutes for most checks, longer for a full release). The flag is required in practice even though the CLI has a default.

See `docs/waiting.md` for the full reference: subcommand semantics, event sources, fallback contract, and the rulesets-unsupported caveat.

## Ship workflow (the main flow)

1. Work on a feature branch. Commit your changes.
2. Run `shipyard ship --json` — this pushes, creates a PR, validates on all
   platforms, and merges when green.
3. If a target fails, read the logs with `shipyard logs <id> --target <name>`.
   If the failure is confined to one platform (which it usually is), **iterate
   locally against that target instead of re-shipping the full matrix** — see
   [Iterating on a single-platform failure](#iterating-on-a-single-platform-failure)
   below. Once the local lane is green, `shipyard ship --json` again.

Shipyard refuses to merge unless every required platform has passing evidence
for the exact HEAD SHA.

On a base branch whose live queue object or evaluated rules require GitHub's
merge queue, Shipyard does not issue a direct merge. It enqueues with GitHub's
server-atomic `expectedHeadOid` set to the exact validated head SHA, then
`shipyard ship` waits for the queue result.
Formal GitHub stacked pull requests are detected at each merge or enqueue
mutation boundary, including the runner steward, regardless of the protected
base's top-level `stacked_pr_mode = "off" | "observe" | "apply"`. Missing
configuration defaults to `off`, which preserves the existing refusal.
`observe` still refuses mutation but adds a deterministic exact-head
`stacked-pr-plan=<json>` receipt with repository, PR, stack number, size,
position, and stack base. It never changes or suppresses required checks and
does not count as validation evidence. `apply` is a parsed reserved value, not
an enabled mutation path: it returns an explicit `apply_unavailable` NO-GO.
Only `off` is accepted in trusted machine-global config, where it overrides a
repository's broader mode as the conservative fleet switch. Invalid values,
partial metadata, and head drift fail closed. Ordinary unstacked auto-merge is
unchanged in every mode. A read-only REST identity fallback may classify a
classic boundary when GraphQL is exhausted, but it cannot authorize a merge.
For an observe-only pilot, validate every layer and use
`gh stack merge <pr> --merge`; do not add Shipyard mutation support until the
asynchronous request UUID and completion lifecycle are modeled durably.
On private repositories whose plan cannot expose evaluated rules, an
authoritative null `mergeQueue` plus the exact private-free plan-entitlement
403 identifies the classic branch, but Shipyard still refuses automatic direct
merge because it cannot prove the mutation credential lacks every bypass path.
Other authorization failures and malformed responses remain fail-closed.
`shipyard auto-merge` remains a cron-safe one-shot: it returns exit 3 after
arming or observing the queue and leaves ship-state active. A queue supervisor
re-enqueues only after it previously observed the PR (persisted across process
restarts) and GitHub reports `invalid_merge_commit`; `failed_checks`,
manual/unknown removal, head drift, and HTTP 403/rate-limit responses stop
fail-closed.

### "Validated green but not merged" — read the status before blaming the PR

`shipyard ship` can validate every target green and still not merge. The
reason is not always on the PR, so do not start by inspecting branch
protection. Read the `status` field in `--json` (or the headline of the human
render) first:

| `status` | Exit | What it means | What to do |
|---|---|---|---|
| `merged` | 0 | Landed. | Nothing. |
| `validation_failed` | 1 | A target genuinely failed. | Read `shipyard logs`. |
| `green_not_merged` | 0 | GitHub rejected the merge — usually a required check Shipyard does not supervise still in flight. | Re-run `shipyard ship --pr <n>` once the remaining checks finish. |
| `green_not_merged_flaky_required` | 0 | A required check is RED on the exact SHA Shipyard validated green. | `shipyard rescue` — see [Rescuing wedged runners](#rescuing-wedged-runners-shipyard-rescue). |
| `green_not_merged_head_superseded` | 0 | The head moved after validation; Shipyard refused rather than land an unvalidated commit. GitHub rejected nothing. | `shipyard ship --pr <n> --adopt-head`. If you did not expect the head to move, look for an unpushed local commit first. |
| `green_not_merged_client_defect` | 8 | **Shipyard sent GitHub a malformed request.** Nothing is wrong with the PR. | Report it with the `merge_error` verbatim. The PR is almost certainly mergeable now; `gh pr merge <n> --auto` lands it without bypassing any gate. |

`merge_error` carries the underlying failure verbatim for every non-merged
state, so automation never has to scrape prose out of the human render.

Two things worth knowing about that last row. It is exit **8**, deliberately
distinct from `1`, so a script can tell a *stalled-green* PR from a *red* one —
the pre-existing states keep their historical exit codes. And when you arm the
merge by hand on a merge-queue-governed branch, pass **no strategy flag**: the
queue owns the merge method and `--squash` is refused with `! The merge strategy
for main is set by the merge queue`.

The known instance of this class: Shipyard ≤0.80.1 selected
`autoMergeRequest{id}` in the merge-queue poll query. GitHub's
`AutoMergeRequest` is a plain OBJECT implementing no interfaces — not a `Node`,
so it has no `id` — and GitHub rejected the whole document with `Field 'id'
doesn't exist on type 'AutoMergeRequest'`. Because that query runs at queue
*admission*, before any mutation, merge-queue admission failed outright on every
queue-governed repo. When adding or editing a GraphQL selection set, verify it
against the live schema rather than assuming a field exists:

```sh
gh api graphql -f query='{__type(name:"AutoMergeRequest"){fields{name}}}'
```

On a multi-host fleet, set `[merge_queue].mutation_machine` to one stored
runner tag in every host's trusted machine-global `config.toml` reported by
`shipyard paths`. Project and checkout-local config cannot select authority.
All other hosts may validate but must fail before a queue write.
Use `shipyard merge-queue hold --reason "<incident>"` / `status` / `resume`
on the configured mutation machine for the authority stop; propagate the hold
when consistent fleet status matters. Shipyard serializes mutations
process-wide and records their correlation id, machine, PID, exact head/base,
action, and outcome under machine-global `merge_queue/mutations.jsonl`.

### Iterating on a single-platform failure

When CI goes red on exactly one platform (e.g. only the Windows leg of a
matrix, only the macOS sanitizer), **do not default to push → wait for full
matrix → read one platform's result → repeat**. That burns the dispatch cost
on every platform you didn't touch — typically 15–25 minutes per iteration
re-validating lanes that were already green.

Use `shipyard run` with target selection to validate the fix against the real
target, fast:

```bash
# Iterate on the Windows lane only (skips mac + ubuntu)
shipyard run --skip-target mac --skip-target ubuntu --json

# Or, equivalent inclusive form
shipyard run --targets windows --json
```

`run` validates locally via the configured backend for that target (SSH host,
local VM, or cloud runner — whichever `.shipyard/config.toml` assigns). You
get a real result in ~5–10 minutes per target with no GitHub Actions runner
minutes burned and no re-validation of lanes you didn't change. Once the
local lane passes cleanly, `shipyard ship --json` to kick the final cross-
platform gate.

**When this loop doesn't fit:**

- **Final pre-merge gate.** `shipyard ship` / `shipyard pr` is still the
  only command that produces a merge-eligible evidence record. `shipyard run`
  iteration is for getting-to-green; `ship` is for landing it.
- **Platform-specific to a backend you don't have.** If the failure is
  specific to a GitHub-hosted runner (e.g. the `[github-hosted]` leg of a
  matrix where your local lane is SSH or Namespace), the local lane is a
  good proxy but not identical. Consider `shipyard cloud run build <branch>`
  as the middle ground — dispatches to the same cloud backend CI uses
  without re-running everything.
- **Cross-target behavioral differences you're actually testing.** If the
  bug only manifests when two targets interact (rare but real — e.g.
  shared caches), the single-target loop hides it.

**When `shipyard run` fails for reasons that don't match your change:**

Long-running SSH or VM backends accumulate per-run state — stale build
artifacts, partially-applied branches from interrupted earlier runs,
environment drift. If `run` errors on a lane with messages that look
unrelated to the code you changed (`cmake` complaining about files you
didn't touch, configure steps timing out on line one, paths pointing at
an earlier branch), check the host before assuming your code is wrong.

Typical diagnostic pass on an SSH backend:

```bash
ssh <backend-host>
cd <worktree>
git log -1 && git status             # did we land on the expected SHA?
ls -la .shipyard-stage-*             # old stage dirs still pinning files?
rm -rf .shipyard-stage-*             # nuclear reset; safe — always re-staged
```

Local VM backends usually have their own `reset` path in the project's
`.shipyard/` config. Re-run `shipyard run` after cleanup.

### Recovering an interrupted ship

If a ship was interrupted (laptop closed, session ended, OS restart), just
run `shipyard ship --json` again. Shipyard writes per-PR state to disk on
every dispatch and evidence event; the second invocation auto-resumes from
the same run IDs without re-dispatching. On SHA or merge-policy drift the
resume is refused with a clear message — re-run with `--no-resume` to
archive the stale state and start fresh. Full details in
[`docs/ship-resume.md`](../../docs/ship-resume.md).

## Queue management

When multiple jobs are queued (common with parallel worktrees):

- `shipyard queue --json` — see what's running and pending
- `shipyard bump <id> high` — make a job run next
- `shipyard bump <id> low` — deprioritize a job
- `shipyard cancel <id>` — cancel a pending or running job

Pending ship jobs are pruned when their exact queued head is observed as
already merged. The observation is keyed by `(repository, PR)`, deduplicated
within a drain, cached for 30 seconds, and bounded to 15 seconds including
configured GitHub App token resolution. An unavailable or mismatched
observation leaves the job queued; never replace this with ambient `gh`, a
checkout-relative PR lookup, or an unbounded per-job poll.

For cross-process, read-only observation of GitHub's server merge queue and
open PR heads, use `shipyard queue-observe`. It persists a canonical snapshot,
emits only initial state or semantic deltas, and backs unchanged polling off
through 15/30/60/120/300 seconds. The command also reports local mutation
authority and `HOLD` state, but it never acquires a mutation lease or calls a
GitHub mutation. Its state file, append-only transition log, and exclusive lock
provide a durable handoff boundary for queue-monitor agents. See
[`docs/queue-observer.md`](../../docs/queue-observer.md).

### Durable agent handoff boundary

`shipyard pr` and `shipyard runner steward-handoff` persist a private,
crash-consistent route receipt for the exact repository, PR, head, workstream,
and context before publishing the public handoff status. Dry-run remains
read-only. Apply mode uses a stable machine identity, keeps provider/session/
surface details private, and exposes only an opaque public route identifier.
An intentional replacement must use the explicit transfer option with the same
immutable work identity; Shipyard increments the ownership generation and
rejects ambiguous provider context, head drift, or competing route ownership.

Acknowledging agent context also establishes the first ownership lease and
writes its holder material to the requested new owner-only file. Treat that
file as the mutation credential: pass it only with `--holder <private-file>` or
strict stdin (`--holder -`), never in argv values, logs, status, or inventory.
Use `work-ledger ownership renew` before expiry and replace the old holder file
with the newly emitted generation; an old generation cannot authorize return,
release, adoption, or custody. Use `work-ledger ownership release` for an
intentional handoff. A successor then uses `work-ledger ownership adopt` with
the exact expected generation and either the durable release digest or the
exact expired timestamp. Adoption atomically advances `owner_generation` and
writes replay-safe successor holder material to a new private output file.
There is no inferred-death takeover mode; never synthesize a death receipt or
reuse a Linear/correlation UUID as the ledger-minted ownership root.

If protected remote custody exists, stage it with `work-ledger ownership
custody-prepare` using the current holder and generation, then let the normal
transport reconciler complete its two-phase rebind. Owner/session rotation is
separate from the configured custody transport-daemon endpoint identity; do
not rewrite fleet transport policy merely because ownership changed. Every
prepare, acknowledgement, replay, abort, and final commit is bound to the exact
repository/PR/head/root/lease tuple and revalidates live authority.

An exact launch profile plus enabled trusted consumer publishes a zero-wake
native ledger obligation before the command reports
`monitoring_transferred=true`. Provider delivery is not part of that boundary.
The originating agent stops monitor-only children after transfer and continues
independent runnable work. It parks only for the exact tuple
`monitoring_transferred=true`, `agent_disposition=pause`, and
`pause_required=true`, after `--after-handoff pause --task-graph` proves no
independent runnable node remains. See `docs/post-handoff-disposition.md`.
Status reconciliation is bounded and
paginated, selects the newest matching status, and reconciles uncertain writes
before retrying so a restart cannot blindly duplicate the public handoff.

Terminal provenance is typed rather than inferred from a generic process
environment. A complete HerdR route requires `HERDR_ENV=1` plus workspace,
tab, and pane identity; optional `HERDR_SESSION` defaults to `default`. The
provider session comes from Shipyard's resolved agent provenance because HerdR
does not export one. Partial or conflicting HerdR metadata is rejected. A
complete cmux route keeps the established legacy route identity, and a plain
terminal records no terminal-specific route.
Shipyard stores raw terminal identifiers only in its private ledger and never
publishes them in GitHub status. These records remain inactive while the typed
wake consumer is unavailable; recording a HerdR or cmux route is not proof
that Shipyard can resume it. An exact launch profile and a receipt with
`wake_consumer_available=true` are the proof surface for native continuation
ownership.

## Target configuration

Targets are defined in `.shipyard/config.toml`:

```toml
[targets.mac]
backend = "local"
platform = "macos-arm64"

[targets.ubuntu]
backend = "ssh"
host = "ubuntu"
platform = "linux-x64"

# Optional fallback chain
fallback = [
    { type = "cloud", provider = "namespace", repository = "owner/repo", workflow = "build" },
]
```

There is no `shipyard config` or `shipyard targets` subcommand yet. Inspect
target definitions in `.shipyard/config.toml` and `.shipyard.local/config.toml`,
and use `shipyard status --json` for live target state.

### Same-backend transient retry (`[ship] transient_local_retries`)

Off by default (`0`, clamped `0..=2`). When set, a **local** leg that fails with
a transient `INFRA` blip is re-run once (up to the bound) on the same backend
before recording the failure — for a momentary network/runner hiccup, not a real
test failure. Deliberately `INFRA`-only: a local `TIMEOUT` would just re-burn its
wall-clock budget, and `CONTRACT`/`TEST`/`TREE_DRIFT` are authoritative. Remote
legs already have next-backend fallback, so same-leg retry is local-only. With
the default `0`, execution is byte-identical to no retry. Details:
`docs/local-mac-pool.md` § Same-backend transient retry.

```toml
[ship]
transient_local_retries = 1   # 0 = off (default)
```

### Local Mac capacity

For simple two-Mac capacity, use explicit ordered fallback:

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

This makes Mac Studio the first backend tried for macOS work, then falls back
locally only for infrastructure failures. Real test failures remain
authoritative.

For named members and lease visibility, use `backend = "host-pool"` with
explicit `[host_pools]` members, then inspect with
`shipyard targets pool status`. Stale lease records can be pruned with
`shipyard targets pool cleanup --fix`. Host-pool targets can drain multiple
non-conflicting queued jobs across available members under one local drain
owner; jobs still serialize when they claim the same checkout, PR state,
evidence lane, or exhausted pool capacity. Use `shipyard targets test mac` and
then `shipyard run --targets mac` when bringing the Mac Studio online. See
`docs/local-mac-pool.md`.

For Pulp/tartci macOS VM lanes, local queueing is preferred over hosted
overflow. A full local fleet should leave jobs queued on the VM self-hosted
labels until a controller/secondary Mac slot opens. Use GitHub-hosted macOS only
as an explicit operator fallback for local-fleet outage/unhealthiness or for a
workflow that deliberately requests hosted coverage.

### Locality routing (`requires`)

Targets can declare capability constraints with `requires = [...]`; the
fallback chain is then filtered to providers whose profile matches
every required capability. Vocabulary: `gpu`, `arm64`, `x86_64`,
`macos`, `linux`, `windows`, `nested_virt`, `privileged` (plus any
user-defined strings). Missing `requires` = no filter (backward
compatible). When nothing matches, the target errors with
`no provider satisfies requires=[…]: tried [namespace.default, …]`.
Full docs: [`docs/targets.md`](../../docs/targets.md) and
[`docs/profiles.md`](../../docs/profiles.md).

## SSH delivery: incremental bundles

SSH-backed targets deliver code via `git bundle`. On the first run the bundle is full (every object reachable from the target SHA, ~443 MB for Pulp-sized repos). On every subsequent run Shipyard probes the remote for its current HEAD over SSH (`git rev-parse HEAD`), verifies that the local clone has that commit as an ancestor, and emits `git bundle create <bundle> <target> ^<remote_head>` — a delta bundle that is typically kilobytes instead of megabytes. Any failure in the probe, ancestry check, or delta create silently falls back to the full-bundle path so the behavior on cold/corrupt remotes is unchanged. Each run logs a `bundle_mode=delta|full bundle_bytes=<N>` line to the per-target log so operators can confirm the optimisation is active.

Source delivery is not build-artifact delivery. The default-off artifact proof
core uses typed exact-head/toolchain/cache-generation manifests, authenticated
chunk resume through opaque manifest/session-bound plans, same-root atomic
publication, and exact-layout safe extraction that rejects traversal, links,
duplicates, and manifest drift. It has no dispatch or live sharding behavior.
Do not copy a full repository, build tree, Skia/Dawn cache, or other heavyweight
cache to feed a shard; prefer an exact cache-generation reference, basis-aware
Git objects, then a compressed immutable artifact. See
[`docs/artifact-transport.md`](../../docs/artifact-transport.md).

## Exact-head metadata-only authority

This tier is deliberately machine-global because a pull request must never
authorize itself to skip native validation. Configure each repository
explicitly in the machine-global `config.toml` reported by `shipyard paths`:

```toml
[metadata_authority]
mode = "authoritative"

[metadata_authority.repositories."generous-corp/pulp"]
schema_version = 1
repository = "generous-corp/pulp"
base_ref = "main"
allowed_paths = ["docs/**"]
required_checks = ["Docs consistency@app:15368", "CodeQL@app:15368"]
```

Use each exact hosted context name plus its GitHub App or status-creator
database identity (`name@app:<database-id>` or
`name@actor:<actor-type>:<database-id>`). Keep the
path list narrow: repository-wide patterns such as `**` are rejected. If the
PR changes any non-allowed path, either changed-path source is incomplete, the
protected base or merge base moved, a required check is missing/pending/red or
duplicated, or the daemon cannot reproduce the receipt identity, Shipyard does
not spend native capacity under this tier. Submission falls back to the normal
full target set when it can do so safely; stale execution is refused.

An authorized queue envelope has an empty target/resource plan. Its durable
`metadata-authority/<repo>/pr-<n>/<head>/receipt.json` binds the exact base,
head, tree, complete changed paths, successful hosted-check observations, and
trusted policy digest. This is authority only for zero-native validation; it
does not relax protected GitHub checks or merge governance.

## Exact-head changed-surface shadow planning

Use `changed-surface-plan` only for a target that declares
`[targets.<name>.changed_surface_selection]` on the authenticated protected
base. The command has no caller-controlled base, head, regex, or test list. It
hard-fails before writing a receipt when local HEAD/tree does not match the PR;
after that boundary, missing/malformed policy, stale or mismatched base
provenance, incomplete/mismatched diffs, unmapped paths, and head-side
policy/schema/test-topology changes force a full-suite receipt.

The command is shadow-only. Its receipt is queryable telemetry, not passing
target evidence, and the configured full validation command must still run.
Every eligible bounded candidate includes the nonempty mandatory baseline and
the complete literal test list of every affected compatible family. A
schema-v2 medium-risk family also includes every reviewed literal
`extended_tests` neighbor; a high-risk family or `full_required_paths` match
selects full. Schema v1 remains affected-only. Unknown paths never become a
bounded success. The receipt's `selection_tier` is shadow telemetry and cannot
authorize a merge. A
build-incompatible family must name a typed, non-advisory secondary target; the
plan stays blocked until evidence from that target proves its own declared
`validation_build_type`, the same exact head, and completion within 24 hours.
Direct and active-profile advisory targets, reused evidence, and older records
do not qualify. The evidence must bind a clean pre-execution checkout at the
authenticated head and tree. Required secondary targets must currently be
concrete local validation contracts, not remote, cloud, or composite wrappers.
Prepared-state reuse must be disabled on the secondary target. Never substitute
resumed or warm-reused stage execution for the full required contract. Never
substitute full Debug for a Release-only installed-SDK
family or treat historical Release evidence as sufficient. See
[`docs/changed-surface-selection.md`](../../docs/changed-surface-selection.md).
The optional POSIX execution canary is independently machine-global and
default-off. For schema v3, `shadow_compare` builds the receipt-bound producer
targets and runs selected tests before the original full build and test suite,
returns the full result, and persists comparison evidence; `authoritative`
requires a separate graduation review. Repository and local overlay config
cannot activate either mode. Authoritative activation also requires the exact
reviewed shadow policy digest for the canonical repository and exact target in
trusted machine-global
`changed_surface_execution.accepted_shadow_policy_digests."<owner/repo>".<target>`.
The legacy scalar remains supported only when the scoped table is absent;
configuring both is ambiguous and fails closed.
For `--resume-from test`, Shipyard authenticates all eligible target plans in a
read-only preflight and refuses schema-v3 execution before activation
persistence or substitution. It could skip producer builds and test stale warm
artifacts. If every plan is schema v2 or ineligible, it preserves the original
stages and resumes the ordinary test stage without a second observation or
activation. Restart schema v3 from `build` or start a fresh validation.

For a prospective `shipyard pr` push, selected execution is transport-only and
remains machine-global default-off. Shipyard accepts exactly one non-delete
branch update and authenticates the configured `core.hooksPath/pre-push` as a
protected-base-tracked regular file with the platform-valid Git tree mode
(executable on POSIX) whose bytes remain identical before and after the push.
The private result path, nonce, and prospective
receipt identity are supplied by Shipyard; the hook result must bind the exact
head, tree, changed paths, selected tests, and hook digest. Any missing,
ambiguous, changed, symlinked, or mismatched input falls back to the full
authoritative validation contract.

Schema v2 has a default-off test-stage promotion contract; schema v3 extends it
to an atomic build-and-test contract for controlled local POSIX canaries. A
bounded command is eligible only after Shipyard re-derives the
exact receipt from protected-base policy and binds it to the original target
validation digest, workflow digest, clean head/tree identity, proven POSIX
transport, and a trusted machine-global enable bit. Any mismatch, unsupported
transport, unknown/high-risk path, or disabled switch keeps the configured full
build and test stages. The payload is size-limited canonical URL-safe base64 plus SHA-256;
test names are never interpolated into a regex or shell expression. The
library contract alone does not activate selection: the queue/orchestration
layer must still snapshot and substitute the immutable plan before bounded
results can become authoritative.

## Cross-PR evidence reuse

When PR B rebases onto PR A's merged SHA and B's diff doesn't touch any
path that a target actually exercises, Shipyard can reuse A's passing
evidence instead of re-running the target. Off by default; opt-in per
target via `reuse_if_paths_unchanged`.

```toml
[targets.ubuntu-cpu]
backend = "ssh"
host = "ubuntu"
platform = "linux-x64"
# Only dispatch this target if HEAD changed one of these paths. If
# none match, borrow the most-recent passing evidence from an ancestor
# SHA and skip dispatch.
reuse_if_paths_unchanged = ["src/backend/**", "Cargo.lock"]
```

### When reuse fires

Pre-dispatch, for each target with `reuse_if_paths_unchanged` set:

1. Walk HEAD's first-parent ancestors and query the evidence store for
   the most recent PASS on this target whose SHA is in that list.
2. If found, compute `git diff --name-only <ancestor>..HEAD`.
3. If no changed file matches any glob, write a synthetic PASS evidence
   record with `reused_from: <ancestor_sha>` and skip dispatch.
4. Otherwise dispatch normally.

### Safety rules (always enforced)

| Refusal | Why |
|---|---|
| Non-fast-forward lineage | `git merge-base --is-ancestor` must succeed; rebases across unrelated history never reuse |
| Validation contract changed | The `[validation.contract]` subtable's digest is stored with each record; any change forces a re-run |
| Stage list changed | Adding / removing a stage between the ancestor and HEAD forces a re-run |
| No passing ancestor | If the most recent ancestor failed, or there's no record, reuse is declined |
| Chain reuse | A reused record is never itself a reuse source — we only borrow from real dispatches |

### How it surfaces

- `shipyard watch --json` emits `{"status": "reused", "reused_from": "<sha>"}` for reused targets (instead of the bare `"pass"`).
- `shipyard watch` human mode prints `evidence: <target>=✓ reused (from a1b2c3)`.
- Evidence records in the store carry `reused_from`; `shipyard evidence --json` shows it verbatim.
- The ship-state merge gate still counts reused targets as `pass`, so PR drain isn't blocked on a borrowed lane.

### When to enable

Reuse pays off on projects where the target's exercised surface is a
small subset of the repo — think a backend-only test lane on a mixed
frontend/backend monorepo, or a Cargo `cargo test -p backend` lane
whose output only changes when the crate or its dependencies move.
Don't enable it on a lane that runs the full suite — the globs would
have to cover the whole tree, at which point you're back to
re-running everything anyway.

## Warm-pool runner reuse

Cross-PR evidence reuse (above) skips the whole target when nothing
the target cares about changed. Warm-pool reuse is a narrower
optimisation: even when the diff *did* touch paths the target runs
against, the *runner itself* (SSH host, local workdir) doesn't need
to be re-cloned and re-dep-installed every time. When a PASS landed
within the last few minutes, the next ship on the same SHA can
re-enter the already-populated workdir and skip the pre-stage
(clone / sync / deps install). Validate — configure / build / test —
re-runs in full, so a code change is never silently masked.

Off by default. Opt in per target:

```toml
[targets.ubuntu]
backend = "ssh"
host = "ubuntu"
platform = "linux-x64"
# Hold the workdir open for 10 minutes after a PASS. Same-SHA ships
# within the window skip clone/sync/deps. Default 0 = feature off.
warm_keepalive_seconds = 600
```

### Three disable levels — why all three exist

| Level | Knob | When to reach for it |
|-------|------|----------------------|
| Per-target | `warm_keepalive_seconds = 0` (default) | Targets that rely on a pristine env (release validation, flaky build scripts) stay cold-only. |
| Global kill switch | `SHIPYARD_NO_WARM_POOL=1` env var | A CI that shells out to `shipyard` from inside another workflow — the outer runner is already ephemeral, and warm-pool state on that runner would be per-job noise. One-shot fresh escape hatch. |
| Per-ship CLI flag | `shipyard ship --no-warm` / `shipyard run --no-warm` | An agent deliberately wants a cold-start for this one ship — typically when debugging a pre-stage regression or confirming a clean-room build. |

The three levels compose: any one of them is enough to force a cold
start. Why this isn't simply always-on:

1. **Cloud runners cost money per second.** Silent always-on reuse on
   a paid provider would surprise a monthly bill.
2. **State drift is real.** Tests leave tmp files, build scripts
   assume fresh `~/.cache`, background processes upgrade deps.
   "Cold every time" is a correctness fence some users rely on.
3. **Sometimes the point IS cold.** Release-validation lanes
   deliberately want a pristine env to catch "works on my machine"
   regressions.

### Mechanics (what gets skipped, what still runs)

When a warm-pool hit fires, the dispatcher passes `resume_from=configure`
to the executor — the same machinery that powers `shipyard run
--resume-from <stage>`. The remote:

- Keeps the existing workdir at the recorded SHA — no re-clone, no
  bundle delivery, no `git checkout`.
- Skips the `setup` stage (the conventional home for deps installs).
- Runs `configure`, `build`, `test` as normal.

A validation config that uses a single `command` field (no stage
breakdown) can still benefit — the pre-stage skip still applies, but
the single command always runs in full.

### Eligibility and eviction

| Condition | Behavior |
|---|---|
| Target is on backend `cloud` / `github-hosted` | Silently ineligible. Workflow runs are ephemeral — there's nothing to keep warm. Shipyard warns once per invocation so a misconfigured target surfaces, not silently. |
| Current job SHA differs from the pool entry's SHA | Miss → cold start. The pool is strictly same-SHA; it is not a cross-SHA workdir cache. |
| Pool entry past `expires_at` | Pruned on lookup; cold start. |
| Any non-PASS outcome after a warm reuse was applied | Entry evicted. The pool never serves a dirty workdir twice. |
| `SHIPYARD_NO_WARM_POOL=1` set | Every lookup short-circuits to miss; no entries are recorded either. |

### How it surfaces

- `shipyard targets warm status --json` lists every live entry with
  target, host, backend, workdir, SHA, TTL remaining, expires_at,
  created_at. Expired entries are pruned as a side effect.
- `shipyard targets warm drain [--yes]` empties the pool — use after a
  host reboot, runner-image change, or any event that invalidates
  the tracked workdirs.
- Pool file lives at `<state_dir>/warm_pool.json`. Safe to delete
  manually; worst case, the next ship cold-starts.

### When to enable

- SSH lanes against a long-lived host where `apt install` / `npm
  install` / `cargo fetch` dominates the per-run wall clock.
- Local lanes with expensive first-run setup (e.g. virtualenv
  creation, system framework bootstrap).

### When NOT to enable

- Release-validation lanes — you want pristine every time.
- Flaky targets that sometimes leave lockfiles behind.
- Cloud / GitHub-hosted lanes — the backend is ineligible; the knob
  has no effect and Shipyard warns to reconcile the config.

## Failure classification

Every non-passing `TargetResult` and `EvidenceRecord` carries a `failure_class` (visible in `shipyard run --json`, `shipyard evidence --json`, and `shipyard watch --json`):

| Class | Meaning | Retry policy |
|-------|---------|--------------|
| `INFRA` | Network/SSH/runner availability problem (`Connection refused`, `ssh: connect`, `Network is unreachable`, `RUN_IN_DAYS_DEAD`, etc.) | Auto-retry on the next backend in the fallback chain |
| `TIMEOUT` | Hit the wall-clock cap | Auto-retry once |
| `CONTRACT` | `[validation.contract]` marker missing | Never retry — product bug |
| `TEST` | Non-zero exit with no infra/contract markers | Never retry — authoritative test failure |
| `UNKNOWN` | Fallback when the heuristics can't decide | Surfaced to the agent; not auto-retried |

Agents should read `failure_class` before deciding whether to retry, escalate, or surface to a human.

## Advisory lanes (lane degrade-mode)

Not every lane should block the merge. A matrix with one noisy runner (flaky Windows, experimental macOS-ARM64) still wants to keep shipping when the known-problem lane is red. Mark it advisory:

```toml
[targets.windows]
backend = "cloud"
platform = "windows-arm64"
advisory = true
```

A red advisory lane surfaces in `shipyard watch` and the PR body but does **not** block `shipyard ship` / `shipyard auto-merge`. Required lanes (the default — `advisory = false` or unset) still must be green.

Queue capacity is replenished per completed worker. A fast job finishing beside
a slow job should admit the next eligible queued job immediately instead of
leaving that slot idle until the whole batch ends. Scheduler deferrals retain
their backoff timestamp, and an admission error must not strand another active
worker's durable job in `running`; the coordinator drains and records active
completions before returning the original error.

### Overriding per PR — the `Lane-Policy:` trailer

Sometimes a release candidate needs to treat a normally-advisory lane as must-green (or vice versa). Put a trailer on the **tip commit** (never in the PR body):

```
Lane-Policy: windows=required
```

Multiple pairs, space- or comma-separated, are fine:

```
Lane-Policy: windows=required macos=advisory
```

The trailer overlays the config for this PR only. Unknown target names are ignored silently.

### Advisory vs quarantine — when to reach for which

| Question | Tool |
|---|---|
| "This lane is permanently flaky, I want to suppress TEST/UNKNOWN failures but still block on INFRA/TIMEOUT/CONTRACT." | `.shipyard/quarantine.toml` |
| "This lane is intentionally noisy / experimental / optional; its status is informational at all times." | `advisory = true` |
| "Just this one PR: escalate a normally-advisory lane to required." | `Lane-Policy: <target>=required` trailer |

They compose cleanly: a target can be both quarantined and advisory; the advisory flag is the wider knob.

### What the surfaces look like

- `shipyard watch` (human) dims advisory evidence/runs and tags them `(advisory)`.
- `shipyard watch --json` emits each dispatched run with a `required: bool` field so a downstream agent can filter without re-reading the config.
- The PR body opened by `shipyard ship` lists advisory lanes under an "Advisory lanes" section, calling out any overrides that came from the `Lane-Policy` trailer.

## Flaky-target quarantine

`.shipyard/quarantine.toml` is an opt-in list of targets whose `TEST` or `UNKNOWN` failures should be treated as advisory during the merge decision. `INFRA`, `TIMEOUT`, and `CONTRACT` failures are *never* suppressed — quarantine only hides authentic test flakiness, not infrastructure or contract bugs.

```toml
[[quarantine]]
target = "windows-arm64"
reason = "flaky Windows runner apr-2026 outage"
added_at = "2026-04-18"
```

Manage via `shipyard quarantine {list,add,remove}` (see table above). The merge check surfaces quarantined failures in the `advisory` field of the JSON payload; reviewers still see them but the merge is not blocked.

Remove a target from quarantine the moment the underlying flakiness is fixed — the list is meant to be short-lived.

### A wall of reds is usually one red

Before triaging N failures in a leg, check whether N-1 of them are cascades of
one. Two mechanisms in this repo manufacture them, and both name healthy tests
as the culprit:

- **A poisoned shared lock.** `std::sync::Mutex` poisons when a holder panics,
  and `lock().expect(..)` re-panics for *every later caller in the binary*. One
  real assertion failure in `merge_steward_cmd` once produced sixteen
  `PoisonError` reds behind it. A lock that guards `()` — pure mutual exclusion,
  no state — cannot have a broken invariant, so poisoning carries no
  information and must be recovered with `PoisonError::into_inner`. Take the
  process-tree lock via `test_support::lock_process_tree_for_test()`, never
  `PROCESS_TREE_TEST_LOCK.lock().expect(..)`.
- **A `SIGABRT`.** A stack overflow or abort kills the whole test binary, so
  every test that had not yet run is reported as not-passed regardless of its
  own health.

The tell is the panic *site*: cascades all panic at the same shared line, and
the real failure is the one panicking somewhere of its own.

**Fix a flake by forcing it, never by re-running it.** A fix you cannot make
fail on demand is a hope. Force the mechanism (poison the lock deliberately;
hold the executable open to force `ETXTBSY`), plant a control that proves the
precondition really happened so the test cannot pass vacuously, then mutate the
fix away and confirm that exact test goes red. If a test is non-deterministic
by construction, say so and propose making it deterministic rather than
quarantining it.

## Troubleshooting

- `shipyard doctor --json` — checks git, ssh, gh, nsc are installed
- `shipyard status --json` — shows configured targets, queue state, and live target status
- `shipyard logs <id> --target <name>` — full log for a failed target
- A row in the run summary that reads `<target>   error   ssh` prints the underlying backend error on the following indented line (`✗ <target>: Bundle apply failed: …` plus the log path). `shipyard targets test` exercises only `ssh <host> echo ok` — it does *not* run bundle create/upload/apply or the remote validation command, so a probe pass does not imply `run`/`pr` will succeed. When the error line says `Bundle apply failed` / `Bundle upload failed`, inspect the per-target log first; the probe's "reachable" verdict is a prerequisite, not a guarantee.
- If a target is unreachable with no fallback, `run` / `ship` / `pr` exit **3** (distinct from 1 validation-failed and 2 config-error) with a message that names the target, the failure category (`auth`, `host_key`, `network`, `timeout`, `unknown`), and the last ssh error.
- `shipyard run --allow-unreachable-targets --json` — proceed with the lane **SKIPPED, NOT validated**. The warning is loud by design because muscle-memory use of this flag (Pulp pre-2026-04-20) hid real backend outages.
- `shipyard run --skip-target <name>` — **deliberately** skip a lane (no probe run). Use this when you already know you don't want to validate the target — `--allow-unreachable-targets` is for "I want this target, but the backend is down right now."
- `shipyard cloud defaults --json` — inspect the current cloud workflow/provider dispatch plan

## Shipping a PR (the `shipyard pr` path)

When the user says "push a PR", "ship this", "ship it", "we're done", "merge this", or "push it" — run `shipyard pr` (or the `/pr` slash command — see `commands/pr.md`). It wraps `shipyard ship` with the versioning gates: skill-sync check, version-bump apply, and a `chore: bump versions` commit before handing off to the push/PR/validate/merge flow.

The orchestration, in order:

1. `skill_sync_check.py --mode=report` — hard-fails if a mapped path was touched without a `SKILL.md` update or a `Skill-Update:` trailer on the tip commit.
2. `version_bump_check.py --mode=apply` — rewrites `Cargo.toml` for CLI-surface bumps and `.claude-plugin/plugin.json` for plugin-surface bumps. The two version streams are independent per `RELEASING.md`.
3. `git commit` + `gh pr create` + `shipyard ship`.
4. If `[pr.provenance]` is configured, run its exact argv with the submitting session's environment. A required hook must succeed before any durable handoff or validation dispatch.
5. With `[merge_steward].auto_handoff = true` on the protected base branch or explicit `--workstream-id`, write the exact-head server receipt and managed label immediately after provenance, before validation begins. The PR branch cannot enable the project default. The fallback workstream is `OWNER/REPO#PR` and the fallback context is the PR URL; `--no-steward-handoff` is an explicit override.
6. On merge, `.github/workflows/auto-release.yml` tags the CLI bump as `v<x.y.z>`. The existing tag-triggered `release.yml` builds the 5-platform binaries and publishes the GitHub Release.

### Atomic PR provenance hook

Use a repo-owned argv hook when PR labels/footer must survive a submitting agent
being interrupted immediately after the server receipt:

```toml
[pr.provenance]
command = ["whence", "--pr", "{pr}", "--auto"]
required = true
```

The command is never shell-evaluated. Shipyard expands `{pr}`, `{repo}`,
`{head}`, `{branch}`, `{base}`, and `{url}` per argument and also exports them as
`SHIPYARD_PR_NUMBER`, `SHIPYARD_PR_REPO`, `SHIPYARD_PR_HEAD`,
`SHIPYARD_PR_BRANCH`, `SHIPYARD_PR_BASE`, and `SHIPYARD_PR_URL`. It inherits the
current agent/cmux/router environment so Whence can record truthful workstream,
launcher, route, and router fields. A configured hook defaults to required and
fails before the exact-head steward receipt, managed label, queue state, or
validation dispatch. `shipyard ship --pr` never invokes it: a recovery session
must not overwrite provenance captured by the original submitter.

`shipyard ship --pr N` is recovery from the exact live PR worktree, not a way
to bind the current checkout to another PR. Before any queue, ship-state, or
validation mutation, Shipyard requires the current GitHub origin, branch, and
full HEAD to equal the authenticated PR repository, head branch, and head SHA.
Switch to the exact PR worktree when this guard rejects a stale, detached,
fork-origin, or unrelated checkout. If that exact worktree intentionally moved
since the previous ship attempt, Shipyard also rejects the stale scoped
ship-state before queue insertion; verify the new head, then acknowledge it
with explicit `--adopt-head`. Never automate that flag.

Never run `gh pr create` + release separately. Never run the gate scripts by hand.

`shipyard cancel <job> --reason <why>` is an execution boundary, not only a
ledger mutation. A running local or SSH validation observes the durable
cancellation through its progress callback and terminates the supervised
process tree, including descendants. The returned job remains `cancelled` with
the exact durable reason; it must not consume a runner until the current build
stage exits naturally. Automatic already-merged cancellation is stricter: it
requires typed repository/PR/exact-head proof, durably freezes and proves the
whole process tree dead before releasing capacity, and generation-CAS removes
only the terminating worker's receipt. A restart resumes that transaction; it
does not require the separate receipt after the exact frozen-tree transaction
is durable, but it must preserve and refuse any present replacement generation.
A missing receipt without a durable termination transaction remains fenced for
agent recovery because root absence cannot prove reparented descendants dead.
Once an exact AlreadyMerged cancellation proof is stored, the daemon stops
repeating the remote merged-head observation and proceeds only through local
termination recovery. A deferred transaction follows the same crash phases and
returns the original job to pending exactly once after tree death and lease
release.

### Gate-script path resolution

`shipyard pr` looks up each gate script in this order — the first hit wins:

1. Env var (`SHIPYARD_SKILL_SYNC_SCRIPT`, `SHIPYARD_VERSION_BUMP_SCRIPT`, `SHIPYARD_VERSIONING_CONFIG`).
2. `.shipyard/config.toml` `[validation]` keys (`skill_sync_script`, `version_bump_script`, `versioning_config`).
3. `tools/scripts/<file>` — common CI-tooling layout (used by Pulp).
4. `scripts/<file>` — Shipyard's own default.

Missing-script errors list every probed location and every override knob. Consumer repos that keep their tooling under `tools/scripts/` need no configuration; other layouts should set the env var or the `[validation]` key rather than moving the script.

## Consumer-repo pin bumps (`shipyard pin bump`)

Consumer repos (pulp, spectr, …) pin a specific Shipyard release via `tools/shipyard.toml` and install it through `./tools/install-shipyard.sh`. `shipyard pin bump` is the one-shot: it rewrites the pin, runs the installer, verifies `shipyard --version` matches, and opens the PR.

**Mental model for multi-worktree / multi-project setups:** just run `shipyard pin bump` in whichever consumer worktree is most up-to-date. Don't hand-edit `tools/shipyard.toml` — the command's guards are what keep you out of trouble. Two refuse-by-default guards fire before any side effect:

1. **Downgrade refusal** — if the target is older than the currently-installed `shipyard` binary (the `~/.local/bin/shipyard` that `install-shipyard.sh` will overwrite), the command refuses. The common trigger is running this in a stale worktree that still pins an old version. Remediation: rebase onto main, or pass `--allow-downgrade` if you really do mean to regress the global.
2. **Redundant-branch refusal** — if `origin/main:tools/shipyard.toml` already pins a version >= the target, the command refuses. Trigger: branch is behind main; opening a PR here produces a no-op at merge time or a conflict. Remediation: rebase/merge `origin/main`, or pass `--allow-redundant`.

Both guards are skipped silently when their inputs are unavailable (no `shipyard` on PATH, offline, no `origin/main`) — advisory, not load-bearing.

`shipyard pin show` reports the current pin and the latest upstream release without touching anything — safe to run anywhere.

## Pulp dependency channels (`shipyard dependency pulp`)

This is the opposite pin direction: a plugin/consumer tracks an immutable Pulp
release in `.shipyard/config.toml` and a committed JSON lock. It is opt-in.
Active first-party repositories should adopt the explicit `latest-qualified`
template; production repositories may set `stable` plus a reviewed
`stable_tag`; frozen repositories may set `fixed` plus an exact tag and peeled
commit. Never substitute `main`, a branch, a prerelease, or an inferred “N-1”
stable release. See `docs/dependency-channels.md` for complete repo-level
templates.

Use `shipyard dependency pulp update` to qualify and open the pin PR. The
command requires trusted machine-global Shipyard GitHub App auth and rejects
ambient credentials. It verifies the immutable GitHub Release proof, exact
asset/checksum inventory, and SLSA build provenance before writing a lock with
the exact tag object, commit, asset digests, and provenance statement digests.
Same-version identity swaps, changed assets, missing proof, and non-fixed
downgrades stop fail-closed. A qualification cache may avoid repeated SDK
downloads when reproducing a tracked proof, but its key includes the complete
immutable release identity; untracked candidates are freshly verified. Scan all
GitHub release pages and every candidate's separately paginated asset inventory.
Only deterministic qualification rejection may try an older version; abort on
API, auth, download, token, or I/O failure. The App-authored writer pins the
validated helper token, resolves its bot identity, disables repository
hooks/helpers, requires explicitly configured trusted absolute native
executables for token-bearing `gh`/`git`, verifies an exact lock-only commit,
and rechecks the consumer base before push/PR creation. Build
provenance must bind the tag ref and peeled commit in the GitHub certificate,
not only workflow-authored predicate fields. Branch identity binds both base SHA
and full lock digest; first push is create-only, and reuse requires the exact
commit/tree plus App-authored PR envelope. Never adopt an orphan or foreign
branch. Later valid attestations cannot replace the exact proof already recorded
in a lock.

Make `shipyard dependency pulp verify` a required consumer PR check. It bypasses
the cache and independently reproduces the lock from freshly downloaded and
verified assets. The consumer build must still verify the SDK bytes it uses and
the extracted `sdk-provenance.json` against that lock; Shipyard qualification is
not build authority.

## State-machine lane + doc-sync gate

A dedicated Rust test suite exercises ship-state transitions under `cargo test --all-targets --locked`. Failures show up in the cross-platform test matrix and the coverage gate.

A doc-sync gate enforces that `docs/ship-state-machine.md` moves whenever the mapped Rust ship-state or command modules change. Mechanism is `scripts/doc_sync_check.py` + `scripts/doc_sync_map.json` (mirrors `skill_sync_check.py` but targets free-form docs). Bypass via `Doc-Update: skip doc=<path> reason="..."` trailer.

## Bypass trailers (tip commit)

### Bounded metrics observation

`shipyard metrics import github` is observational and supervises each GitHub CLI
request under a fixed process-tree deadline. A timeout is an incomplete
observation, not a zero-job or failure result; preserve the nonzero refusal and
retry only through a later bounded invocation.

| Gate          | Trailer                                                      |
|---------------|--------------------------------------------------------------|
| Version bump  | `Version-Bump: <surface>=<patch\|minor\|major\|skip> reason="..."` |
| Skill update  | `Skill-Update: skip skill=<name> reason="..."`              |
| Doc-sync      | `Doc-Update: skip doc=<path> reason="..."`                  |
| Auto-release  | `Release: skip reason="..."`                                 |
| Lane policy   | `Lane-Policy: <target>=required\|advisory` (escalate/demote for this PR only) |

**`Version-Bump` is authoritative when set.** The override wins against both the path-based heuristic and the conventional-commit subject ceiling. If you want a bug fix to ship as `cli=patch` even though it touches many public-API files, write `Version-Bump: cli=patch reason="bug fix"` — the trailer is the author's explicit accountability, and the reason string is reviewable. Two escape hatches stay in place: `skip` zeroes the level, and an override on a surface that wasn't actually touched is ignored (no rubber-stamping unrelated bumps).

**Gotcha:** anything under `.github/workflows/**`, `.claude-plugin/**`, `commands/**`, `agents/**`, `hooks/**`, `scripts/release.sh`, `scripts/ci_matrix.py`, release packaging scripts, or `src/**` triggers the `ci` skill's path map (`scripts/skill_path_map.json`). Update this SKILL.md in the same PR — or use the `Skill-Update: skip` trailer with a real reason.

Self-hosted CI must not inherit the host's production Shipyard configuration.
Run Rust tests and coverage with `SHIPYARD_TEST_HOME: ${{ runner.temp }}`; this
override is enabled by the CI-only `ci-test-home` Cargo feature for unit tests
and production-shaped integration binaries, while official release builds
omit the feature. It leaves the runner-managed HOME, Cargo, and Rustup paths
intact. Golden tests for generated text
compare logical LF content after normalizing checkout-only CRLF; do not weaken
any other byte or semantic assertion merely to accommodate Windows checkout
policy.

**Durable resume projections are two-dimensional and inert until activation.**
Keep terminal runtime (`cmux` or optional HerdR) separate from agent/provider
transport (native Codex/Claude plus any launch-profile router such as
Subrouter). A terminal adapter must not replace or imply the provider route,
and missing Subrouter provenance must never fall back to direct `codex`.
Reconcile resume records even when the authoritative terminal-handoff update is
a no-op so legacy ledgers backfill on restart. Publish or roll back both maps as
one crash-consistent ledger image, and keep `dispatch_enabled=false` until the
outbox, acknowledgment, and physical canary gates are separately complete.

**Pulp macOS cache readiness is exact and default-off.** Generate cache
identities from the complete read-only no-follow tree inventory; a directory
name or `claimed_bytes_avoided` is not evidence. Probe all required M3
generations before M1, require exact policy inventory and freshness on both,
and persist paired zero-model evidence without overwrite. Remote M1 proof must
cross the protected digest-pinned companion transport and bind exact
host/session/route/capability/staging/reserve/terminal/manifest authority plus
carrier-origin byte, digest, and monotonic timing counters. Use only the
explicit pinned strict-SSH carrier: LAN first, independently pinned Tailnet
only after a transport failure, and never ambient SSH/config. Tailnet cache
measurements cannot close the LAN/session gate. The proof supplies no authority
beyond its exact receipt and never authorizes cache mutation or canary execution. See
`docs/pulp-mac-cache-readiness.md`.

**Detached daemon temp roots must not depend on the launching shell.** A daemon
started from launchd or a minimal SSH environment may inherit no `TMPDIR`.
Platform libraries then fall back to `/tmp`, which is a symlink on macOS and is
correctly rejected by hardened consumers that open parent directories with
`O_NOFOLLOW`. `shipyard daemon start` therefore creates an owner-private real
directory under its state-owned daemon root and exports that exact path as
`TMPDIR` before detaching. Preserve the real-directory and mode-0700 checks;
adding a shell-profile export does not fix unattended workers.

**A self-update must refresh through the verified replacement binary.** The
process running `update --refresh-daemon` may predate daemon-spawn fixes in the
release it just installed. After exact version verification, it invokes the
installed binary's `daemon refresh` with explicit mode/global/state paths and
requires the typed refresh receipt. Do not call old in-process refresh code or
accept child exit zero without the exact receipt.

Daemon-owned validation must not use the daemon's protected `TMPDIR` for test
fixtures. When local validation inherits that state-owned path, Shipyard gives
the validation subprocess a fresh owner-private directory under the platform's
real temporary root and retains it for the run. Keep protected-path checks
unchanged; explicit trusted validation environment remains authoritative.

**Manual release fallback:** `./scripts/release.sh` still exists for emergencies but is no longer the happy path. Normal releases flow through `shipyard pr` → merge → auto-release workflow.

**Local Linux lease liveness:** one `runner local-linux-lease` fleet
observation has a single 20-second budget across auth plus all paginated GitHub
reads. Timeout is a reportable `fleet_unreadable` clear decision, including in
`--json` mode; applied variable mutation has a separate 10-second budget. Do
not wrap this operator in an unbounded `gh` polling loop.

**`RELEASE_BOT_TOKEN` is required for the auto-release chain to fire.** Without it, auto-release silently degrades — tags get created via `GITHUB_TOKEN` but GitHub doesn't trigger workflows on `GITHUB_TOKEN`-pushed tags, so `release.yml` never runs and no binaries ship. Run `shipyard doctor` to check; if the secret is missing, follow the "One-time setup" section in `RELEASING.md`. `shipyard pr` will also print a heads-up before pushing the PR if the secret isn't present.

**Vellum local-runner ownership mismatch:** GitHub `offline + busy` is a
reconciliation signal, not ordinary capacity and not permission to cancel a
job. Correlate `shipyard runner status` with `tartci doctor --reap --json`
twice across a bounded interval; record the exact runner, VM, lease,
supervisor, and job IDs. Preserve protected-queue work while ownership is
live or uncertain. Current TartCI emits `offline_busy_wait_for_github`, not a
machine-checked orphan verdict, so preserve and escalate that state. Do not
invent recovery authority from missing telemetry; a future orphan verdict must
land in TartCI and be pinned before job-specific recovery is permitted.

**Rust CLI tests need executable-sized harness stacks.** Shipyard's top-level
Clap dispatch enum is sized for a normal CLI process, while Rust test-harness
workers default to smaller stacks on Linux and Windows. Keep
`RUST_MIN_STACK=8388608` on both the ordinary CI test step and the coverage test
step. A stack overflow that moves between otherwise unrelated `Cli::parse_from`
tests is a harness-policy failure: do not rerun it blindly or keep wrapping
individual tests. Focused low-stack controls may still prove a particular test
body, but they do not replace the shared CI worker-stack contract.

**Fence local queue admission before a host-wide CI transition.** Use
`shipyard queue-hold exec --purpose tartci-pool-off --host-id <id> --service
<label> [--repo <owner/repo>] [--runner <name>] -- <transition-command>` so the
child inherits the live `queue.lock` open-file description. Supply every
applicable repository and persistent-runner identity; provider-only hosts may
have neither. Shipyard assigns
the positive monotonic generation and exports it with the other exact hold
identity fields. The transition must call
`shipyard queue-hold verify` under its own transition lock immediately before
each exact service mutation and again before publishing terminal participation
state. Exit `3` is a typed refusal, `124` means bounded lock timeout/no child,
and `125` means setup or observation failure. A static ledger, PID, filename,
or earlier verification is never authority; stale scope/generation, owner
death, close/reopen of the same inode, or revocation must refuse. Revocation
between service batches preserves already completed safe shutdown but forbids
remaining mutations and terminal-state publication. This host-local hold does
not prove GitHub persistent-runner drain, lease/VM idleness, or host capacity;
those independent fail-closed gates remain required.
**Webhook failures are typed.** Registrar 404/not-found and retryable
408/409/429/5xx/timeout outcomes remain distinct from scope/auth failures;
persisted stale bindings are reconciled only after exact remote evidence is
re-read.
