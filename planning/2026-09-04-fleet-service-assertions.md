# Fleet service assertions: assert service, never liveness

Design note for `src/fleet_service.rs`. Refs #285 (the scheduled half), #363
(the dispatch-time half), #343 (cross-machine visibility).

## The problem this exists for

On 2026-09-04 two machines were found to have been failing for weeks, and every
passive signal we had read healthy the whole time:

| Host | Failure | Duration | How it was found |
| --- | --- | --- | --- |
| macpro | three one-job clones outlived their jobs, holding the whole lease budget and the whole VMID range; the pool crash-looped `NRestarts=36088`; zero Linux capacity | ~19 days | a human asked "are all machines picking up work?" |
| macmini | unreachable over ssh; also the first relay hop for every proxied call on another host | days | a human traced an 18s-vs-2s latency by hand during an outage |
| M3 | release supervisor blind | ~24 h | traced by hand during the same outage |
| M5 | release supervisor blind **with the identical fault**, still present after M3's was found and fixed | hours | only because someone re-checked the other host |

**Every one of those hosts was up.** An uptime ping would have reported all four
green. That is the whole design constraint.

Five independent blindnesses, each sufficient alone:

1. A hosted fallback absorbed the work, so the lane's *output* stayed green
   while its local half was dead. Fallback is a good property; a *silent*
   fallback is not.
2. The dead lanes were not required checks, so no PR blocked.
3. A crash loop is indistinguishable from steady state — `systemctl status` said
   `active (running)` for 19 days. It was running, and failing, every 30
   seconds, 36,088 times.
4. The runner census is scope-blind: `repos/<org>/<repo>/actions/runners` omits
   org-registered runners entirely, and the Linux and Intel runners register at
   the org. The obvious "is anything serving this lane?" query returns empty
   whether the lane is healthy or dead.
5. The one error message we did get named the wrong subsystem. `SCAN BLIND (gh
   queue scan failed)` plus `self-restarting for fresh gh auth` pointed at
   authentication. Auth was never broken; it was a timeout — and the supervisor
   then took a corrective action that could not possibly help.

## Why a typed verdict rather than a boolean

The distinctions *are* the product. Each of these pairs cost a real
investigation when it was collapsed:

- **`Unserved` vs `Idle`** — an idle just-in-time pool legitimately registers
  nothing, so an empty census alone cannot mean "broken". Only pairing the
  census with queued demand decides it. An issue was filed on this confusion
  and closed as wrong.
- **`Unserved` vs `Starved`** — a job queued on labels nothing advertises is
  *unschedulable* and will wait forever; a job queued while an online runner
  does advertise them is a scheduling or capacity fault. Same symptom, opposite
  remedies.
- **`Unknown` vs anything** — a census that could not be read is not a pass.
  Folding an unreadable instrument into "healthy" is the failure mode this
  module exists to end.

`ServiceVerdict` is `Ord` by severity so a roll-up takes the worst without
re-deriving precedence per call site. `roll_up(&[])` is `Unknown`: asserting
nothing is not the same as asserting everything passed.

## Both runner scopes are mandatory

On the fleet this was written against, **three of the six declared self-hosted
lanes are served only by org-scope runners**. A repo-scope-only census reports
those three unserved while they are online — the identical empty reading it
gives when the host is genuinely dead.

`assess_lane_service` therefore takes one census spanning both scopes and
records which scope satisfied each lane, so org-only service is visible in the
output rather than inferred. The matched-pair test
`negative_control_a_repo_only_census_manufactures_a_false_unserved_verdict`
runs both censuses over identical demand so the *only* difference is scope.

## Corrections the live fleet forced

Read-only measurement before writing the assertions changed three decisions
that the incident write-ups alone would have got wrong:

1. **`NRestarts` cannot be a threshold.** macpro reads `36089`/`35707` on a
   *currently healthy* host — the counter is monotonic and survived the repair.
   An absolute-value check would be permanently red, which is operationally
   identical to no alarm. It has to assert a delta per interval against a
   stored baseline, and report the absolute as context only. (Lands with the
   pool assertion, not this slice.)
2. **Supervisor blindness flickers.** A release supervisor's log showed 1598 of
   its last 2000 lines as `SCAN BLIND … N/9` and 80 as `queued=`, with the last
   blind line ~70 lines from EOF and the tail reading healthy. Sampled at that
   instant it is green. The verdict must be a ratio against budget over a
   window, never a single sample. (Also a later slice.)
3. **Routing values use three encodings in one namespace** — a JSON array, a
   JSON string, and a bare unquoted string, plus a `local-only` sentinel that
   names no runner at all. A parser assuming valid JSON silently drops lanes.

## A refusal must name its boundary

The second remit, and the same defect family as (5) above. `Boundary` splits
four facts that otherwise collapse into one opaque failure:

| Boundary | Means | Equivalent path? |
| --- | --- | --- |
| `Grammar` | a command wrapper refused the verb | yes — a raw API call usually is one |
| `Scope` | permitted and well-formed, but issued where the answer is invisible | yes — ask in the other scope |
| `Identity` | wrong principal | yes — retry as the other identity |
| `Permission` | genuinely not allowed | **no** |
| `Parse` | value not understood | yes |
| `Transport` | timeout, transport, rate limit — *not* an auth fault | yes |

`Unknown` always carries one, and no measured verdict may carry one. Only
`Permission` denies that an equivalent path exists; reporting any of the others
as "cannot" is what makes a session stop at a wall that has a door in it.

## Shape

Pure, mirroring `runner_watchdog`: no I/O, no ambient clock, `now` injected.
The CLI layer owns every `gh` call. This slice is the verdict core and its
fixtures; the fetch/render command, the pool/supervisor/relay/guard assertions,
the bounded self-heals, and the off-host escalation follow as their own PRs.

## Acceptance

Every assertion ships a planted negative control that must go red, reproducing
the real incidents as fixtures — a detector that cannot fail its own test is
precisely the failure mode this issue is about. Two of the controls here caught
errors in their own fixtures during development, which is the behaviour a
decorative test would not have.
