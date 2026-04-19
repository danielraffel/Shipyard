---
name: quarantine
description: Manage the flaky-target quarantine list (.shipyard/quarantine.toml)
---

Shipyard's quarantine list lets maintainers mark known-flaky targets so a TEST/UNKNOWN failure on that target during `shipyard ship` doesn't block the merge.

**What quarantine does:**
- TEST or UNKNOWN failure on a quarantined target → treated as advisory (logged, not merge-blocking).
- INFRA / TIMEOUT / CONTRACT failures are *always* authoritative — quarantine never suppresses them.
- Lives in `.shipyard/quarantine.toml` and is opt-in per repo.

**Usage:**

```bash
# List quarantined targets
shipyard quarantine list --json

# Add a target (record a reason — reviewers will see it)
shipyard quarantine add windows-arm64 --reason "flaky runner apr-2026 outage"

# Remove once the underlying flakiness is fixed
shipyard quarantine remove windows-arm64
```

The merge check in `shipyard ship` / `shipyard auto-merge` reads this file automatically. A failing target will surface under `advisory` in the merge check JSON instead of `failing`.

If a target should block the merge regardless of failure class, remove it from quarantine.
