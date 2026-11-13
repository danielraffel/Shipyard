//! The stuck-pull-request thresholds the steward publishes
//! (`app::merge_steward_cmd::stuck`) and `shipyard relay` delivers. One
//! definition, so the two can never disagree on a level.

use serde::{Deserialize, Serialize};

/// Minutes stuck before the owning session is told.
pub const OWNER_MINUTES: i64 = 30;
/// Minutes stuck before the repair lane is told.
pub const REPAIR_MINUTES: i64 = 60;

/// Who an escalation goes to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Escalate {
    /// The session that owns the pull request.
    Owner,
    /// The repair lane.
    Repair,
}

impl Escalate {
    /// The level for `minutes` stuck, if any.
    #[must_use]
    pub const fn for_minutes(minutes: i64) -> Option<Self> {
        if minutes >= REPAIR_MINUTES {
            Some(Self::Repair)
        } else if minutes >= OWNER_MINUTES {
            Some(Self::Owner)
        } else {
            None
        }
    }
}
