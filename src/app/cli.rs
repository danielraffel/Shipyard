use std::ffi::OsString;
use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::identity::RuntimeMode;

/// Top-level command line for Shipyard.
#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Cross-platform CI coordination for local, SSH, and cloud runners."
)]
pub(super) struct Cli {
    /// Emit structured JSON compatible with Shipyard's CLI contract.
    #[arg(long, global = true)]
    pub(super) json: bool,
    /// Runtime path mode. Defaults to production Shipyard paths; use
    /// `--mode isolated` for sandboxed validation.
    #[arg(long, global = true, value_enum, default_value_t = PathMode::Shipyard)]
    pub(super) mode: PathMode,
    /// Override the machine-global state root. Primarily for tests and
    /// explicit compatibility validation.
    #[arg(long, global = true, hide = true)]
    pub(super) state_dir: Option<PathBuf>,
    /// Override the machine-global config root. Primarily for tests and
    /// explicit compatibility validation.
    #[arg(long, global = true, hide = true)]
    pub(super) global_dir: Option<PathBuf>,
    /// Override the working directory used for git-branch-sensitive commands.
    #[arg(long, global = true, hide = true)]
    pub(super) cwd: Option<PathBuf>,
    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
    /// Internal external-writer lease guardian.
    #[command(name = "writer-domain-exec", hide = true)]
    WriterDomainExec {
        /// Protected path the external process may mutate.
        #[arg(long)]
        path: PathBuf,
        /// External command and arguments.
        #[arg(last = true, required = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Internal daemon-owned queue worker.
    #[command(name = "execution-worker", hide = true)]
    ExecutionWorker {
        /// Exact durable queue job identifier.
        #[arg(long)]
        job_id: String,
        /// Unique worker generation used to reject stale PID receipts.
        #[arg(long)]
        generation: String,
    },
    /// Print the resolved runtime paths for the selected mode.
    Paths,
    /// Show or bump a consumer repo's Shipyard version pin.
    Pin {
        /// Pin subcommand.
        #[command(subcommand)]
        command: PinCommand,
    },
    /// Qualify and pin immutable upstream dependencies.
    Dependency {
        /// Dependency family.
        #[command(subcommand)]
        command: DependencyCommand,
    },
    /// Inspect and switch project profiles and configuration.
    Config {
        /// Config subcommand. Defaults to `show`.
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Inspect CI routing profiles and repo runner-placement plans.
    Ci {
        /// CI subcommand.
        #[command(subcommand)]
        command: CiCommand,
    },
    /// Record, import, and inspect runner performance metrics.
    Metrics {
        /// Metrics subcommand.
        #[command(subcommand)]
        command: Box<MetricsCommand>,
    },
    /// Inspect and move Shipyard GitHub auth config without secrets.
    Auth {
        /// Auth subcommand.
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Configure Shipyard for the current project.
    Init {
        /// Show detected config output; preserves Python's current write behavior.
        #[arg(long = "discover-only")]
        discover_only: bool,
    },
    /// Generate and check CHANGELOG.md from git tags.
    Changelog {
        /// Changelog subcommand.
        #[command(subcommand)]
        command: ChangelogCommand,
    },
    /// Manage branch protection for individual branches.
    Branch {
        /// Branch subcommand.
        #[command(subcommand)]
        command: BranchCommand,
    },
    /// Manage branch protection and governance profiles.
    Governance {
        /// Governance subcommand.
        #[command(subcommand)]
        command: GovernanceCommand,
    },
    /// Guided `RELEASE_BOT_TOKEN` provisioning and diagnosis.
    #[command(name = "release-bot")]
    ReleaseBot {
        /// Release-bot subcommand.
        #[command(subcommand)]
        command: ReleaseBotCommand,
    },
    /// Show queue, active runs, and recent results.
    Status,
    /// Show last-good-SHA evidence per target.
    Evidence {
        /// Evidence subcommand.
        #[command(subcommand)]
        command: Option<EvidenceCommand>,
        /// Branch to inspect. Defaults to current git branch or main.
        branch: Option<String>,
    },
    /// Show logs from a run.
    Logs {
        /// Job identifier.
        job_id: String,
        /// Show logs for a specific target.
        #[arg(short, long)]
        target: Option<String>,
    },
    /// Cancel a pending or running job.
    Cancel {
        /// Job identifier.
        job_id: String,
        /// Durable operator/controller reason recorded on the cancelled job.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Change the priority of a pending job.
    Bump {
        /// Job identifier.
        job_id: String,
        /// New priority.
        priority: QueuePriority,
    },
    /// Show all jobs in the queue.
    Queue,
    /// Observe GitHub pull requests and merge queue, emitting only transitions.
    #[command(name = "queue-observe")]
    QueueObserve {
        /// Owner/repo slug. Defaults to the current checkout's repo.
        #[arg(long)]
        repo: Option<String>,
        /// Base branch whose pull requests and merge queue should be observed.
        #[arg(long, default_value = "main")]
        base: String,
        /// Continue with adaptive 15/30/60/120/300-second polling.
        #[arg(long)]
        follow: bool,
        /// Override the durable canonical-state path.
        #[arg(long = "state-file")]
        state_file: Option<PathBuf>,
        /// Override the append-only transition-log path.
        #[arg(long = "transition-log")]
        transition_log: Option<PathBuf>,
        /// Replay a JSON fixture file or directory instead of querying GitHub.
        #[arg(long)]
        replay: Option<PathBuf>,
        /// Stop follow mode after this many polls. Test and supervised-run hook.
        #[arg(
            long = "max-polls",
            hide = true,
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        max_polls: Option<u64>,
    },
    /// Plan a fail-closed exact-head changed-surface test selection in shadow mode.
    #[command(name = "changed-surface-plan")]
    ChangedSurfacePlan {
        /// Target whose base-owned selector declaration should be evaluated.
        #[arg(long)]
        target: String,
        /// Pull request whose authenticated exact head is checked against this checkout.
        #[arg(long)]
        pr: u64,
        /// Owner/repo slug. Defaults to the current checkout's repository.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Verify one exact-head changed-surface shadow-comparison trial.
    #[command(name = "changed-surface-trial-status")]
    ChangedSurfaceTrialStatus {
        /// Canonical owner/repo identity recorded by the activation plan.
        #[arg(long = "repo")]
        repository: String,
        /// Pull request whose exact-head shadow result is inspected.
        #[arg(long = "pr")]
        pull_request: u64,
        /// Target whose shadow-comparison receipt is inspected.
        #[arg(long)]
        target: String,
        /// Exact 40-character lowercase pull-request head SHA.
        #[arg(long = "head")]
        head_sha: String,
    },
    /// Clean up old logs, bundles, evidence, and optional ship-state.
    Cleanup {
        /// Indefinitely pin one job's logs for incident/audit preservation.
        #[arg(long, value_name = "JOB_ID")]
        pin: Option<String>,
        /// Show what would be cleaned up.
        #[arg(long = "dry-run", action = ArgAction::SetTrue, default_value_t = true)]
        dry_run: bool,
        /// Actually delete files.
        #[arg(long)]
        apply: bool,
        /// Also prune aged ship-state files.
        #[arg(long = "ship-state")]
        ship_state: bool,
    },
    /// List, add, remove, and test validation targets.
    Targets {
        /// Targets subcommand. Defaults to `list`.
        #[command(subcommand)]
        command: Option<TargetsCommand>,
    },
    /// Manage the flaky-target quarantine list.
    Quarantine {
        /// Quarantine subcommand. Defaults to `list`.
        #[command(subcommand)]
        command: Option<QuarantineCommand>,
    },
    /// Check environment, dependencies, targets, and effective GitHub auth.
    Doctor {
        /// Additionally dispatch auto-release.yml to verify the release-bot chain.
        #[arg(long = "release-chain")]
        release_chain: bool,
        /// Probe configured non-local runner targets for reachability.
        #[arg(long)]
        runners: bool,
        /// Probe the effective GitHub auth source plus REST and GraphQL
        /// rate-limit buckets. Shows whether Shipyard is using ambient `gh`,
        /// an env token, or a command helper such as a GitHub App installation.
        #[arg(long = "rate-limit")]
        rate_limit: bool,
    },
    /// Validate current HEAD on configured targets.
    Run {
        /// Run subcommand.
        #[command(subcommand)]
        command: Option<RunSubcommand>,
        /// Comma-separated target names. Defaults to all configured targets.
        #[arg(long)]
        targets: Option<String>,
        /// Use smoke validation mode.
        #[arg(long)]
        smoke: bool,
        /// Skip remaining targets after the first failure.
        #[arg(long = "fail-fast")]
        fail_fast: bool,
        /// Resume validation from a specific stage.
        #[arg(long = "resume-from")]
        resume_from: Option<String>,
        /// Allow running outside the checkout that owns the config.
        #[arg(long = "allow-root-mismatch")]
        allow_root_mismatch: bool,
        /// Continue even when preflight cannot reach a backend.
        #[arg(long = "allow-unreachable-targets")]
        allow_unreachable_targets: bool,
        /// Continue even when this host has not converged to the declared
        /// fleet epoch.
        #[arg(long = "allow-fleet-epoch-drift")]
        allow_fleet_epoch_drift: bool,
        /// Skip a target after preflight.
        #[arg(long = "skip-target")]
        skip_targets: Vec<String>,
        /// Disable warm-pool reuse for this invocation.
        #[arg(long = "no-warm")]
        no_warm: bool,
        /// Suppress the staged working-tree drift guard.
        #[arg(long = "allow-tree-drift")]
        allow_tree_drift: bool,
        /// Execute in this terminal for debugging instead of daemon ownership.
        #[arg(long)]
        foreground: bool,
    },
    /// Run configured validation targets for a PR, creating one when omitted.
    Ship {
        /// Pull request number. Omit to find or create a PR for the current branch.
        #[arg(long)]
        pr: Option<u64>,
        /// Base branch recorded in ship-state.
        #[arg(long, default_value = "main")]
        base: String,
        /// Create missing develop/* or release/* base branches before opening a PR.
        #[arg(
            long = "auto-create-base",
            action = ArgAction::SetTrue,
            conflicts_with = "no_auto_create_base"
        )]
        auto_create_base: bool,
        /// Do not create missing base branches automatically.
        #[arg(long = "no-auto-create-base", action = ArgAction::SetTrue)]
        no_auto_create_base: bool,
        /// Disable warm-pool reuse for this invocation.
        #[arg(long = "no-warm")]
        no_warm: bool,
        /// Resume validation from a specific stage.
        #[arg(long = "resume-from")]
        resume_from: Option<String>,
        /// Continue even when preflight cannot reach a backend.
        #[arg(long = "allow-unreachable-targets")]
        allow_unreachable_targets: bool,
        /// Continue even when this host has not converged to the declared
        /// fleet epoch.
        #[arg(long = "allow-fleet-epoch-drift")]
        allow_fleet_epoch_drift: bool,
        /// Skip a target after preflight.
        #[arg(long = "skip-target")]
        skip_targets: Vec<String>,
        /// Adopt the current head SHA when recorded ship-state drifted (amend /
        /// force-push), re-validating the new head instead of failing on
        /// SHA drift (Shipyard #346).
        #[arg(long = "adopt-head")]
        adopt_head: bool,
        /// Execute in this terminal for debugging instead of daemon ownership.
        #[arg(long)]
        foreground: bool,
    },
    /// One-shot push-a-PR: skill-sync, version-bump, then ship.
    Pr {
        /// Base branch to ship into.
        #[arg(long, default_value = "main")]
        base: String,
        /// Run `version_bump_check.py` in apply mode.
        #[arg(long = "apply-bumps", default_value_t = true, action = ArgAction::SetTrue)]
        apply_bumps: bool,
        /// Run `version_bump_check.py` in report mode.
        #[arg(long = "no-apply-bumps")]
        no_apply_bumps: bool,
        /// Continue even when preflight cannot reach a backend.
        #[arg(long = "allow-unreachable-targets")]
        allow_unreachable_targets: bool,
        /// Continue even when this host has not converged to the declared
        /// fleet epoch.
        #[arg(long = "allow-fleet-epoch-drift")]
        allow_fleet_epoch_drift: bool,
        /// Skip a target after preflight.
        #[arg(long = "skip-target")]
        skip_targets: Vec<String>,
        /// Add a Version-Bump skip trailer for a surface.
        #[arg(long = "skip-bump", value_name = "SURFACE")]
        skip_bump: Vec<String>,
        /// Reason used with --skip-bump.
        #[arg(long = "bump-reason")]
        bump_reason: Option<String>,
        /// Add a Skill-Update skip trailer for a skill.
        #[arg(long = "skip-skill-update", value_name = "SKILL")]
        skip_skill_update: Vec<String>,
        /// Reason used with --skip-skill-update.
        #[arg(long = "skill-reason")]
        skill_reason: Option<String>,
        /// Adopt the current head SHA when recorded ship-state drifted (amend /
        /// force-push), re-validating the new head instead of failing on
        /// SHA drift (Shipyard #346).
        #[arg(long = "adopt-head")]
        adopt_head: bool,
        /// Durable workstream identifier for an atomic merge-steward handoff.
        /// Also enables handoff when the project default is disabled.
        #[arg(long = "workstream-id", conflicts_with = "no_steward_handoff")]
        workstream_id: Option<String>,
        /// Durable context URL for the steward receipt. Defaults to the PR URL.
        #[arg(long = "context-url", conflicts_with = "no_steward_handoff")]
        context_url: Option<String>,
        /// Disable a project-configured automatic steward handoff.
        #[arg(long = "no-steward-handoff", action = ArgAction::SetTrue)]
        no_steward_handoff: bool,
    },
    /// Cloud runner operations.
    Cloud {
        /// Cloud subcommand.
        #[command(subcommand)]
        command: CloudCommand,
    },
    /// One-shot rescue for wedged-runner recovery: cancel + redispatch every
    /// stuck workflow run on a PR (or the whole repo) to a different provider.
    Rescue(RescueArgs),
    /// Update the locally-installed Shipyard CLI from a published GitHub Release.
    Update(UpdateArgs),
    /// Merge a PR once all ship-state targets are green.
    #[command(name = "auto-merge")]
    AutoMerge {
        /// Pull request number.
        pr: u64,
        /// Merge strategy passed to `gh pr merge`.
        #[arg(long = "merge-method", value_enum, default_value_t = MergeMethod::Squash)]
        merge_method: MergeMethod,
        /// Delete the head branch on successful merge.
        #[arg(long = "delete-branch", default_value_t = true)]
        delete_branch: bool,
        /// Preserve the head branch on successful merge.
        #[arg(long = "no-delete-branch")]
        no_delete_branch: bool,
        /// Pass `--admin` through to `gh pr merge`.
        #[arg(long)]
        admin: bool,
        /// Hidden test hook to bypass `gh pr view` for archived PR checks.
        #[arg(long, hide = true)]
        pr_snapshot_file: Option<PathBuf>,
        /// Hidden test hook to replace `gh pr merge` with a local command.
        #[arg(long, hide = true)]
        merge_command: Option<PathBuf>,
        /// Hidden test hook to force a merge result without shelling out.
        #[arg(long, hide = true, value_enum)]
        merge_result: Option<MergeResult>,
    },
    /// Run the live-mode IPC broker and ship-state fast path.
    Daemon {
        /// Daemon subcommand.
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Inspect or change this machine's merge-queue mutation hold.
    #[command(name = "merge-queue")]
    MergeQueue {
        /// Merge-queue control subcommand.
        #[command(subcommand)]
        command: MergeQueueCommand,
    },
    /// Wait for a GitHub condition to match.
    Wait {
        /// Wait subcommand.
        #[command(subcommand)]
        command: WaitCommand,
    },
    /// Live view of an in-flight ship.
    Watch {
        /// Watch subcommand. Omit for PR ship-state watch.
        #[command(subcommand)]
        command: Option<WatchSubcommand>,
        /// PR number to watch. Defaults to the active ship for the current branch.
        #[arg(long)]
        pr: Option<u64>,
        /// Keep polling until the ship reaches a terminal state.
        #[arg(long)]
        follow: bool,
        /// Render one snapshot and exit.
        #[arg(long = "no-follow")]
        no_follow: bool,
        /// Seconds between refreshes when `--follow`.
        #[arg(long, default_value_t = 5.0)]
        interval: f64,
    },
    /// Inspect durable in-flight ship-state records.
    #[command(name = "ship-state")]
    ShipState {
        /// Ship-state subcommand.
        #[command(subcommand)]
        command: ShipStateCommand,
    },
    /// Self-hosted runner watchdog: detect and recover stuck runner state.
    Runner {
        /// Runner subcommand.
        #[command(subcommand)]
        command: RunnerCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum EvidenceCommand {
    /// Show command-evidence bundles produced by `shipyard run command`.
    Command {
        /// Evidence id. Defaults to the most recent command-evidence bundle.
        id: Option<String>,
        /// Show all command-evidence bundle summaries.
        #[arg(long)]
        list: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum MergeQueueCommand {
    /// Show whether queue mutations are centrally held on this machine.
    Status,
    /// Block every Shipyard merge-queue mutation before GitHub is contacted.
    Hold {
        /// Human-readable incident or maintenance reason.
        #[arg(long)]
        reason: String,
    },
    /// Remove the local hold. Machine-authority checks still apply.
    Resume,
    /// Resolve an uncertain mutation after authoritative GitHub reconciliation.
    Resolve {
        /// Correlation id shown by `merge-queue status --json`.
        correlation_id: String,
        /// Authoritative result: accepted or rejected.
        #[arg(long)]
        outcome: String,
        /// Human-readable reconciliation evidence.
        #[arg(long)]
        reason: String,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum CiCommand {
    /// Inspect CI routing profiles.
    Profile {
        /// Profile subcommand.
        #[command(subcommand)]
        command: CiProfileCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum CiProfileCommand {
    /// Print a profile file.
    Show {
        /// Profile name.
        name: String,
        /// Explicit profile TOML path. Defaults to .tartci/<name>.toml,
        /// .shipyard/ci-profiles/<name>.toml, then ci-profiles/<name>.toml.
        #[arg(long = "profile-file")]
        profile_file: Option<PathBuf>,
    },
    /// Produce a read-only plan of concrete GitHub variables/selectors.
    Plan {
        /// Profile name.
        name: String,
        /// Owner/repo slug.
        #[arg(long)]
        repo: String,
        /// Explicit profile TOML path. Defaults to .tartci/<name>.toml,
        /// .shipyard/ci-profiles/<name>.toml, then ci-profiles/<name>.toml.
        #[arg(long = "profile-file")]
        profile_file: Option<PathBuf>,
    },
    /// Proof-gate a profile's lanes and write the GitHub routing variables.
    ///
    /// Dry-run by default: prints every gate and what would be written, then
    /// stops. Pass --apply to actually write. A lane that fails any gate is
    /// never written, with or without --apply.
    Apply {
        /// Profile name.
        name: String,
        /// Owner/repo slug.
        #[arg(long)]
        repo: String,
        /// Profile context to apply: `pr`, `merge_group`, `release`, `coverage`.
        #[arg(long)]
        context: String,
        /// Actually write the variables. Without it, nothing is mutated.
        #[arg(long)]
        apply: bool,
        /// How stale dispatch evidence may be, in days.
        #[arg(long = "max-evidence-age-days", default_value_t = crate::profile_apply::DEFAULT_EVIDENCE_MAX_AGE_DAYS)]
        max_evidence_age_days: u32,
        /// Topology checker to run. Defaults to the repo's standard path.
        #[arg(long = "topology-check")]
        topology_check: Option<PathBuf>,
        /// Explicit profile TOML path. Defaults to .tartci/<name>.toml,
        /// .shipyard/ci-profiles/<name>.toml, then ci-profiles/<name>.toml.
        #[arg(long = "profile-file")]
        profile_file: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum MetricsCommand {
    /// Record one explicit step/job timing sample.
    Record(Box<MetricsRecordArgs>),
    /// Import metrics from an external source.
    Import {
        /// Import source.
        #[command(subcommand)]
        source: MetricsImportCommand,
    },
    /// List recent job rows.
    List(MetricsListArgs),
    /// Summarize p50/p90/min/max/failure-rate by project,target,backend,host.
    Summary(MetricsProjectArgs),
    /// Show slowest successful jobs.
    Slowest(MetricsListArgs),
    /// Compare before/after timing windows.
    Compare(MetricsCompareArgs),
    /// Show simple trend rows.
    Trend(MetricsListArgs),
    /// Emit agent-oriented drift findings.
    Watch(MetricsWatchArgs),
    /// Emit agent-oriented placement advice.
    Advise(MetricsAdviseArgs),
}

#[derive(Debug, Subcommand)]
pub(crate) enum MetricsImportCommand {
    /// Import tartci runtime export JSON/JSONL.
    Tartci(MetricsImportTartciArgs),
    /// Import GitHub Actions jobs for recent workflow runs.
    Github(MetricsImportGithubArgs),
}

#[derive(Debug, Args)]
pub(crate) struct MetricsRecordArgs {
    /// Project key, for example `pulp`.
    #[arg(long)]
    pub(crate) project: String,
    /// Owner/repo slug.
    #[arg(long)]
    pub(crate) repo: Option<String>,
    /// Git branch.
    #[arg(long)]
    pub(crate) branch: Option<String>,
    /// Git commit SHA.
    #[arg(long)]
    pub(crate) sha: Option<String>,
    /// Pull request number.
    #[arg(long)]
    pub(crate) pr: Option<i64>,
    /// Workflow name.
    #[arg(long)]
    pub(crate) workflow: Option<String>,
    /// Routing profile name.
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// Routing decision, for example primary/fallback/forced.
    #[arg(long = "routing-decision")]
    pub(crate) routing_decision: Option<String>,
    /// Job/lane name.
    #[arg(long)]
    pub(crate) job: String,
    /// Target/lane key.
    #[arg(long)]
    pub(crate) target: Option<String>,
    /// Platform, for example macos/linux/windows.
    #[arg(long)]
    pub(crate) platform: Option<String>,
    /// Backend, for example local/cloud/vm/ssh.
    #[arg(long)]
    pub(crate) backend: Option<String>,
    /// Provider, for example github-hosted/tart-macos/qemu-windows.
    #[arg(long)]
    pub(crate) provider: Option<String>,
    /// Runner or machine name.
    #[arg(long)]
    pub(crate) runner: Option<String>,
    /// Host name.
    #[arg(long)]
    pub(crate) host: Option<String>,
    /// Step name. Defaults to `total`.
    #[arg(long)]
    pub(crate) step: Option<String>,
    /// Duration in milliseconds.
    #[arg(long = "duration-ms", conflicts_with = "duration")]
    pub(crate) duration_ms: Option<i64>,
    /// Duration with units, for example `18423ms` or `18.4s`.
    #[arg(long)]
    pub(crate) duration: Option<String>,
    /// Status, for example pass/fail/success/failure.
    #[arg(long, default_value = "pass")]
    pub(crate) status: String,
    /// Process exit code.
    #[arg(long = "exit-code")]
    pub(crate) exit_code: Option<i64>,
    /// Failure class when status is not healthy.
    #[arg(long = "failure-class")]
    pub(crate) failure_class: Option<String>,
    /// External dedupe key, for example github:run/job/attempt.
    #[arg(long = "external-id")]
    pub(crate) external_id: Option<String>,
    /// RFC3339 start timestamp.
    #[arg(long = "started-at")]
    pub(crate) started_at: Option<String>,
    /// RFC3339 completion timestamp.
    #[arg(long = "completed-at")]
    pub(crate) completed_at: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct MetricsImportTartciArgs {
    /// Read tartci runtime export JSON/JSONL from this file. Omit or pass `-` for stdin.
    #[arg(long)]
    pub(crate) file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct MetricsImportGithubArgs {
    /// Owner/repo slug.
    #[arg(long)]
    pub(crate) repo: String,
    /// Project key. Defaults to the repo name.
    #[arg(long)]
    pub(crate) project: Option<String>,
    /// Workflow filename or id to list runs for.
    #[arg(long)]
    pub(crate) workflow: Option<String>,
    /// Branch/ref filter.
    #[arg(long)]
    pub(crate) branch: Option<String>,
    /// Number of recent runs to import.
    #[arg(long, default_value_t = 10)]
    pub(crate) limit: u32,
}

#[derive(Debug, Args)]
pub(crate) struct MetricsProjectArgs {
    /// Project key.
    #[arg(long)]
    pub(crate) project: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct MetricsListArgs {
    /// Project key.
    #[arg(long)]
    pub(crate) project: Option<String>,
    /// Maximum rows.
    #[arg(long, default_value_t = 20)]
    pub(crate) limit: usize,
}

#[derive(Debug, Args)]
pub(crate) struct MetricsCompareArgs {
    /// Project key.
    #[arg(long)]
    pub(crate) project: String,
    /// Optional lane key for agent context.
    #[arg(long)]
    pub(crate) lane: Option<String>,
    /// Before window, for example `7d`.
    #[arg(long)]
    pub(crate) before: Option<String>,
    /// After window, for example `7d`.
    #[arg(long)]
    pub(crate) after: Option<String>,
    /// Split point in days ago. Older rows are before; newer rows are after.
    #[arg(long = "split-days-ago", default_value_t = 7)]
    pub(crate) split_days_ago: i64,
}

#[derive(Debug, Args)]
pub(crate) struct MetricsWatchArgs {
    /// Project key.
    #[arg(long)]
    pub(crate) project: String,
    /// Recent window, for example `14d`.
    #[arg(long = "since", default_value = "14d")]
    pub(crate) since: String,
}

#[derive(Debug, Args)]
pub(crate) struct MetricsAdviseArgs {
    /// Project key.
    #[arg(long)]
    pub(crate) project: String,
    /// Profile name used by the caller; included for agent context.
    #[arg(long)]
    pub(crate) profile: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(super) enum RunSubcommand {
    /// Run an arbitrary command on one local or POSIX SSH target and store typed evidence.
    Command(RunCommandEvidenceArgs),
}

#[derive(Debug, Args)]
pub(super) struct RunCommandEvidenceArgs {
    /// Target name from `[targets.<name>]`.
    #[arg(long)]
    pub(super) target: String,
    /// Stable evidence name. Defaults to the target name.
    #[arg(long)]
    pub(super) name: Option<String>,
    /// Expected process exit code.
    #[arg(long = "expect-code", default_value_t = 0)]
    pub(super) expect_code: i32,
    /// Override the target working directory.
    #[arg(long = "target-cwd")]
    pub(super) target_cwd: Option<String>,
    /// Artifact glob relative to the target working directory. May be repeated.
    #[arg(long = "artifact")]
    pub(super) artifacts: Vec<String>,
    /// Local log file path. Defaults under Shipyard state logs.
    #[arg(long = "log-path")]
    pub(super) log_path: Option<PathBuf>,
    /// Wall-clock timeout in seconds.
    #[arg(long = "timeout-secs")]
    pub(super) timeout_secs: Option<u64>,
    /// Environment variable name to fingerprint without recording its value. May be repeated.
    #[arg(long = "env-fingerprint")]
    pub(super) env_fingerprints: Vec<String>,
    /// Command and arguments to execute after `--`.
    #[arg(required = true, trailing_var_arg = true)]
    pub(super) command: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub(super) enum WatchSubcommand {
    /// Run and watch a command on a local or POSIX SSH target.
    Local(WatchLocalArgs),
}

#[derive(Debug, Args)]
pub(super) struct WatchLocalArgs {
    /// Configured Shipyard target to run on.
    #[arg(long)]
    pub(super) target: String,
    /// Shell command to run on the target.
    #[arg(long)]
    pub(super) command: String,
    /// Override the target work directory.
    #[arg(long)]
    pub(super) target_cwd: Option<String>,
    /// Regex emitted as a milestone event when it matches an output line.
    #[arg(long = "milestone-regex")]
    pub(super) milestone_regex: Vec<String>,
    /// Regex emitted as the terminal event and used to stop the command early.
    #[arg(long = "terminal-regex")]
    pub(super) terminal_regex: Vec<String>,
    /// Write the full target output to this log path.
    #[arg(long = "log-path")]
    pub(super) log_path: Option<PathBuf>,
    /// Stop the command after this many seconds.
    #[arg(long = "timeout-secs")]
    pub(super) timeout_secs: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub(super) enum RunnerCommand {
    /// Process durable semantic-recovery requests with the trusted first-line worker.
    RecoveryWorker {
        /// Inspect or process at most one pending request (the default).
        #[arg(long, conflicts_with = "drain")]
        once: bool,
        /// Process a bounded snapshot of all currently pending requests.
        #[arg(long, conflicts_with = "once")]
        drain: bool,
        /// Launch the configured worker and persist its terminal receipt.
        /// Without this flag, only inspect and exact-head-revalidate requests.
        #[arg(long)]
        apply: bool,
    },
    /// One-shot health check. Exit 0 healthy, 1 stuck, 2 offline.
    Status {
        /// Self-hosted runner ID, e.g. 1763. Defaults to `runner.watchdog.runner_id`.
        #[arg(long = "runner-id")]
        runner_id: Option<u64>,
        /// Owner/repo slug. Defaults to the current git repo.
        #[arg(long)]
        repo: Option<String>,
        /// Local actions-runner directory. Defaults to `runner.watchdog.runner_dir`
        /// or `$HOME/actions-runner`.
        #[arg(long = "runner-dir")]
        runner_dir: Option<PathBuf>,
        /// Warn when a Worker has been running longer than this many minutes.
        #[arg(long = "max-job-min")]
        max_job_min: Option<i64>,
        /// Flag queued runs older than this many hours.
        #[arg(long = "max-queue-age-hours")]
        max_queue_age_hours: Option<i64>,
    },
    /// Show or cancel stale queued runs older than the threshold.
    Cleanup {
        /// Show what would be cancelled without making changes.
        #[arg(long = "dry-run", action = ArgAction::SetTrue, default_value_t = true)]
        dry_run: bool,
        /// Cancel stale queued runs (overrides --dry-run).
        #[arg(long = "fix")]
        fix: bool,
        /// Stale-queue cutoff in hours.
        #[arg(long = "stale-hours")]
        stale_hours: Option<i64>,
        /// Owner/repo slug. Defaults to the current git repo.
        #[arg(long)]
        repo: Option<String>,
        /// Forcibly kill the oldest hung Worker process. Requires --fix and
        /// two confirmation prompts; never honoured when stdin is not a TTY
        /// unless --yes is also passed.
        #[arg(long = "force-kill")]
        force_kill: bool,
        /// Bypass the two interactive confirmations for --force-kill. Intended
        /// for tests; in production this still requires --force-kill.
        #[arg(long = "yes", hide = true)]
        yes: bool,
    },
    /// Watch loop. Polls every `watch_interval_seconds` until interrupted.
    Watch {
        /// Self-hosted runner ID. Defaults to `runner.watchdog.runner_id`.
        #[arg(long = "runner-id")]
        runner_id: Option<u64>,
        /// Owner/repo slug. Defaults to the current git repo.
        #[arg(long)]
        repo: Option<String>,
        /// Local actions-runner directory.
        #[arg(long = "runner-dir")]
        runner_dir: Option<PathBuf>,
        /// Polling cadence in seconds.
        #[arg(long)]
        interval: Option<u64>,
        /// Base branch monitored by the durable fleet-liveness tick. Defaults
        /// to `runner.watchdog.fleet_base`, then the repository default branch.
        #[arg(long = "fleet-base")]
        fleet_base: Option<String>,
        /// Auto-cancel stale queued runs.
        #[arg(long = "fix")]
        fix: bool,
        /// Auto-kill hung `Runner.Worker` processes (etime above the watchdog
        /// threshold) using the same recovery sequence as `runner kill`.
        /// Implies `--fix`.
        #[arg(long = "kill-hung-workers")]
        kill_hung_workers: bool,
        /// On every tick, cancel stale GitHub Actions workflow *runs* repo-wide:
        /// runs stuck `in_progress` past the in-progress max age (hung) and runs
        /// stuck `queued` past the queued max age (orphaned). The run-level
        /// complement to `--kill-hung-workers`.
        #[arg(long = "reap-stale-runs")]
        reap_stale_runs: bool,
        /// Cancel `in_progress` runs older than this many minutes (hung).
        /// Defaults to `runner.watchdog.reap_in_progress_max_min` or ~5h.
        #[arg(long = "reap-in-progress-max-min")]
        reap_in_progress_max_min: Option<i64>,
        /// Cancel `queued` runs older than this many minutes (orphaned).
        /// Defaults to `runner.watchdog.reap_queued_max_min` or ~8h.
        #[arg(long = "reap-queued-max-min")]
        reap_queued_max_min: Option<i64>,
        /// With `--reap-stale-runs`, log what would be cancelled without
        /// cancelling anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Maximum number of iterations to run before exiting. Defaults to
        /// looping forever. Test hook.
        #[arg(long = "max-iterations", hide = true)]
        max_iterations: Option<u32>,
        /// Override the SIGTERM-to-SIGKILL grace window in seconds for auto-kill. Test hook.
        #[arg(long = "kill-grace-secs", hide = true)]
        kill_grace_secs: Option<u64>,
    },
    /// Explicitly kill a hung `Runner.Worker` process with full recovery
    /// sequence: snapshot, SIGTERM with grace, SIGKILL, reap children,
    /// quarantine partial build, verify Runner.Listener, optional retrigger.
    Kill {
        /// Worker PID to kill. Required unless `--history` or `--recover`.
        #[arg(long = "pid")]
        pid: Option<u32>,
        /// Free-text reason for the kill, recorded in the recovery log.
        /// Required when `--pid` is set.
        #[arg(long = "reason")]
        reason: Option<String>,
        /// After kill, immediately re-queue the killed PR's CI via
        /// `gh api .../actions/runs/<id>/rerun-failed-jobs`.
        #[arg(long = "retrigger")]
        retrigger: bool,
        /// Skip the typed "KILL" confirmation prompt. Scripted use only.
        #[arg(long = "yes")]
        yes: bool,
        /// Owner/repo slug. Defaults to the current git repo.
        #[arg(long)]
        repo: Option<String>,
        /// Local actions-runner directory. Defaults to
        /// `runner.watchdog.runner_dir` or `$HOME/actions-runner`.
        #[arg(long = "runner-dir")]
        runner_dir: Option<PathBuf>,
        /// Print recent kill events from the recovery log and exit.
        #[arg(long = "history", conflicts_with_all = ["pid", "recover"])]
        history: bool,
        /// Limit history output to the most recent N entries.
        #[arg(long = "last")]
        last: Option<usize>,
        /// Recover a previously-quarantined build by kill-event ID.
        #[arg(long = "recover", conflicts_with_all = ["pid", "history"])]
        recover: Option<String>,
        /// Override the SIGTERM-to-SIGKILL grace window in seconds. Test hook.
        #[arg(long = "grace-secs", hide = true)]
        grace_secs: Option<u64>,
        /// Override the recovery log path. Test hook.
        #[arg(long = "recovery-log", hide = true)]
        recovery_log: Option<PathBuf>,
        /// Override the quarantine root directory. Test hook.
        #[arg(long = "quarantine-root", hide = true)]
        quarantine_root: Option<PathBuf>,
        /// Skip the post-kill GitHub status-flip poll. Test hook.
        #[arg(long = "no-wait-github", hide = true)]
        no_wait_github: bool,
    },
    /// Show or set this machine's runner tag (e.g. `studio`, `m1`, `m5`).
    /// The tag names runners `<repo>-<tag>-NN`; it is stored per-box in
    /// Shipyard state and is never derived from the hostname (two laptops
    /// can share a hostname, which would collide).
    Tag {
        /// New tag to store. Omit to print the current tag.
        #[arg(long)]
        set: Option<String>,
    },
    /// Register N self-hosted GitHub Actions runners on this machine for a
    /// repo. Names continue from the highest existing `<repo>-<tag>-NN` so
    /// re-running appends capacity without collisions.
    Register {
        /// Owner/repo slug. Defaults to the current git repo.
        #[arg(long)]
        repo: Option<String>,
        /// Number of runners to register.
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// Machine tag override. Defaults to the stored per-box tag.
        #[arg(long = "machine-tag")]
        machine_tag: Option<String>,
        /// Comma-separated labels override. Defaults to
        /// `self-hosted,macos,arm64,<repo>-build,<repo>-build-<tag>`.
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// CI root holding per-runner `_work` and shared caches. Defaults to
        /// `runner.provision.ci_root` or `$HOME/actions-ci`.
        #[arg(long = "ci-root")]
        ci_root: Option<PathBuf>,
        /// Print the plan without downloading, configuring, or starting.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// List self-hosted runners across repos, grouped by machine, reconciling
    /// local runner directories against GitHub to flag orphans.
    List {
        /// Owner/repo slug. Repeatable. Defaults to repos discovered from
        /// local `actions-runner-*` dirs plus the current repo.
        #[arg(long)]
        repo: Vec<String>,
        /// Query every repo with a local runner dir on this machine.
        #[arg(long = "all-repos")]
        all_repos: bool,
    },
    /// Audit runners for host-class naming/label drift (`<repo>-<class>-NN` +
    /// `<repo>-build` / `<repo>-build-<class>`). Exit 1 when any runner drifts.
    Audit {
        /// Owner/repo slug. Repeatable. Defaults to repos discovered from
        /// local `actions-runner-*` dirs plus the current repo.
        #[arg(long)]
        repo: Vec<String>,
    },
    /// Report VM-slot-aware free macOS capacity across `[host_class.*]` hosts
    /// (`Σ max(0, cap − running macOS Tart VMs)`). Exit 1 if any host is unreadable.
    Capacity,
    /// Read fleet capacity, tartci supervisor freshness, and queued macOS age.
    /// Exit 1 on unreadable hosts or queued-age-with-capacity alerts.
    FleetStatus {
        /// Owner/repo slug. Defaults to the current checkout's repo.
        #[arg(long)]
        repo: Option<String>,
        /// Base branch whose merge queue should be monitored.
        #[arg(long, default_value = "main")]
        base: String,
        /// Job-name substring used to identify macOS queued work.
        #[arg(long, default_value = "macos")]
        target: String,
        /// Alert when queued macOS work is older than this and a routable slot exists.
        #[arg(long = "queued-age-threshold-secs", default_value_t = 900)]
        queued_age_threshold_secs: i64,
        /// Maximum queued and in-progress workflow runs to inspect in total
        /// (bounded to 50) for queue and capacity-owner attribution.
        #[arg(long = "queue-run-limit", default_value_t = 100)]
        queue_run_limit: u32,
        /// Alert when the queue front has no required-check progress after this
        /// age while routable fleet capacity is idle.
        #[arg(long = "merge-queue-stall-threshold-secs", default_value_t = 900)]
        merge_queue_stall_threshold_secs: i64,
        /// Alert when releasable work, measured no earlier than publication, exceeds this age.
        #[arg(long = "release-stale-threshold-secs", default_value_t = 86400)]
        release_stale_threshold_secs: i64,
    },
    /// Roll one exact Shipyard release across configured host classes and
    /// refresh each daemon. Plans only unless `--apply` is supplied.
    #[command(name = "fleet-update")]
    FleetUpdate {
        /// Exact release tag to install on every host, for example v0.100.0.
        #[arg(long = "to")]
        to: String,
        /// Execute the rollout. Without this flag, emit the exact host plan.
        #[arg(long)]
        apply: bool,
    },
    /// Maintain an expiring repository-variable lease for an approved local
    /// Linux CI lane. Dry-run unless `--apply` is supplied.
    LocalLinuxLease {
        /// Owner/repo slug. Defaults to the current checkout's repo.
        #[arg(long)]
        repo: Option<String>,
        /// Checked-in CI routing profile name.
        #[arg(long, default_value = "normal-local-fast")]
        profile: String,
        /// Explicit profile path. Defaults to Shipyard's normal profile search.
        #[arg(long = "profile-file")]
        profile_file: Option<PathBuf>,
        /// Profile context containing the lease declaration.
        #[arg(long, default_value = "merge_group")]
        context: String,
        /// Profile lane containing the lease declaration.
        #[arg(long, default_value = "linux")]
        lane: String,
        /// Renew or clear the repository variable. Without this flag, print
        /// the decision without changing GitHub.
        #[arg(long)]
        apply: bool,
        /// Continue probing and renewing until interrupted.
        #[arg(long)]
        watch: bool,
        /// Seconds between watch ticks. Must be shorter than the lease TTL.
        #[arg(long = "interval-secs", default_value_t = 60)]
        interval_secs: u64,
        /// Stop after N ticks. Test hook.
        #[arg(long = "max-ticks", hide = true)]
        max_ticks: Option<u32>,
    },
    /// Prove that no superseded queued run can claim a just-in-time runner.
    ///
    /// Emits the flat, versioned verdict consumed by `TartCI` immediately before
    /// JIT registration. Apply mode may cancel only managed, exact-head-
    /// superseded workflow runs, and only on the configured mutation machine.
    AdmissionClean {
        /// Canonical owner/repo slug.
        #[arg(long)]
        repo: String,
        /// Base branch whose open pull requests and merge queue are authoritative.
        #[arg(long, default_value = "main")]
        base: String,
        /// Complete comma-separated label set the prospective runner will advertise.
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Cancel safely superseded compatible runs when this is the mutation authority.
        #[arg(long)]
        apply: bool,
    },
    /// Hand an exact pull-request head to the merge steward.
    ///
    /// Dry-run is the default. Apply mode writes a successful commit-status
    /// receipt on the expected head, then labels the PR as managed only after
    /// re-reading that the head is still exact.
    StewardHandoff {
        /// Owner/repo slug. Defaults to the current repository.
        #[arg(long)]
        repo: Option<String>,
        /// Pull-request number.
        #[arg(long)]
        pr: u64,
        /// Full immutable head SHA expected by the submitting agent.
        #[arg(long)]
        head: String,
        /// Durable work item identifier, such as GEN-7.
        #[arg(long = "workstream-id")]
        workstream_id: String,
        /// Durable context URL, such as a Linear issue or planning document.
        #[arg(long = "context-url")]
        context_url: Option<String>,
        /// Agent provider that owns the handed-off workstream. When omitted,
        /// Shipyard captures a supported provider from the current process.
        #[arg(long = "agent-provider", value_parser = ["codex", "claude"])]
        agent_provider: Option<String>,
        /// Durable provider session identifier used for a later targeted wake.
        #[arg(long = "agent-session-id")]
        agent_session_id: Option<String>,
        /// Optional parent/coordinator session for a swarmed workstream.
        #[arg(long = "agent-parent-session-id")]
        agent_parent_session_id: Option<String>,
        /// Optional cmux surface used only as a diagnosed fallback transport.
        #[arg(long = "agent-surface-id")]
        agent_surface_id: Option<String>,
        /// Private JSON `LaunchProfileV1` containing exact argv and provenance.
        /// Shipyard stores this contract but never executes it.
        #[arg(long = "launch-profile")]
        launch_profile: Option<PathBuf>,
        /// Declare that the owning session has a persistent goal which can be
        /// resumed when Shipyard returns actionable work.
        #[arg(long = "goal-managed")]
        goal_managed: bool,
        /// What the owner should do after monitoring transfers. Continue is the
        /// default; pause only when this PR is its remaining blocker.
        #[arg(long = "after-handoff", value_parser = ["continue", "pause"], default_value = "continue")]
        after_handoff: String,
        /// Explicitly transfer an existing exact-head receipt to a replacement
        /// provider session, incrementing its durable ownership generation.
        #[arg(long = "transfer-agent-owner")]
        transfer_agent_owner: bool,
        /// Write the receipt and managed label. Without this flag, only audit.
        #[arg(long)]
        apply: bool,
    },
    /// Audit and conservatively advance merge-on-green across repositories.
    ///
    /// Dry-run is the default. `--apply` may enqueue an exact green head,
    /// rerun a bounded transient failure, or cancel only queued runs with a
    /// provably superseded PR/merge-group head. Same-head duplicates are never
    /// cancelled. Repositories without a server-owned
    /// merge queue are reported as direct-merge refusals.
    Steward {
        /// Owner/repo slug. Repeatable; defaults to the current repository.
        #[arg(long)]
        repo: Vec<String>,
        /// Target branch.
        #[arg(long, default_value = "main")]
        base: String,
        /// Label that opts a PR out of stewardship.
        #[arg(long = "opt-out-label", default_value = "shipyard:no-auto-merge")]
        opt_out_label: String,
        /// Label that blocks stewardship authority while PR provenance is
        /// unresolved. Repeat to recognize additional repository vocabularies.
        #[arg(long = "provenance-blocking-label", default_value = "5·unresolved")]
        provenance_blocking_labels: Vec<String>,
        /// Maximum reruns of the same transiently-failed run on one exact head.
        #[arg(long = "max-transient-reruns", default_value_t = 1)]
        max_transient_reruns: u32,
        /// Restore queue-front priority after a narrowly proven GitHub-hosted
        /// pre-checkout infrastructure eviction. Disabled by default.
        #[arg(long = "recover-hosted-setup-eviction-priority")]
        recover_hosted_setup_eviction_priority: bool,
        /// Disable queued-run superseded-head cleanup.
        #[arg(long = "no-coalesce")]
        no_coalesce: bool,
        /// Disable bounded preemption of safe preamble-only capacity thieves.
        #[arg(long = "no-preempt-capacity")]
        no_preempt_capacity: bool,
        /// Maximum preemptions of one workflow on one immutable PR head.
        #[arg(long = "max-preemptions-per-head", default_value_t = 1)]
        max_preemptions_per_head: u32,
        /// Perform the planned mutations. Without this flag, only audit.
        #[arg(long)]
        apply: bool,
        /// Override the durable retry/audit ledger path. Test hook.
        #[arg(long = "ledger", hide = true)]
        ledger: Option<PathBuf>,
    },
    /// Watch for cloud-queued macOS jobs and drain them to a local runner when
    /// a VM slot frees up. Observe-only unless `--apply`.
    RerouteWatch {
        /// Owner/repo slug. Defaults to the current checkout's repo.
        #[arg(long)]
        repo: Option<String>,
        /// Lane/job-name substring passed to `cloud retarget --target`.
        #[arg(long, default_value = "macos")]
        target: String,
        /// Seconds between polling ticks.
        #[arg(long, default_value_t = 30)]
        interval: u64,
        /// Suppress re-routing the same PR within this many seconds.
        #[arg(long = "flap-window", default_value_t = 300)]
        flap_window: i64,
        /// Run a single tick and exit.
        #[arg(long)]
        once: bool,
        /// Stop after N ticks (mainly for testing).
        #[arg(long = "max-ticks")]
        max_ticks: Option<u32>,
        /// Actually perform reroutes (default: observe and log only).
        #[arg(long)]
        apply: bool,
    },
    /// Deregister a runner: stop its launchd service and remove it from GitHub.
    Remove {
        /// Runner name, e.g. `pulp-studio-03`.
        #[arg(long)]
        name: String,
        /// Owner/repo slug. Defaults to the current git repo.
        #[arg(long)]
        repo: Option<String>,
        /// Also delete the local `actions-runner-<name>` directory.
        #[arg(long = "purge-dir")]
        purge_dir: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub(super) enum ShipStateCommand {
    /// List active in-flight ship states.
    List,
    /// Show a full saved state for a PR.
    Show {
        /// Pull request number.
        pr: u64,
    },
    /// Archive the active state for a PR.
    Discard {
        /// Pull request number.
        pr: u64,
    },
    /// Re-fetch GitHub check state and heal stale dispatched runs.
    Reconcile {
        /// Pull request number.
        pr: Option<u64>,
        /// Reconcile every active ship-state file.
        #[arg(long = "all")]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum DaemonCommand {
    /// Start the daemon in the background by default.
    Start {
        /// Repo(s) to advertise from the daemon status endpoint.
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// Run in the foreground instead of spawning a child.
        #[arg(long = "no-detach")]
        no_detach: bool,
    },
    /// Run the daemon in the foreground.
    Run {
        /// Repo(s) to advertise from the daemon status endpoint.
        #[arg(long = "repo")]
        repos: Vec<String>,
    },
    /// Ask a running daemon to shut down.
    Stop,
    /// Stop any running daemon and start a fresh one.
    Refresh {
        /// Repo(s) to advertise from the fresh daemon status endpoint.
        #[arg(long = "repo")]
        repos: Vec<String>,
    },
    /// Report daemon liveness and status.
    Status,
}

#[derive(Debug, Subcommand)]
pub(super) enum PinCommand {
    /// Show the currently pinned Shipyard version.
    Show,
    /// Bump the pinned Shipyard version.
    Bump {
        /// Target Shipyard version tag. Defaults to latest release.
        #[arg(long = "to")]
        target: Option<String>,
        /// Leave the pin edit in the working tree without opening a PR.
        #[arg(long = "no-pr")]
        no_pr: bool,
        /// Skip install script and version verification.
        #[arg(long = "skip-verify")]
        skip_verify: bool,
        /// Allow target versions older than the installed global binary.
        #[arg(long = "allow-downgrade")]
        allow_downgrade: bool,
        /// Allow bump when origin/main already pins >= target.
        #[arg(long = "allow-redundant")]
        allow_redundant: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum DependencyCommand {
    /// Manage the tracked Pulp release dependency.
    Pulp {
        /// Pulp dependency operation.
        #[command(subcommand)]
        command: PulpDependencyCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum PulpDependencyCommand {
    /// Show the reviewed policy and current immutable lock without contacting GitHub.
    Show,
    /// Qualify the selected release and open an exact consumer pin pull request.
    Update {
        /// Write the lock in this checkout instead of opening a pull request.
        #[arg(long = "no-pr")]
        no_pr: bool,
    },
    /// Independently re-verify the tracked lock against GitHub release attestations.
    Verify,
}

#[derive(Debug, Subcommand)]
pub(super) enum ConfigCommand {
    /// Print the effective merged configuration as JSON.
    Show,
    /// List defined profiles and which one is active.
    Profiles,
    /// Switch the active project profile.
    Use {
        /// Profile name to activate.
        profile_name: String,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum AuthCommand {
    /// Show the effective GitHub auth source Shipyard will use.
    Doctor,
    /// Export sanitized GitHub auth config without tokens or private keys.
    Export {
        /// Write the bundle to a file instead of stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Import sanitized GitHub auth config without moving secrets.
    Import {
        /// Bundle previously produced by `shipyard auth export`.
        input: PathBuf,
        /// Destination config layer.
        #[arg(long, value_enum, default_value_t = AuthConfigScope::Local)]
        scope: AuthConfigScope,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum AuthConfigScope {
    /// Machine-global config.
    Global,
    /// Tracked project config.
    Project,
    /// Per-project local overlay config.
    Local,
}

#[derive(Debug, Subcommand)]
pub(super) enum ChangelogCommand {
    /// Rebuild CHANGELOG.md from the configured tag graph.
    Regenerate {
        /// Exit 1 if the rendered changelog differs from the file.
        #[arg(long)]
        check: bool,
        /// Print release notes for TAG to stdout instead of writing the file.
        #[arg(long = "release-notes")]
        release_notes_tag: Option<String>,
        /// Print the rendered changelog to stdout instead of writing it.
        #[arg(long)]
        stdout: bool,
    },
    /// Alias for `changelog regenerate --check`.
    Check,
    /// Scaffold release changelog config.
    Init {
        /// Human-facing product name. Defaults to project.name or directory name.
        #[arg(long)]
        product: Option<String>,
        /// GitHub repo URL. Auto-detected from origin if omitted.
        #[arg(long = "repo-url")]
        repo_url: Option<String>,
        /// Overwrite an existing [release.changelog] section.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum BranchCommand {
    /// Apply declared governance rules to one branch, optionally creating it first.
    Apply {
        /// Create this branch from --base when it does not already exist.
        #[arg(long = "create")]
        create_name: Option<String>,
        /// Base branch used when --create is present.
        #[arg(long = "base", default_value = "main")]
        base_branch: String,
        /// Existing branch to apply rules to.
        target_branch: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum GovernanceCommand {
    /// Report declared-vs-live governance drift per branch.
    Status {
        /// Branches to check. Defaults to main.
        #[arg(long = "branch", short = 'b')]
        branches: Vec<String>,
    },
    /// Apply declared governance rules to live state.
    Apply {
        /// Branches to apply. Defaults to main.
        #[arg(long = "branch", short = 'b')]
        branches: Vec<String>,
        /// Show what would change without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Apply rules from a snapshot file instead of project config.
        #[arg(long = "from")]
        from_path: Option<PathBuf>,
    },
    /// Show what governance apply would change.
    Diff {
        /// Branches to check. Defaults to main.
        #[arg(long = "branch", short = 'b')]
        branches: Vec<String>,
    },
    /// Snapshot live GitHub governance state to TOML.
    Export {
        /// Branches to snapshot. Defaults to main.
        #[arg(long = "branch", short = 'b')]
        branches: Vec<String>,
        /// Write snapshot to file instead of stdout.
        #[arg(long = "output", short = 'o')]
        output: Option<PathBuf>,
    },
    /// Switch governance profile and apply.
    Use {
        /// Profile to activate: solo, multi, or custom.
        profile_name: String,
        /// Skip the interactive prompt.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
        /// Show the diff without applying or rewriting config.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum ReleaseBotCommand {
    /// Report `RELEASE_BOT_TOKEN` presence, drift, and recent failures.
    Status {
        /// Other repos to probe for `RELEASE_BOT_TOKEN`.
        #[arg(long = "siblings", value_name = "OWNER/REPO")]
        siblings: Vec<String>,
    },
    /// Store `RELEASE_BOT_TOKEN` and optionally verify the release chain.
    Setup {
        /// Use this PAT name instead of the per-project default.
        #[arg(long = "shared-name")]
        shared_name: Option<String>,
        /// Skip the wizard text and paste a token value you already have.
        #[arg(long = "paste")]
        paste: bool,
        /// Other repos to probe for an existing `RELEASE_BOT_TOKEN`.
        #[arg(long = "siblings", value_name = "OWNER/REPO")]
        siblings: Vec<String>,
        /// Dispatch auto-release.yml after setting the secret.
        #[arg(long = "verify", action = ArgAction::SetTrue, default_value_t = true)]
        verify: bool,
        /// Store the secret without dispatching auto-release.yml.
        #[arg(long = "no-verify")]
        no_verify: bool,
        /// Treat the secret as unset even if present.
        #[arg(long = "reconfigure")]
        reconfigure: bool,
    },
    /// Install and run the post-tag docs-sync workflow.
    Hook {
        /// Hook subcommand.
        #[command(subcommand)]
        command: ReleaseBotHookCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum ReleaseBotHookCommand {
    /// Drop .github/workflows/post-tag-sync.yml into the consumer repo.
    Install {
        /// Glob of tags that should trigger the workflow.
        #[arg(long = "tag-pattern")]
        tag_pattern: Option<String>,
        /// Pinned Shipyard version the workflow installs.
        #[arg(long = "shipyard-version")]
        shipyard_version: Option<String>,
    },
    /// Execute the configured `release.post_tag_hook` for a tag.
    Run {
        /// Tag to sync. Defaults from `GITHUB_REF=refs/tags/<tag>`.
        #[arg(long = "tag")]
        tag: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum QueuePriority {
    Low,
    Normal,
    High,
}

#[derive(Debug, Subcommand)]
pub(super) enum TargetsCommand {
    /// List configured targets with reachability status.
    List,
    /// Probe a single target and report reachability.
    Test {
        /// Target name.
        name: String,
    },
    /// Add a new target to the project config.
    Add {
        /// Target name.
        name: String,
        /// Backend type for this target.
        #[arg(long)]
        backend: TargetBackend,
        /// Platform identifier, for example linux-x64.
        #[arg(long)]
        platform: Option<String>,
        /// SSH host for ssh or ssh-windows targets.
        #[arg(long)]
        host: Option<String>,
        /// Remote repo path for ssh or ssh-windows targets.
        #[arg(long = "repo-path")]
        repo_path: Option<String>,
    },
    /// Remove a target from the project config.
    Remove {
        /// Target name.
        name: String,
    },
    /// Inspect and drain the warm-pool of reusable runners.
    Warm {
        /// Warm-pool subcommand. Defaults to `status`.
        #[command(subcommand)]
        command: Option<TargetsWarmCommand>,
    },
    /// Inspect local host-pool capacity and leases.
    Pool {
        /// Host-pool subcommand. Defaults to `status`.
        #[command(subcommand)]
        command: Option<TargetsPoolCommand>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum TargetBackend {
    Local,
    Ssh,
    #[value(name = "ssh-windows")]
    SshWindows,
    Cloud,
}

impl TargetBackend {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ssh => "ssh",
            Self::SshWindows => "ssh-windows",
            Self::Cloud => "cloud",
        }
    }
}

#[derive(Debug, Subcommand)]
pub(super) enum TargetsWarmCommand {
    /// Show live warm-pool entries.
    Status,
    /// Remove every warm-pool entry.
    Drain {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum TargetsPoolCommand {
    /// Show configured host pools and lease state.
    Status,
    /// Remove stale host-pool lease records from Shipyard state.
    Cleanup {
        /// Show what would be removed.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Actually remove stale lease records.
        #[arg(long)]
        fix: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum QuarantineCommand {
    /// List quarantined targets.
    List,
    /// Add a target to the quarantine list.
    Add {
        /// Target name.
        target: String,
        /// Free-form note.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Remove a target from the quarantine list.
    Remove {
        /// Target name.
        target: String,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum CloudCommand {
    /// List discovered GitHub Actions workflows.
    Workflows,
    /// Show cloud dispatch defaults and resolved workflow plans.
    Defaults,
    /// Dispatch a configured GitHub Actions workflow.
    Run(CloudRunArgs),
    /// Show tracked cloud workflow runs.
    Status {
        /// Dispatch ID to show, or `latest`.
        identifier: Option<String>,
        /// Number of records to show.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Refresh run state from GitHub before rendering.
        #[arg(long, action = ArgAction::SetTrue)]
        refresh: bool,
        /// Preserve Python's --refresh/--no-refresh option shape.
        #[arg(long = "no-refresh", action = ArgAction::SetTrue)]
        no_refresh: bool,
    },
    /// Generalized in-flight runner handoff.
    Handoff {
        /// Handoff subcommand.
        #[command(subcommand)]
        command: CloudHandoffCommand,
    },
    /// Retarget an existing in-flight lane to a new provider.
    Retarget(CloudRetargetArgs),
    /// Add a new lane to an in-flight PR.
    #[command(name = "add-lane")]
    AddLane(CloudAddLaneArgs),
}

#[derive(Debug, Subcommand)]
pub(super) enum CloudHandoffCommand {
    /// List queued GitHub Actions runs older than a threshold.
    #[command(name = "list-stuck")]
    ListStuck {
        /// Minimum queue age. Accepts seconds or Ns/Nm/Nh suffixes.
        #[arg(long, default_value = "10m")]
        threshold: String,
        /// Owner/repo slug. Defaults to the current git repo.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Cancel a queued run and redispatch its workflow with a provider override.
    Run {
        /// GitHub Actions workflow run ID.
        run_id: u64,
        /// Target runner provider.
        #[arg(long = "to")]
        provider: String,
        /// Owner/repo slug. Defaults to the current git repo.
        #[arg(long)]
        repo: Option<String>,
        /// Execute the operation.
        #[arg(long)]
        apply: bool,
        /// Force dry-run behavior.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

#[derive(Clone, Debug, clap::Args)]
pub(super) struct CloudRetargetArgs {
    /// PR number.
    #[arg(long)]
    pub(super) pr: u64,
    /// Target/lane name.
    #[arg(long)]
    pub(super) target: String,
    /// Runner provider for the lane.
    #[arg(long)]
    pub(super) provider: String,
    /// Workflow key.
    #[arg(long)]
    pub(super) workflow: Option<String>,
    /// Execute the operation.
    #[arg(long)]
    pub(super) apply: bool,
    /// Force dry-run behavior.
    #[arg(long = "dry-run")]
    pub(super) dry_run: bool,
    /// Hidden test hook to control the recorded run ID.
    #[arg(long, hide = true)]
    pub(super) run_id: Option<String>,
}

#[derive(Clone, Debug, clap::Args)]
pub(super) struct RescueArgs {
    /// PR number whose stuck runs should be rescued. Required unless `--all-stuck`.
    #[arg(required_unless_present = "all_stuck")]
    pub(super) pr: Option<u64>,
    /// Rescue every stuck queued run in the repo regardless of PR.
    #[arg(long = "all-stuck", action = ArgAction::SetTrue, conflicts_with = "pr")]
    pub(super) all_stuck: bool,
    /// Runner provider to redispatch to. Omit to let resolution decide per
    /// candidate: stuck-queued runs fall back to `github-hosted` (move off the
    /// wedged runner), while re-run failed runs RE-RESOLVE the provider
    /// (local-first with overflow) so a leg that overflowed to a GPU-less
    /// hosted runner can return local. Pass `--to <provider>` to force one.
    #[arg(long = "to")]
    pub(super) provider: Option<String>,
    /// Also re-dispatch completed runs that ended cancelled / failed / timed-out
    /// (e.g. a watchdog sweep, or a flaky required leg) before handoff.
    #[arg(long = "rerun-failed", action = ArgAction::SetTrue)]
    pub(super) rerun_failed: bool,
    /// Plan the rescue without acting.
    #[arg(long = "dry-run", action = ArgAction::SetTrue)]
    pub(super) dry_run: bool,
    /// Minimum queue age for a queued run to count as stuck. Accepts seconds or Ns/Nm/Nh suffixes.
    #[arg(long, default_value = "30m")]
    pub(super) threshold: String,
    /// Owner/repo slug. Defaults to the current git repo.
    #[arg(long)]
    pub(super) repo: Option<String>,
}

#[derive(Clone, Debug, clap::Args)]
// Clap models independent user flags as bools; unattended_fleet is an
// internal trust-context flag rather than another user-selected update mode.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct UpdateArgs {
    /// Report installed/available versions without applying.
    #[arg(long = "check", action = ArgAction::SetTrue)]
    pub(super) check: bool,
    /// Install a specific tag (e.g. `v0.53.0`) instead of `latest`.
    #[arg(long = "to")]
    pub(super) to: Option<String>,
    /// Plan the upgrade without applying.
    #[arg(long = "dry-run", action = ArgAction::SetTrue)]
    pub(super) dry_run: bool,
    /// Refresh the detached daemon only after the update is installed and
    /// smoke-verified. Intended for unattended fleet rollout.
    #[arg(long = "refresh-daemon", action = ArgAction::SetTrue)]
    pub(super) refresh_daemon: bool,
    /// Require self-contained command-based auth suitable for a stripped
    /// unattended fleet environment (internal fleet-update contract).
    #[arg(long = "unattended-fleet", action = ArgAction::SetTrue, hide = true)]
    pub(super) unattended_fleet: bool,
    /// Override the install.sh URL (test hook).
    #[arg(long = "install-script-url", hide = true)]
    pub(super) install_script_url: Option<String>,
    /// Override the releases-API base URL (test hook).
    #[arg(long = "releases-api-base", hide = true)]
    pub(super) releases_api_base: Option<String>,
    /// Override the curl binary (test hook).
    #[arg(long = "curl-bin", hide = true)]
    pub(super) curl_bin: Option<PathBuf>,
    /// Override the shell binary used to pipe install.sh (test hook).
    #[arg(long = "shell-bin", hide = true)]
    pub(super) shell_bin: Option<PathBuf>,
}

#[derive(Clone, Debug, clap::Args)]
pub(super) struct CloudAddLaneArgs {
    /// PR number.
    #[arg(long)]
    pub(super) pr: u64,
    /// Target/lane name.
    #[arg(long)]
    pub(super) target: String,
    /// Runner provider for the lane. Defaults to cloud workflow/provider config.
    #[arg(long)]
    pub(super) provider: Option<String>,
    /// Workflow key.
    #[arg(long)]
    pub(super) workflow: Option<String>,
    /// Execute the operation.
    #[arg(long)]
    pub(super) apply: bool,
    /// Force dry-run behavior.
    #[arg(long = "dry-run")]
    pub(super) dry_run: bool,
    /// Hidden test hook to control the recorded run ID and bypass live gh calls.
    #[arg(long, hide = true)]
    pub(super) run_id: Option<String>,
}

#[derive(Clone, Debug, clap::Args)]
pub(super) struct CloudRunArgs {
    /// Workflow key. Defaults to configured/default workflow.
    pub(super) workflow_key: Option<String>,
    /// Git ref to dispatch. Defaults to current branch.
    pub(super) ref_name: Option<String>,
    /// Runner provider override.
    #[arg(long)]
    pub(super) provider: Option<String>,
    /// Wait for the workflow run to complete.
    #[arg(long, action = ArgAction::SetTrue)]
    pub(super) wait: bool,
    /// Preserve Python's --wait/--no-wait option shape.
    #[arg(long = "no-wait", action = ArgAction::SetTrue)]
    pub(super) no_wait: bool,
    /// Generic runner selector input.
    #[arg(long = "runner-selector")]
    pub(super) runner_selector: Option<String>,
    /// Linux runner selector override.
    #[arg(long = "linux-runner-selector")]
    pub(super) linux_runner_selector: Option<String>,
    /// Windows runner selector override.
    #[arg(long = "windows-runner-selector")]
    pub(super) windows_runner_selector: Option<String>,
    /// macOS runner selector override.
    #[arg(long = "macos-runner-selector")]
    pub(super) macos_runner_selector: Option<String>,
    /// Refuse dispatch unless the remote ref resolves to this SHA or HEAD.
    #[arg(long = "require-sha")]
    pub(super) require_sha: Option<String>,
    /// Hidden test hook to bypass live dispatch/discovery.
    #[arg(long, hide = true)]
    pub(super) run_id: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(super) enum WaitCommand {
    /// Wait for a release tag and manifest to be ready.
    Release {
        /// Release tag/version to wait for.
        version: String,
        /// Give up after N seconds.
        #[arg(long, default_value_t = 600.0)]
        timeout: f64,
        /// Polling cadence when no live daemon transport exists.
        #[arg(long = "poll-interval", default_value_t = 2.0)]
        poll_interval: f64,
        /// Fail with exit 6 rather than polling after the first snapshot miss.
        #[arg(long)]
        no_fallback: bool,
        /// Hidden test hook to bypass git remote detection.
        #[arg(long, hide = true)]
        repo: Option<String>,
        /// Hidden test hook to use a local JSON snapshot file.
        #[arg(long, hide = true)]
        snapshot_file: Option<PathBuf>,
    },
    /// Wait for a PR to reach a target state.
    Pr {
        /// Pull request number.
        pr_number: u64,
        /// What PR state to wait for.
        #[arg(long, value_enum)]
        state: WaitPrState,
        /// Give up after N seconds.
        #[arg(long, default_value_t = 1800.0)]
        timeout: f64,
        /// Polling cadence when no live daemon transport exists.
        #[arg(long = "poll-interval", default_value_t = 30.0)]
        poll_interval: f64,
        /// Fail with exit 6 rather than polling after the first snapshot miss.
        #[arg(long)]
        no_fallback: bool,
        /// Hidden test hook to bypass git remote detection.
        #[arg(long, hide = true)]
        repo: Option<String>,
        /// Hidden test hook to use a local JSON snapshot file.
        #[arg(long, hide = true)]
        snapshot_file: Option<PathBuf>,
    },
    /// Wait for a workflow run to finish.
    Run {
        /// GitHub Actions workflow run ID.
        run_id: String,
        /// Require conclusion=success.
        #[arg(long)]
        success: bool,
        /// Give up after N seconds.
        #[arg(long, default_value_t = 1800.0)]
        timeout: f64,
        /// Polling cadence when no live daemon transport exists.
        #[arg(long = "poll-interval", default_value_t = 15.0)]
        poll_interval: f64,
        /// Fail with exit 6 rather than polling after the first snapshot miss.
        #[arg(long)]
        no_fallback: bool,
        /// Hidden test hook to bypass git remote detection.
        #[arg(long, hide = true)]
        repo: Option<String>,
        /// Hidden test hook to use a local JSON snapshot file.
        #[arg(long, hide = true)]
        snapshot_file: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum WaitPrState {
    Green,
    Merged,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum MergeResult {
    Success,
    Failure,
}

impl MergeMethod {
    pub(super) fn gh_flag(self) -> &'static str {
        match self {
            Self::Merge => "--merge",
            Self::Squash => "--squash",
            Self::Rebase => "--rebase",
        }
    }

    /// Value accepted by GitHub's REST PUT /pulls/:n/merge `merge_method` body field.
    pub(super) fn rest_value(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }
}

impl WaitPrState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum PathMode {
    Isolated,
    Shipyard,
}

impl From<PathMode> for RuntimeMode {
    fn from(value: PathMode) -> Self {
        match value {
            PathMode::Isolated => Self::Isolated,
            PathMode::Shipyard => Self::Shipyard,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command, DependencyCommand, PulpDependencyCommand, RunnerCommand};

    #[test]
    fn dependency_pulp_commands_have_an_explicit_operation() {
        let cli = Cli::try_parse_from(["shipyard", "dependency", "pulp", "verify"])
            .expect("dependency verify command");
        assert!(matches!(
            cli.command,
            Command::Dependency {
                command: DependencyCommand::Pulp {
                    command: PulpDependencyCommand::Verify
                }
            }
        ));
        assert!(Cli::try_parse_from(["shipyard", "dependency", "pulp"]).is_err());
    }

    #[test]
    fn queue_observer_rejects_zero_max_polls() {
        assert!(
            Cli::try_parse_from(["shipyard", "queue-observe", "--follow", "--max-polls", "0",])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["shipyard", "queue-observe", "--follow", "--max-polls", "1",])
                .is_ok()
        );
    }

    #[test]
    fn changed_surface_trial_status_requires_explicit_exact_identity() {
        let cli = Cli::try_parse_from([
            "shipyard",
            "changed-surface-trial-status",
            "--repo",
            "owner/repo",
            "--pr",
            "42",
            "--target",
            "mac",
            "--head",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ])
        .expect("trial status command");
        assert!(matches!(
            cli.command,
            Command::ChangedSurfaceTrialStatus {
                repository,
                pull_request: 42,
                target,
                head_sha,
            } if repository == "owner/repo"
                && target == "mac"
                && head_sha == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        assert!(
            Cli::try_parse_from([
                "shipyard",
                "changed-surface-trial-status",
                "--repo",
                "owner/repo",
                "--pr",
                "42",
                "--target",
                "mac",
            ])
            .is_err()
        );
    }

    #[test]
    fn local_linux_lease_defaults_to_trusted_merge_group_context() {
        let cli = Cli::try_parse_from(["shipyard", "runner", "local-linux-lease"])
            .expect("default local Linux lease command");
        let Command::Runner {
            command: RunnerCommand::LocalLinuxLease { context, lane, .. },
        } = cli.command
        else {
            panic!("expected local Linux lease command");
        };
        assert_eq!(context, "merge_group");
        assert_eq!(lane, "linux");
    }

    #[test]
    fn recovery_worker_is_dry_run_once_by_default_and_bounds_mode_flags() {
        let cli = Cli::try_parse_from(["shipyard", "runner", "recovery-worker"])
            .expect("default recovery worker command");
        let Command::Runner {
            command: RunnerCommand::RecoveryWorker { once, drain, apply },
        } = cli.command
        else {
            panic!("expected recovery worker command");
        };
        assert!(!once);
        assert!(!drain);
        assert!(!apply);
        assert!(
            Cli::try_parse_from(["shipyard", "runner", "recovery-worker", "--once", "--drain",])
                .is_err()
        );
    }
}
