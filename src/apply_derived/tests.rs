use super::safe_zip::{self, Limits};
use super::test_zip::{TestEntry, build};
use super::*;

const PULP: &str = r#"
[derived_files]
paths = ["test/ctest_script_inputs/", ".shipyard/changed-surface-families.toml"]
drift_tests = ["script-test-inputs-drift", "changed-surface-script-families-drift"]
carry_trailers = ["Version-Bump", "Skill-Update"]

[[derived_files.artifacts]]
name = "script-inputs-expected"
root = "test/ctest_script_inputs"
"#;

fn pulp() -> DerivedFilesConfig {
    DerivedFilesConfig::from_config_text(PULP)
}

/// A real `actions/upload-artifact` archive (one small JSON file), as
/// `gh api repos/.../actions/artifacts/<id>/zip` returned it.
const REAL_ARTIFACT_HEX: &str = "504b03041400080008002c997b5d000000000000000000000000170000006132742d73636f70652d68656164726f6f6d2e6a736f6eab564ace2fcd2b51b252303235d45150cac9cccd04f14c0d8d741494325213538af2f37341d26620e9a4d294f45490bcb185492d1700504b070892a48147360000003d000000504b01022d031400080008002c997b5d92a48147360000003d000000170000000000000000002000a481000000006132742d73636f70652d68656164726f6f6d2e6a736f6e504b05060000000001000100450000007b0000000000";

#[test]
fn parses_the_pulp_example() {
    let config = pulp();
    assert_eq!(config.error, None);
    assert!(config.applies());
    assert!(config.advises());
    assert!(config.allows("test/ctest_script_inputs/tests/a.json"));
    assert!(config.allows(".shipyard/changed-surface-families.toml"));
    assert!(!config.allows("test/ctest_script_inputs"));
    assert!(!config.allows("test/ctest_script_inputs.json"));
    assert!(!config.allows(".shipyard/config.toml"));
    assert!(!config.allows("test/ctest_script_inputs_other/a.json"));
}

#[test]
fn absent_or_malformed_config_never_applies() {
    assert!(!DerivedFilesConfig::from_config_text("[project]\nname = \"x\"\n").applies());
    for bad in [
        "[derived_files]\npaths = \"test/\"\n",
        "[derived_files]\npaths = [\"../etc/\"]\n[[derived_files.artifacts]]\nname = \"a\"\nroot = \"\"\n",
        "[derived_files]\npaths = [\"a/\"]\n[[derived_files.artifacts]]\nname = \"a\"\n",
        "[derived_files]\npaths = [\"a/\"]\ncommit_trailers = [\"not a trailer\"]\n[[derived_files.artifacts]]\nname = \"a\"\nroot = \"a\"\n",
        "not toml [",
    ] {
        let config = DerivedFilesConfig::from_config_text(bad);
        assert!(!config.applies(), "{bad}");
        assert!(!config.advises(), "{bad}");
    }
}

#[test]
fn normalize_refuses_escapes() {
    for bad in [
        "",
        "/etc/passwd",
        "../x",
        "a/../b",
        "a/./b",
        "a//b",
        "a\\b",
        "C:/x",
        "a/.git/config",
        ".GIT/hooks/pre-commit",
        "a\u{0}b",
        "a\nb",
    ] {
        assert!(normalize_path(bad).is_err(), "{bad:?}");
    }
    assert_eq!(normalize_path("tests/a.json").unwrap(), "tests/a.json");
}

#[test]
fn destination_maps_inside_the_allowlist_only() {
    let config = pulp();
    let mapping = config.mapping("script-inputs-expected").unwrap().clone();
    assert_eq!(
        destination(&config, &mapping, "tests/a.json").unwrap(),
        "test/ctest_script_inputs/tests/a.json"
    );
    assert!(destination(&config, &mapping, "../ctest_script_inputs.json").is_err());
    let root = ArtifactMapping {
        name: "anything".to_owned(),
        root: String::new(),
    };
    let error = destination(&config, &root, ".github/workflows/evil.yml").unwrap_err();
    assert!(
        error.contains("outside the derived-file allowlist"),
        "{error}"
    );
    assert!(destination(&config, &root, ".shipyard/changed-surface-families.toml").is_ok());
}

#[test]
fn reads_a_real_github_artifact() {
    let bytes = hex::decode(REAL_ARTIFACT_HEX).unwrap();
    let entries = safe_zip::read(&bytes, &Limits::DEFAULT).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "a2t-scope-headroom.json");
    assert_eq!(
        entries[0].data,
        b"{\"count\": 251, \"limit\": 512, \"headroom\": 261, \"budget\": 384}\n"
    );
}

#[test]
fn reads_stored_and_deflated_entries() {
    let mut stored = TestEntry::file("tests/b.json", b"{}\n");
    stored.deflate = false;
    let bytes = build(&[TestEntry::file("tests/a.json", b"{\"a\": 1}\n"), stored]);
    let entries = safe_zip::read(&bytes, &Limits::DEFAULT).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].data, b"{}\n");
}

#[test]
fn refuses_symlink_entries() {
    let bytes = build(&[TestEntry::file("tests/a.json", b"/etc/passwd").with_mode(0o120_777)]);
    let error = safe_zip::read(&bytes, &Limits::DEFAULT).unwrap_err();
    assert!(error.contains("not a regular file"), "{error}");
}

#[test]
fn refuses_special_mode_bits() {
    let bytes = build(&[TestEntry::file("tests/a.json", b"x").with_mode(0o104_755)]);
    let error = safe_zip::read(&bytes, &Limits::DEFAULT).unwrap_err();
    assert!(error.contains("setuid"), "{error}");
}

#[test]
fn refuses_duplicate_names_by_case() {
    let bytes = build(&[
        TestEntry::file("tests/a.json", b"1"),
        TestEntry::file("tests/A.json", b"2"),
    ]);
    let error = safe_zip::read(&bytes, &Limits::DEFAULT).unwrap_err();
    assert!(error.contains("duplicate"), "{error}");
}

#[test]
fn refuses_lying_sizes_and_bombs() {
    let mut liar = TestEntry::file("tests/a.json", &[b'a'; 4096]);
    liar.claimed_size = Some(10);
    let error = safe_zip::read(&build(&[liar]), &Limits::DEFAULT).unwrap_err();
    assert!(error.contains("the directory says"), "{error}");

    let limits = Limits {
        max_entry_bytes: 1024,
        ..Limits::DEFAULT
    };
    let mut bomb = TestEntry::file("tests/a.json", &vec![0_u8; 1 << 20]);
    bomb.claimed_size = Some(100);
    let error = safe_zip::read(&build(&[bomb]), &limits).unwrap_err();
    assert!(error.contains("limit"), "{error}");
}

#[test]
fn refuses_corruption() {
    let mut bytes = build(&[TestEntry::file("tests/a.json", b"hello world")]);
    // Flip the stored CRC in the central directory.
    let cd = bytes
        .windows(4)
        .position(|w| w == 0x0201_4b50_u32.to_le_bytes())
        .unwrap();
    bytes[cd + 16] ^= 0xff;
    assert!(safe_zip::read(&bytes, &Limits::DEFAULT).is_err());
    assert!(safe_zip::read(b"not a zip at all, just text", &Limits::DEFAULT).is_err());
}

#[test]
fn commit_message_carries_gate_trailers() {
    let config = pulp();
    let message = commit_message(
        42,
        &[
            "Version-Bump: skip reason=\"generated\"".to_owned(),
            "Co-Authored-By: Someone <x@example.com>".to_owned(),
            "Skill-Update: skip skill=ci reason=\"none\"".to_owned(),
        ],
        &config,
    );
    assert_eq!(
        message,
        "chore: apply CI's expected derived files from run 42\n\nVersion-Bump: skip \
         reason=\"generated\"\nSkill-Update: skip skill=ci reason=\"none\"\n"
    );
    assert_eq!(
        commit_message(7, &[], &config),
        "chore: apply CI's expected derived files from run 7\n"
    );
}

fn failing(context: &str, tests: Option<&[&str]>) -> FailingCheck {
    FailingCheck {
        context: context.to_owned(),
        failed_tests: tests.map(|tests| tests.iter().map(|t| (*t).to_owned()).collect()),
    }
}

#[test]
fn drift_only_needs_every_failure_to_be_drift() {
    let config = pulp();
    let drift = [failing("macos", Some(&["script-test-inputs-drift"]))];
    assert!(drift_only(&config, &drift, true));
    // No artifact at the head.
    assert!(!drift_only(&config, &drift, false));
    // Another failing test on the same check.
    let mixed = [failing(
        "macos",
        Some(&["script-test-inputs-drift", "format-header-view-boundary"]),
    )];
    assert!(!drift_only(&config, &mixed, true));
    // Another failing check.
    let two = [
        failing(
            "drift-fast",
            Some(&["changed-surface-script-families-drift"]),
        ),
        failing("macos", Some(&["format-header-view-boundary"])),
    ];
    assert!(!drift_only(&config, &two, true));
    // A log not read, or a failure that named no test.
    assert!(!drift_only(&config, &[failing("macos", None)], true));
    assert!(!drift_only(&config, &[failing("macos", Some(&[]))], true));
    assert!(!drift_only(&config, &[], true));
    // Unconfigured.
    assert!(!drift_only(&DerivedFilesConfig::default(), &drift, true));
}
