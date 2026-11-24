//! Application composition and process exit handling.

use std::{ffi::OsString, io, process::ExitCode};

use crate::presentation::cli::{commands, handlers};

pub fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    match commands::parse(args) {
        Ok(command) => {
            let result = handlers::execute(command, &mut io::stdout().lock());
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
                Err(_) => {
                    crate::core::logs::error("cannot write terminal output");
                    ExitCode::FAILURE
                }
            }
        }
        Err(error) => {
            crate::core::logs::error(error.message());
            ExitCode::from(2)
        }
    }
}
