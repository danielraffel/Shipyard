"""First-run disclosure + operator-facing messaging.

The daemon does things a reasonable user might not have consented to
if they'd been told nothing:

  * Registers a GitHub webhook on every repo it tracks.
  * Starts a Tailscale Funnel — i.e. makes a localhost service
    publicly routable over Tailscale's edge.
  * Stores an HMAC secret in the keychain (macOS) or a 600-perm file.

None of these are destructive, but we should tell the user the first
time each machine starts the daemon, and point them at how to
disable. A one-line ack file at
``<state_dir>/daemon/.first-run-acked`` records that the disclosure
was shown so subsequent starts are quiet.
"""

from __future__ import annotations

import sys
import textwrap
import time
from pathlib import Path


def ack_path(state_dir: Path) -> Path:
    return state_dir / "daemon" / ".first-run-acked"


def has_been_shown(state_dir: Path) -> bool:
    return ack_path(state_dir).exists()


def mark_shown(state_dir: Path) -> None:
    path = ack_path(state_dir)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(str(int(time.time())), encoding="utf-8")


def render(repos: list[str]) -> str:
    """The human-readable notice shown on first run of the daemon."""
    repo_lines = (
        "\n".join(f"    - {r}" for r in repos)
        if repos
        else "    (none detected — the daemon will still run for IPC subscribers)"
    )
    return textwrap.dedent(
        f"""\
        ─── first-run notice ──────────────────────────────────────────

        `shipyard daemon` will do the following on this machine:

          1. Start a Tailscale Funnel so GitHub can POST webhook events
             to your Mac. Requires Tailscale to be installed with
             Funnel enabled on your tailnet.
          2. Register a GitHub webhook on each repo it tracks so those
             events are delivered. Repos detected from ship-state:
        {repo_lines}
          3. Store a randomly-generated HMAC secret so deliveries can
             be verified (macOS Keychain, or a 600-perm file on Linux).
          4. Open a Unix socket at ~/.pulp/... for local CLI and the
             macOS app to subscribe to live events.

        None of these are destructive. If you'd rather not use live
        mode:

          * Just don't start the daemon. `shipyard watch`, `shipyard
            ship`, and the macOS app all work without it — they fall
            back to polling.
          * Run `shipyard daemon stop` at any time to tear down the
            tunnel and unregister the webhooks.

        This notice is shown once per machine. To see it again, delete
        the marker file at ~/.local/state/shipyard/daemon/.first-run-acked
        (path varies by OS; `shipyard doctor` prints the exact path).

        ───────────────────────────────────────────────────────────────
        """
    )


def show_if_first_run(state_dir: Path, repos: list[str], stream=None) -> bool:
    """Print the disclosure if it hasn't been acked yet. Returns True
    if it was just shown (so the caller can react, e.g. by briefly
    pausing before continuing)."""
    if has_been_shown(state_dir):
        return False
    out = stream if stream is not None else sys.stderr
    out.write(render(repos))
    out.flush()
    mark_shown(state_dir)
    return True
