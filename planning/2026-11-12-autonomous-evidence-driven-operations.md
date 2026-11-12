# Shipyard: Autonomous, Evidence-Driven Operations

Status: planning only. No implementation, recovery loop, policy change, PR, or
fleet mutation is authorized by this document.

## Objective

Shipyard should become an autonomous, evidence-driven operations partner.

The product is a reliable operations system that continuously establishes the
trustworthy state of software changes, takes the next safe action, recovers from
routine failures, and involves a human only when judgment or authorization is
actually required.

Deterministic Shipyard machinery should handle routine decisions. Foundation
models should be used selectively for ambiguity, novelty, diagnosis, repair
proposals, and policy or architecture review. A model may recommend an action;
it must not authorize a merge, override a required check, weaken a safety
policy, or turn unknown evidence into approval.

The intended outcome is:

> Shipyard makes the correct routine decision, takes the safe next action, and
> asks Daniel only when the decision is genuinely ambiguous, consequential, or
> novel.

## What this plan must establish

This plan is for a one-person team. It should produce small, independently
useful improvements and evidence for the next decision. It must not become a
calendar-driven autonomy program, a new agent framework, or a second
orchestration layer.

The work should answer five questions:

1. Which routine decisions can Shipyard already make safely?
2. Which decisions are blocked by missing provenance, missing policy, or weak
   tooling rather than by genuine complexity?
3. Which agent actions are useful ownership and which are ceremony, repetition,
   verbosity, or avoidable handoffs?
4. Which small deterministic recovery would remove the most human work safely?
5. Where, if anywhere, is an expensive model needed after the existing rules
   and evidence have been exhausted?

## Existing ownership boundaries

Preserve and strengthen the existing division of responsibility.

### Shipyard

Shipyard owns queue coordination, policy evaluation, evidence validity,
admission decisions, evidence reuse, routine recovery decisions, attention
classification, and agent-facing structured output.

Shipyard should classify work as `PASS`, `BLOCKED`, `STALE`,
`INFRASTRUCTURE_WAIT`, `UNSAFE_TO_REUSE`, `HUMAN_REQUIRED`, or `UNKNOWN`.
`UNKNOWN` is a terminally honest classification for an unavailable or
insufficient observation. It must not be coerced into pass or fail.

### TartCI

TartCI remains authoritative for VM lifecycle, JIT runner provisioning, host
capacity, leases, execution, and teardown. Shipyard consumes TartCI runtime
receipts and may classify or act on them, but should not recreate TartCI's VM or
runner authority.

The existing integration remains the preferred path:

```text
tartci runtime export --repo OWNER/REPO | shipyard metrics import tartci
```

TartCI facts must remain distinct from Shipyard conclusions. A Linux VM result
cannot silently satisfy macOS runtime evidence, and a runner-health result
cannot silently satisfy a source-validation result.

### Subrouter

Subrouter owns provider routing, pooled-account selection, provider failover,
and model transport. A Shipyard escalation wrapper must not add a second retry
or account-pooling system on top of Subrouter.

### Harbour Master

Harbour Master is the human-attention and communication layer. It should
communicate consequential transitions, unresolved failures, authorization
requests, and meaningful policy choices. It should not narrate normal retries,
successful recovery, unchanged state, or repeated copies of the same finding.

### GitHub and the steward

GitHub remains authoritative for PR, review, check, and merge state. The
Shipyard steward remains the governed actor for routine queue and merge
operations when policy permits. Agents must not ask Daniel to perform a routine
merge, enqueue, reviewer nudge, or recovery action that the existing steward or
approved automation can safely perform.

## Prior art and adopted patterns

Shipyard should absorb useful patterns from Macroscope and comparable CI systems
as small, inspectable contracts over its existing queue, evidence, and TartCI
integration.

| Pattern | Shipyard treatment |
| --- | --- |
| Prerequisite checks and `waitsFor`/`requires` | Evaluate as typed dependencies with explicit pending, skipped, failed, and unknown states. |
| Isolated CLI reviews and NDJSON | Reuse existing command-evidence and JSON envelopes; add fields only where current receipts cannot express exact provenance. |
| Approvability preview | Provide a read-only explanation of blockers, missing evidence, invalidated evidence, and the next safe action. It remains advisory. |
| Incremental review and reuse | Reuse only when base/head ancestry, policy, toolchain, stage contract, and input surface match. Explain every reuse and invalidation decision. |
| Policy inheritance | Consider bounded composition only with protected policy provenance, visible resolution, conflict refusal, and deny-by-default behavior. |
| Benchmarks such as MacroscopeBench | Build from real Shipyard/TartCI/agent history and planted negatives; measure correctness, cost, latency, recovery, and attention quality. |
| Operating modes | Evaluate existing profiles before adding `fast`, `normal`, `deep`, or `release` modes. Do not add modes without a demonstrated decision tradeoff. |
| Webhooks, Slack, S3, and hosted dashboards | Defer until decisions and evidence are useful. Delivery cannot compensate for noisy or incorrect state. |

For every candidate pattern, record whether equivalent functionality already
exists, the demonstrated problem it solves, the smallest implementation, and a
decision of `ADOPT`, `CONSTRAIN`, `DEFER`, or `REJECT`.

## Non-negotiable constraints

- Bind evidence and decisions to repository, PR, base SHA, exact head SHA, and
  the policy revision used.
- Fail closed on missing, stale, malformed, or contradictory required evidence.
- Never allow model output to authorize a merge, bypass a required check, or
  redefine safety policy.
- Do not introduce duplicate scheduling, execution, retry, caching, policy, or
  model-routing authority.
- Do not create a new cloud control plane, orchestration framework, or parallel
  provenance store without evidence that existing mechanisms cannot do the job.
- Treat untrusted PR content, logs, comments, and generated artifacts as input
  to a proposal, never as executable authority.
- Keep platform and provider evidence separate.
- Bound retries, deadlines, resource use, and escalation depth.
- Preserve durable action receipts and explicit terminal outcomes.
- Keep review output concise, consolidated, exact-head, and useful to the next
  actor.

## Deliverable A: reliable model escalation

This is a tooling contract, not a new model-control plane.

### Desired behavior

Provide one reusable entry point for an agent session that requests a model tier
such as `ultra` and returns a structured receipt. The entry point should:

- route through Subrouter;
- verify the actual served model and reasoning level when trustworthy metadata
  is available;
- reject a requested/actual model mismatch;
- apply a hard deadline;
- preserve Subrouter's own provider/account retry behavior without duplicating it;
- record route, session, request ID, requested and actual model, reasoning level,
  verification evidence, and terminal result;
- return explicit outcomes including `COMPLETED`, `MODEL_MISMATCH`,
  `AUTH_FAILURE`, `TIMEOUT`, `VERIFICATION_UNAVAILABLE`, `QUOTA_EXHAUSTED`,
  `CANCELLED`, and `OUTPUT_INVALID`;
- never attribute an opinion to a model without a receipt proving which model
  produced it.

### Required proof before implementation

Before building a general wrapper, establish:

1. Whether Subrouter exposes reliable served-model and reasoning metadata.
2. Whether a real Shipyard or agent workflow needs this entry point.
3. Whether the wrapper can remain thin and avoid retry/account-pool duplication.

If verification is unavailable or no real caller exists, record that result and
defer implementation rather than building ceremony.

The current aliases must be treated as distinct and verified. The observed
`fable` alias resolves to `claude-fable-5-2`; the observed `opus` alias resolved
to `claude-opus-5-5`. A requested alias is not proof of the served model.

## Deliverable B: historical autonomy and usefulness benchmark

Start with existing history rather than inventing a synthetic benchmark.

### Sources

Join, where available:

- Shipyard `metrics.db`, ship states, queue records, evidence bundles, reuse
  records, and action receipts;
- TartCI runtime exports, lease events, runner health, VM boot, execution, and
  teardown evidence;
- GitHub check, review, queue, steward, repair, and merge records;
- Harbour Master notifications and activity threads;
- agent-session journals and transcripts across fleet hosts;
- Subrouter/model receipts and usage metadata.

Cross-host session identifiers must include host identity, session identity,
surface identity when available, and a durable transcript or journal reference.
Raw cmux workspace/surface handles are not portable across hosts and must not be
treated as durable identity.

### Initial sample

Begin with a manageable representative sample, such as 30–50 cases, then
expand only if the first findings justify it. Include:

- merge-queue failures and missing checks;
- stale or invalid evidence;
- runner, lease, VM, and infrastructure failures;
- repeated or unnecessary builds and retries;
- incorrect reuse or invalidation;
- recovery loops and successful recoveries;
- holds, approvals, and human interventions;
- repeated, verbose, or duplicate agent reviews;
- requests for manual merges, enqueues, reviewer nudges, or other avoidable
  operator actions;
- Harbour Master notifications that were useful, redundant, or noise.

Each case must record the state snapshot available at decision time, the
available evidence, the expected classification, the safe next action, the
terminal outcome, the source of the label, and whether human involvement was
actually necessary. If ground truth is unavailable, record `UNKNOWN` and report
the unknown rate. Do not use hindsight to make a decision-time classifier look
better.

### Measures

Measure safety and usefulness separately:

- false approvals and false holds;
- stale or incorrectly reused evidence;
- unnecessary builds, retries, and model calls;
- autonomous recovery rate and time to safe resolution;
- human interventions per 100 PRs;
- avoidable versus human-required handoffs;
- review duplication, review length, token use, and latency;
- model usage, cost, and verification failures;
- notification signal-to-noise and repeated-message rate;
- percentage of cases classified `UNKNOWN`.

An aggregate efficiency improvement cannot hide a safety regression. A benchmark
result is incomplete until it reports both the benefit and the uncertainty.

### Review usefulness contract

An agent review is useful when it is tied to the current head, states the
decision-relevant finding, names one owner and one next action, and does not
repeat an already active finding. The preferred output is:

```text
OUTCOME: APPROVE | HOLD | REQUEST_CHANGES | UNVERIFIED | UNKNOWN
HEAD: <full SHA>
FINDINGS: <deduplicated decision-relevant findings, or none>
NEXT: <exact action and owner>
EVIDENCE: <receipt/check/evidence identifiers>
```

The review should not ask Daniel to perform an action that existing policy and
automation can safely perform. If the action truly requires Daniel, it must say
why, what authority is needed, and what will happen after approval.

## First deterministic recovery candidate

After B identifies the highest-value low-risk class, recommend exactly one
recovery loop. Do not implement it automatically from this plan.

The recommendation must include:

- historical frequency and cost;
- explicit preconditions and typed evidence;
- permitted actions and their authority;
- bounded retries, deadlines, and resource budgets;
- idempotency and concurrency protection;
- circuit breaker and quarantine behavior;
- durable action receipt;
- terminal states and escalation conditions;
- expected reduction in human intervention;
- negative tests and a planted-negative control.

The likely candidates include a known stale/wedged-run rerun path, safe runner
lease recovery, or a narrowly classified infrastructure retry. The benchmark
must choose among them; the plan must not assume one in advance.

## Agent-session and fleet review

Review historical sessions as operational evidence, not as a model-quality
showcase. For each session, ask:

1. Did it understand the actual goal and current authority boundary?
2. Did it inspect current evidence before acting?
3. Did it take ownership of safe routine work?
4. Did it repeat findings or generate unnecessary tokens?
5. Did it ask for a human action that automation could perform?
6. Did it leave a durable, exact next action and receipt?
7. Did it stop when evidence was unavailable, or invent confidence?

The result should identify improvements to shared tooling, prompts, receipts,
review output, routing, and onboarding. Do not fix each symptom with a new
agent instruction when one deterministic contract would address the class.

## Execution order for a future implementation session

1. Read this document, `CLAUDE.md`, `AGENTS.md`, the current Shipyard state
   machine, metrics/evidence contracts, and the TartCI integration docs.
2. Establish exact repository heads and preserve dirty checkouts.
3. Inventory existing behavior before proposing new code.
4. Run the historical benchmark slice and report evidence, unknowns, and
   decision-time labels.
5. Verify model attestation and identify a real escalation caller.
6. Recommend the first recovery loop without implementing it.
7. Implement only explicitly authorized, independently reviewable changes.
8. Keep any model or recovery proposal subordinate to deterministic policy and
   exact-head evidence.

## Success criteria

Success is measured by outcomes, not by the number of agents, features, model
calls, or comments:

- more correct routine decisions;
- more safely reused evidence;
- fewer unnecessary builds, retries, and model calls;
- more routine failures resolved without human intervention;
- faster safe recovery;
- fewer avoidable manual handoffs;
- shorter, less repetitive, more decision-useful reviews;
- fewer low-value notifications;
- no increase in false approvals or provenance failures;
- explicit, honest `UNKNOWN` coverage.

The desired result is increasing autonomy because operational knowledge is encoded
into reliable, testable machinery—not because foundation models are given more
authority.
