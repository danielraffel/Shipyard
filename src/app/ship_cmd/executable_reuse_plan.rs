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
    },
}

fn closeout(category: &'static str, diagnostic: impl Into<String>) -> Keyed {
    Keyed::Closeout {
        category,
        diagnostic: diagnostic.into(),
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
    let binding = match bound {
        Ok(Binding::Bound(binding)) => binding,
        Ok(Binding::NoBase(why)) => return closeout("executable_reuse_no_base", why),
        Err(error) => return closeout("executable_reuse_bind_error", error),
    };
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
        Ok(Some(plan)) => Keyed::Planned(Box::new(plan)),
        Ok(None) => closeout(
            "executable_reuse_not_runnable",
            "the protected-base execution policy cannot carry a keyed build-and-test run",
        ),
        Err(error) => closeout("executable_reuse_plan_error", error.to_string()),
    }
}

/// Whether a record's commit is in the planned base's history.
fn merged_into(cwd: &Path, commit: &str, base: &str) -> bool {
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
fn probe_platform(
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
}
