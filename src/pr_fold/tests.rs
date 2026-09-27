use serde_json::json;

use super::*;

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("fixture time")
        .with_timezone(&Utc)
}

fn body(session: &str) -> String {
    format!(
        "Fix it.\n\n<!-- whence {{\"labels\": [\"1·codex\"], \"prov\": {{\"agent\": \"codex\", \"session\": \"{session}\"}}}} -->\n---\n### Provenance\n<!-- /whence -->"
    )
}

fn noise() -> Vec<String> {
    DEFAULT_NOISE_PATHS
        .iter()
        .map(|path| (*path).to_owned())
        .collect()
}

#[test]
fn the_session_is_read_from_the_whence_block_and_the_environment() {
    assert_eq!(whence_session(&body("abc-123")).as_deref(), Some("abc-123"));
    assert_eq!(whence_session("no stamp here"), None);
    assert_eq!(whence_session(&body("")), None);

    let env = |name: &str| match name {
        "CLAUDE_CODE_SESSION_ID" => Some("claude-1".to_owned()),
        "CODEX_SESSION_ID" => Some("codex-1".to_owned()),
        "WHENCE_SESSION_ID" => Some("  ".to_owned()),
        _ => None,
    };
    assert_eq!(current_session(env).as_deref(), Some("claude-1"));
    assert_eq!(current_session(|_| None), None);
}

#[test]
fn families_skip_root_files_and_gate_forced_paths() {
    let touched = [
        "CMakeLists.txt",
        "core/view/src/widgets.cpp",
        "core/view/include/pulp/view/widgets.hpp",
        "test/cmake/view_tests.cmake",
        ".agents/skills/ci/SKILL.md",
        ".claude-plugin/plugin.json",
        "planning",
    ];
    let found = families(touched, &noise());
    assert_eq!(
        found.into_iter().collect::<Vec<_>>(),
        ["core/view", "test/cmake"]
    );
}

#[test]
fn only_open_same_session_prs_from_the_window_and_another_branch_are_candidates() {
    let page = json!([
        {"number": 1, "head": {"ref": "feat/a"}, "created_at": "2026-09-27T08:00:00Z", "body": body("s1")},
        {"number": 2, "head": {"ref": "feat/b"}, "created_at": "2026-09-27T01:00:00Z", "body": body("s1")},
        {"number": 3, "head": {"ref": "feat/c"}, "created_at": "2026-09-27T09:00:00Z", "body": body("s2")},
        {"number": 4, "head": {"ref": "feat/here"}, "created_at": "2026-09-27T09:30:00Z", "body": body("s1")},
        {"number": 5, "head": {"ref": "feat/d"}, "created_at": "2026-09-27T09:40:00Z", "body": null},
    ]);
    let open = parse_open_prs(&page);
    assert_eq!(open.len(), 5);
    let picked: Vec<u64> = session_candidates(&open, "s1", "feat/here", at("2026-09-27T10:00:00Z"))
        .iter()
        .map(|pr| pr.number)
        .collect();
    assert_eq!(
        picked,
        [1],
        "2 is older than 6 h, 3 is another session, 4 is this branch, 5 is unstamped"
    );
}

#[test]
fn a_candidate_is_suggested_only_when_it_shares_a_family() {
    let open = parse_open_prs(&json!([
        {"number": 10, "head": {"ref": "feat/view"}, "created_at": "2026-09-27T09:00:00Z", "body": body("s1")},
        {"number": 11, "head": {"ref": "feat/docs"}, "created_at": "2026-09-27T09:00:00Z", "body": body("s1")},
    ]));
    let here = families(
        ["core/view/src/a.cpp", "docs/guides/x.md"].iter().copied(),
        &noise(),
    );
    let view = families(
        ["core/view/src/b.cpp", ".agents/skills/ci/SKILL.md"]
            .iter()
            .copied(),
        &noise(),
    );
    let only_skills = families([".agents/skills/ci/SKILL.md"].iter().copied(), &noise());
    let found = suggestions(&here, &[(&open[0], view), (&open[1], only_skills)]);
    assert_eq!(
        found,
        [FoldSuggestion {
            number: 10,
            branch: "feat/view".to_owned(),
            shared: vec!["core/view".to_owned()],
        }]
    );
    let lines = render(&found);
    assert!(lines[0].contains("advisory"), "{lines:?}");
    assert!(
        lines[1].contains("#10 feat/view — shared: core/view"),
        "{lines:?}"
    );
    assert!(render(&[]).is_empty());
}
