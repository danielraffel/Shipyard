//! Binary entrypoint for Shipyard.

use std::process::ExitCode;

#[cfg(not(windows))]
fn main() -> ExitCode {
    shipyard::app::run()
}

#[cfg(windows)]
fn main() -> ExitCode {
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
