//! Plain terminal output. No alternate screen or terminal mode changes.

use crate::core::settings;

pub fn help() -> &'static str {
    "Cortex - command-line client for macOS and Linux\n\nUsage: cortex [COMMAND] [OPTIONS]\n\nStart without options for an inline conversation.\n\nCommands:\n  login                  Sign in to Asodya through the browser\n  logout                 Sign out of this device\n  provider set NAME      Save a provider API key in the OS keyring\n  provider list          Show configured providers\n  provider remove NAME  Remove a provider API key\n  voice --remove-model   Remove the downloaded voice model\n\nOptions:\n  -p, --print TEXT        Send one prompt and print its answer\n      --provider NAME    Use openrouter, openai, anthropic or gemini\n      --model ID         Provider model identifier (use with --provider)\n  -c, --continue          Continue the most recent Cortex chat\n  -r, --conversation ID  Resume a Cortex chat (--resume is an alias)\n  -h, --help              Show this help\n  -V, --version           Show the version\n\nExternal provider mode requires --print, --provider and --model. In chat, hold Space to dictate text; Esc cancels. Ctrl+O toggles activity details; Ctrl+D exits.\n"
}

pub fn version() -> String {
    format!("{} {}", settings::APP_NAME, settings::VERSION)
}
