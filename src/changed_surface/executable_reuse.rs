//! The protected-base declaration of executable-keyed reuse for one target.
//!
//! A keyed plan derives which executables it may skip by running the
//! project's key-derivation code. That code is a policy surface: a pull
//! request that could change it, or the list naming it, could make its own
//! changes look unchanged. So the declaration is read only from the
//! protected base's configuration (it is part of [`super::ChangedSurfacePolicy`],
//! which [`super::policy_from_base`] parses), Shipyard copies exactly the
//! listed `derivation_paths` from the base, and a head-side change to any of
//! them sends the plan to the full suite, as a `policy_paths` change does.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default share of would-skip executables rebuilt and rerun in every plan.
pub const DEFAULT_SAMPLE_PERCENT: u32 = 5;

/// Executable-keyed reuse for one target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableReusePolicy {
    /// Repository variable whose exact value `live` lets keyed plans skip work
    /// (see [`super::live_switch`]); any other state records only.
    pub switch_variable: String,
    /// Repository files the key derivation runs from. Shipyard copies these
    /// from the protected base for every keyed plan; a head-side change to any
    /// of them forces the full suite.
    pub derivation_paths: Vec<String>,
    /// Percentage of would-skip executables rebuilt and rerun anyway.
    #[serde(default = "default_sample_percent")]
    pub sample_percent: u32,
    /// The lane's build directory, relative to the checkout.
    pub build_dir: String,
    /// Command printing the lane's platform as one JSON object read through
    /// the same `base_record.platform` pointer as a record's `job.json`. It
    /// runs from the materialized base derivation code, and every
    /// `{build_dir}` in it becomes the lane's absolute build directory. The
    /// toolchain is not probed here: only the lane, after its configure,
    /// knows the toolchain it builds with.
    pub platform_probe: Vec<String>,
    /// The commands that re-derive a keyed run's manifest and selection on
    /// the host, in order, each run from the materialized base derivation
    /// code over the run's copied inputs. Placeholders: `{source_root}`,
    /// `{base_sha}`, `{head_sha}`, `{base_record_dir}`,
    /// `{base_record_run_id}`, `{result_dir}`, `{build_dir}`, `{out_dir}`,
    /// `{sample_seed}`, `{sample_percent}`, `{audit_report}` (the staged
    /// read-audit report; when the plan bound none, the argument holding it
    /// and the flag before it are dropped). They must leave
    /// `{out_dir}/executable-keys.json` and `{out_dir}/selection.json`.
    pub rederive: Vec<Vec<String>>,
    /// How to read a base reuse record's `job.json`, and what it must state
    /// before a plan may key against it.
    pub base_record: BaseRecordRules,
}

/// The project's record format, as JSON pointers into a record's `job.json`.
/// Shipyard knows no record format; the project names where each fact lives.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BaseRecordRules {
    /// Pointer to the record's platform (architecture and OS family).
    pub platform: String,
    /// Pointer to the record's toolchain identity digest.
    pub toolchain: String,
    /// Pointer to the toolchain's field object (what the digest covers), read
    /// only to explain a refusal; absent fields leave the explanation generic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain_fields: Option<String>,
    /// Further facts a usable record must state, each checked in order; the
    /// first one that fails names why the record is refused.
    #[serde(default)]
    pub require: Vec<RecordRequirement>,
}

/// One fact a usable base record states.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRequirement {
    /// JSON pointer into `job.json`.
    pub pointer: String,
    /// The exact value it must hold (a missing value never equals anything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<serde_json::Value>,
    /// Set to `true` when the value only has to be present.
    #[serde(default)]
    pub present: bool,
}

impl RecordRequirement {
    /// Why `job` fails this requirement, `None` when it holds.
    #[must_use]
    pub fn failure(&self, job: &serde_json::Value) -> Option<String> {
        let found = job.pointer(&self.pointer);
        match (&self.equals, found) {
            (Some(want), Some(have)) => {
                (want != have).then(|| format!("{} is {have}, not {want}", self.pointer))
            }
            (Some(want), None) => Some(format!("{} is missing (needs {want})", self.pointer)),
            (None, None) if self.present => Some(format!("{} is missing", self.pointer)),
            (None, _) => None,
        }
    }
}

fn default_sample_percent() -> u32 {
    DEFAULT_SAMPLE_PERCENT
}

impl ExecutableReusePolicy {
    /// Check the declaration.
    ///
    /// # Errors
    ///
    /// Why it is invalid: an empty or malformed switch variable, no derivation
    /// paths, a path that is not a plain repository-relative file path (globs
    /// would let the copied set differ from the forcing set), a duplicate, or
    /// a sample percentage outside 1..=100.
    pub fn validate(&self) -> Result<(), String> {
        let name_ok = !self.switch_variable.is_empty()
            && self
                .switch_variable
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !name_ok {
            return Err(format!(
                "executable_reuse.switch_variable {:?} must be a repository variable name (A-Z, 0-9, _)",
                self.switch_variable
            ));
        }
        if self.derivation_paths.is_empty() {
            return Err(
                "executable_reuse.derivation_paths must name the key-derivation code".to_owned(),
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        for path in &self.derivation_paths {
            if !plain_relative_path(path) {
                return Err(format!(
                    "executable_reuse.derivation_paths entry {path:?} must be a plain repository-relative file path"
                ));
            }
            if !seen.insert(path.as_str()) {
                return Err(format!(
                    "executable_reuse.derivation_paths lists {path:?} twice"
                ));
            }
        }
        for pointer in [&self.base_record.platform, &self.base_record.toolchain]
            .into_iter()
            .chain(self.base_record.require.iter().map(|r| &r.pointer))
        {
            if !pointer.starts_with('/') {
                return Err(format!(
                    "executable_reuse.base_record pointer {pointer:?} must be a JSON pointer (start with /)"
                ));
            }
        }
        for requirement in &self.base_record.require {
            if requirement.equals.is_some() == requirement.present {
                return Err(format!(
                    "executable_reuse.base_record.require {:?} needs exactly one of `equals` or `present = true`",
                    requirement.pointer
                ));
            }
        }
        if !plain_relative_path(&self.build_dir) {
            return Err(format!(
                "executable_reuse.build_dir {:?} must be a plain repository-relative path",
                self.build_dir
            ));
        }
        if !isolated_python(&self.platform_probe) {
            return Err(
                "executable_reuse.platform_probe must run a Python interpreter with -I".to_owned(),
            );
        }
        if self.rederive.is_empty() || !self.rederive.iter().all(|command| isolated_python(command))
        {
            return Err(
                "executable_reuse.rederive must list commands, each running a Python interpreter with -I"
                    .to_owned(),
            );
        }
        if !(1..=100).contains(&self.sample_percent) {
            return Err(format!(
                "executable_reuse.sample_percent {} must be between 1 and 100",
                self.sample_percent
            ));
        }
        Ok(())
    }
}

/// Whether `command` runs a Python interpreter (`python`, `python3` or
/// `python3.N`, by name or path) in isolated mode (`-I`), so the base key
/// code runs without the user's site packages, `PYTHON*` variables or the
/// script directory on its import path.
fn isolated_python(command: &[String]) -> bool {
    let [program, flag, ..] = command else {
        return false;
    };
    let name = program.rsplit('/').next().unwrap_or(program);
    let versioned = name
        .strip_prefix("python3.")
        .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()));
    (name == "python" || name == "python3" || versioned) && flag == "-I"
}

/// A repository-relative path with no glob, no backslash and no `.`/`..` or
/// empty component, so the path Shipyard copies is the path it forces on.
fn plain_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(['*', '?', '[', '{'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The derivation code copied from the protected base, with its digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivationCode {
    /// Each listed path and its bytes at the protected base.
    pub files: BTreeMap<String, Vec<u8>>,
    /// sha256 over the sorted `path\0sha256\n` lines.
    pub digest: String,
}

/// Read every `derivation_paths` entry from the protected base. `read_base`
/// is bound to the base commit (`git show <base>:<path>`); the head's tree is
/// never consulted, so a pull request cannot shrink or alter the copy.
///
/// # Errors
///
/// The first path the base does not hold, or a read failure: a keyed plan
/// cannot run without its whole derivation code.
pub fn derivation_copy<R>(
    policy: &ExecutableReusePolicy,
    read_base: R,
) -> Result<DerivationCode, String>
where
    R: Fn(&str) -> Result<Vec<u8>, String>,
{
    let mut files = BTreeMap::new();
    for path in &policy.derivation_paths {
        let bytes =
            read_base(path).map_err(|error| format!("derivation code {path:?}: {error}"))?;
        files.insert(path.clone(), bytes);
    }
    let lines = files
        .iter()
        .fold(String::new(), |mut lines, (path, bytes)| {
            let _ = writeln!(lines, "{path}\0{}", sha256_hex(bytes));
            lines
        });
    Ok(DerivationCode {
        digest: sha256_hex(lines.as_bytes()),
        files,
    })
}

/// [`crate::reuse_record_store::BaseCriteria`] read from the project's
/// declared record format, with `merged` deciding ancestry of the protected
/// base.
pub struct ConfiguredCriteria<'a, M: Fn(&str) -> bool> {
    /// The declared format.
    pub rules: &'a BaseRecordRules,
    /// Whether a commit is an ancestor of the plan's protected base.
    pub merged: M,
}

impl<M: Fn(&str) -> bool> crate::reuse_record_store::BaseCriteria for ConfiguredCriteria<'_, M> {
    fn platform(&self, job: &serde_json::Value) -> Option<String> {
        job.pointer(&self.rules.platform)?
            .as_str()
            .map(str::to_owned)
    }
    fn toolchain(&self, job: &serde_json::Value) -> Option<String> {
        job.pointer(&self.rules.toolchain)?
            .as_str()
            .map(str::to_owned)
    }
    fn unusable(&self, record: &crate::reuse_record_store::StoredRecord) -> Option<String> {
        self.rules
            .require
            .iter()
            .find_map(|r| r.failure(&record.job))
    }
    fn merged(&self, commit: &str) -> bool {
        (self.merged)(commit)
    }
}

/// The digest of a reuse-record directory, as the project's key code
/// computes it: every regular file beneath `dir` (symlinks followed), its
/// path relative to `dir` with `/` separators, ordered by the path's UTF-8
/// bytes; one sha256 fed `path`, NUL, the file's lowercase hex sha256 and a
/// newline for each, in that order.
///
/// # Errors
///
/// The I/O error of a directory or file that cannot be read, or a path that
/// is not valid UTF-8 (it could not be named the same way on both sides).
pub fn record_digest(dir: &std::path::Path) -> std::io::Result<String> {
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<(String, std::path::PathBuf)>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            // `metadata` follows symlinks, as the key code's walk does.
            let meta = std::fs::metadata(&path)?;
            if meta.is_dir() {
                walk(root, &path, out)?;
            } else if meta.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .map_err(std::io::Error::other)?
                    .components()
                    .map(|c| {
                        c.as_os_str().to_str().map(str::to_owned).ok_or_else(|| {
                            std::io::Error::other(format!("{} is not UTF-8", path.display()))
                        })
                    })
                    .collect::<std::io::Result<Vec<_>>>()?
                    .join("/");
                out.push((rel, path));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut hasher = Sha256::new();
    for (rel, path) in files {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(sha256_hex(&std::fs::read(path)?).as_bytes());
        hasher.update(b"\n");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Whether `dir` holds exactly `code`: every file with the bytes it digests,
/// and nothing else. A directory's name is only a claim about its content.
fn holds(dir: &std::path::Path, code: &DerivationCode) -> bool {
    let mut present = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(path),
                Ok(kind) if kind.is_file() => present.push(path),
                _ => return false, // a symlink or anything else was not written by us
            }
        }
    }
    present.len() == code.files.len()
        && code
            .files
            .iter()
            .all(|(rel, bytes)| std::fs::read(dir.join(rel)).is_ok_and(|on_disk| on_disk == *bytes))
}

/// Write the derivation code into `root/<digest>/<path>` and return that
/// directory. It is named by the code's digest, and an existing directory of
/// that name is used only when its content is exactly this code; otherwise it
/// is replaced. Publication is a rename, so no reader sees a partial copy.
///
/// # Errors
///
/// The I/O error of a directory or file that cannot be written or replaced.
pub fn materialize(
    code: &DerivationCode,
    root: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    let dir = root.join(&code.digest);
    if holds(&dir, code) {
        return Ok(dir);
    }
    let staging = root.join(format!(".{}-{}", code.digest, std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    for (path, bytes) in &code.files {
        let dest = staging.join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(dest, bytes)?;
    }
    if dir.exists() {
        // A directory under this name that does not hold this code: move it
        // aside rather than serve it.
        let stale = root.join(format!(".{}-stale-{}", code.digest, std::process::id()));
        std::fs::rename(&dir, &stale)?;
        let _ = std::fs::remove_dir_all(&stale);
    }
    match std::fs::rename(&staging, &dir) {
        Ok(()) => Ok(dir),
        // Another plan published the same code first; verify, do not trust.
        Err(_) if holds(&dir, code) => {
            let _ = std::fs::remove_dir_all(&staging);
            Ok(dir)
        }
        Err(error) => Err(error),
    }
}

/// File name, in a trial directory, of a keyed run's [`KeyedRunContext`].
pub const KEYED_CONTEXT_RECEIPT: &str = "executable-reuse-context.json";

/// What the host re-derivation needs back after a keyed run: the checkout
/// it ran in and the exact payload the activation's digest covers (and so
/// the binding inside it).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeyedRunContext {
    /// Context schema.
    pub schema_version: u32,
    /// Repository the run validated.
    pub repository: String,
    /// The checkout the run's stages ran in.
    pub checkout: String,
    /// The execution payload, URL-safe base64 without padding.
    pub execution_payload_b64: String,
}

/// Most base records one plan binds as candidates.
pub const MAX_CANDIDATES: usize = 8;

/// One stored record a keyed plan may key against.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BaseCandidate {
    /// The record's run directory name.
    pub run_id: String,
    /// [`record_digest`] of the record.
    pub record_sha256: String,
    /// Where the record lives on this host.
    pub record_path: String,
    /// The commit the record's run validated.
    pub commit: String,
}

/// What a keyed plan binds before any stage runs, for the runner to echo
/// and the result check to verify (`executable_reuse` in the selection
/// receipt). Field names are the runner's contract.
///
/// The candidates passed every host-side rule (platform, merged, the declared
/// `require` list), newest first. The runner picks the first whose toolchain
/// equals the one it configured with, and names its pick in the result; a
/// pick outside this set is refused.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableReuseBinding {
    /// The records the runner may pick from, newest first.
    pub candidates: Vec<BaseCandidate>,
    /// Digest of the `base_record` rules the candidates were chosen by.
    pub rules_digest: String,
    /// The materialized derivation code.
    pub derivation_code_dir: String,
    /// [`DerivationCode::digest`].
    pub derivation_code_sha256: String,
    /// [`sample_seed`] for this plan.
    pub sample_seed: String,
    /// The policy's sample percentage.
    pub sample_percent: u32,
    /// The lane's build directory, as the identical string the runner uses.
    pub build_dir: String,
    /// The read-audit report the key code is handed, or why there is none.
    /// Absent in bindings written before the audit existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<AuditBinding>,
}

/// File a keyed plan's read-audit report is staged as in its evidence
/// directory, beside [`crate::changed_surface::EXECUTABLE_REUSE_BINDING_FILE`].
pub const AUDIT_REPORT_FILE: &str = "read-audit.json";

/// The read-audit report a keyed plan bound. Without one the key code keys
/// nothing (every executable is uncovered), so such a plan is not a reuse
/// observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuditBinding {
    /// A clean report, staged as [`AUDIT_REPORT_FILE`].
    Staged {
        /// The read-audit workflow run that produced it.
        run_id: String,
        /// The commit that run audited.
        audit_commit: String,
        /// How many commits the plan's base is ahead of `audit_commit`.
        commits_behind: u64,
        /// sha256 of the staged file's bytes.
        report_sha256: String,
    },
    /// No report: `no_clean_run`, `fetch_failed` or `no_credentials`.
    None {
        /// Why.
        reason: String,
    },
}

/// How the host's re-run of the base key code compares with the runner's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Rederivation {
    /// Same selection, same manifest.
    Match,
    /// Same selection; the manifests differ only in fields that legitimately
    /// differ between two runs (named here), so the run stands.
    MatchWithDiagnostics(Vec<String>),
    /// The selections differ, or the manifests differ outside those fields:
    /// the run is not validated.
    Refuse(String),
}

/// Producer fields two honest derivations may disagree on.
const RUN_SPECIFIC_PRODUCER_FIELDS: [&str; 2] = ["head_sha", "base_record_run_id"];

/// Replace every run-specific value with a fixed marker, recording where.
fn normalise(value: &mut serde_json::Value, at: &str, seen: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) if text.starts_with("unknown:") => {
            "unknown:".clone_into(text);
            seen.push(at.to_owned());
        }
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let here = format!("{at}/{key}");
                if at == "/producer" && RUN_SPECIFIC_PRODUCER_FIELDS.contains(&key.as_str()) {
                    *child = serde_json::Value::Null;
                    seen.push(here);
                } else {
                    normalise(child, &here, seen);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                normalise(child, &format!("{at}/{index}"), seen);
            }
        }
        _ => {}
    }
}

/// Compare the runner's derivation with the host's re-run: the selection
/// byte-for-byte, the manifest after normalising the run-specific fields
/// (producer `head_sha` and `base_record_run_id`, and every `unknown:<nonce>`
/// value, whose nonce differs on every codemodel digest by design).
#[must_use]
pub fn compare_rederivation(
    runner_manifest: &[u8],
    host_manifest: &[u8],
    runner_selection: &[u8],
    host_selection: &[u8],
) -> Rederivation {
    if runner_selection != host_selection {
        return Rederivation::Refuse("the selections differ".to_owned());
    }
    if runner_manifest == host_manifest {
        return Rederivation::Match;
    }
    let parse = |bytes: &[u8]| serde_json::from_slice::<serde_json::Value>(bytes);
    let (Ok(mut runner), Ok(mut host)) = (parse(runner_manifest), parse(host_manifest)) else {
        return Rederivation::Refuse("a manifest is not JSON".to_owned());
    };
    let (mut runner_seen, mut host_seen) = (Vec::new(), Vec::new());
    normalise(&mut runner, "", &mut runner_seen);
    normalise(&mut host, "", &mut host_seen);
    if runner != host {
        return Rederivation::Refuse("the manifests differ beyond run-specific fields".to_owned());
    }
    runner_seen.extend(host_seen);
    runner_seen.sort();
    runner_seen.dedup();
    Rederivation::MatchWithDiagnostics(runner_seen)
}

/// A keyed plan's binding, or why it has none.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Binding {
    /// Every input bound; the runner derives and Shipyard re-derives against it.
    Bound(Box<ExecutableReuseBinding>),
    /// No base record qualified; nothing is keyed and the plan runs as usual.
    NoBase(String),
}

/// Inputs a binding needs from the ship path.
pub struct BindRequest<'a> {
    /// The host-local reuse-record store for this repository.
    pub store: &'a std::path::Path,
    /// Where derivation code is materialized.
    pub derivation_root: &'a std::path::Path,
    /// Exact pull-request head.
    pub head_sha: &'a str,
    /// Digest of the protected-base selector policy.
    pub policy_digest: &'a str,
    /// The lane's build directory string.
    pub build_dir: &'a str,
}

/// Bind a keyed plan before any stage runs: copy the derivation code from the
/// protected base and materialize it, probe the lane's platform with that
/// copy, list the candidate records by the declared rules, and fix the seed.
///
/// `read_base` reads a file at the protected base; `probe` runs the
/// materialized key code's platform probe; `merged` says whether a commit is
/// an ancestor of the protected base.
///
/// # Errors
///
/// A failure to copy, materialize or probe, or to read the chosen record:
/// the plan then binds nothing and runs as usual.
pub fn bind<R, P, M>(
    policy: &ExecutableReusePolicy,
    request: &BindRequest<'_>,
    read_base: R,
    probe: P,
    merged: M,
) -> Result<Binding, String>
where
    R: Fn(&str) -> Result<Vec<u8>, String>,
    P: Fn(&std::path::Path) -> Result<String, String>,
    M: Fn(&str) -> bool,
{
    use crate::reuse_record_store::{NoBase, select_candidates};
    let code = derivation_copy(policy, read_base)?;
    let code_dir = materialize(&code, request.derivation_root)
        .map_err(|error| format!("cannot materialize the derivation code: {error}"))?;
    let platform = probe(&code_dir)?;
    let criteria = ConfiguredCriteria {
        rules: &policy.base_record,
        merged,
    };
    let records = match select_candidates(request.store, &platform, &criteria, MAX_CANDIDATES) {
        Ok(records) => records,
        Err(NoBase::Empty) => {
            return Ok(Binding::NoBase(
                "the store holds no reuse record yet".to_owned(),
            ));
        }
        Err(NoBase::NoneQualify(refused)) => {
            return Ok(Binding::NoBase(no_base_closeout(&refused)));
        }
    };
    let mut candidates = Vec::with_capacity(records.len());
    for record in records {
        candidates.push(BaseCandidate {
            run_id: record
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| "a candidate record has no run directory name".to_owned())?,
            record_sha256: record_digest(&record.path)
                .map_err(|error| format!("cannot digest a candidate record: {error}"))?,
            record_path: record.path.to_string_lossy().into_owned(),
            commit: record.commit,
        });
    }
    let digests: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.record_sha256.as_str())
        .collect();
    let seed = sample_seed(request.head_sha, request.policy_digest, &digests);
    let rules = serde_json::to_vec(&policy.base_record)
        .map_err(|error| format!("cannot serialize the base record rules: {error}"))?;
    Ok(Binding::Bound(Box::new(ExecutableReuseBinding {
        sample_seed: seed,
        candidates,
        rules_digest: sha256_hex(&rules),
        derivation_code_dir: code_dir.to_string_lossy().into_owned(),
        derivation_code_sha256: code.digest,
        sample_percent: policy.sample_percent,
        build_dir: request.build_dir.to_owned(),
        // The ship path fills this in before planning; bind() reads no
        // network.
        audit: None,
    })))
}

/// One line saying why no record qualified.
fn no_base_closeout(refused: &crate::reuse_record_store::Refusals) -> String {
    format!(
        "no stored record qualified: {} with unknown platform, {} other platform, {} unknown toolchain, \
         {} unusable, {} not merged",
        refused.unknown_platform,
        refused.other_platform,
        refused.unknown_toolchain,
        refused.unusable,
        refused.not_merged
    )
}

/// The plan's sample seed: deterministic per (head, policy, candidate set),
/// so a re-run of the same plan samples the same executables whichever
/// candidate the runner picks, and different across heads.
#[must_use]
pub fn sample_seed(head_sha: &str, policy_digest: &str, candidate_digests: &[&str]) -> String {
    let mut text = format!("{head_sha}\0{policy_digest}");
    for digest in candidate_digests {
        text.push('\0');
        text.push_str(digest);
    }
    sha256_hex(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> ExecutableReusePolicy {
        ExecutableReusePolicy {
            switch_variable: "PULP_REUSE_LIVE".to_owned(),
            derivation_paths: vec!["tools/ci/executable_keys.py".to_owned()],
            sample_percent: DEFAULT_SAMPLE_PERCENT,
            build_dir: "build".to_owned(),
            platform_probe: vec!["python3".to_owned(), "-I".to_owned(), "probe.py".to_owned()],
            rederive: vec![vec![
                "python3".to_owned(),
                "-I".to_owned(),
                "keys.py".to_owned(),
            ]],
            base_record: rules(),
        }
    }

    fn rules() -> BaseRecordRules {
        BaseRecordRules {
            platform: "/platform".to_owned(),
            toolchain: "/toolchain/digest".to_owned(),
            toolchain_fields: Some("/toolchain/fields".to_owned()),
            require: vec![
                RecordRequirement {
                    pointer: "/dirty".to_owned(),
                    equals: Some(serde_json::Value::Bool(false)),
                    present: false,
                },
                RecordRequirement {
                    pointer: "/suites/full".to_owned(),
                    equals: None,
                    present: true,
                },
                RecordRequirement {
                    pointer: "/problems".to_owned(),
                    equals: Some(serde_json::Value::Array(Vec::new())),
                    present: false,
                },
            ],
        }
    }

    #[test]
    fn a_record_must_state_every_required_fact() {
        use serde_json::json;
        let good = json!({"dirty": false, "suites": {"full": 120}, "problems": []});
        let failure = |job: &serde_json::Value| rules().require.iter().find_map(|r| r.failure(job));
        assert_eq!(failure(&good), None);
        for (job, why) in [
            (
                json!({"dirty": true, "suites": {"full": 1}, "problems": []}),
                "/dirty is true",
            ),
            (
                json!({"dirty": null, "suites": {"full": 1}, "problems": []}),
                "/dirty is null",
            ),
            (
                json!({"suites": {"full": 1}, "problems": []}),
                "/dirty is missing",
            ),
            (
                json!({"dirty": false, "suites": {"pr-affected": 3}, "problems": []}),
                "/suites/full is missing",
            ),
            (
                json!({"dirty": false, "suites": {"full": 1}, "problems": ["object deps unavailable"]}),
                "/problems is",
            ),
        ] {
            let got = failure(&job).unwrap_or_default();
            assert!(got.starts_with(why), "{job}: {got}");
        }
    }

    #[test]
    fn requirements_are_pointers_with_exactly_one_test() {
        let mut policy = valid();
        policy.base_record.platform = "platform".to_owned();
        assert!(policy.validate().unwrap_err().contains("JSON pointer"));
        let mut policy = valid();
        policy.base_record.require[0].present = true; // equals and present both set
        assert!(policy.validate().unwrap_err().contains("exactly one"));
        let mut policy = valid();
        policy.base_record.require[1].present = false; // neither set
        assert!(policy.validate().unwrap_err().contains("exactly one"));
    }

    #[test]
    fn a_plain_declaration_is_valid() {
        assert_eq!(valid().validate(), Ok(()));
    }

    #[test]
    fn every_derivation_path_is_one_exact_file() {
        for bad in [
            "",
            "/abs/path.py",
            "tools/*.py",
            "tools/../x.py",
            "tools//x.py",
            "./x.py",
            "a\\b.py",
        ] {
            let policy = ExecutableReusePolicy {
                derivation_paths: vec![bad.to_owned()],
                ..valid()
            };
            assert!(policy.validate().is_err(), "{bad:?} must be refused");
        }
        let twice = ExecutableReusePolicy {
            derivation_paths: vec!["a.py".to_owned(), "a.py".to_owned()],
            ..valid()
        };
        assert!(twice.validate().unwrap_err().contains("twice"));
        let none = ExecutableReusePolicy {
            derivation_paths: Vec::new(),
            ..valid()
        };
        assert!(none.validate().is_err());
    }

    #[test]
    fn the_build_dir_and_platform_probe_are_checked() {
        for bad in ["", "/abs", "../up", "build/*"] {
            let policy = ExecutableReusePolicy {
                build_dir: bad.to_owned(),
                ..valid()
            };
            assert!(policy.validate().is_err(), "{bad:?} must be refused");
        }
        for bad in [Vec::new(), vec![String::new()]] {
            let policy = ExecutableReusePolicy {
                platform_probe: bad,
                ..valid()
            };
            assert!(policy.validate().unwrap_err().contains("platform_probe"));
        }
        let argv = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        for bad in [
            Vec::new(),
            vec![Vec::new()],
            vec![argv(&["python3", "keys.py"])],
            vec![argv(&["sh", "-I", "keys.sh"])],
            vec![argv(&["python3", "-I", "a.py"]), argv(&["python3", "b.py"])],
        ] {
            let policy = ExecutableReusePolicy {
                rederive: bad,
                ..valid()
            };
            assert!(policy.validate().unwrap_err().contains("rederive"));
        }
        for good in [
            argv(&["python3", "-I", "k.py"]),
            argv(&["/usr/bin/python3", "-I", "k.py"]),
            argv(&["python3.12", "-I", "k.py"]),
        ] {
            let policy = ExecutableReusePolicy {
                platform_probe: good.clone(),
                rederive: vec![good],
                ..valid()
            };
            assert_eq!(policy.validate(), Ok(()));
        }
        for bad in [argv(&["python3.x", "-I"]), argv(&["mypython3", "-I"])] {
            let policy = ExecutableReusePolicy {
                platform_probe: bad,
                ..valid()
            };
            assert!(policy.validate().unwrap_err().contains("platform_probe"));
        }
        assert_eq!(valid().validate(), Ok(()));
    }

    #[test]
    fn the_switch_and_sample_rate_are_bounded() {
        for bad in ["", "pulp_reuse_live", "PULP-REUSE"] {
            let policy = ExecutableReusePolicy {
                switch_variable: bad.to_owned(),
                ..valid()
            };
            assert!(policy.validate().is_err(), "{bad:?}");
        }
        for (percent, ok) in [(0, false), (1, true), (100, true), (101, false)] {
            let policy = ExecutableReusePolicy {
                sample_percent: percent,
                ..valid()
            };
            assert_eq!(policy.validate().is_ok(), ok, "{percent}");
        }
    }

    #[test]
    fn the_seed_is_stable_per_plan_and_moves_with_each_input() {
        let seed = sample_seed("head", "policy", &["a", "b"]);
        assert_eq!(seed, sample_seed("head", "policy", &["a", "b"]));
        for other in [
            sample_seed("head2", "policy", &["a", "b"]),
            sample_seed("head", "policy2", &["a", "b"]),
            sample_seed("head", "policy", &["a"]),
            sample_seed("head", "policy", &["b", "a"]),
        ] {
            assert_ne!(seed, other);
        }
    }

    #[test]
    fn the_derivation_copy_is_every_base_listed_file_and_its_digest_moves_with_any() {
        let policy = ExecutableReusePolicy {
            derivation_paths: vec!["tools/ci/a.py".to_owned(), "tools/ci/b.py".to_owned()],
            ..valid()
        };
        let base = |path: &str| -> Result<Vec<u8>, String> {
            match path {
                "tools/ci/a.py" => Ok(b"A".to_vec()),
                "tools/ci/b.py" => Ok(b"B".to_vec()),
                _ => Err("absent".to_owned()),
            }
        };
        let copy = derivation_copy(&policy, base).expect("copy");
        assert_eq!(
            copy.files.keys().cloned().collect::<Vec<_>>(),
            policy.derivation_paths
        );
        let edited = derivation_copy(&policy, |path: &str| {
            if path == "tools/ci/b.py" {
                Ok(b"B2".to_vec())
            } else {
                base(path)
            }
        })
        .expect("copy");
        assert_ne!(copy.digest, edited.digest);
        let missing = ExecutableReusePolicy {
            derivation_paths: vec!["tools/ci/c.py".to_owned()],
            ..valid()
        };
        assert!(
            derivation_copy(&missing, base)
                .unwrap_err()
                .contains("tools/ci/c.py")
        );
    }

    #[test]
    fn the_record_digest_orders_paths_by_bytes_and_covers_every_file() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("a")).expect("a");
        std::fs::create_dir_all(dir.path().join("suites")).expect("suites");
        std::fs::write(dir.path().join("a/b"), b"1").expect("a/b");
        std::fs::write(dir.path().join("a.b"), b"2").expect("a.b");
        std::fs::write(dir.path().join("suites/x.xml"), b"3").expect("x");
        // "a.b" (0x2e) sorts before "a/b" (0x2f) by bytes; path parts would
        // put "a/b" first.
        let line = |rel: &str, bytes: &[u8]| format!("{rel}\0{}\n", sha256_hex(bytes));
        let expected = sha256_hex(
            [
                line("a.b", b"2"),
                line("a/b", b"1"),
                line("suites/x.xml", b"3"),
            ]
            .concat()
            .as_bytes(),
        );
        assert_eq!(record_digest(dir.path()).expect("digest"), expected);
        // The value the project's key code prints for this same tree.
        assert_eq!(
            expected,
            "a8e914329111dab28a4a761e788829cce2ed8e0b01cc736e2602cafd78924a38"
        );
        std::fs::write(dir.path().join("suites/x.xml"), b"changed").expect("x");
        assert_ne!(record_digest(dir.path()).expect("digest"), expected);
    }

    #[test]
    fn materialized_code_is_named_by_its_digest_and_reused() {
        let policy = ExecutableReusePolicy {
            derivation_paths: vec!["tools/ci/a.py".to_owned()],
            ..valid()
        };
        let code = derivation_copy(&policy, |_: &str| Ok(b"print(1)".to_vec())).expect("copy");
        let root = tempfile::tempdir().expect("root");
        let dir = materialize(&code, root.path()).expect("write");
        assert_eq!(dir, root.path().join(&code.digest));
        assert_eq!(
            std::fs::read(dir.join("tools/ci/a.py")).expect("file"),
            b"print(1)"
        );
        assert_eq!(materialize(&code, root.path()).expect("again"), dir);
        // A directory with the right name but other content is never served.
        std::fs::write(dir.join("tools/ci/a.py"), b"print(2)").expect("tamper");
        let again = materialize(&code, root.path()).expect("rewrite");
        assert_eq!(
            std::fs::read(again.join("tools/ci/a.py")).expect("file"),
            b"print(1)"
        );
        std::fs::write(again.join("extra.py"), b"x").expect("extra");
        let again = materialize(&code, root.path()).expect("rewrite");
        assert!(
            !again.join("extra.py").exists(),
            "a file the code does not hold is not kept"
        );
    }

    #[test]
    fn run_specific_manifest_fields_are_a_diagnostic_and_selections_are_exact() {
        use serde_json::json;
        let manifest = |head: &str, nonce: &str, key: &str| {
            json!({"producer": {"head_sha": head, "base_record_run_id": "r1", "base_sha": "b"},
                   "executables": {"test/x": {"head_key": key, "codemodel": format!("unknown:{nonce}")}}})
            .to_string()
            .into_bytes()
        };
        let selection = br#"{"tests":["a"]}"#;
        let runner = manifest("h1", "n1", "k");
        assert_eq!(
            compare_rederivation(&runner, &runner, selection, selection),
            Rederivation::Match
        );
        let Rederivation::MatchWithDiagnostics(fields) =
            compare_rederivation(&runner, &manifest("h2", "n2", "k"), selection, selection)
        else {
            panic!("a nonce and producer difference is not a refusal");
        };
        assert!(
            fields.contains(&"/producer/head_sha".to_owned()),
            "{fields:?}"
        );
        assert!(
            fields.contains(&"/executables/test/x/codemodel".to_owned()),
            "{fields:?}"
        );
        assert!(
            matches!(
                compare_rederivation(
                    &runner,
                    &manifest("h1", "n1", "other"),
                    selection,
                    selection
                ),
                Rederivation::Refuse(_)
            ),
            "a key difference refuses"
        );
        assert!(
            matches!(
                compare_rederivation(&runner, &runner, selection, br#"{"tests":["b"]}"#),
                Rederivation::Refuse(_)
            ),
            "any selection difference refuses"
        );
        let base_changed = manifest("h1", "n1", "k");
        let mut tampered: serde_json::Value = serde_json::from_slice(&base_changed).expect("json");
        tampered["producer"]["base_sha"] = json!("other");
        assert!(
            matches!(
                compare_rederivation(
                    &runner,
                    tampered.to_string().as_bytes(),
                    selection,
                    selection
                ),
                Rederivation::Refuse(_)
            ),
            "a producer field outside the run-specific set still refuses"
        );
    }

    /// The same committed record directory the project's key code hashes in
    /// its own test (`executable_keys.record_digest_bytes`), with the value it
    /// prints: the two implementations agree byte for byte, or a keyed plan's
    /// bound record digest could never equal the runner's.
    #[test]
    fn the_record_digest_matches_the_key_codes_value_for_the_shared_fixture() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/reuse-record-digest");
        assert_eq!(
            record_digest(&fixture).expect("digest"),
            "e35428c1f78bd1c222fcb5efa8b0ecfd07d575c9712ae59254e3ec60922288fb"
        );
    }

    #[test]
    fn a_plan_binds_every_candidate_newest_first_and_leaves_the_toolchain_to_the_lane() {
        use crate::reuse_record_store::{create_pending, file};
        use serde_json::json;
        let store = tempfile::tempdir().expect("store");
        let derivation = tempfile::tempdir().expect("derivation");
        let mut policy = valid();
        policy.base_record.require.clear();
        let put = |commit: &str, platform: &str, toolchain: &str| {
            let pending =
                create_pending(store.path(), commit, chrono::Utc::now()).expect("pending");
            let job = json!({"platform": platform, "toolchain": {"digest": toolchain}});
            std::fs::write(pending.join("job.json"), job.to_string()).expect("job");
            file(store.path(), &pending, commit).expect("file");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        put("old", "darwin-arm64", "tc1");
        put("linux", "linux-x86_64", "tc1");
        put("new", "darwin-arm64", "tc2");
        let request = BindRequest {
            store: store.path(),
            derivation_root: derivation.path(),
            head_sha: "head",
            policy_digest: "pd",
            build_dir: "build",
        };
        let read = |_: &str| -> Result<Vec<u8>, String> { Ok(b"code".to_vec()) };
        let platform = |name: &'static str| {
            move |_: &std::path::Path| -> Result<String, String> { Ok(name.to_owned()) }
        };
        let Binding::Bound(binding) =
            bind(&policy, &request, read, platform("darwin-arm64"), |_| true).expect("bind")
        else {
            panic!("expected a binding");
        };
        let commits: Vec<&str> = binding
            .candidates
            .iter()
            .map(|candidate| candidate.commit.as_str())
            .collect();
        assert_eq!(commits, ["new", "old"], "both toolchains, newest first");
        for candidate in &binding.candidates {
            assert!(candidate.run_id.starts_with(&candidate.commit));
            assert_eq!(
                candidate.record_sha256,
                record_digest(std::path::Path::new(&candidate.record_path)).expect("digest")
            );
        }
        let digests: Vec<&str> = binding
            .candidates
            .iter()
            .map(|candidate| candidate.record_sha256.as_str())
            .collect();
        assert_eq!(binding.sample_seed, sample_seed("head", "pd", &digests));
        assert_eq!(
            binding.rules_digest,
            sha256_hex(&serde_json::to_vec(&policy.base_record).expect("rules"))
        );
        assert_eq!(binding.build_dir, "build");
        let Binding::NoBase(why) =
            bind(&policy, &request, read, platform("windows-x86_64"), |_| {
                true
            })
            .expect("bind")
        else {
            panic!("expected no base");
        };
        assert!(why.contains("3 other platform"), "{why}");
    }

    #[test]
    fn the_audit_is_one_status_tagged_object_and_absent_when_unset() {
        let mut bound = ExecutableReuseBinding {
            candidates: Vec::new(),
            rules_digest: "r".repeat(64),
            derivation_code_dir: "/code".to_owned(),
            derivation_code_sha256: "d".repeat(64),
            sample_seed: "s".repeat(64),
            sample_percent: 5,
            build_dir: "build".to_owned(),
            audit: None,
        };
        let plain = serde_json::to_value(&bound).expect("json");
        assert!(
            plain.get("audit").is_none(),
            "older bindings stay byte-identical"
        );
        bound.audit = Some(AuditBinding::Staged {
            run_id: "42".to_owned(),
            audit_commit: "a".repeat(40),
            commits_behind: 3,
            report_sha256: "b".repeat(64),
        });
        assert_eq!(
            serde_json::to_value(&bound).expect("json")["audit"],
            serde_json::json!({"status": "staged", "run_id": "42", "audit_commit": "a".repeat(40),
                               "commits_behind": 3, "report_sha256": "b".repeat(64)})
        );
        bound.audit = Some(AuditBinding::None {
            reason: "no_clean_run".to_owned(),
        });
        assert_eq!(
            serde_json::to_value(&bound).expect("json")["audit"],
            serde_json::json!({"status": "none", "reason": "no_clean_run"})
        );
        let unknown: Result<AuditBinding, _> = serde_json::from_value(
            serde_json::json!({"status": "none", "reason": "x", "extra": 1}),
        );
        assert!(unknown.is_err(), "the shape is exact");
    }
}
