# AGENTS.md


Read `CLAUDE.md` before making any changes.

## Autonomous architecture contract

Before changing review, queue, CI, release, recovery, fleet, or handoff behavior, read [`docs/autonomy-contract.md`](docs/autonomy-contract.md). Treat its A1-A12 invariants as required behavior. Load live GitHub, Shipyard, TartCI, and host state separately from documentation; never infer current authority or health from a handoff alone. A recovery change must prove remediation/redispatch and authoritative post-action progress, including controller restart and duplicate-effect tests.
