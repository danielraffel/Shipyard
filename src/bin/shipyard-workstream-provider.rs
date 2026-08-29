//! Protected stdio entrypoint for Shipyard's cmux workstream provider.

fn main() -> std::process::ExitCode {
    if std::env::args_os().len() == 2
        && std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--version"))
    {
        println!("shipyard-workstream-provider {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    match shipyard::workstream_provider_adapter::run_stdio() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("shipyard-workstream-provider: {error}");
            std::process::ExitCode::from(2)
        }
    }
}
