"""Release-bot token provisioning helpers.

The `shipyard release-bot` command group guides users through the
fine-grained PAT + secret dance that is otherwise 8+ manual steps
across the GitHub UI. Failure modes this module prevents:

- Fine-grained PAT missing the target repo in its "Selected
  repositories" list → `actions/checkout@v5` rejects with
  `fatal: could not read Username`. Pure-UI mistake, detected only
  when auto-release next fires.
- Secret value drift — `RELEASE_BOT_TOKEN` on a repo holds a
  different token than the PAT the user is editing, usually
  because the secret was seeded from an earlier PAT that was later
  regenerated. Detectable by comparing `gh secret list` timestamps
  against PAT regeneration time.
- Multi-project ambiguity — one shared PAT for all Shipyard
  consumers vs a per-project PAT. Library defaults to per-project
  (least privilege, clear attribution) but supports shared via
  explicit flag.

Every public entry point in this module returns structured data
(dataclasses, not printed strings) so the CLI layer owns the
presentation. Tests import from here directly and never go through
Click.
"""

from __future__ import annotations

from shipyard.release_bot.setup import (
    ReleaseBotState,
    SetupPlan,
    describe_state,
    plan_setup,
    render_pat_creation_url,
)

__all__ = [
    "ReleaseBotState",
    "SetupPlan",
    "describe_state",
    "plan_setup",
    "render_pat_creation_url",
]
