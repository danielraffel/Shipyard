# Webhook registration diagnosis and permission ask

Shipyard's webhook registrar is read and write scoped through the configured
GitHub credential. A registration failure must be classified before changing
tokens:

| host | observed failure | diagnosis | code behavior |
| --- | --- | --- | --- |
| m1 | `403` naming `repository_hooks` | The GitHub App installation lacks repository hook permission | Shipyard keeps polling and prints the exact permission ask |
| m5 | `403` naming `repository_hooks` | Same App permission gap, not a stale local secret | Shipyard keeps polling and prints the exact permission ask |
| m5s | `gh` times out before an API response | Host or credential transport is unreachable | Shipyard records an unreachable registration failure and keeps the single polling path |

## Team-lead ask

Please grant the Shipyard GitHub App **Repository hooks: Read and write** on
the Pulp, Forge, Vellum, Shipyard, and TartCI repositories, then accept the
updated installation. Do not rotate credentials as a workaround. After the
permission change, run the registrar's read-only reconciliation on m1, m5,
and m5s and attach the resulting hook IDs and callback URLs to the W4 review.

The registrar remains the only writer. A failed host never creates a second
local ledger or a competing webhook.
