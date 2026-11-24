use cortex_cli::{
    domain::services::chat::ChatState,
    presentation::cli::{
        commands::{Command, parse},
        terminal::{Editor, Surface, activity_lines, editor_view},
    },
};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

#[test]
fn editing_preserves_unicode_and_multiline_paste_without_submitting() {
    let mut editor = Editor::default();
    editor.insert("สวัสดี\nPortuguês\t");
    let original = editor.text.clone();
    editor.left();
    editor.right();
    assert_eq!(editor.text, original);
    editor.cursor = 0;
    editor.insert("EN ");
    assert!(editor.text.starts_with("EN สวัสดี\n"));
    for width in [20, 40, 80, 120] {
        let (line, cursor) = editor_view(&editor, width);
        assert!(line.width() < width);
        assert!(usize::from(cursor) < width);
    }
    editor.cursor = editor.text.len();
    editor.backspace();
    assert!(!editor.text.ends_with('\t'));
    editor.cursor = 0;
    editor.delete();
    assert!(editor.text.starts_with("N "));
}

#[test]
fn detail_toggle_is_data_sensitive_and_estimates_are_never_invented() {
    let state = ChatState {
        activity: vec!["Read page 3 of 10".into()],
        stage: "gather".into(),
        ..Default::default()
    };
    let collapsed = activity_lines(&state, false, Some(Duration::from_millis(2500)), 24).join("\n");
    let expanded = activity_lines(&state, true, Some(Duration::from_millis(2500)), 24).join("\n");
    assert!(!collapsed.contains("Read page"));
    assert!(expanded.contains("Read page 3 of 10"));
    assert!(expanded.contains("2.5s"));
    assert!(!expanded.contains("average"));
    let state = ChatState {
        expected_ms: Some(4000),
        ..state
    };
    assert!(
        activity_lines(&state, true, Some(Duration::from_secs(1)), 24)
            .join("\n")
            .contains("recent average 4.0s")
    );
}

#[test]
fn inline_frames_do_not_capture_mouse_or_use_alternate_screen() {
    let mut output = Vec::new();
    let mut surface = Surface::new(40, 24, true);
    let state = ChatState {
        activity: vec!["Reading a source".into()],
        ..Default::default()
    };
    surface.text(&mut output, "สวัสดี answer").unwrap();
    surface
        .paint(
            &mut output,
            &state,
            true,
            Some(Duration::from_secs(1)),
            None,
        )
        .unwrap();
    surface
        .paint(&mut output, &state, false, None, Some(&Editor::default()))
        .unwrap();
    surface.clear(&mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    for forbidden in ["?1049", "?1047", "?1000", "?1002", "?1003", "?1006"] {
        assert!(!text.contains(forbidden));
    }
    assert!(text.contains("Reading a source"));
    assert!(text.contains("\x1b[J"));
}

#[test]
fn mapped_flags_reject_extra_or_private_arguments_without_echo() {
    let args = |values: &[&str]| {
        values
            .iter()
            .map(|value| std::ffi::OsString::from(*value))
            .collect::<Vec<_>>()
    };
    assert!(matches!(
        parse(args(&["-p", "hello"])),
        Ok(Command::Chat(_))
    ));
    assert!(matches!(parse(args(&["-c"])), Ok(Command::Chat(_))));
    for invalid in [
        vec!["-c", "--continue"],
        vec!["-r", "not-an-id"],
        vec!["--model", "private-value"],
        vec!["-p"],
    ] {
        assert!(parse(args(&invalid)).is_err());
    }
}

#[test]
fn narrow_activity_wrap_preserves_thai_and_complete_detail_text() {
    let text = "Reading example.txt 3/10 สวัสดีภาษาไทย";
    let rows = cortex_cli::presentation::cli::terminal::wrapped(text, 20);
    assert!(rows.iter().all(|row| row.width() <= 20));
    assert_eq!(rows.join(""), text);
}
