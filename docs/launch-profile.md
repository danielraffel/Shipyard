# Durable launch profiles

`LaunchProfileV1` is Shipyard's private, terminal-neutral contract for preserving
the exact process recipe needed to launch or restore an agent after a durable
steward handoff. Shipyard stores and validates the profile, projects only typed
provider options into its protected adapter request, and can publish a wake to
the enabled daemon consumer. It never executes profile argv directly.

Pass a profile when creating a steward handoff:

```bash
shipyard runner steward-handoff \
  --repo owner/repo \
  --pr 123 \
  --head "$EXACT_HEAD" \
  --workstream-id SY-LF-123 \
  --agent-provider codex \
  --agent-session-id "$SESSION_ID" \
  --launch-profile ./launch-profile.json \
  --apply
```

The atomic PR path accepts the same private file:

```bash
shipyard pr --no-apply-bumps --workstream-id SY-LF-123 \
  --context-url https://linear.app/example/issue/SY-LF-123/example \
  --launch-profile ./launch-profile.json
```

Generate the profile only after automatic version and skill bumps have been
committed. Shipyard refuses `--launch-profile` in its default bump-apply mode
because a new bump commit would invalidate the profile's exact-head authority.

When trusted machine-global continuation is enabled, apply mode publishes the
exact profile and waits for the live daemon to complete its fenced provider
delivery before returning `monitoring_transferred=true`. A missing, disabled,
refused, wrong-machine, or unauthorized consumer fails closed and leaves
`wake_consumer_available=false`.

The JSON schema is intentionally composed only of strings, argv arrays, exact
provenance, and a recovery-policy enum:

```json
{
  "schema_version": 1,
  "launch_argv": ["codex", "--model", "gpt-5.6-sol", "-c", "model_reasoning_effort=\"medium\""],
  "resume_argv": ["codex", "resume", "--model", "gpt-5.6-sol", "-c", "model_reasoning_effort=\"medium\"", "session-7"],
  "provider": {
    "provider_id": "codex",
    "account_id": "account-a",
    "model_id": "gpt-5.6-sol",
    "reasoning_effort": "medium"
  },
  "session": {
    "agent_provider": "codex",
    "provider_session_id": "session-7"
  },
  "checkpoint": {
    "checkpoint_id": "checkpoint-7",
    "generation": 4,
    "digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  },
  "worktree": {
    "repository": "owner/repo",
    "path": "/absolute/private/worktree/path",
    "head_sha": "0123456789abcdef0123456789abcdef01234567",
    "lineage_id": "feature/exact-worktree-branch"
  },
  "continuation_bootstrap": {
    "workstream_handle": "SY-LF-123",
    "context_url": "https://linear.app/example/issue/SY-LF-123/example",
    "plan_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "root_revision": 7,
    "issue_revision": 9,
    "projection_revision": 4,
    "material_event_revision": 6,
    "checkpoint_id": "checkpoint-7",
    "checkpoint_generation": 4,
    "checkpoint_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "repository": "owner/repo",
    "head_sha": "0123456789abcdef0123456789abcdef01234567",
    "expected_resume_context_digest": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    "success_continuation_digest": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
    "failure_continuation_digest": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
  },
  "recovery_policy": "exact_session_then_fresh_checkpoint"
}
```

The supported recovery policies are:

- `exact_session_only`: only the recorded provider session may be restored.
- `exact_session_then_fresh_checkpoint`: prefer that exact session, with a new
  agent from the exact checkpoint as a policy-authorized fallback.
- `fresh_checkpoint_only`: launch a new agent from the checkpoint; this is the
  only policy accepted when no original agent route exists.

Both argv values are persisted exactly as arrays. Provider CLIs use different
and sometimes incompatible resume flags, so Shipyard never canonicalizes them
or reconstructs a command from provider/model metadata. Empty or oversized
argv, unknown JSON fields, relative worktree paths, zero checkpoint generations,
invalid digests, and repository/head mismatches fail closed.

Before publishing a profile, Shipyard resolves the worktree path and verifies
that it is the canonical Git root, its `origin` is the claimed GitHub
repository, and its live `HEAD` is the claimed commit. `lineage_id` is the
worktree's exact branch name and must have an `active` worktree-lineage record
whose durable SHA and last path match that same checkout. Missing, detached,
superseded, merged, archived, moved, or stale lineage records fail closed.

The profile is stored inside the exact repository/PR/head handoff receipt under
Shipyard's protected private state. Its envelope has its own generation,
revision, content digest, and integrity hash bound to the enclosing opaque agent
route and ownership generation. A route/session, checkpoint, worktree,
generation, revision, or digest mismatch makes the profile unusable.

Exact-session policies require `session` provenance. Shipyard compares its
opaque provider/session identity with the enclosing private agent route; it
does not infer session identity by parsing provider-specific argv. A same-owner
restart may omit `--launch-profile` after the receipt is durable and will reuse
that stored profile. Supplying a different profile still fails closed, and an
owner transfer must supply a profile bound to the replacement session.

Argv, account/model fields, checkpoint, provider session, and worktree paths are
never projected to GitHub or Linear. Do not put credentials, environment values,
raw prompts, or tokens in a profile. Use wrapper-owned credential lookup and
opaque account identifiers instead.

## Protected executor boundary

Shipyard now contains an internal, default-off wake-consumer contract. It
selects the canonical outbox, durably claims a generation-fenced wake, and
projects only typed launch choices from validated profile metadata
into a capability-matched provider request. The provider adapter owns its
launch grammar; arbitrary launch or resume argv is never copied into the
durable provider request. Native fresh-agent publication accepts retained
legacy argv only when it is a recognized prompt-free codex/claude grammar.
The durable provider-request object is schema v2; argv-bearing schema-v1
objects are not migrated into executable authority and fail closed.
Restart reconciliation inspects the same idempotency fence; an unproven outcome
remains `uncertain` and is never blindly relaunched. A successful
acknowledgement advances the same canonical work item to agent-owned repair in
the final transaction.

Before provider I/O, native publication binds the exact PR head, base ref, base
SHA, GitHub App installation identity, and a live terminal capability. cmux
authority requires a unique local process/surface match plus the exact native
checkpoint from `surface resume show`; workspace moves are evidence updates,
not identity changes. Stored labels never authorize delivery. HerdR is an
explicit capability request but remains refused until its runtime exposes an
equivalent independently observable server/process/checkpoint contract.
Subrouter/account/model/resume argv remain digest-bound; refusal never falls
back to direct Codex.

Activation is explicit trusted machine-global policy and default-off. The
subscriber-independent daemon owns delivery only after exact handoff
publication succeeds. Its wrapper revalidates the canonical work item, route,
profile digest, owner generations, machine, repository allowlist, and pinned
adapter identity. Persisting a profile alone does not transfer monitoring; the
returned receipt is the authority. Linear remains execution state, not process
authority.
