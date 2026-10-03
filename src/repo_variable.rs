//! Read and write one GitHub Actions repository variable through `gh api`.
//!
//! Callers pass the `gh` runner as a closure (`gh(args) -> stdout`), the same
//! seam [`crate::base_health::read_latest`] uses, so the decision code above
//! this module is testable with an in-process fake and every mutation is an
//! explicit argv.

/// What a write did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOutcome {
    /// The variable existed and now holds the value.
    Updated,
    /// The variable did not exist and was created with the value.
    Created,
}

/// The `gh api` argv that writes `name=value` with `method` at `path`.
#[must_use]
pub fn write_args(method: &str, path: &str, name: &str, value: &str) -> Vec<String> {
    vec![
        "api".to_owned(),
        "--method".to_owned(),
        method.to_owned(),
        path.to_owned(),
        "--raw-field".to_owned(),
        format!("name={name}"),
        "--raw-field".to_owned(),
        format!("value={value}"),
    ]
}

/// Whether a `gh` failure message is GitHub's 404.
#[must_use]
pub fn is_not_found(message: &str) -> bool {
    message.to_ascii_lowercase().contains("http 404")
}

/// Read the variable's value; `Ok(None)` when it does not exist.
///
/// # Errors
///
/// Any failure other than a 404, with the `gh` message. A caller that gates
/// anything on the value must treat an error as "unknown", never as unset.
pub fn read<F>(gh: &F, repo: &str, name: &str) -> Result<Option<String>, String>
where
    F: Fn(&[String]) -> Result<String, String> + ?Sized,
{
    let args = vec![
        "api".to_owned(),
        format!("repos/{repo}/actions/variables/{name}"),
        "--jq".to_owned(),
        ".value".to_owned(),
    ];
    match gh(&args) {
        // `--jq` ends its output with one newline; the value itself may hold
        // surrounding whitespace, which the caller decides how to read.
        Ok(raw) => Ok(Some(raw.strip_suffix('\n').unwrap_or(&raw).to_owned())),
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Set the variable, creating it when it does not exist.
///
/// # Errors
///
/// The `gh` message when neither the update nor the create succeeds.
pub fn write<F>(gh: &F, repo: &str, name: &str, value: &str) -> Result<WriteOutcome, String>
where
    F: Fn(&[String]) -> Result<String, String> + ?Sized,
{
    let path = format!("repos/{repo}/actions/variables/{name}");
    match gh(&write_args("PATCH", &path, name, value)) {
        Ok(_) => Ok(WriteOutcome::Updated),
        Err(error) if is_not_found(&error) => {
            let create = format!("repos/{repo}/actions/variables");
            gh(&write_args("POST", &create, name, value)).map(|_| WriteOutcome::Created)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn read_returns_the_value_none_when_unset_and_an_error_otherwise() {
        let live = |_: &[String]| Ok::<_, String>("live\n".to_owned());
        assert_eq!(read(&live, "o/r", "V"), Ok(Some("live".to_owned())));
        let unset = |_: &[String]| Err::<String, _>("gh: Not Found (HTTP 404)".to_owned());
        assert_eq!(read(&unset, "o/r", "V"), Ok(None));
        let forbidden = |_: &[String]| Err::<String, _>("gh: HTTP 403 forbidden".to_owned());
        assert!(
            read(&forbidden, "o/r", "V").is_err(),
            "a 403 is unknown, not unset"
        );
    }

    #[test]
    fn write_patches_and_creates_only_on_a_404() {
        let calls = RefCell::new(Vec::<String>::new());
        let missing = |args: &[String]| {
            calls.borrow_mut().push(args.join(" "));
            if args[2] == "PATCH" {
                Err("HTTP 404".to_owned())
            } else {
                Ok(String::new())
            }
        };
        assert_eq!(
            write(&missing, "o/r", "V", "off"),
            Ok(WriteOutcome::Created)
        );
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains("PATCH repos/o/r/actions/variables/V "));
        assert!(calls[1].contains("POST repos/o/r/actions/variables "));
        assert!(calls[1].contains("value=off"));

        let failing = |args: &[String]| {
            if args[2] == "PATCH" {
                Err("HTTP 500".to_owned())
            } else {
                Ok(String::new())
            }
        };
        assert!(
            write(&failing, "o/r", "V", "off").is_err(),
            "only a 404 falls back to create"
        );
    }
}
