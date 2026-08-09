# Exact-head changed-surface selection

Shipyard 0.85 adds a fail-closed, shadow-only planner for target-declared test
families. It computes a bounded candidate suite while the existing full target
suite remains authoritative. It does not change a validation command, pass a
regex to a test runner, skip a target, or create reusable target evidence.

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
schema_version = 1
full_test_count = 20091
build_type = "debug"
build_flags = ["-DCMAKE_BUILD_TYPE=Debug"]
baseline_tests = [
  "smoke: CLI starts",
  "smoke: plugin registry loads",
]
baseline_only_paths = ["docs/**"]
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
supported_build_types = ["debug", "release"]

[[targets.mac.changed_surface_selection.families]]
name = "audio-runtime"
paths = ["core/audio/**", "include/pulp/audio/**"]
tests = [
  "audio runtime smoke",
  "audio runtime RT safety",
]
supported_build_types = ["debug", "release"]

[[targets.mac.changed_surface_selection.families]]
name = "installed-sdk"
paths = ["tools/cli/**", "include/pulp/capability/**"]
tests = ["agent capability installed SDK"]
supported_build_types = ["release"]
required_secondary_target = "release-installed-sdk"
required_secondary_build_type = "release"

[targets.release-installed-sdk]
backend = "local"
platform = "macos-arm64"
advisory = false
validation_build_type = "release"

[targets.release-installed-sdk.validation]
command = "cmake -S . -B build-release -DCMAKE_BUILD_TYPE=Release && cmake --build build-release && ctest --test-dir build-release --output-on-failure"
```

`tests` are literal reviewed test identities, not regexes. Every family must
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
represents a concrete validation execution.

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
test set for every affected family. A path that matches multiple families
selects all of them. The full suite remains authoritative throughout the shadow
phase; activating bounded execution is a separate promotion decision.
