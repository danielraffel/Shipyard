# Queue observer

`shipyard queue-observe` replaces repeated per-PR and per-run status polling
with one deterministic, read-only GraphQL snapshot per tick. It emits only an
initial observation or a real state transition.

```bash
# One observation; JSON is compact and stable.
shipyard --json queue-observe --repo Generous-Corp/pulp --base main

# Continue with adaptive polling. Unchanged polls print nothing.
shipyard --json queue-observe --repo Generous-Corp/pulp --follow

# Exercise the complete state machine without network access or sleeping.
# Replay defaults to a fixture-specific state/log namespace, never live state.
shipyard --json queue-observe \
  --repo acme/pulp \
  --replay tests/fixtures/queue-observer \
  --state-file /tmp/queue-observer-state.json \
  --transition-log /tmp/queue-observer-transitions.jsonl
```

## Contract

Each live tick issues one GraphQL `query` containing:

- exact base-branch SHA and commit URL;
- up to 100 open PRs with exact heads, URLs, assignees/owner labels,
  blocker labels, auto-merge state, and latest check contexts;
- the observed base ref's effective classic branch-protection required
  contexts, unioned with
  `governance.required_status_checks` from Shipyard configuration;
- up to 100 server merge-queue entries in order, with PR heads, admission
  timestamps, speculative merge-group SHAs, and the merge-group commit's
  latest required-check contexts.

Bounded GraphQL connections set `truncated=true` rather than silently claiming
completeness. Ruleset-required contexts that are not present in Shipyard config
or classic branch protection remain visible as checks but cannot be marked
`required`; configure `governance.required_status_checks` for authoritative
classification.

The command also reads Shipyard's existing machine-local mutation-authority and
`HOLD` records. This records the owner and blocker boundary without acquiring a
mutation lease. PR-level ownership is derived from
`shipyard:owner/<name>` labels when present, otherwise from assignees. Explicit
PR blockers use `shipyard:blocker/<reason>` labels. The observer never creates
or edits labels.

There is no mutation flag or mutation query. A metadata/read-only GitHub token
is sufficient; no write credential is required. Queue enqueue/dequeue, rerun,
refresh, push, merge, and hold/resume operations are outside this command.

## Durable state and output

The default state files live under Shipyard's machine state root:

```text
queue-observer/<repo-and-base-digest>.json
queue-observer/<repo-and-base-digest>.transitions.jsonl
```

The first file is atomically replaced and contains the canonical snapshot,
SHA-256 hash, and backoff cursor. Its hash is verified on load. The second is an
append-only NDJSON transition log. Transition append precedes cursor advance,
giving crash recovery at-least-once delivery rather than silently losing a
transition. Each record is encoded before append, serialized by a log-specific
lock, and an incomplete crash tail is removed before the next append. Consumers
can deduplicate by `state_hash`. A new process resumes
from these files and does not reconstruct queue history from agent context.
An exclusive per-state-path lock rejects a second collector before either
process can overwrite the other's cursor. A separate log-path lock prevents
collectors with distinct state files from interleaving a shared log.

JSON transitions include the full snapshot plus semantic JSON-pointer changes.
The Markdown renderer retains every commit, PR, and check URL; every observed
SHA; owners; hold reason; and explicit blockers. If the canonical hash is
unchanged, stdout and the transition log remain silent while only the backoff
cursor advances.

Polling begins at 15 seconds, then backs off through 30, 60, 120, and 300
seconds while unchanged. Any transition resets the next delay to 15 seconds.
Follow mode also retries five consecutive read or parse failures with those
same bounded delays while preserving the last durable state. Each GitHub read
attempt has a 60-second timeout, so a stalled credential helper or network call
cannot escape that retry budget.
Over an unchanged hour this makes 16 queries versus 240 fixed 15-second polls:
93.33% fewer queries. Fixture replay covers initial state, queue admission,
merge-group materialization, required-check failure, refreshed PR head, local
ownership hold, and merge; every adjacent fixture produces exactly one
transition.

The observer is intentionally a deterministic collector. A local or cloud LLM
may summarize emitted transitions afterward, but it should not sit in this
polling loop.
