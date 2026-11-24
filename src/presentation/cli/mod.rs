//! Inline terminal interaction. Commands parse, handlers coordinate,
//! formatters render, and outputs define machine-facing responses.

pub mod auth;
pub mod chat;
pub mod commands;
pub mod formatters;
pub mod handlers;
pub mod terminal;
pub mod outputs {}
