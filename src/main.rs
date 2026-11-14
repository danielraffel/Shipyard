//! Binary entrypoint for Shipyard.

use std::process::ExitCode;

#[cfg(not(windows))]
fn main() -> ExitCode {
    if let Some(code) = shipyard::queue_arm_gate::relaunch_without_inherited_queue_authority() {
        return code;
    }
    // A launchd job whose program is this release binary becomes the stable
    // ShipyardLauncher.app identity when the host activated it (returns only
    // when it did not).
    #[cfg(target_os = "macos")]
    shipyard::launcher_app::route_launchd_job();
    shipyard::app::run()
}

#[cfg(windows)]
fn main() -> ExitCode {
    if let Some(code) = shipyard::queue_arm_gate::relaunch_without_inherited_queue_authority() {
        return code;
    }
    // The Windows process main thread has a smaller default stack than the
    // CLI's production dispatch shape requires. Match the test-worker stack
    // contract without changing Unix process or signal semantics.
    std::thread::Builder::new()
        .name("shipyard-cli".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(shipyard::app::run)
        .expect("failed to start Shipyard CLI thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}
