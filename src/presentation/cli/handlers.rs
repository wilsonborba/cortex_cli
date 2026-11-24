//! Coordinate terminal commands without business rules.

use std::io::{self, Write};

use super::{commands::Command, formatters};

pub fn execute(command: Command, output: &mut impl Write) -> io::Result<()> {
    match command {
        Command::Help => output.write_all(formatters::help().as_bytes()),
        Command::Version => writeln!(output, "{}", formatters::version()),
    }
}
