//! Inline terminal rendering. No alternate screen and no mouse capture.

use crate::{core::terminal_text::clean, domain::services::chat::ChatState};
use crossterm::{
    cursor::{MoveToColumn, MoveUp, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::{Color, ResetColor, SetForegroundColor},
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use std::{
    io::{self, Write},
    time::Duration,
};
use unicode_width::UnicodeWidthStr;

pub struct RawTerminal {
    keyboard_enhancement: bool,
}
impl RawTerminal {
    pub fn enter(output: &mut impl Write) -> io::Result<Self> {
        enable_raw_mode()?;
        let keyboard_enhancement =
            crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
        let guard = Self {
            keyboard_enhancement,
        };
        execute!(output, EnableBracketedPaste)?;
        if keyboard_enhancement {
            execute!(
                output,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )?;
        }
        Ok(guard)
    }

    pub fn reports_key_releases(&self) -> bool {
        self.keyboard_enhancement
    }
}
impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.keyboard_enhancement {
            let _ = execute!(
                io::stdout(),
                PopKeyboardEnhancementFlags,
                DisableBracketedPaste,
                Show,
                ResetColor
            );
        } else {
            let _ = execute!(io::stdout(), DisableBracketedPaste, Show, ResetColor);
        }
    }
}

#[derive(Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
}
impl Editor {
    pub fn insert(&mut self, text: &str) {
        let text = clean(text);
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }
    pub fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }
    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }
    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.text.drain(self.cursor..end);
    }
    pub fn delete(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.text.drain(self.cursor..self.cursor + c.len_utf8());
        }
    }
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}

pub fn clipped(text: &str, cells: usize) -> String {
    let text = clean(text).replace('\n', " ");
    let mut out = String::new();
    for c in text.chars() {
        let mut next = out.clone();
        next.push(c);
        if next.width() > cells {
            break;
        }
        out = next;
    }
    out
}

pub fn wrapped(text: &str, cells: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for character in clean(text).replace('\t', "    ").chars() {
        if character == '\n' {
            rows.push(std::mem::take(&mut row));
            continue;
        }
        let mut next = row.clone();
        next.push(character);
        if next.width() > cells && !row.is_empty() {
            rows.push(row);
            row = character.to_string();
        } else {
            row = next;
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

pub fn editor_view(editor: &Editor, columns: usize) -> (String, u16) {
    let budget = columns.saturating_sub(4);
    let before = editor.text[..editor.cursor]
        .replace('\n', " ↵ ")
        .replace('\t', "    ");
    let after = editor.text[editor.cursor..]
        .replace('\n', " ↵ ")
        .replace('\t', "    ");
    let mut prefix = String::new();
    for c in before.chars().rev() {
        let next = format!("{c}{prefix}");
        if next.width() > budget {
            break;
        }
        prefix = next;
    }
    let marker = if prefix.len() < before.len() { "<" } else { "" };
    let column = 2 + marker.width() + prefix.width();
    let tail = clipped(&after, columns.saturating_sub(column + 1));
    (format!("> {marker}{prefix}{tail}"), column as u16)
}

pub fn activity_lines(
    state: &ChatState,
    expanded: bool,
    elapsed: Option<Duration>,
    height: usize,
) -> Vec<String> {
    let mut lines = vec![if let Some(time) = elapsed {
        let mut line = format!(
            "● {}  {:.1}s",
            if state.stage.is_empty() {
                "Working"
            } else {
                &state.stage
            },
            time.as_secs_f64()
        );
        if let Some(expected) = state.expected_ms.filter(|n| *n > 0) {
            line.push_str(&format!(
                "  recent average {:.1}s",
                expected as f64 / 1000.0
            ));
        }
        line
    } else if state.complete {
        format!(
            "✓ Complete  {:.1}s",
            state.total_ms.unwrap_or(0) as f64 / 1000.0
        )
    } else {
        "Cortex".into()
    }];
    if expanded {
        let count = height.saturating_sub(5).min(8);
        lines.extend(
            state
                .activity
                .iter()
                .rev()
                .take(count)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|s| format!("  {s}")),
        );
    }
    lines.push(format!(
        "Ctrl+O {} |{}",
        if expanded { "collapse" } else { "expand" },
        if elapsed.is_some() {
            if state.output_id.is_some() {
                " Ctrl+C stop"
            } else {
                " Ctrl+C disconnect"
            }
        } else {
            " Ctrl+D exit"
        }
    ));
    lines
}

pub struct Surface {
    columns: usize,
    height: usize,
    line: String,
    panel_rows: u16,
    colour: bool,
}
impl Surface {
    pub fn new(columns: u16, height: u16, colour: bool) -> Self {
        Self {
            columns: usize::from(columns).max(8),
            height: usize::from(height).max(5),
            line: String::new(),
            panel_rows: 0,
            colour,
        }
    }
    pub fn clear(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.panel_rows > 0 {
            queue!(
                out,
                MoveUp(self.panel_rows),
                MoveToColumn(self.line.width().min(self.columns - 1) as u16),
                Clear(ClearType::FromCursorDown)
            )?;
            self.panel_rows = 0;
        }
        Ok(())
    }
    pub fn text(&mut self, out: &mut impl Write, text: &str) -> io::Result<()> {
        self.clear(out)?;
        for c in text.chars() {
            if c == '\t' {
                self.text(out, &" ".repeat(8 - self.line.width() % 8))?;
                continue;
            }
            if c == '\n' {
                write!(out, "\r\n")?;
                self.line.clear();
                continue;
            }
            let mut next = self.line.clone();
            next.push(c);
            if next.width() > self.columns {
                write!(out, "\r\n")?;
                self.line.clear();
                next = c.to_string();
            }
            write!(out, "{c}")?;
            self.line = next;
        }
        out.flush()
    }
    pub fn paint(
        &mut self,
        out: &mut impl Write,
        state: &ChatState,
        expanded: bool,
        elapsed: Option<Duration>,
        editor: Option<&Editor>,
        voice_status: Option<&str>,
    ) -> io::Result<()> {
        self.clear(out)?;
        let mut lines = activity_lines(state, expanded, elapsed, self.height);
        if expanded && lines.len() > 2 {
            let mut details = lines[1..lines.len() - 1]
                .iter()
                .flat_map(|line| wrapped(line.strip_prefix("  ").unwrap_or(line), self.columns - 3))
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>();
            let maximum = self.height.saturating_sub(5).min(12);
            if details.len() > maximum {
                details.drain(..details.len() - maximum);
            }
            let footer = lines.pop().unwrap();
            let heading = lines.remove(0);
            lines = vec![heading];
            lines.extend(details);
            lines.push(footer);
        }
        let input = editor.map(|e| editor_view(e, self.columns));
        if let Some(status) = voice_status {
            lines.push(status.to_owned());
        }
        if let Some((line, _)) = &input {
            lines.push(line.clone());
        }
        if self.colour {
            queue!(out, SetForegroundColor(Color::DarkGrey))?;
        }
        for line in &lines {
            write!(out, "\r\n{}", clipped(line, self.columns - 1))?;
        }
        if self.colour {
            queue!(out, ResetColor)?;
        }
        if let Some((_, column)) = input {
            queue!(out, MoveToColumn(column))?;
        }
        self.panel_rows = lines.len() as u16;
        out.flush()
    }
    pub fn resize(&mut self, out: &mut impl Write, columns: u16, height: u16) -> io::Result<()> {
        // Keep prior scrollback intact when the terminal reflows its rows.
        self.panel_rows = 0;
        write!(out, "\r\n")?;
        self.line.clear();
        self.columns = usize::from(columns).max(8);
        self.height = usize::from(height).max(5);
        Ok(())
    }
}

pub fn logo(out: &mut impl Write, colour: bool) -> io::Result<()> {
    if colour {
        queue!(out, SetForegroundColor(Color::Green))?;
    }
    write!(out, "        ▄\r\n        █\r\n")?;
    if colour {
        queue!(out, SetForegroundColor(Color::Magenta))?;
    }
    write!(out, "   ▀██▄ █")?;
    if colour {
        queue!(out, ResetColor)?;
    }
    write!(
        out,
        " ▄██▀\r\n       ▀●▀\r\n      ▄█ █▄\r\n     ██   ██\r\n\r\n"
    )?;
    if colour {
        queue!(out, SetForegroundColor(Color::Reset))?;
    }
    write!(out, "Cortex\r\n")?;
    if colour {
        queue!(out, ResetColor)?;
    }
    out.flush()
}
