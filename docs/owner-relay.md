# PR owner registry and event relay

An agent that opened a pull request usually stops watching it. When the pull
request merges, goes red, gets a HOLD, or starts conflicting after the base
moved, nobody tells that agent, and it sits waiting on something that already
happened. The owner registry records which session owns each pull request;
the relay tells that session when something it waits on changes, and what to
do next. Neither one uses a model.

## Owner registration

`shipyard pr` and `shipyard ship` (without `--pr`) record the pushing session
as the owner of the pull request every time they push. `shipyard ship --pr N`
pushes nothing (it is how other sessions wait on a pull request), so it never
registers. Registration is opt-in per host: a machine without a `[relay]` table
in its config registers nothing and writes nothing into pull-request bodies.
A push from a session without a cmux surface (a headless or scheduled run)
never displaces a live owner that has one.

| field | from |
|---|---|
| `host` | `SHIPYARD_HOST_NAME`, else `relay.host_name` in the machine config, else `hostname -s` |
| `client` | `claude-code` (`CLAUDE_CODE_SESSION_ID`), `codex` (`CODEX_THREAD_ID`, `CODEX_SESSION_ID`, `CODEX_COMPANION_SESSION_ID`), or `other` |
| `cmux_socket` | `CMUX_SOCKET_PATH` (or `CMUX_SOCKET`) |
| `surface` | `CMUX_SURFACE_ID` |
| `agent` | `SHIPYARD_AGENT_ID` |
| `session` | `SHIPYARD_STAMP_SESSION_ID`, else the client's session id |

The record is written twice:

- atomically to `<state_dir>/owners/<owner>/<repo>/<pr>.json` on the host
  that pushed (`~/Library/Application Support/shipyard` on macOS);
- as one line in the pull-request body,
  `<!-- shipyard-owner: host=m5 client=codex surface=... socket=... session=... at=... -->`,
  so the address survives the loss of that host and any host can read it.
  The body is written over GraphQL (`updatePullRequest`), not REST.

The body is only rewritten when the address changes (host, client, surface,
socket, session, agent, released), never for a new timestamp alone: every body
edit fires `pull_request: edited`, which some required gates listen to.

Values are kept only when they consist of `[A-Za-z0-9._:/@+-]`, so a marker can
never close its comment or carry shell syntax into a transport. The record is
an address for a notification, never an authority: nothing reads it to arm,
merge or approve.

```
shipyard owner register [--pr N] [--repo OWNER/REPO]   # claim or move a PR to this session
shipyard owner register --all                          # after a restart: re-register this session's open PRs
shipyard owner release --pr N [--repo OWNER/REPO]      # hand it to the repo's repair lane
```

`register` without `--pr` uses the open pull request whose head is the current
branch. `--all` re-registers every record on this host whose session id or
cmux surface matches this session. `SHIPYARD_OWNER_REGISTER=0` stops
`shipyard pr`/`ship` from registering (automation pushing for someone else).
A plain `git push` does not register; run `shipyard owner register` from the
pushing session (or a pre-push hook) when ownership moves that way.

## The relay

```
shipyard relay [--follow] [--interval SECS] [--repo OWNER/REPO]... [--send | --shadow] [--json]
```

One pass per repository:

1. **Owners.** This host's stored records, plus the body marker of every open
   pull request that has one.
2. **One GraphQL query** with an alias per owned pull request: state, merge
   state, merge-queue membership, head and base, the newest 20 comments
   (verdicts, `repair-claim:` lines) and the required checks. No REST. The
   pass is counted by the steward's per-pass `GhPass` and reported as
   `api_calls`. When the steward's tick report for the repository (in
   `relay.steward_tick_dir`) is fresh and judged the same head, its
   `next_action` is the next step in the message, at no GitHub cost.
3. **Events**, comparing with the last pass. A first sighting is a baseline,
   not an event. Verdicts and `repair-claim:` lines count only from a
   repository owner, member or collaborator, or a GitHub App: an outside
   contributor's `verdict: APPROVE` is never relayed. A pull request with more
   than 100 check contexts is never reported green from a partial page.

| event | fires when |
|---|---|
| `merged` / `closed` | the pull request merged / closed unmerged |
| `ejected` | it left the merge queue without merging (reason: red checks, conflict, merge state) |
| `enqueued` | it entered the merge queue (off by default) |
| `verdict` | a `verdict: APPROVE\|HOLD head:<sha>` names the current head |
| `required_check` | the required checks went red (or a different set went red), or went green; review gates (`review-verdict`, `post-review-verdict`, `release-path-review`) do not count |
| `dirty_after_base_move` | it became `DIRTY` while its head did not move |
| `claim_taken` | a new `repair-claim: <lane> until <time>` comment |
| `stacked_base_merged` | the pull request whose head is its base merged, or it was retargeted to the default branch |

4. **Delivery.** Each event has an idempotency key (`repo#pr:event:head:detail`)
   and is delivered once. Repeatable events (`enqueued`, `ejected`,
   `required_check`) carry a per-PR sequence number, so a second ejection or a
   re-run that fails again at the same head is a new message; a verdict
   re-posted at the same head is not. The state and the log record are written
   after each delivery, before the next one is sent. A pull request gets at most one message per
   `relay.rate_limit_minutes` (default 10); events in between are batched into
   the next one. `merged` and `closed` skip the wait.

The fallback chain, first success wins:

1. the owner's cmux surface, unless the owner released it;
2. `relay.repo."<repo>".lead`;
3. `relay.repo."<repo>".repair_lane`;
4. one pull-request comment (GraphQL `addComment`) starting `shipyard-relay:`,
   when `relay.comment_fallback` is on.

A batch nothing could take is retried one rate-limit window later, up to six
times, then dropped (logged as `refused`). At most one fallback comment is
posted per head. A pull request GitHub does not know is dropped after three
passes.

Run **one** relay per fleet: every relay that reads a repository's body
markers delivers to every owner. Owner hosts must be this host or listed in
`relay.hosts`; a host named only by a body marker is never an ssh destination,
a marker's `socket` is used only when it is one of `relay.sockets`, and a
marker's time is clamped to now (a store record on this host wins a tie).

### Transport

On the relay's own host it runs `cmux`; on another host,
`ssh -o BatchMode=yes <host> 'cmux' '--socket' ... 'agent' 'message' '<surface>' '--from' 'shipyard-relay' '--' '-'`
with every argument single-quoted and the **body on stdin**, never inside an
argument a remote shell could expand. A host can run cmux NIGHTLY
(`/tmp/cmux-nightly.sock`) and stable cmux (its default socket); a surface
belongs to one of them. The relay proves which with
`read-screen --surface <id> --lines 1`, which fails on a socket that does not
own the surface (`identify` cannot prove it: it reports the focused surface),
then tries the remembered socket first next time.

### Never a verdict

A message, or a fallback comment, that contains `verdict: APPROVE|HOLD`,
`reviewed: <sha>`, a `<!-- session: -->` stamp, or `post-merge: OK` is
refused and logged, never sent. Messages are one line.

### Delivery log

`<state_dir>/relay/deliveries.jsonl` gets one `delivery` record per message
(`events`, `keys`, `tier`, `transport`, `delivered`, failed `attempts`,
`refused`, the message) and later one `acted` record: the owner's first action
after it (`push`, `comment`, `merged`, `closed`, or `none` after 24 hours),
when the relay saw it, and the pull request's state then. Shadow mode writes
`shadow-state.json` and `shadow-deliveries.jsonl` instead, so a shadow day
never marks anything delivered.

## `shipyard who --contact`

```
shipyard who --contact N [--repo OWNER/REPO] [--json]
shipyard who --pr N --contact
```

Names the live address of pull request N's owner: the newest record among this
host's store, every SSH host class's store (`host_class.<name>.shipyard_state_dir`),
and the body marker; then probes it over cmux and says which socket reaches
it, or that none does. It also probes the repository's lead and repair-lane
fallbacks, and prints the last stamped verdict author from the comments.

## Configuration

`[relay]` in the machine-global config (host policy, never a repository's):

```toml
[relay]
enabled = true            # false: the relay does nothing (rollback switch)
mode = "shadow"           # "send" to deliver; --send/--shadow override
host_name = "m3"
interval_secs = 120
rate_limit_minutes = 10
repos = ["Generous-Corp/pulp", "Generous-Corp/vellum"]
events = ["merged", "closed", "ejected", "verdict", "required_check",
          "dirty_after_base_move", "claim_taken", "stacked_base_merged"]
sockets = ["/tmp/cmux-nightly.sock", ""]   # "" is cmux's default socket
comment_fallback = true
steward_tick_dir = "/Users/me/Library/Logs/shipyard-steward-tick"
steward_tick_max_age_secs = 600

[relay.hosts.m5]
ssh = "m5"                # defaults to the host name
cmux = "/opt/homebrew/bin/cmux"

[relay.repo."Generous-Corp/pulp"]
lead = "m3/surface:1000120036"
repair_lane = "m1/surface:1000010002"
```

Set `relay.host_name` on every host that registers owners, so the name in a
record is the name the relay host reaches it by. A malformed key is an error,
never ignored.

## Rollout

1. Shadow for a day on the steward host (`mode = "shadow"`), alongside the
   stopgap `pr_relay.py`; compare `shadow-deliveries.jsonl` with its log.
2. `mode = "send"` with only that host's surfaces registered.
3. The ssh transports (owners on other hosts).

Rollback: `relay.enabled = false`. Keep `pr_relay.py` until the shadow day is
clean.
