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
                Err(error) => {
                    // All adapter failures reach this boundary as fixed, safe messages.
                    let message = if matches!(
                        command,
                        commands::Command::Login | commands::Command::Logout
                    ) {
                        error.to_string()
                    } else {
                        "cannot write terminal output".to_owned()
                    };
                    crate::core::logs::error(&message);
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
