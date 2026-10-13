# watch_inputs fixtures

Live captures from m3 on 2026-10-13. They are read-only reads; nothing was sent to any session.

| File | Source | What it shows |
|---|---|---|
| `screen-1000010001.txt` | `cmux read-screen --workspace workspace:1000010001 --lines 20` | a Codex session that failed to resume (`Model provider subrouter not found`) and left a shell prompt |
| `screen-1000010022.txt` | same, `workspace:1000010022` | a Codex session killed (`subrouter: signal: terminated` on its prompt) with the shell prompt after it |
| `screen-1000010004.txt` | same, `workspace:1000010004` | an idle Codex session with an empty prompt: nothing to report |
| `screen-1000010012.txt` | same, `workspace:1000010012` | a Codex session working (`Working (`) with an empty prompt: nothing to report |
| `screen-1000010012-reconnecting.txt` | same, `workspace:1000010012`, a few minutes later | the same session retrying `Unexpected status 503 Service Unavailable` by itself (`Reconnecting... 2/5`, `esc to interrupt`): busy, not errored |
| `screen-1000010023.txt` | same, `workspace:1000010023` | an idle Codex session holding a multi-line unsent draft |
| `pool-status-m3.json` | `tartci pool status --json` on m3, trimmed to `host`, `state`, `participating`, and each `fleet.config` section's `problem` and first two `lines` | two sections with a problem (`self_update`, `tool_freshness`) |

The queued-draft screen and one errored variant in the tests are synthetic, built from these captures: They are built from the captured layout, with the error and queue hint text Codex prints.
