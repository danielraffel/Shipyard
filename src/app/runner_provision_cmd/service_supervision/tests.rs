//! Tests for the restart-on-exit patch.
//!
//! The fixture is the real plist that `svc.sh install` wrote for
//! `Shipyard-studio-02` — the runner a force-push removed permanently.

use super::*;

/// The installer's actual output shape: `RunAtLoad`, no `KeepAlive`.
fn as_the_installer_writes_it() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
  <dict>
      <key>Label</key>
      <string>actions.runner.danielraffel-Shipyard.Shipyard-studio-02</string>
      <key>ProgramArguments</key>
      <array>
        <string>/Users/danielraffel/actions-ci/Shipyard-studio-02/runsvc.sh</string>
      </array>
      <key>UserName</key>
      <string>danielraffel</string>
      <key>WorkingDirectory</key>
      <string>/Users/danielraffel/actions-ci/Shipyard-studio-02</string>
      <key>RunAtLoad</key>
      <true/>
  </dict>
</plist>
"#
    .to_owned()
}

#[test]
fn the_installers_plist_gains_a_restart_policy() {
    let patched = ensure_restart_on_exit(&as_the_installer_writes_it(), false)
        .expect("patch")
        .expect("a change was needed");

    assert!(patched.contains("<key>KeepAlive</key>"), "{patched}");
    // The policy must be enabled, not merely mentioned.
    let at = patched.find("<key>KeepAlive</key>").expect("key");
    assert!(
        patched[at..]
            .trim_start_matches(|c| c != '\n')
            .trim_start()
            .starts_with("<true/>"),
        "{patched}"
    );
}

/// Everything the installer wrote must survive. A patch that silently drops the
/// program arguments would produce a service that loads and runs nothing —
/// indistinguishable, from GitHub's side, from the outage it is meant to fix.
#[test]
fn the_patch_preserves_every_original_entry() {
    let original = as_the_installer_writes_it();
    let patched = ensure_restart_on_exit(&original, false)
        .expect("patch")
        .expect("changed");

    for required in [
        "<key>Label</key>",
        "actions.runner.danielraffel-Shipyard.Shipyard-studio-02",
        "<key>ProgramArguments</key>",
        "runsvc.sh",
        "<key>WorkingDirectory</key>",
        "<key>RunAtLoad</key>",
        "</plist>",
    ] {
        assert!(
            patched.contains(required),
            "lost {required} from:\n{patched}"
        );
    }
    assert!(patched.len() > original.len());
}

#[test]
fn the_inserted_keys_keep_the_anchors_indentation() {
    let patched = ensure_restart_on_exit(&as_the_installer_writes_it(), false)
        .expect("patch")
        .expect("changed");
    assert!(
        patched.contains("      <key>KeepAlive</key>\n      <true/>\n      <key>RunAtLoad</key>"),
        "{patched}"
    );
}

// ---------------------------------------------------------------------------
// Idempotence, and the control that proves the check is real
// ---------------------------------------------------------------------------

/// Re-provisioning must not stack a second `KeepAlive` into the dict. A plist
/// with a duplicate key is malformed, so a non-idempotent patch would turn a
/// merely-unsupervised runner into an unloadable one.
#[test]
fn negative_control_an_already_supervised_definition_is_left_alone() {
    let already = ensure_restart_on_exit(&as_the_installer_writes_it(), false)
        .expect("first")
        .expect("changed");

    assert_eq!(
        ensure_restart_on_exit(&already, false).expect("second"),
        None,
        "a second pass must report no change"
    );
    assert_eq!(already.matches("<key>KeepAlive</key>").count(), 1);
}

/// Pairing control: the unpatched fixture *does* need a change. Without this,
/// "already supervised is left alone" would also pass for a function that never
/// patched anything at all.
#[test]
fn control_an_unsupervised_definition_does_need_a_change() {
    assert!(
        ensure_restart_on_exit(&as_the_installer_writes_it(), false)
            .expect("patch")
            .is_some()
    );
}

// ---------------------------------------------------------------------------
// Ephemeral runners must NOT be given a restart policy
// ---------------------------------------------------------------------------

/// An ephemeral runner is supposed to exit after one job. Restarting it in
/// place would respawn it forever and defeat the isolation it exists for, so
/// the "fix" would be a new bug.
#[test]
fn negative_control_an_ephemeral_runner_is_never_given_a_restart_policy() {
    assert_eq!(
        ensure_restart_on_exit(&as_the_installer_writes_it(), true).expect("ephemeral"),
        None
    );
}

// ---------------------------------------------------------------------------
// Refuse rather than guess
// ---------------------------------------------------------------------------

/// If the installer's template changes shape, this must fail loudly. Returning
/// the definition unpatched would recreate the original bug invisibly on every
/// runner provisioned from then on — the exact silent-degradation this whole
/// workstream exists to prevent.
#[test]
fn negative_control_an_unrecognised_definition_errors_rather_than_passing_through() {
    let unfamiliar = "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
                      <key>Label</key><string>x</string></dict></plist>";
    let error = ensure_restart_on_exit(unfamiliar, false)
        .expect_err("an unrecognised shape must not be silently accepted");
    assert!(error.contains("RunAtLoad"), "{error}");
    assert!(error.contains("untouched"), "{error}");
}

/// Control for the one above: the recognised shape does not error, so the
/// refusal is a real discrimination rather than a function that always fails.
#[test]
fn control_the_recognised_shape_does_not_error() {
    assert!(ensure_restart_on_exit(&as_the_installer_writes_it(), false).is_ok());
}
