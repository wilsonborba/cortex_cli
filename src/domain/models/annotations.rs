//! Selected Annotations note context. Never format note content in Debug output.

use crate::domain::models::chat::ChatFailure;

#[derive(Clone)]
pub struct NoteSection {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteOmission {
    pub kind: String,
    pub count: usize,
}

#[derive(Clone)]
pub struct SelectedNote {
    pub note_id: String,
    pub revision: u64,
    pub revision_id: String,
    pub title: Option<String>,
    pub sections: Vec<NoteSection>,
    pub omissions: Vec<NoteOmission>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedContext {
    pub text: String,
    pub omissions: Vec<NoteOmission>,
}

impl SelectedContext {
    pub fn omission_notice(&self) -> Option<String> {
        if self.omissions.is_empty() {
            return None;
        }
        let details = self
            .omissions
            .iter()
            .map(|item| format!("{} ({})", item.kind, item.count))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Selected typed block text and note titles only (512 KiB context limit). Omitted note blocks: {details}. Referenced content was not loaded."
        ))
    }
}

pub trait NotesApi: Send + Sync {
    fn selected_note(&self, note_id: &str) -> Result<SelectedNote, ChatFailure>;
}
