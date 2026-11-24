//! Selected Annotations note context. Never format note content in Debug output.

use crate::domain::models::chat::ChatFailure;

#[derive(Clone)]
pub struct NoteSection {
    pub id: String,
    pub text: String,
}

#[derive(Clone)]
pub struct SelectedNote {
    pub note_id: String,
    pub revision: u64,
    pub revision_id: String,
    pub title: Option<String>,
    pub sections: Vec<NoteSection>,
}

pub trait NotesApi: Send + Sync {
    fn selected_note(&self, note_id: &str) -> Result<SelectedNote, ChatFailure>;
}
