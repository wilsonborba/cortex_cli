//! The CLI prints one link and waits for explicit input before each Finish request.

use std::io::{self, BufRead, Write};

use crate::{
    core::settings,
    dal::{
        local::session::OsSessionStore,
        remote::{gateway::GatewayClient, login_context},
    },
    domain::{
        models::native_auth::AuthFailure,
        services::auth::{AuthService, Gateway, SessionStore},
    },
};

use super::commands::Command;

fn io_error(error: AuthFailure) -> io::Error {
    io::Error::other(error.to_string())
}

pub fn execute(command: Command, output: &mut impl Write) -> io::Result<()> {
    let gateway = GatewayClient::new(settings::GATEWAY_URL).map_err(io_error)?;
    let mut service = AuthService::new(gateway, OsSessionStore);
    if command == Command::Logout {
        service.logout().map_err(io_error)?;
        return writeln!(output, "Signed out of this device.");
    }
    let handoff = service.start().map_err(io_error)?;
    let url = login_context::browser_url(handoff).map_err(io_error)?;
    writeln!(
        output,
        "Sign in to Cortex. Match code: {}",
        handoff.match_code
    )?;
    writeln!(
        output,
        "Continue only if the browser shows the same code.\n{url}"
    )?;
    finish_from_input(&mut service, &mut io::stdin().lock(), output)
}

pub fn finish_from_input<G: Gateway, S: SessionStore>(
    service: &mut AuthService<G, S>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<()> {
    loop {
        writeln!(output, "Press Enter when you finish, or type cancel.")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 || line.trim().eq_ignore_ascii_case("cancel") {
            service.cancel().map_err(io_error)?;
            return writeln!(output, "Login cancelled.");
        }
        if !line.trim().is_empty() {
            continue;
        }
        match service.finish() {
            Ok(result) => {
                writeln!(output, "{}", result.location_message)?;
                if !result.persisted {
                    writeln!(output, "{}", AuthFailure::Keyring)?;
                }
                return Ok(());
            }
            Err(
                error @ (AuthFailure::NotFinished
                | AuthFailure::Transport
                | AuthFailure::RateLimited),
            ) => writeln!(output, "{error}")?,
            Err(error) => return Err(io_error(error)),
        }
    }
}
