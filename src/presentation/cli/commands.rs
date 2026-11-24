//! Command syntax only. Never echo unknown arguments, which can contain secrets.

use std::ffi::OsString;

#[derive(PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Login,
    Logout,
    Chat(ChatOptions),
}

#[derive(Default, PartialEq, Eq)]
pub struct ChatOptions {
    pub prompt: Option<String>,
    pub conversation: Option<String>,
    pub continue_latest: bool,
    pub note_ids: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError;

impl ParseError {
    pub fn message(&self) -> &'static str {
        "unsupported arguments; use --help for usage"
    }
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, ParseError> {
    let values: Vec<String> = args
        .into_iter()
        .map(|s| s.into_string().map_err(|_| ParseError))
        .collect::<Result<_, _>>()?;
    if values.len() == 1 {
        match values[0].as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--version" | "-V" => return Ok(Command::Version),
            "login" => return Ok(Command::Login),
            "logout" => return Ok(Command::Logout),
            _ => {}
        }
    }
    let mut options = ChatOptions::default();
    let mut args = values.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--print" | "-p" => {
                if options.prompt.is_some() {
                    return Err(ParseError);
                }
                let prompt = args.next().ok_or(ParseError)?;
                if prompt.trim().is_empty() {
                    return Err(ParseError);
                }
                options.prompt = Some(prompt.clone());
            }
            "--continue" | "-c" => {
                if options.continue_latest || options.conversation.is_some() {
                    return Err(ParseError);
                }
                options.continue_latest = true;
            }
            "--conversation" | "--resume" | "-r" => {
                if options.continue_latest || options.conversation.is_some() {
                    return Err(ParseError);
                }
                let id = args.next().ok_or(ParseError)?;
                if !crate::domain::models::chat::valid_id(id) {
                    return Err(ParseError);
                }
                options.conversation = Some(id.to_ascii_lowercase());
            }
            "--note-id" => {
                let id = args.next().ok_or(ParseError)?;
                if !crate::domain::models::chat::valid_id(id)
                    || options
                        .note_ids
                        .iter()
                        .any(|selected| selected.eq_ignore_ascii_case(id))
                {
                    return Err(ParseError);
                }
                options.note_ids.push(id.to_ascii_lowercase());
            }
            _ => return Err(ParseError),
        }
    }
    Ok(Command::Chat(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn note_context_requires_explicit_unique_valid_ids() {
        let Command::Chat(options) = parse(args(&[
            "--note-id",
            "10000000-0000-4000-8000-000000000001",
            "--print",
            "Explain this",
        ]))
        .unwrap() else {
            panic!("expected chat command")
        };
        assert_eq!(options.note_ids, ["10000000-0000-4000-8000-000000000001"]);
        assert!(parse(args(&["--note-id", "not-a-note", "--print", "hello"])).is_err());
        assert!(
            parse(args(&[
                "--note-id",
                "10000000-0000-4000-8000-000000000001",
                "--note-id",
                "10000000-0000-4000-8000-000000000001",
                "--print",
                "hello",
            ]))
            .is_err()
        );
    }
}
