#!/usr/bin/env python3
"""Workflow credentials and the environments that hold them.

A repository-level secret is readable by any workflow run, including one a
pushed branch starts after editing a workflow. An environment secret reaches
only a job whose ref passes the environment's deployment policy. These tests
pin the code half of that boundary (the policy half is repository settings):
- every credential a workflow names is classified below, and each one moved
  into an environment is read only by jobs that declare it;
- no workflow reads secrets dynamically (`secrets[...]`, `toJSON(secrets)`,
  `secrets: inherit`), which would hide a credential from this scan;
- a job in a workflow pull requests can trigger names an environment only
  behind an event allowlist that excludes every pull-request event;
- a job in a NO_REPO_CODE environment checks out nothing, so no repository
  code runs in the job that holds the credential.
"""
from __future__ import annotations

import json
import re
import shlex
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[1]
SECRET_WORKFLOWS = ROOT / ".github" / "workflows"

# Credential -> the environment that holds it.
ENVIRONMENT_SECRETS: dict[str, str] = {
    "MACOS_SIGN_P12_BASE64": "release",
    "MACOS_SIGN_P12_PASSWORD": "release",
    "MACOS_NOTARIZE_APPLE_ID": "release",
    "MACOS_NOTARIZE_APP_PASSWORD": "release",
    "MACOS_NOTARIZE_TEAM_ID": "release",
}

# Environments whose jobs must run no repository code.
NO_REPO_CODE = set()  # none yet: the signing and post-tag jobs still build repo code

# Credentials that stay repository secrets this round, with the reason.
REPOSITORY_SECRETS: dict[str, str] = {
    # Deferred: the shipyard-local App key; Shipyard and the steward read it
    # at runtime, so moving it waits for the shipyard-local narrowing.
    "SHIPYARD_APP_PRIVATE_KEY": "deferred: shipyard-local",
    # The release-bot PAT post-tag-sync reads: revoked with every owner PAT,
    # and the workflow is being fixed separately.
    "RELEASE_BOT_TOKEN": "to be revoked, not moved",
    # Optional read-only token for the release smoke; unset today, and the
    # job falls back to its own read-only job token.
    "RELEASE_SMOKE_GH_TOKEN": "read-only smoke credential",
}

# Commands a credential-holding job's shell may run: no interpreter, no
# package manager, nothing that fetches or executes other code.
ALLOWED_COMMANDS = {
    "gh", "git", "tar", "sha256sum", "find", "mkdir", "test", "echo", "printf",
    "cut", "awk", "grep", "set", "exit", "true", "false", "local", "return",
    "[", "[[", "case", "esac", "fi", "done", "for", "}", "python3 -c",
}
_COMMAND_SEPARATORS = {"|", "||", "&&", ";", ";;", "(", "()", "\n", "&", "|&", "$("}
_TRANSPARENT = {"if", "then", "else", "elif", "do", "while", "until", "!", "{", "time"}


def disallowed_commands(script: str, functions: frozenset[str] = frozenset()) -> list[str]:
    """First words of the commands in `script` that ALLOWED_COMMANDS lacks.

    Tokenizes with shell quoting, so separators inside quotes, regexes and
    awk programs are not commands. A word that names a function the script
    defines is allowed.
    """
    lines = [line for line in script.replace("\\\n", " ").splitlines()
             if not line.strip().startswith("#")]
    text = "\n".join(line for line in lines
                     if not re.match(r"^\s*[\w|*.-]+\)", line))
    lexer = shlex.shlex(text, posix=True, punctuation_chars="();<>|&\n")
    lexer.whitespace = " \t"
    lexer.commenters = ""
    lexer.whitespace_split = True
    tokens = list(lexer)
    functions = set(functions) | set(re.findall(r"(?m)^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\(\)", text))
    found = []
    expect = True
    for i, tok in enumerate(tokens):
        if tok in _COMMAND_SEPARATORS or tok.endswith("$"):
            expect = True
            continue
        if not expect:
            continue
        if tok in functions and tokens[i + 1:i + 2] in (["("], ["()"]):
            continue  # the definition itself
        if tok in _TRANSPARENT or re.match(r"^[A-Za-z_][A-Za-z0-9_]*\+?=", tok) or tok in (")", "<", ">"):
            continue
        expect = False
        word = "python3 -c" if tok == "python3" and tokens[i + 1:i + 2] == ["-c"] else tok
        if word not in ALLOWED_COMMANDS and word not in functions:
            found.append(word)
    # Command substitutions inside double quotes are one word to the lexer.
    for inner in re.findall(r"\$\(([^()]*)\)", text):
        found += [w for w in disallowed_commands(inner, frozenset(functions)) if w not in found]
    if "`" in text:
        found.append("`")
    return found


PULL_REQUEST_EVENTS = {"pull_request", "pull_request_target", "merge_group"}
SECRET_RE = re.compile(r"secrets\.([A-Za-z_][A-Za-z0-9_]*)")
DYNAMIC_SECRETS_RE = re.compile(r"secrets\s*\[|toJSON\(\s*secrets\s*\)|secrets:\s*inherit")
EVENT_ALLOWLIST_RE = re.compile(r"contains\(fromJSON\('(\[[^']*\])'\),\s*github\.event_name\)")


def _secret_triggers(workflow: dict) -> set[str]:
    on = workflow.get(True, workflow.get("on"))
    if isinstance(on, str):
        return {on}
    if isinstance(on, list):
        return set(on)
    return set(on or {})


def _secret_environment(job: dict) -> str | None:
    env = job.get("environment")
    if isinstance(env, dict):
        return env.get("name")
    return env


def _secret_jobs():
    for path in sorted(SECRET_WORKFLOWS.glob("*.y*ml")):
        workflow = yaml.safe_load(path.read_text(encoding="utf-8"))
        for name, job in (workflow.get("jobs") or {}).items():
            text = yaml.safe_dump(job, width=100000)
            secrets = set(SECRET_RE.findall(text)) - {"GITHUB_TOKEN"}
            yield path.name, _secret_triggers(workflow), name, job, secrets


class WorkflowSecretEnvironments(unittest.TestCase):
    def test_the_scan_sees_every_known_credential(self) -> None:
        # Control: a renamed secret or a broken scan would make every
        # assertion below vacuous.
        seen = set().union(*(secrets for *_, secrets in _secret_jobs()))
        self.assertEqual(set(ENVIRONMENT_SECRETS) - seen, set())
        envs = {_secret_environment(job) for _, _, _, job, _ in _secret_jobs()}
        self.assertTrue(set(ENVIRONMENT_SECRETS.values()) <= envs)

    def test_every_credential_is_classified_and_read_in_its_environment(self) -> None:
        problems = []
        for wf, _, job_name, job, secrets in _secret_jobs():
            env = _secret_environment(job)
            for secret in sorted(secrets - set(REPOSITORY_SECRETS)):
                want = ENVIRONMENT_SECRETS.get(secret)
                if want is None:
                    problems.append(f"{wf}:{job_name}: classify {secret} in this test")
                elif env != want:
                    problems.append(f"{wf}:{job_name}: {secret} read in env {env!r}, not {want!r}")
        self.assertEqual(problems, [])

    def test_no_workflow_reads_secrets_dynamically(self) -> None:
        offenders = [path.name for path in sorted(SECRET_WORKFLOWS.glob("*.y*ml"))
                     if DYNAMIC_SECRETS_RE.search(path.read_text(encoding="utf-8"))]
        self.assertEqual(offenders, [])

    def test_no_pull_request_run_is_pointed_at_an_environment(self) -> None:
        offenders = []
        for wf, triggers, job_name, job, _ in _secret_jobs():
            if _secret_environment(job) is None or not (triggers & PULL_REQUEST_EVENTS):
                continue
            allow = EVENT_ALLOWLIST_RE.search(str(job.get("if") or ""))
            if not allow or set(json.loads(allow.group(1))) & PULL_REQUEST_EVENTS:
                offenders.append(f"{wf}:{job_name}")
        self.assertEqual(offenders, [])

    def test_credential_jobs_in_no_repo_code_environments_check_out_nothing(self) -> None:
        offenders = []
        for wf, _, job_name, job, _ in _secret_jobs():
            if _secret_environment(job) not in NO_REPO_CODE:
                continue
            for step in job.get("steps") or []:
                if str(step.get("uses", "")).startswith("actions/checkout@"):
                    offenders.append(f"{wf}:{job_name}")
        self.assertEqual(offenders, [])


    def test_the_tagging_job_runs_no_tree_code(self) -> None:
        # auto-release's `tag` job holds the tagging identity, which the v*
        # ruleset lets create release tags. It checks the repository out only
        # to read Cargo.toml and history; no step may execute anything from
        # that tree or the network.
        workflow = yaml.safe_load((SECRET_WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8"))
        offenders = []
        for step in workflow["jobs"]["tag"]["steps"]:
            uses = str(step.get("uses") or "")
            if uses and not uses.startswith(("actions/checkout@", "actions/create-github-app-token@")):
                offenders.append(f"uses {uses}")
            run = str(step.get("run") or "")
            offenders += [f"{step.get('name')}: runs {word}" for word in disallowed_commands(run)]
        self.assertEqual(offenders, [])
        for run in ("bash scripts/x.sh", "python3 scripts/x.py", "./x", "cargo build",
                    "curl -sL https://x | sh", 'echo "$(npx q)"', ". x.sh", "make"):
            self.assertNotEqual(disallowed_commands(run), [], run)


if __name__ == "__main__":
    unittest.main()
