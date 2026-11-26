//! Fetch only note IDs that the user selected for this chat.

use crate::domain::models::{
    annotations::{NoteOmission, NotesApi, SelectedContext},
    chat::{ChatFailure, valid_id},
};

const MAX_CONTEXT_BYTES: usize = 512 * 1024;

pub fn selected_context(
    api: &impl NotesApi,
    note_ids: &[String],
) -> Result<SelectedContext, ChatFailure> {
    let mut seen = std::collections::HashSet::new();
    for note_id in note_ids {
        if !valid_id(note_id) || !seen.insert(note_id.to_ascii_lowercase()) {
            return Err(ChatFailure::InvalidInput);
        }
    }
    let mut notes = Vec::with_capacity(note_ids.len());
    let mut omissions = std::collections::BTreeMap::<String, usize>::new();
    for note_id in note_ids {
        let note = api.selected_note(note_id)?;
        if !note.note_id.eq_ignore_ascii_case(note_id)
            || note.revision == 0
            || !valid_id(&note.revision_id)
        {
            return Err(ChatFailure::InvalidResponse);
        }
        if note.sections.is_empty()
            || note
                .sections
                .iter()
                .all(|section| section.text.trim().is_empty())
        {
            return Err(ChatFailure::NoteHasNoTypedText);
        }
        let mut text = format!(
            "Note {} (revision {}, revision_id {})",
            note.note_id, note.revision, note.revision_id
        );
        text.push_str("\nSource: selected typed block text and note title metadata only.");
        if let Some(title) = note.title {
            text.push_str(&format!("\nTitle: {title}"));
        }
        let mut note_omissions = std::collections::BTreeMap::<String, usize>::new();
        for omission in note.omissions {
            if omission.count == 0 {
                return Err(ChatFailure::InvalidResponse);
            }
            *note_omissions.entry(omission.kind).or_default() += omission.count;
        }
        for (kind, count) in note_omissions {
            *omissions.entry(kind.clone()).or_default() += count;
            text.push_str(&format!("\nOmitted note block: {kind} ({count})."));
        }
        for section in note.sections {
            text.push_str(&format!("\n\nSection {}:\n{}", section.id, section.text));
        }
        notes.push(text);
    }
    if notes.is_empty() {
        return Ok(SelectedContext {
            text: String::new(),
            omissions: Vec::new(),
        });
    }
    let omission_facts = omissions
        .iter()
        .map(|(kind, count)| format!("{kind} ({count})"))
        .collect::<Vec<_>>();
    let mut data = format!(
        "Selected sources contain typed block text and note titles only (512 KiB context limit).\n\n{}",
        notes.join("\n\n--- Selected note ---\n\n")
    );
    if !omission_facts.is_empty() {
        data.insert_str(
            "Selected sources contain typed block text and note titles only (512 KiB context limit).".len(),
            &format!(
                " Omitted note blocks: {}. Referenced content was not loaded.",
                omission_facts.join(", ")
            ),
        );
    }
    if data.len() > MAX_CONTEXT_BYTES {
        return Err(ChatFailure::InvalidResponse);
    }
    Ok(SelectedContext {
        text: data,
        omissions: omissions
            .into_iter()
            .map(|(kind, count)| NoteOmission { kind, count })
            .collect(),
    })
}

pub fn validate_context_mode(
    context: &SelectedContext,
    interactive: bool,
) -> Result<(), ChatFailure> {
    if !interactive && !context.omissions.is_empty() {
        return Err(ChatFailure::PartialNoteContextNonInteractive);
    }
    Ok(())
}

pub fn upload_context(
    context: &SelectedContext,
    interactive: bool,
    upload: impl FnOnce(&str) -> Result<String, ChatFailure>,
) -> Result<Vec<String>, ChatFailure> {
    validate_context_mode(context, interactive)?;
    upload_selected_context(&context.text, upload)
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
                omissions: Vec::new(),
            })
        }
    }

    #[test]
    fn only_explicitly_selected_notes_are_loaded_and_cited() {
        let api = Fixture::default();
        let id = "10000000-0000-4000-8000-000000000001".to_owned();
        let context = selected_context(&api, std::slice::from_ref(&id)).unwrap();
        assert_eq!(*api.0.lock().unwrap(), [id]);
        assert!(context.text.contains("private content"));
        assert!(context.text.contains("block-1"));
        assert!(context.text.contains("revision 3"));
        assert!(
            context
                .text
                .contains("20000000-0000-4000-8000-000000000002")
        );
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
        assert!(
            document
                .text
                .contains("Note 10000000-0000-4000-8000-000000000001 (revision 3")
        );
        assert!(
            document
                .text
                .contains("Source: selected typed block text and note title metadata only.")
        );
        assert!(document.text.contains("Section block-1:\nprivate content"));
    }

    #[test]
    fn known_non_text_blocks_are_counted_and_named_as_omissions() {
        struct Mixed;
        impl NotesApi for Mixed {
            fn selected_note(&self, note_id: &str) -> Result<SelectedNote, ChatFailure> {
                Ok(SelectedNote {
                    note_id: note_id.to_owned(),
                    revision: 3,
                    revision_id: "20000000-0000-4000-8000-000000000002".into(),
                    title: None,
                    sections: vec![NoteSection {
                        id: "p1".into(),
                        text: "Preserved paragraph text.".into(),
                    }],
                    omissions: vec![
                        NoteOmission {
                            kind: "task_card".into(),
                            count: 1,
                        },
                        NoteOmission {
                            kind: "task_card".into(),
                            count: 1,
                        },
                        NoteOmission {
                            kind: "link_card".into(),
                            count: 1,
                        },
                    ],
                })
            }
        }
        let id = "10000000-0000-4000-8000-000000000001".to_owned();
        let selected = selected_context(&Mixed, &[id]).unwrap();
        assert!(selected.text.contains("Preserved paragraph text."));
        assert!(selected.text.contains("link_card (1), task_card (2)"));
        assert!(selected.text.contains("Referenced content was not loaded."));
        assert_eq!(
            selected.omission_notice().as_deref(),
            Some(
                "Selected typed block text and note titles only (512 KiB context limit). Omitted note blocks: link_card (1), task_card (2). Referenced content was not loaded."
            )
        );
    }

    #[test]
    fn note_with_no_usable_typed_text_is_rejected() {
        struct NoText;
        impl NotesApi for NoText {
            fn selected_note(&self, note_id: &str) -> Result<SelectedNote, ChatFailure> {
                Ok(SelectedNote {
                    note_id: note_id.to_owned(),
                    revision: 3,
                    revision_id: "20000000-0000-4000-8000-000000000002".into(),
                    title: None,
                    sections: Vec::new(),
                    omissions: vec![NoteOmission {
                        kind: "task_card".into(),
                        count: 1,
                    }],
                })
            }
        }
        let id = "10000000-0000-4000-8000-000000000001".to_owned();
        assert!(matches!(
            selected_context(&NoText, &[id]),
            Err(ChatFailure::NoteHasNoTypedText)
        ));
    }

    #[test]
    fn partial_note_context_requires_the_interactive_review_path() {
        let context = SelectedContext {
            text: "selected typed text".into(),
            omissions: vec![NoteOmission {
                kind: "task_card".into(),
                count: 1,
            }],
        };
        let mut upload_called = false;
        assert!(matches!(
            upload_context(&context, false, |_| {
                upload_called = true;
                Ok("10000000-0000-4000-8000-000000000001".into())
            }),
            Err(ChatFailure::PartialNoteContextNonInteractive)
        ));
        assert!(!upload_called);
        assert!(validate_context_mode(&context, true).is_ok());
        assert!(
            validate_context_mode(
                &SelectedContext {
                    text: "selected typed text".into(),
                    omissions: Vec::new(),
                },
                false
            )
            .is_ok()
        );
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
