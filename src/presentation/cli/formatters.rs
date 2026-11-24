//! Plain terminal output. No alternate screen or terminal mode changes.

use crate::core::settings;

pub fn help() -> &'static str {
    "Cortex - command-line client for macOS and Linux\n\nUsage: cortex [COMMAND] [OPTIONS]\n\nStart without options for an inline conversation.\n\nCommands:\n  login                  Sign in to Asodya through the browser\n  logout                 Sign out of this device\n\nOptions:\n  -p, --print TEXT        Send one prompt and print its answer\n  -c, --continue          Continue the most recent Cortex chat\n  -r, --conversation ID  Resume a Cortex chat (--resume is an alias)\n  --note-id UUID          Select one Annotations note as chat context\n  -h, --help              Show this help\n  -V, --version           Show the version\n\nIn chat: Ctrl+O toggles activity details; Ctrl+D exits the prompt.\n"
}

pub fn version() -> String {
    format!("{} {}", settings::APP_NAME, settings::VERSION)
}
