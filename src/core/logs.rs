//! Diagnostics use stderr and fixed messages without user input or credentials.

use std::io::{self, Write};

pub fn error(message: &str) {
    let _ = writeln!(io::stderr().lock(), "Cortex: {message}");
}
