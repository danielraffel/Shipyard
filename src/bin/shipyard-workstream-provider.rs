//! Release-paired Shipyard companion executable.
//!
//! The verified installer, `shipyard fleet update`, and the project CLI hook
//! all require this executable to sit beside `shipyard` in the same directory
//! at the same version, so it reports its own version and serves the bounded
//! read-only remote cache observation endpoint the parallel-proof canary
//! invokes over its authenticated carrier.

fn main() -> std::process::ExitCode {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    match (arguments.next(), arguments.next()) {
        (Some(flag), None) if flag == "--version" => {
            println!("shipyard-workstream-provider {}", env!("CARGO_PKG_VERSION"));
            std::process::ExitCode::SUCCESS
        }
        (Some(flag), None) if flag == "--observe-m1-cache" => {
            match shipyard::parallel_proof_canary_remote_cache::run_remote_m1_cache_observer_stdio()
            {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("shipyard-workstream-provider: {error}");
                    std::process::ExitCode::from(2)
                }
            }
        }
        _ => {
            eprintln!("shipyard-workstream-provider: unsupported arguments");
            std::process::ExitCode::from(2)
        }
    }
}
