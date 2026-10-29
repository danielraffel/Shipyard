# Cause records

A cause record names why one CI job failed, in a few hundred bytes instead of
a job log of many megabytes. Shipyard writes them as JSON objects with the
schema `ci.cause/v1`.

All of this is read-only toward GitHub, except `shipyard diagnose --annotate
post`, which creates one neutral check run. None of it runs inside `ship`,
`pr` or `run`.

## Where the cause comes from

C2 is a rules-only failing-line extractor (`src/diagnose/c2.rs`). It reads a
job log, finds the lines that explain the failure, and ranks them: lines from
the step that ended in `##[error]` come first, and a bare make or ninja
failure ranks below every runner and compiler line. There is no model in the
path. Each log format it knows has a fixture in `tests/fixtures/c2/`, and the
fixtures are the contract.

`shipyard diagnose` uses the same extractor for a pull request's failing
required checks, and the scorecard (`shipyard metrics gate-cost --scorecard
--causes`) uses it for each red case.

## The record

A `ci.cause/v1` record holds:

- where the job ran: `repo`, `run_id`, `attempt`, `job_id`, `job`,
  `head_sha`, `event`, `pr`, `failed_step`;
- what failed: `failure_class` (`test`, `build`, `infra`, `cancelled`,
  `timeout`, `drift`, `gate_policy` or `unknown`), `format` (the log format,
  for example `ctest` or `compiler`), `signature` (the most specific stable
  name for the cause) and `detail`;
- `evidence`: the log line number and an excerpt;
- `fix_hint`: set only for a generated-file drift check, and only when the
  command the check printed passes the allowlist below; otherwise null;
- `confidence` (`high`, `medium` or `low`), `source`, `extractor`,
  `named_at`, `supersedes`, and C2's `kind`, `location` and `step_failed`;
- `retrospective`: true when the cause was named after the fact from a
  stored log (a backfill or a scorecard read); false when it was named while
  it was being watched, by the job itself or by `cause record` within one
  pass of the job ending.

A cause counts as named when its class is not `unknown` and its confidence is
`high` or `medium`.

**`fix_hint` is never executed.** It comes from the job log, and a pull
request's code can print any line into its own log. So Shipyard keeps a
command only when it has exactly one shape:

```
python3 tools/scripts/<name>.py [--flag [value]]... --write
```

with no shell metacharacters, quoting or substitution. Every value must be a
relative path inside the checkout: no leading `/`, `~` or `-`, and no `..`
segment. Anything else (`bash -c '...'`, a pipe, a redirect) leaves `fix_hint`
null. The line as logged stays in `detail`, which is never a command.

A `fix_hint` is a suggestion to review. Read it before running it, and run it
from a checkout of the main branch, never from the pull request's own
checkout, where `tools/scripts/<name>.py` is the pull request's own script.
Nothing in Shipyard executes or auto-applies one.

## Commands

```bash
# Name one job log's cause. Prints the record as JSON.
shipyard cause extract --log job.log --repo OWNER/REPO --run-id 123 --job "build (linux)"

# Inside a workflow: print the record as a ::notice line in the job's own log.
shipyard cause extract --log "$RUNNER_TEMP/test.log" --job "<the job's display name>" --notice || true

# C2's raw records instead of a ci.cause/v1 record.
shipyard cause extract --log job.log --format c2

# Append records to an archive, one file per month of named_at.
shipyard cause archive --dir causes/ --input records.jsonl

# One observer pass over a workflow's recent runs.
shipyard cause record --repo OWNER/REPO --workflow build.yml --dir data/ci-cause

# Read-only historical lexical advice for one exact current head.
shipyard cause advise --dir data/ci-cause --repo OWNER/REPO \
  --head-sha <40-hex-head> --query "failing test" --pr 123
```

`cause extract` takes where the job ran from its flags, else from the
runner's `GITHUB_*` environment. Pass `--job` with the job's display name: the
default is the job's key (`GITHUB_JOB`), and a record whose name does not
match the display name is not trusted by the scorecard. It never fails the
step it runs in; only a missing or unreadable log exits non-zero.

`cause archive` is append-only. A record whose id is already archived is
skipped, and nothing is rewritten.

For a local integrity check, run `shipyard cause audit --dir DIR`. It reads
the cause month files and `fixlinks.jsonl`, reports malformed rows, duplicate
cause IDs, duplicate red run attempts, and fix-links with no archived cause,
and exits nonzero when it finds one. A structurally proven `rerun_green` link
(same red and fix run/head, empty changed-file lists, and
`own_files_method = rerun_same_head`) is allowed without a cause row because
GitHub may have deleted the original failed log before the archive pass;
compare-derived and malformed rerun links still require an archived cause.
Findings are sorted by archive path and line. It never rewrites the archive,
contacts GitHub, or exposes invalid line contents.

## The observer: `shipyard cause record`

Each pass:

1. lists the workflow's completed runs created since `--since` (default:
   three days ago);
2. for every red job not yet in the archive, writes one `ci.cause/v1` row to
   `<dir>/causes/YYYY-MM.jsonl`, by the job's completion time;
3. for every red run attempt without a fix-link, looks for the earliest later
   successful run of the same workflow on the same pull request or branch.
   When it finds one, it writes a fix-link to `<dir>/fixlinks.jsonl`: the red
   run, the fixing commit, and the files it changed. A rerun that turned the
   same run green is recorded as `rerun_green`.

A bare exit that C2 cannot name gets one of two labels. A summary job, such
as `linux` checking the jobs it needs, takes the cause of the named job in
the same run attempt whose name it begins (`Linux (x64) [github-hosted]`),
and says so in `inherited_from_job_id`; its evidence is that job's log line
too, named by `evidence_job_id`, and so are its failing step and `step_failed`.
Any other bare exit is named by the last line the step printed after its
echoed script (the step itself stays in `failed_step`), and stays low
confidence.

Since C2 v7 a CMake error is named by its call site and the first clause of
its error, not by the bare command in parentheses (which named every
`string(JSON ...)` error "string"), and a bare exit is named by the last line
the step printed rather than the step's script line.

A fix-link's `own_files` are the files of the pull request's own commits
between the red and the fix. Some reds have no own commit chain, and these
are handled as follows:
- **Merge-queue red:** its head is the queue's commit, so own files come from
  the pull request head that the queue merged.
- **Force-pushed red:** own files come from the fix's place in the pull
  request's commit list, within 72 hours.
- **Neither:** `own_files` is null, and `own_files_method` says why.

A cause row is written live: `named_at` is the pass that named it (or the
earlier time the job printed its own record), `retrospective` is false, and
`named_at` minus `completed_at` is the time to cause. A red the first pass
finds from before the observer started is still named at that pass.

A red with no green yet stays pending and is looked at again on the next
pass. Cancelled jobs are counted but get no cause row. A lock file in `--dir`
keeps two passes from writing at once.

The observer reads runs, jobs, logs, pull requests and comparisons. It never
writes to GitHub. Before a row is written, local paths, IP addresses, email
addresses, credential-like values, GitHub tokens (`ghp_`, `gho_`, `ghs_`,
`ghu_`, `ghr_` and `github_pat_`) and private host names are replaced with
placeholders; run and job URLs are kept. Job logs are read with
`--allow-escape-sequences`, since logs carry ANSI colour, falling back to a
plain read on a `gh` without that flag.

It prints one summary line (or, with `--json`, the same counts as JSON): runs
listed and read, causes written, already recorded, cancelled jobs, fix-links
written, reds pending (split into those that went red under 24 hours,
24 to 72 hours, and over 72 hours ago), and reads that failed. Each failed read is also printed
on stderr. Run it on a schedule you choose; Shipyard does not schedule it.

## Related

`cause advise` reads only the local archive. It requires a full current head
SHA, excludes that head and the current pull request, ignores unnamed/unknown
rows, and uses case-insensitive lexical token overlap with stable tie breaks.
It can take newline-delimited outage windows (`start`, `end`, optional
`reason`) and suppresses rows completed inside a declared window. It never
contacts GitHub, executes `fix_hint`, or writes the archive. Rows containing
unscrubbed paths, addresses, private hosts, or credential-like values are
discarded. A malformed JSON line in `fixlinks.jsonl` fails closed with its
file and line number instead of being silently ignored. JSON output uses
`shipyard.cause-advice/v1`; human output is a
stable table intended for review, not an automatic decision; when a matching
fix-link exists, that table names the fixing head and how the link was derived
(`rerun_green` or `compare_red_head_to_fix_head`).

When one red attempt has both the valid same-head `rerun_green` link and a
valid `compare_red_head_to_fix_head` link, advice selects the compare link so
the table names the later push that fixed the failure. The rerun remains in the
archive as recovery evidence; a second distinct compare link still suppresses
the fix as ambiguous.

- [CLI reference](cli-reference.md): every flag of `diagnose` and `cause`.
- The scorecard's `--export-cases` rows carry the same record in their
  `cause` field, so `shipyard cause archive` can archive them too.
