//! Host attestation: the artifact that turns a delegation into evidence.
//!
//! Before this existed, the two halves of the fleet each passed their own
//! check by pointing at the other. tartci's launchd watchdog printed
//!
//! ```text
//! ✓ actions.runner.…pulp-preamble-m5: declared runner executable exists;
//!   runtime health is owned by Shipyard
//! ```
//!
//! over a service that was in a `spawn scheduled` crash loop with 3,684
//! launches and no `.runner` registration file, while Shipyard's runner status
//! knew about one configured runner id and nothing about that host at all. The
//! checkmark was true as written and the lane was dead.
//!
//! The rule this module implements: **no side may pass a check by delegation
//! unless it names the artifact carrying the other side's verdict, and absence
//! of that artifact is a fault, not a pass.** tartci writes a per-host
//! attestation; Shipyard reads it; a missing or stale file makes the lane
//! `Unknown`, which is loud, rather than `Served`, which is a lie.
//!
//! ## What the attestation is for
//!
//! Exactly one question, asked of the host side because GitHub cannot answer
//! it: *does any machine on this fleet declare and supervise this label set?*
//! An empty runner census means "nothing is registered right now", which for a
//! just-in-time pool between jobs is normal and for a persistent runner that
//! crash-looped away is fatal. Only the host knows which.
//!
//! ## Generation, so skew is visible as skew
//!
//! Each record carries the generation that produced it. A prior incident had a
//! *stale checker* report every lane on a healthy host as `heartbeat_missing`
//! — the reader was behind, not the fleet. So a reader that finds an
//! unexpected generation reports `generation_behind`, a distinct condition
//! from both "dead" and "unserved", instead of silently comparing new fields
//! against an old writer's output.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::ATTESTATION_FRESH_SECS;

/// Schema version this reader understands.
pub const ATTESTATION_SCHEMA: u32 = 1;

/// Default file name under a host's tartci state directory.
pub const ATTESTATION_FILE: &str = "host-attestation.json";

/// One persistent Actions runner as the host sees it.
///
/// "Declared → installed → loaded → alive → **registered**" is five separate
/// facts, and the incident this was written for passed the first four. A plist
/// can be present, loaded and respawning forever while `.runner` does not
/// exist, which means GitHub has never heard of it.
// Five independent booleans on purpose: "is the runner up" is five separate
// facts (declared, installed, loaded, not-looping, registered), and the
// 2026-09-13 incident passed the first four. Collapsing them into a struct of
// sub-structs would hide exactly the distinction this record exists to carry.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PersistentRunner {
    /// launchd label, e.g. `actions.runner.Generous-Corp-pulp.pulp-preamble-m3`.
    pub label: String,
    /// Whether the host profile declares it.
    pub declared: bool,
    /// Whether the plist and runner directory exist.
    pub installed: bool,
    /// Whether launchd has it bootstrapped.
    pub loaded: bool,
    /// launchd `state` from `launchctl print` — `running`, `spawn scheduled`,
    /// `waiting`, and so on. **Not** derived from `launchctl list`, whose
    /// `- 0` column renders a crash loop identically to health.
    pub state: String,
    /// launchd `runs` counter. A number climbing fast is a crash loop.
    pub runs: u64,
    /// Whether the host classified it as crash-looping.
    pub crash_loop: bool,
    /// Whether `.runner` exists and parses.
    pub registered: bool,
    /// Repository slug from `.runner`, when readable.
    pub registration_repo: Option<String>,
    /// Labels this runner advertises, from `.runner` or the profile.
    pub advertises: Vec<String>,
    /// Host-side verdict: `healthy`, `broken`, `unregistered`, `stale_repo`.
    pub verdict: String,
    /// Human explanation of the verdict.
    pub reason: String,
}

impl PersistentRunner {
    /// Whether this runner can be counted as serving its labels.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.verdict == "healthy" && self.registered && self.loaded && !self.crash_loop
    }
}

/// One just-in-time lane as the host sees it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct JitLane {
    /// Lane id from the host profile.
    pub id: String,
    /// Repository the lane serves.
    pub repo: String,
    /// Labels runners minted by this lane advertise.
    pub labels: Vec<String>,
    /// Supervisors the profile declares.
    pub supervisors: u32,
    /// Supervisors with a fresh heartbeat.
    pub fresh: u32,
    /// Age of the freshest heartbeat, in seconds.
    pub heartbeat_age_secs: Option<i64>,
    /// Host-side verdict: `attested` or `unattested`.
    pub verdict: String,
    /// Human explanation.
    pub reason: String,
}

impl JitLane {
    /// Whether this lane counts as supervised.
    #[must_use]
    pub fn attested(&self) -> bool {
        self.verdict == "attested" && self.fresh > 0
    }
}

/// Generation stamps of everything that produced an attestation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Generation {
    /// Git SHA of the tartci tree that wrote it.
    pub tartci_root_sha: Option<String>,
    /// SHA-256 of the host profile in force.
    pub profile_sha256: Option<String>,
    /// Name and version of the writer, e.g. `tartci_host_attestation.py@1`.
    pub writer: Option<String>,
}

/// One host's attestation, as written by tartci and read by Shipyard.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct HostAttestation {
    /// Schema version.
    pub schema: u32,
    /// Short host name.
    pub host: String,
    /// When this file was written.
    pub written_at: Option<DateTime<Utc>>,
    /// Interval the writer runs at, in seconds. Freshness is judged against
    /// three times this, falling back to [`ATTESTATION_FRESH_SECS`].
    pub interval_secs: Option<i64>,
    /// Generation stamps.
    pub generation: Generation,
    /// Whether the writer could read this host's launchd domain.
    ///
    /// `false` means an empty runner list is a **scope error**, not a census
    /// of zero: over SSH the GUI domain can be invisible, and an empty answer
    /// then says nothing about the host.
    pub launchd_readable: Option<bool>,
    /// Whether the writer could read this host's fleet profile.
    ///
    /// The first deployment of the writer ran under macOS's `/usr/bin/python3`
    /// (3.9, no `tomllib`), so the profile parsed as empty and the record
    /// declared **zero** lanes — a sensor that ran, wrote a file, and measured
    /// nothing. A reader cannot tell that apart from a host with no lanes
    /// unless the file says so, so a `false` here disqualifies the record as
    /// evidence for anything.
    pub profile_readable: Option<bool>,
    /// Human explanation of `profile_readable`.
    pub profile_detail: Option<String>,
    /// Persistent Actions runners on this host.
    pub persistent_runners: Vec<PersistentRunner>,
    /// Just-in-time lanes on this host.
    pub jit_lanes: Vec<JitLane>,
}

impl HostAttestation {
    /// Whether this record is recent enough, and complete enough, to be
    /// evidence.
    ///
    /// Recency alone is not sufficient: a record written thirty seconds ago by
    /// a writer that could not read the host profile declares no lanes, and
    /// counting it as evidence would let a blind sensor vouch for a lane it
    /// never looked at.
    #[must_use]
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        if self.profile_readable == Some(false) || self.launchd_readable == Some(false) {
            return false;
        }
        let Some(written_at) = self.written_at else {
            return false;
        };
        let ceiling = self
            .interval_secs
            .map_or(ATTESTATION_FRESH_SECS, |interval| {
                interval.saturating_mul(3)
            });
        now.signed_duration_since(written_at) <= Duration::seconds(ceiling)
            && now.signed_duration_since(written_at) >= Duration::seconds(-300)
    }

    /// Age in seconds, negative if the clock disagrees.
    #[must_use]
    pub fn age_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        self.written_at
            .map(|written_at| now.signed_duration_since(written_at).num_seconds())
    }
}

/// What one host has to say about one label set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaneCoverage {
    /// A healthy persistent runner or a supervised JIT lane covers it.
    Supervised {
        /// Human detail naming the covering record.
        detail: String,
    },
    /// The host declares this label set and it is broken.
    ///
    /// The most valuable verdict in the set: it is the difference between
    /// "nobody was ever going to serve this" and "the machine that serves this
    /// is sick, here is which one and how".
    Broken {
        /// Human detail naming the fault.
        detail: String,
    },
    /// The host says nothing about this label set.
    NotDeclared,
}

/// Every host attestation this process could read.
#[derive(Clone, Debug, Default)]
pub struct AttestationSet {
    /// Attestations by host, in read order.
    pub hosts: Vec<HostAttestation>,
    /// Paths that were expected and could not be read, with the reason.
    pub unreadable: Vec<String>,
}

impl AttestationSet {
    /// Read attestations from a set of candidate paths.
    ///
    /// A path that does not exist is recorded in `unreadable` rather than
    /// skipped: the difference between "this host says the lane is fine" and
    /// "this host said nothing" is the entire point of the artifact, and a
    /// reader that silently tolerates absence reintroduces the delegation
    /// checkmark it replaced.
    #[must_use]
    pub fn read_from(paths: &[PathBuf]) -> Self {
        let mut set = Self::default();
        for path in paths {
            match read_attestation(path) {
                Ok(attestation) => set.hosts.push(attestation),
                Err(reason) => set.unreadable.push(format!("{}: {reason}", path.display())),
            }
        }
        set
    }

    /// Host names whose attestation is fresh enough to count as evidence.
    #[must_use]
    pub fn fresh_hosts(&self, now: DateTime<Utc>) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|attestation| attestation.is_fresh(now))
            .map(|attestation| attestation.host.clone())
            .collect()
    }

    /// One line explaining why nothing is fresh, for the `Unknown` detail.
    #[must_use]
    pub fn describe_staleness(&self, now: DateTime<Utc>) -> String {
        if self.hosts.is_empty() {
            return if self.unreadable.is_empty() {
                "no attestation paths were checked".to_owned()
            } else {
                format!("unreadable: {}", self.unreadable.join("; "))
            };
        }
        let stale = self
            .hosts
            .iter()
            .map(|attestation| {
                if attestation.profile_readable == Some(false) {
                    return format!(
                        "{} BLIND: {}",
                        attestation.host,
                        attestation
                            .profile_detail
                            .clone()
                            .unwrap_or_else(|| "profile unreadable".to_owned())
                    );
                }
                if attestation.launchd_readable == Some(false) {
                    return format!("{} BLIND: launchd domain unreadable", attestation.host);
                }
                format!(
                    "{} written {}s ago",
                    attestation.host,
                    attestation.age_secs(now).unwrap_or(-1)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!("no usable attestation: {stale}")
    }

    /// What `host` says about `labels`.
    ///
    /// Matching is case-insensitive and in the *record's* direction: a runner
    /// or lane covers the requested labels when it advertises every one of
    /// them, which is how GitHub schedules.
    #[must_use]
    pub fn coverage(&self, host: &str, labels: &[String]) -> LaneCoverage {
        let Some(attestation) = self.hosts.iter().find(|entry| entry.host == host) else {
            return LaneCoverage::NotDeclared;
        };

        let mut broken: Option<String> = None;

        for lane in &attestation.jit_lanes {
            if !advertises_all(&lane.labels, labels) {
                continue;
            }
            if lane.attested() {
                return LaneCoverage::Supervised {
                    detail: format!(
                        "jit lane `{}` supervised ({}/{} fresh, heartbeat {}s)",
                        lane.id,
                        lane.fresh,
                        lane.supervisors,
                        lane.heartbeat_age_secs.unwrap_or(-1)
                    ),
                };
            }
            broken.get_or_insert(format!(
                "jit lane `{}` declared but unattested: {}",
                lane.id, lane.reason
            ));
        }

        for runner in &attestation.persistent_runners {
            if !advertises_all(&runner.advertises, labels) {
                continue;
            }
            if runner.healthy() {
                return LaneCoverage::Supervised {
                    detail: format!("persistent runner `{}` healthy", runner.label),
                };
            }
            broken.get_or_insert(format!(
                "persistent runner `{}` {}: {}",
                runner.label, runner.verdict, runner.reason
            ));
        }

        broken.map_or(LaneCoverage::NotDeclared, |detail| LaneCoverage::Broken {
            detail,
        })
    }
}

fn advertises_all(have: &[String], want: &[String]) -> bool {
    !want.is_empty()
        && want.iter().all(|label| {
            have.iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(label))
        })
}

/// Read and validate one attestation file.
pub fn read_attestation(path: &Path) -> Result<HostAttestation, String> {
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let attestation: HostAttestation =
        serde_json::from_str(&raw).map_err(|error| format!("malformed: {error}"))?;
    if attestation.schema != ATTESTATION_SCHEMA {
        return Err(format!(
            "generation_behind: schema {} (this reader speaks {ATTESTATION_SCHEMA})",
            attestation.schema
        ));
    }
    if attestation.host.trim().is_empty() {
        return Err("missing host name".to_owned());
    }
    Ok(attestation)
}

/// Candidate paths for the local host's attestation.
///
/// Both the tartci-native state root (`$TARTCI_HOME/state`, default
/// `~/.tartci/state`) and the XDG-ish path are checked, because the fleet
/// currently has both conventions in use and hard-coding one would make the
/// reader's own blindness look like a dead host.
#[must_use]
pub fn local_attestation_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(explicit) = std::env::var("SHIPYARD_HOST_ATTESTATION")
        && !explicit.trim().is_empty()
    {
        paths.push(PathBuf::from(explicit));
        return paths;
    }
    let tartci_home = std::env::var("TARTCI_HOME").ok().map(PathBuf::from);
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    if let Some(root) = tartci_home {
        paths.push(root.join("state").join(ATTESTATION_FILE));
    } else if let Some(home) = &home {
        paths.push(home.join(".tartci").join("state").join(ATTESTATION_FILE));
    }
    if let Some(home) = &home {
        paths.push(
            home.join(".local")
                .join("state")
                .join("tartci")
                .join(ATTESTATION_FILE),
        );
    }
    paths
}
