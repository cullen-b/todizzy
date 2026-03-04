/// Note persistence.
///
/// Each note is stored as a plain UTF-8 text file:
///   ~/Library/Application Support/todizzy/notes/note-<N>.txt
///
/// The index of notes (order, ids) is stored in:
///   ~/Library/Application Support/todizzy/notes/index.json
///
/// Design decisions:
///   • Plain text files = zero parsing cost on load, human-inspectable.
///   • Writes are atomic (write to tmp, rename) so we never lose a note.
///   • No embedded DB — notes are small and simple.
use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

// ── Note ID ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NoteId(pub u32);

impl NoteId {
    fn file_name(self) -> String {
        format!("note-{}.txt", self.0)
    }
}

// ── Index ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct Index {
    /// Ordered list of note IDs (order = page order shown to user).
    notes: Vec<NoteId>,
    /// Monotonically increasing counter for generating new IDs.
    next_id: u32,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            notes: vec![NoteId(0)],
            next_id: 1,
        }
    }
}

// ── NoteStore ─────────────────────────────────────────────────────────────────

pub struct NoteStore {
    dir: PathBuf,
    index: Index,
}

impl NoteStore {
    /// Open (or create) the note store at `dir`.
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let index = Self::load_index(&dir);
        let mut store = NoteStore { dir, index };
        // Ensure at least one note exists.
        if store.index.notes.is_empty() {
            let id = store.create_note();
            drop(store.save_note(id, ""));
        }
        Ok(store)
    }

    // ── Queries ──────────────────────────────────────────────────────────────

    pub fn note_ids(&self) -> &[NoteId] {
        &self.index.notes
    }

    pub fn len(&self) -> usize {
        self.index.notes.len()
    }

    pub fn id_at(&self, index: usize) -> NoteId {
        self.index.notes[index]
    }

    /// Load note content from disk.  Returns empty string on error.
    pub fn load_note(&self, id: NoteId) -> String {
        let path = self.note_path(id);
        fs::read_to_string(&path).unwrap_or_default()
    }

    // ── Mutations ─────────────────────────────────────────────────────────────

    /// Write note content atomically (tmp → rename).
    pub fn save_note(&self, id: NoteId, content: &str) -> std::io::Result<()> {
        let path = self.note_path(id);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, content)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Create a new empty note at the end of the list.  Returns its ID.
    pub fn create_note(&mut self) -> NoteId {
        let id = NoteId(self.index.next_id);
        self.index.next_id += 1;
        self.index.notes.push(id);
        let _ = self.save_index();
        id
    }

    /// Delete the note at `idx`.  Keeps at least one note alive.
    pub fn delete_note(&mut self, idx: usize) -> std::io::Result<()> {
        if self.index.notes.len() <= 1 {
            return Ok(());
        }
        let id = self.index.notes.remove(idx);
        let path = self.note_path(id);
        let _ = fs::remove_file(path); // best-effort
        self.save_index()
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn note_path(&self, id: NoteId) -> PathBuf {
        self.dir.join(id.file_name())
    }

    fn index_path(dir: &Path) -> PathBuf {
        dir.join("index.json")
    }

    fn load_index(dir: &Path) -> Index {
        let path = Self::index_path(dir);
        match fs::read_to_string(&path) {
            Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
            Err(_) => Index::default(),
        }
    }

    fn save_index(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(&self.index).expect("index serialise");
        let path = Self::index_path(&self.dir);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}
