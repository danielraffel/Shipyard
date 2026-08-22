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
also requires machine-global `accepted_shadow_policy_digest` to match the exact
protected policy digest; changing only `mode` cannot bypass shadow review.

Schema v2 accepts only a staged local POSIX target with an exact `test` stage.
Schema v3 requires both exact `build` and `test` stages and the literal
`build_and_test` declaration; Shipyard substitutes both or neither. The
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
