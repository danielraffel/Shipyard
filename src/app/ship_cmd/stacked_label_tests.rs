//! `shipyard pr --stacked-on <n>` labels the pull request `stacked:<n>`.

use std::cell::RefCell;

use super::stacked_label_line;

#[test]
fn the_stacked_label_names_the_parent_on_the_new_pull_request() {
    let calls = RefCell::new(Vec::new());
    let gh = |argv: &[String]| -> Result<String, String> {
        calls.borrow_mut().push(argv.join(" "));
        Ok("[]".to_owned())
    };
    let line = stacked_label_line(&gh, "Generous-Corp/pulp", 42, 41);
    assert_eq!(
        calls.borrow().as_slice(),
        ["api -X POST repos/Generous-Corp/pulp/issues/42/labels -f labels[]=stacked:41"]
    );
    assert!(line.contains("Labelled #42 `stacked:41`"), "{line}");
}

#[test]
fn a_failed_label_write_is_reported_not_swallowed() {
    let gh = |_: &[String]| -> Result<String, String> { Err("HTTP 403".to_owned()) };
    let line = stacked_label_line(&gh, "Generous-Corp/pulp", 42, 41);
    assert!(
        line.starts_with("⚠︎ Could not label #42 `stacked:41`"),
        "{line}"
    );
    assert!(line.contains("HTTP 403"), "{line}");
}
