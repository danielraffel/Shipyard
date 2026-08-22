# Pulp dependency channels

Shipyard can turn a reviewed Pulp GitHub Release into a deterministic consumer
lock and pull request. It never persists or builds from `main`, another branch,
or a floating tag. A repository opts in through its tracked
`.shipyard/config.toml`; Shipyard does not silently apply a dependency policy to
unrelated repositories.

## Channel templates

Choose a template for the repository's operating profile and commit it. The
active first-party template explicitly follows the newest release that passes
every qualification:

```toml
[dependencies.pulp]
repository = "Generous-Corp/pulp"
channel = "latest-qualified"
required_assets = ["pulp-sdk-darwin-arm64.tar.gz"]
signer_workflow = "github.com/Generous-Corp/pulp/.github/workflows/release-cli.yml"
```

This is the recommended template for actively developed first-party plugins.
It is a template choice, not Shipyard's implicit default. List every SDK asset
the repository's CI/build matrix consumes.

A production repository can promote one reviewed release as stable. `stable`
does not mean “the previous release” and does not move until `stable_tag` is
changed in a reviewed commit:

```toml
[dependencies.pulp]
repository = "Generous-Corp/pulp"
channel = "stable"
stable_tag = "v0.810.1"
required_assets = [
  "pulp-sdk-darwin-arm64.tar.gz",
  "pulp-sdk-windows-x64.tar.gz",
]
signer_workflow = "github.com/Generous-Corp/pulp/.github/workflows/release-cli.yml"
```

A frozen or incident-response repository can bind an exact tag and its fully
peeled source commit. This is also the only channel that permits a reviewed
downgrade:

```toml
[dependencies.pulp]
repository = "Generous-Corp/pulp"
channel = "fixed"
fixed_tag = "v0.810.1"
fixed_commit = "7584d3953bf55bd9de2bc55bdf60c100e2fcdff7"
required_assets = ["pulp-sdk-darwin-arm64.tar.gz"]
signer_workflow = "github.com/Generous-Corp/pulp/.github/workflows/release-cli.yml"
```

All templates may override the tracked lock path within Shipyard's reserved
dependency-lock namespace and the consumer PR base:

```toml
[dependencies.pulp]
# ...one complete channel declaration from above...
lock_file = ".shipyard/dependencies/pulp.lock.json"
base_branch = "production"
```

## Qualification and lock identity

`shipyard dependency pulp update` rejects drafts, prereleases, non-canonical
version tags, missing or non-uploaded assets, missing GitHub SHA-256 digests,
and a `SHA256SUMS` file that does not exactly cover the published non-manifest
asset set. Latest-qualified discovery exhausts every GitHub release page before
semantic-version ordering and separately exhausts the authoritative asset pages
for each candidate it examines. A deterministic policy/proof rejection may
advance to the next version; an API, auth, download, token-expiry, or local I/O
failure aborts instead of silently selecting an older release. It peels the
release tag to a commit, cryptographically verifies the GitHub immutable-release
attestation, and verifies SLSA provenance for every configured SDK asset against
the exact signer workflow, asset digest, and Actions invocation. The GitHub
certificate itself must bind the exact `refs/tags/<tag>` source ref and peeled
commit; workflow-authored predicate fields are additional evidence, not the
source-identity authority.

The tracked JSON lock materializes the channel, repository, tag object, peeled
commit, GitHub release id, complete asset inventory, checksum-manifest digest,
release-attestation statement digest, and each required asset/provenance
statement digest. A rewrite of the same version, changed asset set, missing
proof, or implicit downgrade fails closed. Re-running against the same identity
is deterministic and produces no change.

Qualification receipts may be cached in machine-global Shipyard state. The
cache key includes repository, tag object, peeled commit, release id, complete
asset set, manifest digest, release-statement digest, required assets, and
signer workflow. Shipyard still refreshes the release, tag, manifest, and
immutable-release proof before accepting a cache hit. A cached receipt is used
only to reproduce the exact proof in an existing tracked lock; an untracked
candidate is freshly verified so an interrupted first qualification cannot make
proof selection machine-history-dependent. This avoids downloading a large SDK
again during unchanged routine polling without turning the cache into consumer
build authority.

If an asset has more than one valid provenance statement, initial selection is
deterministic. Once a lock exists, refresh and CI verification search for that
exact statement digest and Actions invocation rather than silently replacing
it with a newer proof for the same bytes.

## Commands and authority

```bash
shipyard dependency pulp show
shipyard dependency pulp update
shipyard dependency pulp verify
```

`show` is local and read-only. `update` fetches the configured consumer base
into a Shipyard-initialized isolated temporary repository, writes only the lock, commits, pushes an immutable
release-named branch, and opens (or reuses) the exact PR. Network reads, the
HTTPS push, and the REST PR creation require Shipyard's trusted machine-global
GitHub App command helper with `token_kind=github-app-installation`; ambient
`gh` and tracked/local auth overrides are rejected. Shipyard pins the exact
validated App token and bot identity for the operation. Token-bearing `gh` and
`git` calls require machine-global `github.auth.privileged_gh_binary` and
`github.auth.privileged_git_binary` to name trusted absolute native executables,
so no PATH candidate ever receives the token. A later helper response cannot
change the authoring authority. The authenticated Git process ignores inherited,
system, and global Git configuration; its local config comes only from the
isolated repository, and its credential helper releases the token only for
exact HTTPS `github.com` requests. Both privileged `gh` and Git children start
from a minimal allowlisted environment, so inherited loader, proxy, CA, trace,
and tool-routing overrides cannot accompany the token. Repository hooks and other credential
helpers are disabled at the privileged commit/push boundary, and Shipyard
verifies that the commit contains the exact lock bytes and no other file. The
branch name includes the qualified consumer base and a digest of the complete
lock, so either a moved base or policy-only promotion gets a new identity. The
first push uses an atomic branch-absence lease. Reuse requires an exact
single-parent lock tree and an open PR whose author, head, base, title, and body
match the pinned App envelope; an orphan or foreign pre-created branch is never
adopted. Shipyard also rechecks the consumer base SHA before push and PR
creation; if policy or its current lock moved during qualification, the command
fails and must be rerun from the new reviewed base. `update --no-pr` is an
explicit bootstrap/debug escape that writes the lock in the current checkout.

Consumer CI should run `shipyard dependency pulp verify` as a required PR
check. It bypasses the machine qualification cache, downloads every required
asset again, verifies its bytes and SLSA attestation, and requires the fresh
result to reproduce the tracked lock exactly.

The consumer build remains the final authority. It must verify the exact SDK
bytes it actually uses and validate the extracted SDK's
`sdk-provenance.json` (including source commit and distribution eligibility)
against the lock. A green Shipyard qualification receipt accelerates release
selection; it does not replace that build-time proof.
