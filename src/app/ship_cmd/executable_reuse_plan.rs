//! Bind a keyed shadow plan for a target whose protected-base policy declares
//! executable reuse. Every input comes from the protected base or from this
//! host's own record store; nothing is read from the PR head.

use super::super::changed_surface_cmd::ChangedSurfaceObservation;
use super::super::reuse_rederive::{git_bytes, run_base_command};
use crate::changed_surface::executable_reuse::{BindRequest, Binding, ExecutableReusePolicy, bind};
use crate::changed_surface::{
    AuthoritativeExecutionPlan, ChangedSurfacePolicy, ExecutionCommandTransport,
    plan_keyed_execution,
};
use std::path::Path;
use std::process::{Command, Stdio};

/// What keying one target produced.
#[derive(Debug)]
pub(super) enum Keyed {
    /// A keyed shadow plan to run in place of the configured stages.
    Planned(Box<AuthoritativeExecutionPlan>),
    /// No keyed run; the configured stages run unchanged, and this says why.
    Closeout {
        category: &'static str,
        diagnostic: String,
        /// The payload's size, when the plan was refused by the payload cap.
        payload_bytes: Option<usize>,
    },
}

fn closeout(category: &'static str, diagnostic: impl Into<String>) -> Keyed {
    Keyed::Closeout {
        category,
        diagnostic: diagnostic.into(),
        payload_bytes: None,
    }
}

/// Inputs to keying one target.
pub(super) struct KeyRequest<'a> {
    pub(super) observation: &'a ChangedSurfaceObservation,
    pub(super) policy: &'a ChangedSurfacePolicy,
    pub(super) reuse: &'a ExecutableReusePolicy,
    /// Repository whose host-local record store the lane writes to, when the
    /// target records at all.
    pub(super) record_repository: Option<&'a str>,
    pub(super) cwd: &'a Path,
    pub(super) state_dir: &'a Path,
    pub(super) contract_digest: &'a str,
    /// Finds the read-audit report for a plan against the given base.
    pub(super) audit: &'a dyn Fn(&str) -> super::read_audit::Fetched,
}

/// Bind the base record and plan the keyed run.
pub(super) fn plan_keyed(request: &KeyRequest<'_>) -> Keyed {
    let Some(repository) = request.record_repository else {
        return closeout(
            "executable_reuse_no_store",
            "the target writes no reuse records (reuse_record is not set)",
        );
    };
    let receipt = &request.observation.receipt;
    let base = receipt.planned_base().to_owned();
    let store = crate::reuse_record_store::store_dir(request.state_dir, repository);
    let derivation_root = request
        .state_dir
        .join("executable-reuse")
        .join("derivation");
    let policy_digest = crate::changed_surface::policy_digest(request.policy);
    let build_dir = request.cwd.join(&request.reuse.build_dir);
    let cwd = request.cwd;
    let bound = bind(
        request.reuse,
        &BindRequest {
            store: &store,
            derivation_root: &derivation_root,
            head_sha: &receipt.head_sha,
            policy_digest: &policy_digest,
            build_dir: &request.reuse.build_dir,
        },
        |path| git_bytes(cwd, &["show", &format!("{base}:{path}")]),
        |code_dir| probe_platform(request.reuse, code_dir, &build_dir),
        |commit| merged_into(cwd, commit, &base),
    );
    let mut binding = match bound {
        Ok(Binding::Bound(binding)) => binding,
        Ok(Binding::NoBase(why)) => return closeout("executable_reuse_no_base", why),
        Err(error) => return closeout("executable_reuse_bind_error", error),
    };
    let fetched = (request.audit)(&base);
    if let crate::changed_surface::executable_reuse::AuditBinding::None { reason } = &fetched.audit
    {
        eprintln!(
            "shipyard: keyed plan at {} has no clean read-audit report ({reason}); the key code \
             keys nothing, so this run is not a reuse observation",
            receipt.head_sha
        );
    }
    binding.audit = Some(fetched.audit);
    match plan_keyed_execution(
        receipt,
        &request.observation.input,
        request.policy,
        &binding,
        true,
        ExecutionCommandTransport::PosixShell,
        request.contract_digest,
        &request.observation.workflow_digest,
    ) {
        Ok(Some(mut plan)) => {
            plan.audit_report = fetched.report;
            Keyed::Planned(Box::new(plan))
        }
        Ok(None) => closeout(
            "executable_reuse_not_runnable",
            "the protected-base execution policy cannot carry a keyed build-and-test run",
        ),
        Err(error) => Keyed::Closeout {
            category: "executable_reuse_plan_error",
            payload_bytes: error.selection_payload_over_cap(),
            diagnostic: error.to_string(),
        },
    }
}

/// What this host's store holds for a keyed plan against `base`: the
/// candidate records the ship path would bind, or why none qualifies.
#[derive(Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct BindableRecords {
    pub(crate) repository: String,
    pub(crate) target: String,
    pub(crate) base_sha: String,
    /// This host's platform, as the base's probe states it.
    pub(crate) platform: String,
    /// How many records the plan would bind (at most its candidate cap).
    pub(crate) bindable: usize,
    pub(crate) candidates: Vec<crate::changed_surface::executable_reuse::BaseCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) no_base: Option<String>,
    /// Every filed record, newest first, each judged by the same rules.
    pub(crate) records: Vec<RecordJudgment>,
}

/// One filed record and whether a keyed plan against the base may bind it.
#[derive(Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct RecordJudgment {
    /// The commit the record's run validated.
    pub(crate) sha: String,
    pub(crate) target: String,
    /// The record's run directory name.
    pub(crate) run_id: String,
    pub(crate) path: String,
    /// When the store filed it (RFC 3339, UTC).
    pub(crate) filed_at: String,
    /// It passes every rule the plan applies.
    pub(crate) bindable: bool,
    /// It is among the records the plan binds (bindable and within the cap).
    pub(crate) candidate: bool,
    /// Why it is not bindable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

/// List the records a keyed plan against `base` would bind on this host,
/// through the same base policy, platform probe, store and rules as the ship
/// path. Read-only apart from materializing the base's derivation code.
///
/// # Errors
///
/// The base policy cannot be read, does not declare executable reuse for
/// `target`, or binding failed.
pub(crate) fn bindable_records(
    cwd: &Path,
    state_dir: &Path,
    base: &str,
    target: &str,
) -> Result<BindableRecords, String> {
    let base_sha = String::from_utf8(git_bytes(
        cwd,
        &["rev-parse", &format!("{base}^{{commit}}")],
    )?)
    .map_err(|_| "the base commit is not UTF-8".to_owned())?
    .trim()
    .to_owned();
    let read_text = |path: &str| {
        git_bytes(cwd, &["show", &format!("{base_sha}:{path}")]).and_then(|bytes| {
            String::from_utf8(bytes)
                .map(|text| text.trim().to_owned())
                .map_err(|_| format!("{path} is not UTF-8"))
        })
    };
    let config = read_text(".shipyard/config.toml")?;
    let repository = config
        .parse::<toml::Table>()
        .ok()
        .and_then(|table| {
            table
                .get("project")
                .and_then(|project| project.get("repository"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| "the base configuration names no [project].repository".to_owned())?;
    let policy = crate::changed_surface::policy_from_base(&config, target, read_text)?;
    let reuse = policy
        .executable_reuse
        .as_ref()
        .ok_or_else(|| format!("target {target} declares no executable_reuse at {base_sha}"))?;
    let store = crate::reuse_record_store::store_dir(state_dir, &repository);
    let derivation_root = state_dir.join("executable-reuse").join("derivation");
    let policy_digest = crate::changed_surface::policy_digest(&policy);
    let build_dir = cwd.join(&reuse.build_dir);
    let probed = std::cell::RefCell::new(None);
    let bound = bind(
        reuse,
        &BindRequest {
            store: &store,
            derivation_root: &derivation_root,
            head_sha: &base_sha,
            policy_digest: &policy_digest,
            build_dir: &reuse.build_dir,
        },
        |path| git_bytes(cwd, &["show", &format!("{base_sha}:{path}")]),
        |code_dir| {
            let platform = probe_platform(reuse, code_dir, &build_dir)?;
            probed.replace(Some(platform.clone()));
            Ok(platform)
        },
        |commit| merged_into(cwd, commit, &base_sha),
    )?;
    let platform = probed
        .into_inner()
        .ok_or_else(|| "binding finished without probing the platform".to_owned())?;
    let (candidates, no_base) = match bound {
        Binding::Bound(binding) => (binding.candidates, None),
        Binding::NoBase(why) => (Vec::new(), Some(why)),
    };
    let criteria = crate::changed_surface::executable_reuse::ConfiguredCriteria {
        rules: &reuse.base_record,
        merged: |commit: &str| merged_into(cwd, commit, &base_sha),
    };
    let records = judge_records(&store, target, &platform, &criteria, &candidates);
    Ok(BindableRecords {
        repository,
        target: target.to_owned(),
        base_sha,
        platform,
        bindable: candidates.len(),
        candidates,
        no_base,
        records,
    })
}

/// Judge every filed record, readable or not, as the plan would.
fn judge_records<C: crate::reuse_record_store::BaseCriteria>(
    store: &Path,
    target: &str,
    platform: &str,
    criteria: &C,
    candidates: &[crate::changed_surface::executable_reuse::BaseCandidate],
) -> Vec<RecordJudgment> {
    crate::reuse_record_store::survey(store)
        .into_iter()
        .map(|filed| {
            let path = filed.path.to_string_lossy().into_owned();
            let reason = match &filed.record {
                Err(why) => Some(why.clone()),
                Ok(record) => crate::reuse_record_store::judge(record, platform, criteria)
                    .err()
                    .map(|refusal| refusal.to_string()),
            };
            RecordJudgment {
                sha: filed.commit,
                target: target.to_owned(),
                run_id: filed
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                filed_at: chrono::DateTime::<chrono::Utc>::from(filed.filed_at)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                bindable: reason.is_none(),
                candidate: candidates
                    .iter()
                    .any(|candidate| candidate.record_path == path),
                reason,
                path,
            }
        })
        .collect()
}

/// Whether a record's commit is in the planned base's history.
pub(crate) fn merged_into(cwd: &Path, commit: &str, base: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", commit, base])
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Run the base's platform probe from the materialized derivation code and
/// read its output through the record's platform pointer.
pub(crate) fn probe_platform(
    reuse: &ExecutableReusePolicy,
    code_dir: &Path,
    build_dir: &Path,
) -> Result<String, String> {
    let build_dir = build_dir.to_string_lossy();
    let command = reuse
        .platform_probe
        .iter()
        .map(|arg| arg.replace("{build_dir}", &build_dir))
        .collect::<Vec<_>>();
    let bytes = run_base_command(&command, code_dir, "the platform probe")?;
    read_probe(reuse, &bytes)
}

/// The lane's platform from probe output, through the record's pointer.
fn read_probe(reuse: &ExecutableReusePolicy, bytes: &[u8]) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("the platform probe printed no JSON object: {error}"))?;
    let pointer = &reuse.base_record.platform;
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .filter(|found| !found.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("the platform probe states no {pointer}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changed_surface::executable_reuse::{BaseRecordRules, DEFAULT_SAMPLE_PERCENT};

    fn reuse(probe: &[&str]) -> ExecutableReusePolicy {
        ExecutableReusePolicy {
            switch_variable: "PULP_REUSE_LIVE".to_owned(),
            derivation_paths: vec!["probe.sh".to_owned()],
            sample_percent: DEFAULT_SAMPLE_PERCENT,
            build_dir: "build".to_owned(),
            platform_probe: probe.iter().map(|arg| (*arg).to_owned()).collect(),
            rederive: vec![vec!["true".to_owned()]],
            base_record: BaseRecordRules {
                platform: "/platform".to_owned(),
                toolchain: "/toolchain/digest".to_owned(),
                toolchain_fields: Some("/toolchain/fields".to_owned()),
                require: Vec::new(),
            },
        }
    }

    #[test]
    fn the_probe_is_read_through_the_platform_pointer() {
        let platform = read_probe(
            &reuse(&["true"]),
            br#"{"platform":"darwin-arm64","toolchain":{"digest":"toolchain_unknown"}}"#,
        )
        .expect("probe");
        assert_eq!(
            platform, "darwin-arm64",
            "a cold build dir's toolchain is not read"
        );
        for missing in [
            r#"{"toolchain":{"digest":"abc"}}"#,
            r#"{"platform":""}"#,
            "not json",
        ] {
            assert!(
                read_probe(&reuse(&["true"]), missing.as_bytes()).is_err(),
                "{missing}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_probe_runs_from_the_derivation_code_with_the_build_dir() {
        let code = tempfile::tempdir().expect("code");
        std::fs::write(
            code.path().join("probe.sh"),
            "printf '{\"platform\":\"%s|%s\"}' \"$(basename \"$PWD\")\" \"$1\"\n",
        )
        .expect("probe");
        let platform = probe_platform(
            &reuse(&["sh", "probe.sh", "{build_dir}"]),
            code.path(),
            Path::new("/lane/build"),
        )
        .expect("probe");
        let name = code
            .path()
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            platform,
            format!("{name}|/lane/build"),
            "runs from the derivation code with {{build_dir}} substituted"
        );
        assert!(
            probe_platform(
                &reuse(&["sh", "-c", "exit 3"]),
                code.path(),
                Path::new("/b")
            )
            .unwrap_err()
            .contains("failed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_bytes_keep_the_files_exact_bytes() {
        let repo = tempfile::tempdir().expect("repo");
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(repo.path())
                    .output()
                    .expect("git")
                    .status
                    .success(),
                "{args:?}"
            );
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("k.py"), "x\n\n").expect("write");
        git(&["add", "k.py"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "k",
        ]);
        assert_eq!(
            git_bytes(repo.path(), &["show", "HEAD:k.py"]).expect("show"),
            b"x\n\n"
        );
        let head = String::from_utf8(git_bytes(repo.path(), &["rev-parse", "HEAD"]).expect("sha"))
            .expect("utf8");
        assert!(merged_into(repo.path(), head.trim(), "HEAD"));
        assert!(!merged_into(
            repo.path(),
            "0000000000000000000000000000000000000000",
            "HEAD"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_measure_reads_zero_until_a_merged_record_is_filed_then_one() {
        use crate::reuse_record_store::{create_pending, file, store_dir};
        let root = tempfile::tempdir().expect("root");
        let repo = root.path().join("repo");
        let state = root.path().join("state");
        std::fs::create_dir_all(repo.join(".shipyard")).expect("repo");
        std::fs::write(
            repo.join(".shipyard/config.toml"),
            r#"
[project]
repository = "Owner/Repo"

[targets.mac]
validation_build_type = "debug"

[targets.mac.changed_surface_selection]
schema_version = 1
full_test_count = 10
build_type = "debug"
baseline_tests = ["smoke"]
test_topology_paths = ["tests/**"]

[targets.mac.changed_surface_selection.executable_reuse]
switch_variable = "REUSE_LIVE"
derivation_paths = ["probe.py"]
build_dir = "build"
platform_probe = ["python3", "-I", "probe.py"]
rederive = [["python3", "-I", "probe.py"]]

[targets.mac.changed_surface_selection.executable_reuse.base_record]
platform = "/platform"
toolchain = "/toolchain/digest"
require = [{ pointer = "/dirty", equals = false }]

[[targets.mac.changed_surface_selection.families]]
name = "core"
paths = ["src/**"]
tests = ["smoke"]
supported_build_types = ["debug"]
"#,
        )
        .expect("config");
        std::fs::write(
            repo.join("probe.py"),
            "print('{\"platform\": \"darwin-arm64\"}')\n",
        )
        .expect("probe");
        let git = |args: &[&str]| -> String {
            let output = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git");
            assert!(output.status.success(), "{args:?}");
            String::from_utf8(output.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]);

        let empty = bindable_records(&repo, &state, "main", "mac").expect("measure");
        assert_eq!(empty.bindable, 0, "today's truth before any record");
        assert!(empty.no_base.is_some());

        let store = store_dir(&state, "Owner/Repo");
        let put = |commit: &str, dirty: bool| {
            let pending = create_pending(&store, commit, chrono::Utc::now()).expect("pending");
            std::fs::write(
                pending.join("job.json"),
                serde_json::json!({"platform": "darwin-arm64",
                    "toolchain": {"digest": "t"}, "dirty": dirty})
                .to_string(),
            )
            .expect("job");
            file(&store, &pending, commit).expect("file");
        };
        put(&"9".repeat(40), false);
        let unmerged = bindable_records(&repo, &state, "main", "mac").expect("measure");
        assert_eq!(
            unmerged.bindable, 0,
            "a commit not on the base never counts"
        );
        put(&base, true);
        let dirty = bindable_records(&repo, &state, "main", "mac").expect("measure");
        assert_eq!(
            dirty.bindable, 0,
            "a record failing the policy never counts"
        );
        put(&base, false);
        let one = bindable_records(&repo, &state, "main", "mac").expect("measure");
        assert_eq!(one.bindable, 1, "{one:?}");
        assert_eq!(one.candidates[0].commit, base);
        assert_eq!(one.base_sha, base);
        assert_eq!(one.repository, "Owner/Repo");
        assert_eq!(one.platform, "darwin-arm64");

        // Every filed record is listed and judged by the plan's own rules,
        // so the per-record view and the count can never disagree.
        let judged: Vec<(&str, bool, bool, Option<&str>)> = one
            .records
            .iter()
            .map(|record| {
                (
                    record.sha.as_str(),
                    record.bindable,
                    record.candidate,
                    record.reason.as_deref(),
                )
            })
            .collect();
        assert_eq!(judged.len(), 3, "{judged:?}");
        assert_eq!(
            judged
                .iter()
                .filter(|(_, bindable, _, _)| *bindable)
                .count(),
            one.bindable
        );
        assert!(judged.contains(&(base.as_str(), true, true, None)));
        assert!(judged.iter().any(|(sha, bindable, _, why)| *sha == base
            && !bindable
            && why.is_some_and(|why| why.starts_with("unusable: "))));
        let nine = "9".repeat(40);
        assert!(judged.contains(&(
            nine.as_str(),
            false,
            false,
            Some("its commit is not merged into the base")
        )));
        assert!(one.records.iter().all(|record| record.target == "mac"
            && record.filed_at.ends_with('Z')
            && !record.run_id.is_empty()));

        // A run directory whose job.json is gone is reported, never dropped.
        let unmerged_dir = one
            .records
            .iter()
            .find(|record| record.sha == nine)
            .map(|record| std::path::PathBuf::from(&record.path))
            .expect("unmerged record");
        std::fs::remove_file(unmerged_dir.join("job.json")).expect("remove job");
        let broken = bindable_records(&repo, &state, "main", "mac").expect("measure");
        assert_eq!(broken.records.len(), 3);
        assert!(broken.records.iter().any(|record| {
            record.sha == nine
                && !record.bindable
                && record
                    .reason
                    .as_deref()
                    .is_some_and(|why| why.starts_with("no readable job.json"))
        }));
        assert_eq!(broken.bindable, 1);
    }
}
