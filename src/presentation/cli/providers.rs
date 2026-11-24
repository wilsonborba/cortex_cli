//! Provider key setup and one-shot API requests.

use crate::{
    dal::{local::provider_key, remote::provider::ProviderClient},
    domain::models::provider::Provider,
    presentation::cli::commands::{ChatOptions, ProviderCommand},
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal,
};
use std::io::{self, IsTerminal, Write};
use zeroize::Zeroizing;

struct RawInput;

impl RawInput {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn read_key() -> io::Result<Zeroizing<String>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(io::Error::other(
            "Provider keys require an interactive terminal.",
        ));
    }
    let _raw = RawInput::enter()?;
    let mut value = Zeroizing::new(String::new());
    loop {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Enter => break,
                KeyCode::Esc => return Err(io::Error::other("Provider key entry cancelled.")),
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(_) if !key.modifiers.is_empty() => {}
                KeyCode::Char(c) => value.push(c),
                _ => {}
            }
        }
    }
    if value.trim().is_empty() {
        return Err(io::Error::other("Provider key cannot be empty."));
    }
    Ok(value)
}

pub fn manage(command: ProviderCommand, out: &mut impl Write) -> io::Result<()> {
    match command {
        ProviderCommand::Set(provider) => {
            write!(
                io::stderr().lock(),
                "Enter {} API key (hidden): ",
                provider.display_name()
            )?;
            io::stderr().flush()?;
            let key = read_key()?;
            writeln!(io::stderr().lock())?;
            provider_key::write(provider, key.as_str()).map_err(io::Error::other)?;
            writeln!(
                out,
                "{} API key saved in the OS keyring.",
                provider.display_name()
            )
        }
        ProviderCommand::Remove(provider) => {
            provider_key::delete(provider).map_err(io::Error::other)?;
            writeln!(
                out,
                "{} API key removed from the OS keyring.",
                provider.display_name()
            )
        }
        ProviderCommand::List => {
            for provider in Provider::ALL {
                let state = if provider_key::configured(provider).map_err(io::Error::other)? {
                    "set"
                } else {
                    "not set"
                };
                writeln!(out, "{}: {state}", provider.id())?;
            }
            Ok(())
        }
    }
}

pub fn complete(options: ChatOptions, out: &mut impl Write) -> io::Result<()> {
    let provider = options
        .provider
        .ok_or_else(|| io::Error::other("Provider is required."))?;
    let model = options
        .model
        .ok_or_else(|| io::Error::other("Model is required."))?;
    let prompt = options
        .prompt
        .ok_or_else(|| io::Error::other("Prompt is required."))?;
    let key = provider_key::read(provider)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("Set this provider key first with cortex provider set."))?;
    let client = ProviderClient::new().map_err(io::Error::other)?;
    let answer = client
        .complete(provider, key.as_str(), &model, &prompt)
        .map_err(io::Error::other)?;
    writeln!(out, "{answer}")
}
