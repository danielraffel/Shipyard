# Fleet health lessons

A Rust design that implemented these checks as typed assertions (scan blindness, relay hop cost, guard arming, escalation with hysteresis, leak detection, handback, health reconciliation, runner survivability) was never wired to a command and was removed. Commit `cc1415c66e988aedbb760ca10105d66525c28a60` is the last main commit that contains it.

The live fleet watchdog is separate: the `fleet-health` (daily) and `fleet-liveness` (every 10 minutes) scripts in the agent-skills repo check reachability, the Shipyard daemon, runner registration, TartCI pool and self-update state, and post failures to the fleet Discord channel. See the fleet-handbook's `docs/monitoring.md`. The lessons below, each paid for by a real incident, apply to that watchdog or any successor.

## Supervisor scan blindness is a ratio, never a sample

A tartci supervisor decides whether to boot a VM by scanning the queue. When the scan cannot finish in time it logs `SCAN BLIND (gh queue scan failed) N/9`, boots nothing, and every job on that lane's labels waits with no error visible anywhere in GitHub. The jobs are simply `queued`.

Do not judge it from a sample. A release supervisor measured on this fleet was blind on 1598 of its last 2000 log lines against 80 sighted, while the final 70 lines or so read perfectly healthy. A single check taken at that moment returns green.

Rules:
- Report the blind to sighted ratio over a window, next to the supervisor's own consecutive-blind counter. Nine in a row is a different fault from nine scattered, so track both.
- Do not add a minimum-window guard. "Only trust the ratio after N cycles" lets a short, entirely blind window read as a pass, which is a blind spot inside a blindness detector.

## A timeout is not an auth failure

The supervisor logs `self-restarting the supervisor for fresh gh auth` for what is really a timeout. Authentication was never broken, and the restart cannot help.

Rules:
- Treat a timeout as a transport problem and do not re-authenticate in response.
- Call something an auth fault only on real credential evidence (`HTTP 401`, `bad credentials`).
- A rate limit is a completed call, not credential evidence.
- Keep two separate answers: why your own check could not measure, and what the supervisor's own failures classify as. The second is the one that reads "transport" on a log that says "auth".

## A budget set by absence is the shape of the bug

A lane that declares no scan timeout inherits the 15 second default. That is how the release lane ended up with a budget nobody chose, while the gate lane on the same host declared 180 seconds and absorbed identical latency.

Rules:
- Treat a missing budget as its own finding, not as "fine".
- Report observed latency as a ratio against whichever budget applies, so the same measurements can read healthy under a declared budget and degraded under an inherited one.

## Relay hops: "does the proxy answer?" is the wrong question

A relay was started with `--relay-host macmini --relay-host m1` while macmini was unreachable. The relay kept working because the fallback succeeded, so every liveness question stayed green. The cost was a tax paid on every connection before the fallback: 18 s via proxy against 2 s direct on one host, 5.5 s against 0.2 s on another. The tax silently exceeded a downstream timeout. A supervisor whose queue scan inherited a 15 s budget went blind, booted no VM, and a release lane starved for about a day, twice.

Rules:
- Check each declared hop, in order, against a budget. Do not ask whether the relay as a whole responds.
- Position decides the cost. A hop failing before the first hop that answers is paid by every connection. The same hop behind an answer costs nothing today, but it is redundancy already lost.
- Treat connect time as a ratio against its budget. A hop at 4.9 s of a 5 s budget is one bad afternoon from the incident.
- A hop that cannot be measured is unknown, even when its siblings are healthy.
- If no hop answers, the relay is severed, not taxed.
- "Any hop connected" is a fact to report, never a verdict. It is exactly the naive question that stayed green through the outage.
- Reordering healthy hops first and dropping dead ones is a reasonable remedy, but refuse to do it if any hop was unmeasured or if the drop would leave nothing that connects.
- When reproducing, do not use `env -i`. It strips the `*_proxy` variables, so the control was cleaner than the thing it controlled for and passed every time.

## A guard is not a guard until something proves it is armed

**Units must be armed, not merely present.** A Linux host served zero work for about 19 days while the reaper that would have recovered it sat on disk. The script was installed (a stale copy), but its `.service` and `.timer` were never installed, so nothing ever invoked it. `enabled` alone is also not enough: a timer that is enabled with no next elapse never fires, which is as useless as absent. Keep these as separate states with separate remedies: not installed, not enabled, masked, enabled but no next elapse, armed.

**Assert `NRestarts` as a rate, never a level.** The counter is monotonic and survives the repair. The host above still reads `NRestarts=36089` while perfectly healthy. A threshold on the absolute value is a permanently red alarm, which is the same as no alarm. Alert on the delta against a recorded baseline over a known interval, and show the absolute as context only. A first observation with no baseline is unknown, since one sample gives no rate. A counter that went backwards is also unknown, because the interval spanned a reset.

**Compare only what actually executes.** A drift measurement against `~/.local/share/tartci` reported hundreds of changed lines, but that directory is inert. The supervisors exec a content-addressed generation under `~/.local/share/tartci-generations/<commit>-<manifest16>/`, and the gate LaunchAgent names that path directly. Measured against the generation, the files were byte-identical to their source commit: zero drift, merely some commits behind head. An inert copy of a real thing passes every check except "is this the artifact that executes?". If the executing path is not proven, the answer is "undetermined", not a drift verdict.

**Drift has three values.** In sync, behind the repo (deploy), or ahead of the repo (upstream the delta and do not redeploy), plus the size of the delta so two changed lines and two hundred are not the same alarm. Treating "ahead" as "behind" licenses a redeploy that deletes whatever the host was carrying. An artifact with no recorded upstream ref cannot be compared.

For a content-addressed runtime the identity is already recorded twice: the generation directory is named `<source_commit>-<manifest_sha256[:16]>`, and the install receipt records `support.source_commit`. Resolve and compare those instead of inventing a new identity scheme. A change reaches such a host by PR plus a newly cut generation, never by editing a file in place.

## Escalation leaves the host, and it has hysteresis

A journal line on a broken machine is not a signal, because nobody reads that machine. The target is the shape that already works elsewhere in this stack: open a tracking issue on degradation and auto-close it on recovery. The bar is that a lane unserved for an hour should cost a human one glance, not nineteen days and a lucky question.

Hysteresis is the design, not a refinement. Every signal here flickers. A supervisor blind on 1598 of 2000 cycles reads healthy in its last 70 lines, and a just-in-time pool registers and deregisters around every job. Reacting to a single sample would open and close an issue repeatedly, and a flapping alarm is worse than none: it gets ignored, and the one real occurrence is ignored with it.

Rules:
- A subject must fail continuously for a raise delay (900 s was the default) before anything opens, and be healthy continuously for a longer clear delay (1800 s) before it closes. Clearing is slower than raising so a fault that briefly looks fixed does not close its own issue and restart the cycle.
- Leave an unchanged body alone. Re-posting the same text every cycle buries the one edit that mattered.
- One issue per subject key, never a fleet roll-up. A roll-up hides the second instance of a fault behind the first, which is how a supervisor fault on one host stayed dark for hours after its twin was found and fixed.
- "Unknown" escalates like any other fault, and the body names the boundary that stopped the measurement and the next action. An assertion that could not measure did not pass.
- Every body names the host, the lane, what was already tried, and the next action. An alert that only says something is wrong makes its reader start from nothing.

## The leak assertion, and the handback contract

A live object whose subject has already ended is itself a defect. Three leaks found on one day: three VM clones outlived their jobs by nineteen days, a queued run that could not be cancelled became an immortal demand signal, and a `--follow` watcher polled for three days and thirteen hours past its pull request's merge. Each was found by a person happening to look.

It is not a rare shape. Of 188 ship states measured on one host, 46 (a quarter) carried empty evidence and no dispatched runs, which reports "in flight" forever. 44 belonged to one repository, and the oldest was forty days old.

**Absence is never success.** A narrow fix for the watcher was designed and adversarially reviewed, and the review found it could emit exit 0 for a pull request closed without merging, a false success worse than the original bug. A terminal state must record how the subject ended, success must mean a positively observed success, and there must be no path from "I cannot see it" to a pass.

**The authority wins on terminality in both directions.** Local evidence is authoritative for quality, never for whether the subject ended.

| Subject | Local | Winner | Action | Losing signal |
|---|---|---|---|---|
| terminal | terminal | agree | hand back with the subject's outcome | recorded |
| active | active | agree | keep tracking | recorded, quiet |
| terminal | appears active | subject | hand back, after deciding cancel vs drain | recorded, raises |
| active | appears terminal | subject | do not hand back | recorded, quiet |

Row 4 is the everyday wait on review, a required check, or the merge queue: locally complete and upstream open. Terminating there is the false terminal, which is why the authority cannot override in one direction only.

Row 3 is the only row needing a policy: drain if the remaining cost is provably bounded and small, cancel otherwise. Unbounded work is not drainable. Unmeasured work is escalated rather than cancelled, because missing measurement is no licence to cancel work somebody still needs. Whatever is chosen is stated, since silently abandoning the work is the leak in miniature.

More rules:
- Never discard the losing signal. Two disagreements are findings in their own right: a subject that landed past a failing gate, and one that closed unmerged although validation passed.
- Merged is permanent. Closed is revocable, because pull requests reopen, and a terminal that cannot be revoked strands a live subject as permanently ended. A successor never inherits a terminal, or a replacement is born dead.
- Refuse an under-qualified subject reference before probing. It resolves against an ambient default and can report an unrelated subject as ended. A wrong terminal is worse than none.

## Being right does not discharge the duty

A process that exits fail-closed because it cannot prove a resource is gone has made the correct decision, and it must never be "fixed" by making it proceed. But a correct refusal is the start of an obligation, not the end of one.

At the moment it stops, that process knows three things nobody else does: that it stopped, what it was holding, and why it could not finish. If it exits without saying them, that knowledge dies with it. The real incident: a teardown refused correctly, raised nothing, and left a failed lease release unresolved, so a shared resource stayed held by a process that no longer existed.

The three obligations, most consequential first:
1. **Bound the retry.** Repeating an unproven action is not recovery. An unbounded restart into unproven state is the `NRestarts=36088` crash loop, and it does damage on a timer.
2. **Dispose.** Every held resource gets an owner or an explicit release. A lease held by a dead process is a leak whatever the exit code was.
3. **Raise.** Somewhere that outlives the host, because a journal line on a machine nobody is looking at is not a signal. An empty reference does not count.

Two things this must not do:
- A clean exit holding nothing owes nothing. Faulting on every quiet success buries the real findings under ordinary ones.
- An unobservable exit is "unknown", not "discharged". Do not score one by passing defaults into the normal path.

A related rule for self-repair: when idleness cannot be proven, escalate instead of acting. The handback duty above is what the caller owes on receiving that escalation.

## A health verdict needs a control too

A verdict you cannot contradict is not a verdict, it is an assertion. tartci #188 is the case: a fleet health checker reported a host dead while it was serving a busy runner. This is the one shape in this family where the system was confident and wrong rather than silent, and it is the most corrosive. Once an operator sees one confidently wrong "dead", they stop trusting every other verdict the same system emits.

Check a health claim against independent evidence that the host is doing work. The asymmetry is the whole design:
- Service evidence is positive and hard to fake. A busy runner is executing someone's job, and a job completed after the claim finished on that host. Neither happens on a dead machine, so both refute a critical claim and should downgrade it from blocking to a warning. The host keeps taking work and the operator learns the signal is wrong.
- Silence refutes nothing. An idle host serves nothing and is healthy. Reading quiet as dead would condemn every unused machine in the fleet.

Two traps:
- `online` is not proof of life. A registration outlives the machine behind it (tartci #189, an offline runner still advertising its labels), so only work counts as evidence. Census presence never refutes a health claim.
- Staleness is not health. A vitals file whose producer died repeats its last word forever with total confidence. Past a ceiling the claim is neither green nor critical. It is unknown, because nobody is answering.

When triaging a host the fleet calls unusable, get the counter-evidence before the diagnosis: is anything on it busy right now? If so, the signal is the fault.

## Survivability: will the lane still be there after an ordinary exit?

Knowing a lane is being served does not tell you it will survive, and the two come apart badly.

On 2026-09-05 this repository's only macOS runner was `online` and `busy`, correctly counted as served. A routine force-push tripped `cancel-in-progress`, the cancel arrived at the runner as a signal (`Runner will be shutdown for UserCancelled`), and it exited. It never came back: not ephemeral, `RunAtLoad` with no `KeepAlive`, and not loaded in launchd at all. The lane went from served to permanently unserved with no state in between, while the host stayed up and kept serving three other runners. The precondition was statically visible the whole time.

Classify each runner as supervised, self-replacing, unsupervised, or unknown, and roll that up per lane: survivable, fragile, single point of failure, or unknown.

Two traps that produce a confident false fault:
- A LaunchAgent lives in the per-user GUI domain. `launchctl list` over SSH prints nothing for a job that is loaded and running, so "not loaded" from an SSH session is not a fact. Query `launchctl print gui/$(id -u)/<label>`, and always run it against a job you know is loaded first. Without that control an empty result is indistinguishable from a broken query. An unreadable domain is unknown, never a fault.
- An empty runner census is a scope error until proven otherwise. Zero runners means unknown, not single point of failure. The org versus repo scope mistake has already misled this fleet once.

`RunAtLoad` is not supervision. It starts a job once and says nothing about what happens when it exits, which is exactly the plist that took the lane out.
