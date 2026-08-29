# Terminal and provider adapter boundaries

Shipyard treats terminal transport and provider routing as independent
authorities. A terminal endpoint selects where a bounded operation may occur;
the protected Subrouter route selects which provider, account, model, and native
session may receive it. Neither is inferred from terminal labels.

Tagged terminal endpoints use provider-wrapper wire schema v2. A v1
cmux-only controller/wrapper pair must be upgraded together with its configured
wrapper digest; mixed v1/v2 requests refuse rather than cross-decoding.

## Current capability state

- `cmux` is the only physically implemented terminal delivery adapter. Its
  executable path, socket path, and Apple signing team are bound into each
  request. The signing team comes from trusted machine-global
  `[workstream_continuation.terminal_trust]` policy.
- Existing installations that omit `terminal_trust` retain the previously
  shipped cmux signing-team identity. New installations should set
  `cmux_signing_team_id` explicitly.
- `herdr` has a registered endpoint shape, but execution remains fail-closed.
  A request must carry server-incarnation and direct-fresh-launch proof merely
  to validate, and the current adapter still returns
  `herdr-capability-unproven` without touching HerdR. There is no cmux or direct
  provider fallback.

Subrouter route validation registers `codex`, `claude`, `qwen`, `agy`, and
`kimi`. Registration proves only that strict resume/fresh argument shapes,
digest binding, account environment binding, and prompt isolation can be
validated. It is not physical provider acceptance evidence. Each provider and
HerdR still require separate live canaries before activation can be claimed.

Example trusted policy override:

```toml
[workstream_continuation.terminal_trust]
cmux_signing_team_id = "ABCDEFGHIJ"
```
