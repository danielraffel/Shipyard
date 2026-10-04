# Exact-head changed-surface selection

Shipyard's fail-closed planner computes a bounded candidate suite while the
existing full target suite remains authoritative by default. Schema v2 adds
reviewed mandatory, affected, extended, and full risk tiers without changing a
validation command unless an independently trusted machine-global canary is
enabled. Schema v3 additionally binds the reviewed `CMake` producer targets
needed to materialize those tests and permits atomic build-and-test selection.
Test identities are never passed as a regex.

## Configuration

The declaration lives in the protected base commit, under the target it
describes. Shipyard reads that exact tracked file with `git show <base
sha>:.shipyard/config.toml`; the head checkout and machine-local overlays cannot
change policy for their own validation.

```toml
[targets.mac]
backend = "local"
platform = "macos-arm64"
validation_build_type = "debug"

[targets.mac.changed_surface_selection]
schema_version = 3
full_test_count = 20091
build_type = "debug"
build_flags = ["-DCMAKE_BUILD_TYPE=Debug"]
baseline_tests = [
  "smoke: CLI starts",
  "smoke: plugin registry loads",
]
baseline_build_targets = ["pulp-smoke", "pulp-cli"]
baseline_only_paths = ["docs/**"]
full_required_paths = ["CMakeLists.txt", "cmake/**", "security/**"]
policy_paths = [
  "tools/schemas/changed-surface-selection.json",
  "tools/scripts/test_changed_surface_config.py",
]
test_topology_paths = [
  "CMakeLists.txt",
  "test/**/CMakeLists.txt",
  "test/**/registry.*",
]

[[targets.mac.changed_surface_selection.families]]
name = "capability-registry"
paths = ["core/capability/**", "include/pulp/capability/**"]
tests = [
  "capability registry exact contract",
  "capability registry no-exceptions contract",
]
build_targets = ["pulp-capability-tests"]
supported_build_types = ["debug", "release"]
risk_class = "low"

[[targets.mac.changed_surface_selection.families]]
name = "audio-runtime"
paths = ["core/audio/**", "include/pulp/audio/**"]
tests = [
  "audio runtime smoke",
  "audio runtime RT safety",
]
build_targets = ["pulp-audio-tests"]
supported_build_types = ["debug", "release"]
risk_class = "medium"
extended_tests = [
  "audio graph integration",
  "audio prior co-failure regression",
]

[[targets.mac.changed_surface_selection.families]]
name = "installed-sdk"
paths = ["tools/cli/**", "include/pulp/capability/**"]
tests = ["agent capability installed SDK"]
build_targets = ["pulp-installed-sdk-tests"]
supported_build_types = ["release"]
required_secondary_target = "release-installed-sdk"
required_secondary_build_type = "release"
risk_class = "low"

[targets.release-installed-sdk]
backend = "local"
platform = "macos-arm64"
advisory = false
validation_build_type = "release"

[targets.release-installed-sdk.validation]
command = "cmake -S . -B build-release -DCMAKE_BUILD_TYPE=Release && cmake --build build-release && ctest --test-dir build-release --output-on-failure"
```

Schema v1 remains accepted and maps every family to low-risk affected selection.
Schema v2's `risk_class = "low"` selects the family tests, `medium` also selects
its nonempty reviewed `extended_tests`, and `high` forces the full suite.
`full_required_paths` likewise forces full validation before family selection.
These paths are for known global-risk surfaces; unknown or unmapped paths
already fail closed to full and must not be listed merely to suppress mapping
work. The receipt records `selection_tier` as `mandatory`, `affected`,
`extended`, or `full`.

Schema v3 requires a nonempty canonical `baseline_build_targets` list and a
nonempty `build_targets` list for every family. The bounded receipt contains the
ordered union and its digest. The repository adapter must independently prove
from the configure-produced `CMake` File API codemodel that every selected
native test executable is produced by an allowed target. Missing targets,
ambiguous artifacts, or codemodel drift refuse bounded execution; no caller can
inject a target beginning with `-` or a shell fragment.

`tests` and `extended_tests` are literal reviewed test identities, not regexes. Every family must
have at least one path and one test, the baseline must be nonempty, family names
must be unique, and the union of declared literal tests cannot exceed
`full_test_count`. `baseline_only_paths` cannot match the entire repository.
Unknown fields are rejected, so the schema has no caller regex or test-free
success representation.

Build compatibility is typed. A family that does not support the current
target's `build_type` must name a different, non-advisory secondary target and
its supported build type. For example, a Release-only installed-SDK test is
never selected in a Debug bound. The plan remains blocked until Shipyard's
evidence store contains a passing, non-reused record from the required Release
target for the same exact head. The execution record must itself carry the
matching `validation_build_type`, must be no more than 24 hours old, and its
completion time and contract digest are bound into the receipt. Historical,
ancestor-reused, direct- or profile-advisory, wrong-build, or wrong-head evidence
does not satisfy the requirement. The evidence must also record a clean source
checkout whose pre-execution HEAD and tree exactly match the authenticated PR
head and tree. Secondary targets must currently use a concrete local validation
contract; remote, cloud, host-pool, and fallback targets are rejected because
this phase does not yet capture their pre-execution source-tree provenance.
Prepared-state reuse must be disabled so a fresh completion timestamp always
represents a concrete validation execution. Evidence from an explicit or
warm-pool stage resume is also rejected; the required target must run its full
declared validation contract.

A repository that generates its families can keep them out of the
hand-edited config. `families_file = ".shipyard/<name>.toml"` names a tracked
file that holds only `[[families]]` tables, read from the same authenticated
commit as the config (never from the checkout). Its families are appended to
any inline ones and validated together, so a duplicate name across the two is
rejected, and the file's path joins `policy_paths`: a pull request that edits
it selects the full suite like one that edits the config. The path must be a
relative `.toml` path under `.shipyard/` with no `.` or `..` component; an
unreadable, empty or malformed file fails the declaration closed. Both files
feed the policy digest.

## Planning an exact PR head

Run from a clean checkout at the published PR head:

```bash
shipyard --json changed-surface-plan \
  --repo owner/repo \
  --pr 123 \
  --target mac
```

The command uses Shipyard's configured GitHub auth. It resolves PR head/base,
the live protected base ref, the head tree, the GitHub merge base, and every PR
file. It independently checks local HEAD, tree, merge base, ancestry, and
changed paths. There is deliberately no `--head`, `--base`, `--regex`, or
`--tests` option.

A valid shadow receipt is stored under
`<state-dir>/changed-surface/<repo>/<pr>/<head>/<target>.json` and returned in
the JSON envelope. It binds repository/PR identity, protected ref, PR and live
base SHAs, merge base, head/tree SHAs, changed-path and policy digests, affected
families, complete selected tests, mandatory baseline, family/count telemetry,
planner/full-suite outcomes, elapsed time, and any fallback reason. The receipt
explicitly says `shadow_only: true`, `authoritative_suite: full`, and
`authoritative_execution: not_observed_by_shadow_planner`; it is not target
evidence and cannot satisfy a merge gate.

When a release-only family is affected under Debug, the receipt either binds
the required exact-head Release target evidence under `secondary_proofs`, or it
reports `planned_suite: blocked` and exits nonzero. It does not fall back to a
known-incompatible full Debug suite. This preserves the independent Release
installed-SDK proof instead of weakening or treating it as advisory history.

### Recording a shadow plan from CI

`--record <dir>` plans the same exact head but writes a record under
`<dir>/<repo>/<pr>/<head>/<target>.json` instead of the state directory, so a
CI job can plan every PR and upload the directory as an artifact without
touching any host's ship state. The record is an envelope:

- `origin: shadow_plan_step` and `shadow_only: true`. These plans can never
  execute, so proxies such as executed-bounded plans divide by lane plans,
  never by these.
- `outcome`: `planned`, or `planner_error` when the plan could not be made
  (config, GitHub, or git failure). A planner error is a labelled outcome
  written to `<dir>/<repo>/<pr>/planner-error-<target>.json`, not a missing
  record.
- `planned_suite`, `planner_reason` (the receipt's `fallback_reason`, absent
  for a bounded plan), `elapsed_ms`, and the full `receipt`.

Every planner outcome, including `blocked`, exits zero. A planner error still
exits nonzero after its record is written, so the calling step should not
block on it (`continue-on-error`).

## Failure and fallback boundary

These conditions hard-fail and write no receipt:

- unresolved repository, PR, head, base, or tree identity;
- local HEAD differs from the authenticated PR head;
- local tree differs from GitHub's tree for that head;
- the checkout is dirty;
- a receipt is later checked against a different PR/head/tree/path identity.

After exact head/tree verification, Shipyard selects the full suite and records
the stable reason when:

- the base ref is unresolved, unprotected, stale, or equal to the head;
- ancestry or local/GitHub merge-base provenance disagrees;
- either changed-path observation is incomplete or the path sets disagree;
- policy is missing, malformed, unknown-version, or test-free;
- baseline-only patterns collectively cover the authenticated base tree;
- a path is unmapped;
- the head modifies `.shipyard/config.toml`, another declared policy/schema
  path, or declared test-topology path.

A head that sits on an older commit of the protected branch than the PR's
recorded base (its merge base is a strict ancestor of that base, which the
observation proves with `git merge-base --is-ancestor`) is planned against that
merge base instead of refusing with `base_policy_mismatch`: the lane tests the
head tree, so the merge base's policy, tracked tree and changed paths
(`merge_base..head`) are the consistent ones. The receipt records
`planned_base_sha` and the recorded base's `recorded_base_policy_digest`.
Every other rule still applies, so a head that edits a policy or topology path
selects the full suite. Authoritative execution of such a plan additionally
requires the two policy digests to be equal; otherwise the plan runs only as a
shadow comparison and the run records `merge_base_policy_diverged`. Any other
merge-base disagreement still refuses.

`stale_base` remains one of those authoritative full-suite reasons. In trusted
machine mode `shadow_compare`, Shipyard additionally computes a strictly
shadow-only stale-base assessment. It reads both old and live base policies,
the complete old-to-live delta, and a conflict-free synthesized integration
tree; then it maps the cumulative base-plus-head surface through the live
policy. The receipt binds repository/PR/head/tree, old and live base SHAs,
merge base, integration tree, policy/workflow/validation-contract digests, and
both changed-path digests. Harmless movement can therefore produce a bounded
selection assessment, and an affected-family movement expands that selection.
When that assessment is bounded, Shipyard materializes a content-addressed,
fenced checkout of the exact synthesized integration commit and runs the
selected-versus-full comparison there. Checkout identity, submodules, tracked
content, activation, result publication, cleanup intent, and daemon restart
recovery are all receipt-bound; ambiguity preserves the checkout and leaves the
ordinary full path authoritative. Policy, topology, toolchain,
producer-target, full-required, unmapped, conflicting, or incomplete movement
remains full. This path always records
`merge_authority: blocked_until_current_merge_tree`; it is not accepted by the
authoritative planner and cannot satisfy a merge gate.

An eligible bound always contains every baseline test plus the complete literal
test set for every affected family and, for medium risk, every declared extended
neighbor. A path that matches multiple families selects all of them and the
highest applicable tier. High-risk and `full_required_paths` matches select
full. The full suite remains authoritative throughout the shadow phase;
activating bounded execution is a separate promotion decision.

## Controlled POSIX execution canary

An authenticated protected-base target may declare an execution template:

```toml
[targets.mac.changed_surface_selection.execution]
mode = "authoritative"
stage = "build_and_test"
command = "python3 tools/scripts/run_changed_surface_tests.py --selection-receipt-b64 {selection_receipt_b64} --selection-receipt-sha256 {selection_receipt_digest}"
```

This declaration is permission, not activation. Shipyard loads
`changed_surface_execution.mode` from machine-global config only. Missing or
`off` leaves every target command unchanged. `shadow_compare` snapshots the
protected command into the durable queue request with a trusted result
directory and tells the repository adapter to run the selected-target build
and selected tests, then the original full build and tests; the full path's
result remains authoritative. `authoritative` omits the full comparison and is
reserved for a separately reviewed graduation after comparison evidence. It
also requires an exact repository-and-target entry in trusted machine-global
config:

```toml
[changed_surface_execution]
mode = "authoritative"

[changed_surface_execution.accepted_shadow_policy_digests."Generous-Corp/pulp"]
mac = "<64-character lowercase policy SHA-256>"

[changed_surface_execution.accepted_shadow_policy_digests."Generous-Corp/forge"]
mac = "<different reviewed policy SHA-256>"
```

Repository matching is case-insensitive, while target matching is exact. An
entry authorizes only that repository, target, and digest tuple. The legacy
scalar `accepted_shadow_policy_digest` remains valid when it is the only
accepted-digest setting, so existing single-policy machines continue to work.
Migrate by replacing that scalar with the scoped table atomically. Configuring
both forms, duplicate repository keys that differ only by case, malformed
tables, or malformed digests fails closed before any target command changes.
Changing only `mode` cannot bypass shadow review.

Schema v2 accepts only a staged local POSIX target with an exact `test` stage.
Schema v3 requires both exact `build` and `test` stages and the literal
`build_and_test` declaration; Shipyard substitutes both or neither. The
`--resume-from test` performs a read-only preflight over every eligible target
and hard-refuses an authenticated schema-v3 transaction before any activation
is persisted or stage is substituted, because resuming after `build` must
never turn a substituted test stage—or stale warm artifacts—into passing
selected evidence. If every authenticated plan is schema v2 or ineligible,
Shipyard preserves the original stages and resumes the ordinary test stage; it
does not observe again or activate changed-surface selection in that invocation.
Restart schema v3 from `build` or start a fresh validation. The
plan binds the exact PR head/tree, protected base, changed paths, selector
policy, original validation contract, protected workflow, selection receipt,
and expanded command. Shipyard persists that activation without overwrite and
syncs its file and containing directory before enqueue; the queued target
snapshot carries the same command. Full, ambiguous, oversized, incompatible,
unsupported, and observation-failure plans preserve the ordinary full suite
and append a bounded diagnostic. A typed blocked plan may still stop instead of
executing a known-incompatible suite. Receipt path components include a digest
of their canonical identity, so values such as `a/b` and `a_b` cannot alias.
While a target runs, Shipyard checks the live PR head at a
bounded interval and durably requests cancellation when it no longer matches
the queued SHA; transient head-query failures do not manufacture cancellation.

## Reading a shadow-comparison trial

After a `shadow_compare` target finishes, inspect its immutable activation and
adapter result without starting another build or changing policy:

```bash
shipyard --json changed-surface-trial-status \
  --repo owner/repo \
  --pr 123 \
  --target mac \
  --head "$EXACT_PR_HEAD"
```

The command is read-only and reports one stable state under `trial.state`:

- `collecting` (exit 3): the exact shadow activation or its result has not
  arrived;
- `ready` (exit 0): exactly one result matches the activation's repository,
  PR, target, base/head/tree, execution payload, policy, selection,
  validation-contract, workflow, selected-test, and selected-build-target
  identities; the full suite is explicitly authoritative; and either the
  verdict is `matched_pass` with both return codes zero, or it is
  `matched_fail` (below). `trial.reason` names which;
- `terminal` (exit 0 for `blocked` or `full_required`, exit 1 for
  `invalidated`): stale-base planning safely ended with an immutable typed
  receipt instead of waiting indefinitely for an activation that should never
  run;
- `rejected` (exit 1): evidence is malformed, unsafe to read, non-passing,
  identity/digest-inconsistent, or ambiguous. More than one append-only result
  for the exact identity is intentionally ambiguous even if the files are
  byte-equivalent.

`matched_fail` is the failure-set verdict for a lane whose full suite is not
green. Both test legs failed and both builds succeeded, and Shipyard recomputes
the rule from the receipt's named sets rather than trusting the label: the
`selected_tests` names must hash to the plan's selected-tests digest; every
selected-leg failure also failed in the full suite; every full-suite failure
inside the selection also failed in the selected leg; and every full-suite
failure outside the selection is in `lane_red_allowlist`, the protected base's
named lane-only reds, with an entry in `lane_red_allowlist_expires` that had not
expired on the receipt's recorded date (so a fixed test cannot keep hiding a new
failure under its name); and `allowlisted_failure_count` equals the number of
those failures the allowlist absorbed, which reports how much of a graduation
rested on it. It proves the bounded leg reproduced every failure it
could observe and invented none, not that the full suite was green, so it is
reported as `matched_fail` and never as `matched_pass`. The adapter must read
the allowlist from the protected base and keep that file, with the adapter
itself, among its policy paths so a PR that edits either selects the full
suite.

For a build-and-test plan, `trial.timing` preserves the verified input-check,
selected-build, selected-test, incremental-full-build, estimated-total-full-
build, and full-test durations from the adapter receipt. Shipyard recomputes
the selected total (including receipt verification), estimated full total,
seconds saved, and speedup ratio. The
full-build estimate is deliberately the selected build plus the incremental
remainder; treating the warm incremental remainder as a standalone full build
would overstate savings. Missing, negative, non-finite, or internally
inconsistent timing rejects the result. Legacy test-only trials remain
verifiable without timing telemetry.

`selection_receipt_digest` is the authoritative cross-receipt selection
binding. Shipyard computes it over the complete exact selection receipt,
including its changed-paths digest; the activation and adapter result must echo
that same digest. The activation plan's standalone `changed_paths_digest` is a
diagnostic duplicate that the adapter result schema does not emit, so trial
status neither treats that duplicate as independent authority nor pretends to
compare a field that is absent from the result.

This status is comparison evidence only. A stale `recomputed` or `reused`
assessment is terminal selection telemetry, not an execution result; receipt
replay or head/tree/contract mismatch is invalidated. The status does not
promote the selector,
write a graduation decision, enqueue work, mutate a receipt, or replace any
merge/release gate.

## Default-off supervised pre-push shadow receipt

Shipyard can prepare the same protected-base changed-surface selection before
it supervises the first branch push. This is a machine-trusted canary enabled
only by `changed_surface_prepush.mode = "shadow_compare"` in the machine-global
config. Missing or `off` preserves the pre-v0.107 behavior. `authoritative` is
parsed so configuration drift is visible but is intentionally inert: a
pre-push result never replaces the downstream full suite or an authoritative
selected execution.

The prospective receipt requires one resolved selector target, a GitHub-
authenticated protected base ref/SHA, a clean local HEAD/tree, a merge base
equal to that protected SHA, the exact NUL-delimited `base..HEAD` paths, and the
selector policy read from the protected base object. No CLI-selected test,
regex, target, or arbitrary unprotected base enters the plan. Policy, selector,
test-topology, unknown-path, dirty-tree, stale-base, and target ambiguity all
retain the ordinary full path.

The supervised `git push` child receives only versioned receipt-path, receipt-
digest, transaction-nonce, and private-result-directory environment variables,
alongside the existing `SHIPYARD_PR_RUNNING=1` marker. A repository hook must
independently require exactly one non-delete branch update matching the bound
HEAD/ref/tree and must fail closed for direct, tag, deletion, or multi-ref
pushes. The active repository-relative `core.hooksPath/pre-push` must itself be
tracked by the protected base, covered by that policy's `policy_paths` or
`test_topology_paths`, be a regular non-symlink file, and remain byte-identical
to the protected blob before and after push. Untracked, absolute, changed, or
uncovered hook implementations have no dedupe authority. Its bounded
`hook-result.json` is not trusted by itself. After PR creation Shipyard
re-observes authenticated PR/base/head/tree/path/policy/test identity and
accepts a hook result only when every digest, hook identity, and nonce agrees.
The JSON does not assert pass authority. Only Shipyard's parent process
observing the supervised `git push` exit zero creates the private successful-
push state required by the snapshot. The protected hook contract must return
nonzero when its selected run fails, so an untrusted test descendant can write
telemetry but cannot turn an aborted push into a reusable result.

An exact passing bounded result creates an immutable snapshot with disposition
`full_only_due_exact_prepush_shadow`. This is only a dedupe seam for a later
queue integration: it may eventually suppress the redundant downstream
selected shadow half, never the downstream full validation. There is no cross-
invocation artifact reuse, selected build-target substitution, or authoritative
activation in this slice.

## Live-reuse kill switch

A plan that skips building and testing an executable because nothing it is
built from changed acts only while a repository variable reads exactly
`live`. Every other state is shadow, where the plan is recorded and nothing is
skipped: `off`, an unset variable, any other value (including `LIVE`), a
variable that could not be read, and a read older than the plan's freshness
bound or stamped in the future. No setup is needed for the safe default; the
variable does not have to exist.

```bash
shipyard reuse switch --variable PULP_REUSE_LIVE           # live or shadow, and why
shipyard reuse trip --variable PULP_REUSE_LIVE --reason "sampled re-run failed: <test>"          # dry run
shipyard reuse trip --variable PULP_REUSE_LIVE --reason "sampled re-run failed: <test>" --apply
```

`--repo OWNER/REPO` overrides the checkout's repository. The variable's name is
always given explicitly; nothing defaults it.

A trip runs on the host that observed the problem, never from a pull request's
workflow. It writes `off` first, because turning live reuse off is the action
that protects, then opens one tracking issue (found again by the body marker
`<!-- shipyard-reuse-trip: OWNER/REPO:VARIABLE -->`, so a retitled issue is not
duplicated) or appends the new reason to it. A reason the issue already
records is not added twice, and a switch that already reads `off` is not
rewritten, so repeating a trip is harmless. Anything other than a clean `off`,
including an unreadable variable, is written `off`. If the open issues cannot
be listed the trip refuses rather than risk a duplicate; if the variable write
fails the issue is still filed and the command exits non-zero naming the
failure.

`crate::changed_surface::live_switch` holds the policy (`SwitchReading`,
`plan_trip`) as pure functions; `shipyard reuse` is its `gh` half.

## Host-local reuse records

A plan that keys executables against an earlier build needs that build's
reuse record (link members, object dependencies, codemodel, verdicts), and the
record must come from a run on the same toolchain. The local lane has no GitHub
credentials, so it neither publishes nor fetches artifacts: its records stay on
the host.

A local target opts in with `reuse_record = true` in its validation table,
which needs `[project].repository` as an exact `OWNER/REPO` slug (configuration
fails otherwise). Each run of an opted-in target then gets a fresh, owner-only
directory exported to its stages as `SHIPYARD_REUSE_RECORD_DIR`; the project's
own recorder writes there. After the stages finish, whatever their verdict, the
directory is filed under `<state>/reuse-records/OWNER__REPO/records/<commit>/<run>`
when it holds a non-empty `job.json` that parses, and removed otherwise. The
run log ends with one `=== reuse-record: ... ===` line saying which. The store
keeps the newest 40 records.

`reuse_record_store::select_base` chooses the record a plan compares against:
the newest one whose platform (architecture and OS family), then toolchain, are
stated and equal the plan's, that passes the record format's own usability
rules, and whose commit is an ancestor of the plan's protected base. Platform
is checked first because a record from another OS or architecture can carry a
plausible-looking toolchain string. An empty or `unknown` value is unstated,
and an unstated platform or toolchain is a refusal, never a match. A record
from a commit that has not merged is never chosen, since the code that wrote
it is unreviewed. "Merged" means the commit is an ancestor of the protected
base, which holds for a pull request's own commits only when it lands with a
merge commit; a squash or rebase landing rewrites them, so their records are
never chosen. The project supplies how to read its records through the
`BaseCriteria` trait; when nothing qualifies the caller gets a count per
refusal reason. A pending directory left by a run that never reached filing (a
cancelled run, or a killed process) is removed after a day.

The lane's stages run the pull request's own code as the host user, so they can
also write into the store directly. The merged-commit rule limits which records
a plan trusts, but a record's bytes are only as trustworthy as the lane; that is
why live reuse runs only in a non-required lane, with a sampled re-run behind
it.
