//! Deterministic evaluation of the latest reviewer HOLD against a repair diff.

use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Finding {
    pub id: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub problem: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangedFile {
    pub name: String,
    pub hunks: Vec<Hunk>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub new_start: u32,
    pub new_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct FindingResult {
    pub id: String,
    pub file: String,
    pub line: Option<u32>,
    pub status: Status,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum Status {
    Addressed,
    Answered,
    Open,
}

pub(crate) fn parse_findings(body: &str) -> Vec<Finding> {
    let structured =
        Regex::new(r"^\s*-\s*([^|\s]+)\s*\|\s*(?:(.+?):(\d+)\s*\|\s*(.*)|([^|]+)\|\s*(.*))\s*$")
            .expect("structured finding regex");
    let free = Regex::new(r"(?i)^\s*[-*]\s+(?:\[[^\]]*\]\s+)?`?([^`\s:]+(?:/[^`\s:]*)?)`?[:\s]+(\d+)\b\s*[-–—:]?\s*(.*)$")
        .expect("free finding regex");
    let id_re = Regex::new(r"\b\d+-r\d+-f\d+\b").expect("finding id regex");
    let mut findings = Vec::new();
    let mut in_structured = false;
    for line in body.lines() {
        if line.trim().eq_ignore_ascii_case("FINDINGS:") {
            in_structured = true;
            continue;
        }
        if in_structured {
            if let Some(c) = structured.captures(line) {
                let (file, line_no, problem) = if let (Some(file), Some(line_no), Some(problem)) =
                    (c.get(2), c.get(3), c.get(4))
                {
                    (
                        Some(file.as_str().trim().to_owned()),
                        line_no.as_str().parse().ok(),
                        problem.as_str().trim().to_owned(),
                    )
                } else {
                    (
                        None,
                        None,
                        c.get(6).map_or("", |m| m.as_str()).trim().to_owned(),
                    )
                };
                findings.push(Finding {
                    id: c[1].to_owned(),
                    file,
                    line: line_no,
                    problem,
                });
                continue;
            }
            if !line.trim().is_empty() && !line.trim_start().starts_with('-') {
                in_structured = false;
            }
        }
        if let Some(c) = free.captures(line) {
            let file = c[1].trim().to_owned();
            let line_no = c[2].parse().ok();
            let problem = c[3].trim().to_owned();
            let id = id_re.find(&problem).map_or_else(
                || format!("{file}:{}", c[2].trim()),
                |m| m.as_str().to_owned(),
            );
            findings.push(Finding {
                id,
                file: Some(file),
                line: line_no,
                problem,
            });
        }
    }
    findings
}

pub(crate) fn parse_hunks(patch: &str) -> Vec<Hunk> {
    let re = Regex::new(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@").expect("hunk regex");
    re.captures_iter(patch)
        .filter_map(|c| {
            Some(Hunk {
                new_start: c[1].parse().ok()?,
                new_count: c.get(2).map_or(1, |m| m.as_str().parse().ok().unwrap_or(1)),
            })
        })
        .collect()
}

pub(crate) fn evaluate(
    findings: &[Finding],
    changed: &[ChangedFile],
    answers: &BTreeMap<String, String>,
) -> Vec<FindingResult> {
    findings
        .iter()
        .map(|finding| {
            if let Some(answer) = answers.get(&finding.id) {
                return FindingResult {
                    id: finding.id.clone(),
                    file: display_file(finding),
                    line: finding.line,
                    status: Status::Answered,
                    evidence: answer.clone(),
                };
            }
            let touched = finding
                .file
                .as_ref()
                .and_then(|file| changed.iter().find(|item| item.name == *file));
            let addressed = match (finding.file.as_ref(), touched) {
                (None, _) => !changed.is_empty(),
                (Some(_), Some(item)) => match finding.line {
                    Some(line) => item.hunks.iter().any(|h| {
                        let end = h.new_start.saturating_add(h.new_count.saturating_sub(1));
                        line.saturating_add(30) >= h.new_start
                            && line.saturating_sub(30) <= end.saturating_add(30)
                    }),
                    None => true,
                },
                (Some(_), None) => false,
            };
            let evidence = if addressed {
                touched.and_then(|item| item.hunks.first()).map_or_else(
                    || "changed file".to_owned(),
                    |h| format!("@@ +{},{} @@", h.new_start, h.new_count),
                )
            } else {
                "no matching changed hunk".to_owned()
            };
            FindingResult {
                id: finding.id.clone(),
                file: display_file(finding),
                line: finding.line,
                status: if addressed {
                    Status::Addressed
                } else {
                    Status::Open
                },
                evidence,
            }
        })
        .collect()
}

fn display_file(finding: &Finding) -> String {
    finding
        .file
        .clone()
        .unwrap_or_else(|| "unlocated".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_block_is_parsed_first() {
        let findings = parse_findings(
            "FINDINGS:\n- f-1 | src/a.rs:10 | fix this\n- f-2 | unlocated | explain",
        );
        assert_eq!(findings[0].id, "f-1");
        assert_eq!(findings[0].file.as_deref(), Some("src/a.rs"));
        assert_eq!(findings[1].line, None);
    }

    #[test]
    fn free_text_fixture_formats_are_parsed() {
        let findings = parse_findings(
            "Findings:\n- [blocking] Dockerfile 12 bad\n- Makefile:3 other\n- src/a.rs 20 id 7-r1-f1",
        );
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[2].id, "7-r1-f1");
    }

    #[test]
    fn hunk_near_line_is_addressed_and_far_line_is_open() {
        let changed = vec![ChangedFile {
            name: "src/a.rs".to_owned(),
            hunks: vec![Hunk {
                new_start: 100,
                new_count: 1,
            }],
        }];
        let near = Finding {
            id: "near".to_owned(),
            file: Some("src/a.rs".to_owned()),
            line: Some(125),
            problem: "x".to_owned(),
        };
        let far = Finding {
            id: "far".to_owned(),
            file: Some("src/a.rs".to_owned()),
            line: Some(200),
            problem: "x".to_owned(),
        };
        let results = evaluate(&[near, far], &changed, &BTreeMap::new());
        assert_eq!(results[0].status, Status::Addressed);
        assert_eq!(results[1].status, Status::Open);
    }

    #[test]
    fn answers_and_unlocated_findings() {
        let findings = vec![Finding {
            id: "u".to_owned(),
            file: None,
            line: None,
            problem: "x".to_owned(),
        }];
        assert_eq!(
            evaluate(&findings, &[], &BTreeMap::new())[0].status,
            Status::Open
        );
        let mut answers = BTreeMap::new();
        answers.insert("u".to_owned(), "explained".to_owned());
        assert_eq!(
            evaluate(&findings, &[], &answers)[0].status,
            Status::Answered
        );
        let changed = vec![ChangedFile {
            name: "src/any.rs".to_owned(),
            hunks: vec![Hunk {
                new_start: 1,
                new_count: 1,
            }],
        }];
        assert_eq!(
            evaluate(&findings, &changed, &BTreeMap::new())[0].status,
            Status::Addressed
        );
    }

    #[test]
    fn captured_hold_fixtures_are_parseable() {
        let value: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/repair-check/recent-holds.json"
        ))
        .expect("fixture JSON");
        let comments = value.as_array().expect("fixture array");
        assert_eq!(comments.len(), 20);
        assert!(comments.iter().all(|entry| {
            !parse_findings(entry["comment"]["body"].as_str().unwrap_or_default()).is_empty()
        }));
    }
}
