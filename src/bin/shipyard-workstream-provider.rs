//! Protected stdio entrypoint for Shipyard's cmux workstream provider.

fn main() -> std::process::ExitCode {
    match shipyard::workstream_provider_adapter::run_stdio() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("shipyard-workstream-provider: {error}");
            std::process::ExitCode::from(2)
        }
    }
}
