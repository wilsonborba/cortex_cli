//! Command syntax only. Never echo unknown arguments, which can contain secrets.

use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Login,
    Logout,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError;

impl ParseError {
    pub fn message(&self) -> &'static str {
        "unsupported arguments; use --help for usage"
    }
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, ParseError> {
    let mut args = args.into_iter();
    let command = match args.next() {
        None => Command::Help,
        Some(value) => match value.to_str() {
            Some("--help" | "-h") => Command::Help,
            Some("--version" | "-V") => Command::Version,
            Some("login") => Command::Login,
            Some("logout") => Command::Logout,
            _ => return Err(ParseError),
        },
    };
    if args.next().is_some() {
        return Err(ParseError);
    }
    Ok(command)
}
