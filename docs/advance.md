# `shipyard advance`: routine release-movement acts

Some steps that move a pull request toward landing are routine but used to
need the owner, because the rule that made them safe lived in comments and
decision files. `shipyard advance` checks that rule itself, performs one act
through `ghapp`, and leaves an audit trail. The merge steward stays the only
writer for admission: `advance` never arms, enqueues, merges or posts a
verdict.

```bash
shipyard advance OWNER/REPO#N --retrigger --reason "merge ref is stale after main moved" [--head SHA] [--dry-run] [--json]
shipyard advance OWNER/REPO#N --release-hold [--decision REF] [--head SHA] [--dry-run] [--json]
```

`--dry-run` reads everything, prints the decision and the evidence, and
writes nothing. `--head` binds the act to the head you expect.

## The acts

### `--retrigger`

Closes and reopens the pull request so GitHub rebuilds a stale merge ref.
Refused unless all of these hold:

| Condition | Refusal token |
|---|---|
| the pull request is open | `not_open` |
| the live head is the one `--head` names, and the REST and GraphQL reads agree on it | `head_moved` |
| it is not in the merge queue | `queued` |
| native auto-merge is not armed (closing would disarm it) | `armed` |
| no trusted `repair-claim: <lane> until <time>` comment is live; a time that does not parse counts as live | `repair_claim_live` |
| the base names required checks, at least one reported at this exact head, and none is still running | `required_checks_unknown`, `no_required_run_at_head`, `required_run_in_progress` |
| `--reason` says why, in at least 10 characters | `reason_missing` |

### `--release-hold`

Removes `shipyard:no-auto-merge`. Refused unless all of these hold:

| Condition | Refusal token |
|---|---|
| the label is on the pull request | `hold_label_absent` |
| every required check is green at the exact head | `required_checks_unknown`, `required_not_green` |
| the steward's own approval gate (`head_approval`, policy read strictly from the base, judged as a dry run) approves the exact head with an APPROVED review on that commit, or a verdict comment that carries a `ghapp` stamp | `approval_missing`, `approval_not_exact_head` (a carried, rollup or admission approval), `approval_unstamped` |
| every condition the hold comment names is met, or `--decision` names a decision file that names this pull request | see below |

The hold comment is the one comment, by the login that last added the
label, posted within 15 minutes of that event, whose body names
`shipyard:no-auto-merge`. None is `hold_comment_missing`, more than one is
`hold_comment_ambiguous`, and a label whose last event is a removal (or no
event at all) is `hold_event_unread`.

A hold names its conditions as lines:

```text
hold-condition: merged OWNER/REPO#N      that pull request merged
hold-condition: after 2026-11-14T00:00:00Z
hold-condition: approved                 exact-head approval (checked anyway)
hold-condition: green                    required checks green (checked anyway)
```

Any other `hold-condition:` text is `hold_condition_unchecked`, a hold with
no such line is `hold_conditions_unnamed`, and a condition not yet true is
`hold_condition_unmet`. Each of those needs `--decision`.

`--decision` takes `OWNER/REPO:PATH[#anchor]` or a GitHub blob URL to a file
whose name starts with `DECISIONS`. The file is read through the API and
must mention `OWNER/REPO#N` or the pull request's URL
(`decision_unrelated` otherwise). A decision replaces the hold's conditions
only; it never excuses a red check or a missing approval.

## Both acts

- **Never your own pull request.** The caller is identified the way `ghapp`
  stamps verdicts and `shipyard pr` records owners: `SHIPYARD_AGENT_ID` and
  the session ids (`SHIPYARD_STAMP_SESSION_ID`, `CLAUDE_CODE_SESSION_ID`,
  `CODEX_COMPANION_SESSION_ID`, and the `whence` and Codex ids). A caller
  named by the body's `shipyard-owner` or `whence` marker, or by this host's
  owner record, is `actor_authored`. A caller whose stamp is on any APPROVE
  verdict on the pull request is `actor_approved`. A caller with no id at
  all is `actor_unidentified`. Members of one Claude Code team share a
  session id, so a teammate's pull request is refused too.
- **Fail closed.** Every read must succeed and be complete (check runs are
  counted against `total_count`; GraphQL errors and a missing pull request
  are refused). An unread fact exits 2 and changes nothing. One exception
  matches the steward: when the App has no administration access, GitHub
  answers the classic branch-protection read with 403, and the required set
  then comes from the rulesets alone. The evidence line says so whenever
  that happens.
- **Bound to the head.** Every fact is read at one head, and the pull
  request is read again immediately before the write; a different head is
  `head_moved`. A retrigger also re-reads the merge-queue state then, and a
  pull request queued or armed since the decision is `queue_changed`.
- **Through `ghapp`, with no override.** Writes run the `ghapp` wrapper with
  every `GHAPP_ALLOW_*` variable and Shipyard queue marker stripped, so its
  guards judge them exactly as they judge a hand-typed call. A guard refusal
  is reported (`close_refused`, `label_removal_refused`) and nothing changed.
- **Audited once.** An act posts one comment on the pull request (what, why,
  the evidence checked, the caller, and a `<!-- shipyard-advance:v1 ... -->`
  marker) and appends one JSON line to `advance.jsonl` in Shipyard's state
  directory, beside `queue-removals.jsonl`. If that log cannot be opened the
  act is not taken.

Exit codes: 0 acted (or would, under `--dry-run`); 1 refused, with the
token; 2 a fact could not be read or the log cannot be written; 3 the act
started and did not finish cleanly (for example closed but not reopened),
which the comment and log both record.

Defect dequeues are not an act here. They already have one audited path,
the queue-removal guard's `defect-fix` reason (see the operator reference,
`docs/ghapp-guards.md`).

## `pr-close-guard` refuses every retrigger today

`pr-close-guard` refuses closing a pull request whose head is not contained
in its base, and an open pull request's head never is. So until the guard
changes, `--retrigger` reaches the guard, is refused (`close_refused`), and
changes nothing. `advance` does not step around it.

Proposed change, the narrowest one found: let the guard allow one close it
can tie to this command, using the one-shot nonce scheme the queue guards
already use (`src/queue_arm_gate.rs`, `consume_nonce` in
`scripts/ghapp_queue_removal_guard.py`). Just before the close, `advance`
issues a nonce record for the purpose
`advance-retrigger:<owner/repo>#<n>@<head>` and puts the nonce only in that
`ghapp` child's environment. The guard computes the same purpose from the
request and the live head it already reads, consumes the record (owner,
mode 0600, ten-minute age, unlinked on use), and only then allows the close.
Any other close, a reused or mismatched nonce, or a moved head is judged as
today. The guard is a member of the authenticated `ghapp` generation, so the
change ships through `shipyard runner fleet-update` and needs its own review.

## Steward follow-up

The decision functions (`crate::advance::decide_retrigger`,
`decide_release_hold`) are pure and take already-read facts, so the steward
can call them to perform these acts itself, for example retriggering a
stale merge ref it finds in a pass. This command does not wire that up.
