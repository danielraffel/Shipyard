# Terminal and provider adapter boundaries

## In plain language

Shipyard keeps two questions separate: **where can a coding tool be reached?**
and **which provider route may receive the request?** A terminal adapter
answers the first; a provider adapter answers the second. Keeping them
separate means a familiar terminal label can never be mistaken for proof that
the right person, session, account, or model is available.

Today, cmux is the only terminal adapter that has completed physical delivery
work. HerdR has a registered shape so it can be added without changing the
durable handoff format, but it remains disabled until a live canary proves it.
Subrouter routes can be validated for the supported providers, but route
validation is not itself proof that a provider accepted a request. Shipyard
therefore refuses an unproven route instead of falling back to a different
terminal or direct provider.

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
