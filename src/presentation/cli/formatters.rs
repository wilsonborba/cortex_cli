//! Plain terminal output. No alternate screen or terminal mode changes.

use crate::core::settings;

pub fn help() -> &'static str {
    "Cortex - command-line client for macOS and Linux\n\nUsage: cortex [OPTIONS]\n\nOptions:\n  -h, --help     Show this help\n  -V, --version  Show the version\n\nThis bootstrap build provides help and version information.\n"
}

pub fn version() -> String {
    format!("{} {}", settings::APP_NAME, settings::VERSION)
}
