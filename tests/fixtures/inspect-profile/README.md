# `shipyard inspect --profile` fixtures

One file per repository: every GitHub answer `inspect::profile::gather` reads,
captured read-only on 2026-10-13 from `main`, keyed by the `gh api` path
(`graphql:workflows` for the one GraphQL call). Each value is `{"ok": <JSON>}`
or `{"err": "<gh stderr>"}`.

List payloads keep GitHub's object shapes but only the fields the profile
reads: tree entries keep `path` and `type`; runs keep `id`, `workflow_id`,
`path`, `event`, `status`, `conclusion`, `created_at`; jobs keep `id`,
`run_id`, `name`, `labels`, `runner_name`, `runner_group_name`, `status`,
`conclusion`. Runs and jobs were captured for every workflow, so the fake
serves whichever subset the run budget picks.

Commits: Shipyard `1b759888`, tartci `ccfebb0a`, vellum `d53917d7`, pulp
`526eeeb2`.
