//! Fail-closed companion entrypoint for protected workstream recovery.

fn main() -> std::process::ExitCode {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    match (arguments.next(), arguments.next()) {
        (Some(flag), None) if flag == "--version" => {
            println!("shipyard-workstream-provider {}", env!("CARGO_PKG_VERSION"));
            std::process::ExitCode::SUCCESS
        }
        (Some(flag), Some(path)) if flag == "--launch-capsule" && arguments.next().is_none() => {
            match shipyard::workstream_provider_adapter::run_launch_capsule(std::path::Path::new(
                &path,
            )) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("shipyard-workstream-provider: {error}");
                    std::process::ExitCode::from(2)
                }
            }
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
        (None, None) => match shipyard::workstream_provider_adapter::run_stdio() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("shipyard-workstream-provider: {error}");
                std::process::ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("shipyard-workstream-provider: unsupported arguments");
            std::process::ExitCode::from(2)
        }
    }
}
