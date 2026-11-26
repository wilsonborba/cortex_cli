//! Fetch only note IDs that the user selected for this chat.

use crate::domain::models::{
    annotations::NotesApi,
    chat::{ChatFailure, valid_id},
};

const MAX_CONTEXT_BYTES: usize = 512 * 1024;

pub fn selected_context(api: &impl NotesApi, note_ids: &[String]) -> Result<String, ChatFailure> {
    let mut seen = std::collections::HashSet::new();
    for note_id in note_ids {
        if !valid_id(note_id) || !seen.insert(note_id.to_ascii_lowercase()) {
            return Err(ChatFailure::InvalidInput);
        }
    }
    let mut notes = Vec::with_capacity(note_ids.len());
    for note_id in note_ids {
        let note = api.selected_note(note_id)?;
        if !note.note_id.eq_ignore_ascii_case(note_id)
            || note.revision == 0
            || !valid_id(&note.revision_id)
            || note.sections.is_empty()
        {
            return Err(ChatFailure::InvalidResponse);
        }
        let mut text = format!(
            "Note {} (revision {}, revision_id {})",
            note.note_id, note.revision, note.revision_id
        );
        if let Some(title) = note.title {
            text.push_str(&format!("\nTitle: {title}"));
        }
        for section in note.sections {
            text.push_str(&format!("\n\nSection {}:\n{}", section.id, section.text));
        }
        notes.push(text);
    }
    let data = notes.join("\n\n--- Selected note ---\n\n");
    if data.len() > MAX_CONTEXT_BYTES {
        return Err(ChatFailure::InvalidResponse);
    }
    Ok(data)
}

/// Publish the selected sources before any chat request and retain only a
/// valid owner-scoped identity returned by the existing upload contract.
pub fn upload_selected_context(
    context: &str,
    upload: impl FnOnce(&str) -> Result<String, ChatFailure>,
) -> Result<Vec<String>, ChatFailure> {
    if context.is_empty() {
        return Ok(Vec::new());
    }
    let file_id = upload(context)?;
    if !valid_id(&file_id) {
        return Err(ChatFailure::InvalidResponse);
    }
    Ok(vec![file_id])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::annotations::{NoteSection, SelectedNote};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fixture(Mutex<Vec<String>>);
    impl NotesApi for Fixture {
        fn selected_note(&self, note_id: &str) -> Result<SelectedNote, ChatFailure> {
            self.0.lock().unwrap().push(note_id.to_owned());
            Ok(SelectedNote {
                note_id: note_id.to_owned(),
                revision: 3,
                revision_id: "20000000-0000-4000-8000-000000000002".into(),
                title: Some("Selected only".into()),
                sections: vec![NoteSection {
                    id: "block-1".into(),
                    text: "private content".into(),
                }],
            })
        }
    }

    #[test]
    fn only_explicitly_selected_notes_are_loaded_and_cited() {
        let api = Fixture::default();
        let id = "10000000-0000-4000-8000-000000000001".to_owned();
        let context = selected_context(&api, std::slice::from_ref(&id)).unwrap();
        assert_eq!(*api.0.lock().unwrap(), [id]);
        assert!(context.contains("private content"));
        assert!(context.contains("block-1"));
        assert!(context.contains("revision 3"));
        assert!(context.contains("20000000-0000-4000-8000-000000000002"));
    }

    #[test]
    fn malformed_or_duplicate_selection_fails_before_an_extra_read() {
        let api = Fixture::default();
        let id = "10000000-0000-4000-8000-000000000001".to_owned();
        assert!(selected_context(&api, &["not-an-id".into()]).is_err());
        assert!(selected_context(&api, &[id.clone(), id]).is_err());
        assert!(api.0.lock().unwrap().is_empty());
    }

    #[test]
    fn selected_context_document_has_source_citations_and_never_rewrites_question() {
        let api = Fixture::default();
        let id = "10000000-0000-4000-8000-000000000001".to_owned();
        let document = selected_context(&api, &[id]).unwrap();
        assert!(document.contains("Note 10000000-0000-4000-8000-000000000001 (revision 3"));
        assert!(document.contains("Section block-1:\nprivate content"));
    }

    #[test]
    fn upload_requires_a_returned_real_file_id_and_skips_empty_context() {
        let mut called = false;
        assert!(
            upload_selected_context("", |_| {
                called = true;
                Err(ChatFailure::Unauthorized)
            })
            .unwrap()
            .is_empty()
        );
        assert!(!called);

        assert!(matches!(
            upload_selected_context("selected note", |_| Ok("invented".into())),
            Err(ChatFailure::InvalidResponse)
        ));
        assert!(matches!(
            upload_selected_context("selected note", |_| Err(ChatFailure::Unauthorized)),
            Err(ChatFailure::Unauthorized)
        ));
        assert_eq!(
            upload_selected_context("selected note", |document| {
                assert_eq!(document, "selected note");
                Ok("10000000-0000-4000-8000-000000000001".into())
            })
            .unwrap(),
            ["10000000-0000-4000-8000-000000000001"]
        );
    }
}
