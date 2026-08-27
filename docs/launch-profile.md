# Durable launch profiles

`LaunchProfileV1` is Shipyard's private, terminal-neutral contract for preserving
the exact process recipe needed to launch or restore an agent after a durable
steward handoff. Shipyard stores and validates the profile; it does not execute
the argv, choose a model, translate provider flags, or enable wake delivery.

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

The JSON schema is intentionally composed only of strings, argv arrays, exact
provenance, and a recovery-policy enum:

```json
{
  "schema_version": 1,
  "launch_argv": ["provider-router", "agent", "--new"],
  "resume_argv": ["provider-router", "agent", "-r", "session-7"],
  "provider": {
    "provider_id": "subscription-router",
    "account_id": "account-a",
    "model_id": "model-tier-a"
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

## Executor boundary

A future trusted executor may consume a profile only after revalidating the
canonical work item and owner generations. Executor-specific types do not live
in this schema. For example, HerdR owns conversion of `resume_argv` into its
`AgentRestoreOverride`, including the official provider-session binding hash,
generation/revision fence, command hash, and inert-shell refusal on mismatch.
Other terminal runtimes can implement equivalent adapters without changing the
persisted Shipyard contract.

`wake_consumer_available` remains `false`. Persisting a launch profile does not
transfer monitoring, authorize pausing, start a process, or make Linear an
execution authority. The deterministic monitor continues to run without a
model.
