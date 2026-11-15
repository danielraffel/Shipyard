use super::{repository_config_query, retired_tables, retired_tables_warning};

/// Pulp's real `[train]` block and an `[auto_merge.rollup]` table.
const RETIRED: &str = include_str!("../../../tests/fixtures/retired-config/pulp-train.toml");

/// Settings the base-config readers do act on, so a reader that broke on the
/// retired tables would show up as a changed policy, not only as an error.
const LIVE: &str = r#"
[auto_merge]
arm_requires_head_approval = true
reviewer_identities = ["pulp-reviewer"]
carry_excluded_paths_extra = ["docs/private/**"]

[auto_merge.dwell]
required_pending = 45
act = "on"

[base_health]
auto_jump = "dry-run"
merge_group_hold = "on"

[queue]
tiered_required_checks = ["slow"]
tier_record_app = "tier-recorder"
"#;

/// One base-config reader, rendered for comparison.
type Reader = Box<dyn Fn(&toml::Table) -> String>;

fn table(text: &str) -> toml::Table {
    text.parse::<toml::Table>().expect("toml")
}

#[test]
fn repository_config_query_encodes_the_base_ref() {
    assert_eq!(
        repository_config_query("owner/repo", "release/1.0?x=y&z"),
        "repos/owner/repo/contents/.shipyard/config.toml?ref=release%2F1.0%3Fx%3Dy%26z"
    );
}

#[test]
fn retired_tables_are_named_and_others_are_not() {
    assert_eq!(
        retired_tables(&table(RETIRED)),
        vec!["[train]", "[auto_merge.rollup]"]
    );
    assert!(retired_tables(&table(LIVE)).is_empty());
    assert_eq!(
        retired_tables(&table("[train]\n")),
        vec!["[train]"],
        "an empty [train] is still a retired table"
    );
    let warning = retired_tables_warning("o/r", "main", &table(RETIRED)).expect("warning");
    assert!(
        warning.starts_with(
            "[train] / [auto_merge.rollup] in .shipyard/config.toml on o/r@main are no longer read"
        ),
        "{warning}"
    );
    assert!(retired_tables_warning("o/r", "main", &table(LIVE)).is_none());
}

/// Every base-config reader reads the same policy with Pulp's real `[train]`
/// block and an `[auto_merge.rollup]` table present as without them: the
/// retired tables are ignored, never an error.
#[test]
fn every_base_config_reader_ignores_the_retired_tables() {
    let without = table(LIVE);
    let with = table(&format!("{LIVE}\n{RETIRED}"));
    let both = |read: &dyn Fn(&toml::Table) -> String| (read(&without), read(&with));

    let readers: Vec<(&str, Reader)> = vec![
        (
            "ApprovalPolicy::from_table",
            Box::new(|t| format!("{:?}", crate::head_approval::ApprovalPolicy::from_table(t))),
        ),
        (
            "ApprovalPolicy::from_table_for_repo",
            Box::new(|t| {
                format!(
                    "{:?}",
                    crate::head_approval::ApprovalPolicy::from_table_for_repo(
                        t,
                        "Generous-Corp/pulp",
                        true
                    )
                )
            }),
        ),
        (
            "DwellPolicy::from_table",
            Box::new(|t| {
                format!(
                    "{:?}",
                    crate::merge_steward::dwell::DwellPolicy::from_table(t)
                )
            }),
        ),
        (
            "EjectionGuards::from_table",
            Box::new(|t| {
                format!(
                    "{:?}",
                    crate::merge_steward::EjectionGuards::from_table(t, true)
                )
            }),
        ),
        (
            "FlakePolicy::from_table",
            Box::new(|t| {
                format!(
                    "{:?}",
                    crate::merge_steward::flake_requeue::FlakePolicy::from_table(t, true)
                )
            }),
        ),
        (
            "BasePoisonPolicy::from_table",
            Box::new(|t| {
                format!(
                    "{:?}",
                    crate::base_health::BasePoisonPolicy::from_table(t, true)
                )
            }),
        ),
    ];
    for (name, read) in &readers {
        let (plain, retired) = both(read.as_ref());
        assert_eq!(plain, retired, "{name} reads the retired tables");
    }

    let tier = |text: &str| crate::deferred_tier::tier_policy_from_config(text);
    assert_eq!(
        tier(LIVE),
        tier(&format!("{LIVE}\n{RETIRED}")),
        "tier_policy_from_config reads the retired tables"
    );
    assert!(
        tier(LIVE).is_ok_and(|policy| policy != crate::deferred_tier::TierPolicy::default()),
        "the live table must be one the reader acts on"
    );
}
