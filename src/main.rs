use std::process::ExitCode;

fn main() -> ExitCode {
    cortex_cli::app::run(std::env::args_os().skip(1))
}
