//! Command syntax only. Never echo unknown arguments, which can contain secrets.

use crate::domain::models::provider::Provider;
use std::ffi::OsString;

#[derive(PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Login,
    Logout,
    Provider(ProviderCommand),
    RemoveVoiceModel,
    Chat(ChatOptions),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProviderCommand {
    Set(Provider),
    Remove(Provider),
    List,
}

#[derive(Default, PartialEq, Eq)]
pub struct ChatOptions {
    pub prompt: Option<String>,
    pub conversation: Option<String>,
    pub continue_latest: bool,
    pub remote_control: bool,
    pub provider: Option<Provider>,
    pub model: Option<String>,
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
    if values.first().is_some_and(|value| value == "provider") {
        return parse_provider(&values[1..]);
    }
    if values.len() == 2 && values[0] == "voice" && values[1] == "--remove-model" {
        return Ok(Command::RemoveVoiceModel);
    }
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
            "--remote-control" | "--remote" => {
                if options.remote_control {
                    return Err(ParseError);
                }
                options.remote_control = true;
            }
            "--provider" => {
                if options.provider.is_some() {
                    return Err(ParseError);
                }
                options.provider =
                    Some(Provider::parse(args.next().ok_or(ParseError)?).ok_or(ParseError)?);
            }
            "--model" => {
                if options.model.is_some() {
                    return Err(ParseError);
                }
                let model = args.next().ok_or(ParseError)?;
                if model.trim().is_empty() || model.contains(['\r', '\n']) {
                    return Err(ParseError);
                }
                options.model = Some(model.clone());
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
            _ => return Err(ParseError),
        }
    }
    if (options.remote_control && options.prompt.is_some())
        || options.provider.is_some() != options.model.is_some()
        || options.provider.is_some()
            && (options.prompt.is_none()
                || options.continue_latest
                || options.conversation.is_some())
    {
        return Err(ParseError);
    }
    Ok(Command::Chat(options))
}

fn parse_provider(values: &[String]) -> Result<Command, ParseError> {
    match values {
        [action, id] if action == "set" => Ok(Command::Provider(ProviderCommand::Set(
            Provider::parse(id).ok_or(ParseError)?,
        ))),
        [action, id] if action == "remove" => Ok(Command::Provider(ProviderCommand::Remove(
            Provider::parse(id).ok_or(ParseError)?,
        ))),
        [action] if action == "list" => Ok(Command::Provider(ProviderCommand::List)),
        _ => Err(ParseError),
    }
}
