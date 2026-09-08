# Terminal and provider adapter boundaries

## In plain language

Shipyard keeps two questions separate: **where can a coding tool be reached?**
and **which provider route may receive the request?** A terminal adapter
answers the first; a provider adapter answers the second. Keeping them
separate means a familiar terminal label can never be mistaken for proof that
the right person, session, account, or model is available.

Today, cmux is the only terminal adapter with a physically implemented
capability check. HerdR has a registered request shape so it can be added
without changing the durable handoff format, but every HerdR request is refused
as unsupported. Shipyard refuses an unproven route instead of falling back to a
different terminal or a direct provider call.

Shipyard treats terminal transport and provider routing as independent
authorities. A terminal endpoint selects where a bounded operation may occur;
the recorded provider route selects which provider, account, and model may
receive it. Neither is inferred from terminal labels.

## Current capability state

- `cmux` is the only physically implemented terminal adapter. A request binds
  its executable path, socket path, surface and workspace ids, native session
  id, provider kind, and the live local process incarnation (boot id, pid, and
  start identity). The mutation endpoint deliberately carries only the
  authenticated executable and socket paths: workspace, tab, surface, and
  session labels identify occupants, not the terminal service itself.
- Verification is a single read-only observation and fails closed. It refuses
  on an unobservable or missing method, an invalid response, no match, multiple
  matches, a changed process incarnation, or a native-session mismatch.
- `herdr` has a registered request shape, but it exposes no mutation endpoint
  and every capability check refuses as unsupported. There is no cmux or direct
  provider fallback.

The provider route is recorded separately from the terminal, as an opaque
reference carrying a profile digest, integrity hash, generation, revision,
provider name, and optional account and model. Recording a route proves only
that the reference is well-formed and integrity-bound; it is not evidence that
a provider accepted a request. Each provider still requires its own live canary
before activation can be claimed.
