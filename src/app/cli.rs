use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

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
    /// Print the resolved runtime paths for the selected mode.
    Paths,
    /// Show or bump a consumer repo's Shipyard version pin.
    Pin {
        /// Pin subcommand.
        #[command(subcommand)]
        command: PinCommand,
    },
    /// Inspect and switch project profiles and configuration.
    Config {
        /// Config subcommand. Defaults to `show`.
        #[command(subcommand)]
        command: Option<ConfigCommand>,
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
    /// Clean up old logs, bundles, evidence, and optional ship-state.
    Cleanup {
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
        /// Skip a target after preflight.
        #[arg(long = "skip-target")]
        skip_targets: Vec<String>,
        /// Disable warm-pool reuse for this invocation.
        #[arg(long = "no-warm")]
        no_warm: bool,
        /// Suppress the staged working-tree drift guard.
        #[arg(long = "allow-tree-drift")]
        allow_tree_drift: bool,
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
        /// Skip a target after preflight.
        #[arg(long = "skip-target")]
        skip_targets: Vec<String>,
        /// Adopt the current head SHA when recorded ship-state drifted (amend /
        /// force-push), re-validating the new head instead of failing on
        /// SHA drift (Shipyard #346).
        #[arg(long = "adopt-head")]
        adopt_head: bool,
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
    /// Wait for a GitHub condition to match.
    Wait {
        /// Wait subcommand.
        #[command(subcommand)]
        command: WaitCommand,
    },
    /// Live view of an in-flight ship.
    Watch {
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
pub(super) enum RunnerCommand {
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
        /// Auto-cancel stale queued runs.
        #[arg(long = "fix")]
        fix: bool,
        /// Auto-kill hung `Runner.Worker` processes (etime above the watchdog
        /// threshold) using the same recovery sequence as `runner kill`.
        /// Implies `--fix`.
        #[arg(long = "kill-hung-workers")]
        kill_hung_workers: bool,
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
    /// (`Σ max(0, cap − running tart VMs)`). Exit 1 if any host is unreadable.
    Capacity,
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
    /// Runner provider to redispatch to.
    #[arg(long = "to", default_value = "github-hosted")]
    pub(super) provider: String,
    /// Also re-arm completed-as-cancelled runs (e.g. ones a watchdog sweep marked failed) before handoff.
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
