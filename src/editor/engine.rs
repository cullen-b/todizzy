/// Editor state machine.
///
/// The engine owns:
///   • The text `Buffer`
///   • Modal editing state (`Mode`)
///   • A `VimPending` for multi-key sequences
///   • A small undo stack
///   • A yank register (clipboard-equivalent)
///
/// It does NOT know about ObjC, NSTextView, or any platform UI.
/// Tests can drive it with synthetic `Key` values.

use std::collections::VecDeque;

use super::{
    buffer::Buffer,
    helix::HelixHandler,
    vim::{Motion, VimHandler, VimPending},
};
use crate::settings::MotionMode;

// ── Public types ──────────────────────────────────────────────────────────────

/// Platform-normalised key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Escape,
    Backspace,
    Delete,
    Enter,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

/// Editor mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual { line: bool },
}

impl Default for Mode {
    fn default() -> Self {
        Self::Normal
    }
}

/// Actions the engine produces for the UI layer to execute.
/// The UI applies them to the on-screen text view.
#[derive(Debug, Clone)]
pub enum EditorAction {
    /// Move cursor according to motion.
    Move(Motion),
    /// Insert a single character at cursor.
    InsertChar(char),
    /// Delete one character to the left (backspace).
    DeleteBackward,
    /// Delete the character under the cursor.
    DeleteCharForward,
    /// Delete text covered by a motion.
    DeleteMotion(Motion),
    /// Delete the current line.
    DeleteLine,
    /// Delete the current selection.
    DeleteSelection,
    /// Yank text covered by a motion.
    YankMotion(Motion),
    /// Yank the current line.
    YankLine,
    /// Yank the current selection.
    YankSelection,
    /// Select the current line.
    SelectLine,
    /// Paste after cursor.
    PasteAfter,
    /// Paste before cursor.
    PasteBefore,
    /// Undo last change.
    Undo,
    /// Switch to a different mode.
    SetMode(Mode),
}

// ── Trait for motion handlers ─────────────────────────────────────────────────

pub trait MotionHandler {
    fn handle_key(
        &self,
        key: Key,
        mode: &Mode,
        pending: &mut VimPending,
        actions: &mut Vec<EditorAction>,
    );
}

// ── EditorEngine ──────────────────────────────────────────────────────────────

const MAX_UNDO: usize = 128;

pub struct EditorEngine {
    pub buf: Buffer,
    pub mode: Mode,
    pub selection_anchor: Option<usize>, // byte offset of selection start
    pending: VimPending,
    yank_reg: String,
    undo_stack: VecDeque<(String, usize)>, // (snapshot, cursor)
    undo_pos: usize,
    motion_mode: MotionMode,
}

impl EditorEngine {
    pub fn new(content: String, motion_mode: MotionMode) -> Self {
        let buf = Buffer::new(content);
        Self {
            buf,
            mode: Mode::Normal,
            selection_anchor: None,
            pending: VimPending::default(),
            yank_reg: String::new(),
            undo_stack: VecDeque::new(),
            undo_pos: 0,
            motion_mode,
        }
    }

    pub fn set_content(&mut self, content: String) {
        self.buf.set_content(content);
        self.mode = Mode::Normal;
        self.pending = VimPending::default();
        self.selection_anchor = None;
        self.undo_stack.clear();
        self.undo_pos = 0;
    }

    pub fn set_motion_mode(&mut self, m: MotionMode) {
        self.motion_mode = m;
    }

    /// Process one key event.  Returns true if the buffer was mutated.
    pub fn process_key(&mut self, key: Key) -> bool {
        if self.motion_mode == MotionMode::None {
            return self.process_plain(key);
        }

        // Snapshot before for undo
        let before = self.buf.as_str().to_owned();
        let before_cursor = self.buf.cursor();

        let mut actions = Vec::new();
        match self.motion_mode {
            MotionMode::Vim => {
                VimHandler.handle_key(key, &self.mode, &mut self.pending, &mut actions)
            }
            MotionMode::Helix => {
                HelixHandler.handle_key(key, &self.mode, &mut self.pending, &mut actions)
            }
            MotionMode::None => unreachable!(),
        }

        let mutated = self.apply_actions(actions);

        if mutated {
            // Push undo snapshot
            if self.undo_stack.len() >= MAX_UNDO {
                self.undo_stack.pop_front();
            }
            self.undo_stack.push_back((before, before_cursor));
            self.undo_pos = self.undo_stack.len();
        }

        mutated
    }

    // ── Plain (non-modal) mode ────────────────────────────────────────────────

    fn process_plain(&mut self, key: Key) -> bool {
        match key {
            Key::Char(c) => {
                self.buf.insert(&c.to_string());
                true
            }
            Key::Enter => {
                self.buf.insert("\n");
                true
            }
            Key::Backspace => {
                self.buf.delete_backward();
                true
            }
            Key::Left => {
                self.buf.move_left(1);
                false
            }
            Key::Right => {
                self.buf.move_right(1);
                false
            }
            Key::Up => {
                self.buf.move_up(1);
                false
            }
            Key::Down => {
                self.buf.move_down(1);
                false
            }
            _ => false,
        }
    }

    // ── Apply actions ─────────────────────────────────────────────────────────

    fn apply_actions(&mut self, actions: Vec<EditorAction>) -> bool {
        let mut mutated = false;
        for action in actions {
            match action {
                EditorAction::SetMode(m) => {
                    if matches!(m, Mode::Visual { .. }) && self.selection_anchor.is_none() {
                        self.selection_anchor = Some(self.buf.cursor());
                    } else if m == Mode::Normal || m == Mode::Insert {
                        self.selection_anchor = None;
                    }
                    self.mode = m;
                }

                EditorAction::Move(motion) => {
                    self.apply_motion(&motion);
                }

                EditorAction::InsertChar(c) => {
                    self.buf.insert(&c.to_string());
                    mutated = true;
                }

                EditorAction::DeleteBackward => {
                    self.buf.delete_backward();
                    mutated = true;
                }

                EditorAction::DeleteCharForward => {
                    self.buf.delete_char_forward();
                    mutated = true;
                }

                EditorAction::DeleteMotion(motion) => {
                    let start = self.buf.cursor();
                    self.apply_motion(&motion);
                    let end = self.buf.cursor();
                    let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
                    self.yank_reg = self.buf.as_str()[lo..hi].to_owned();
                    self.buf.delete_range(lo, hi);
                    mutated = true;
                }

                EditorAction::DeleteLine => {
                    let (lo, hi) = self.buf.current_line_range();
                    self.yank_reg = self.buf.as_str()[lo..hi].to_owned();
                    self.buf.delete_range(lo, hi);
                    mutated = true;
                }

                EditorAction::DeleteSelection | EditorAction::YankSelection => {
                    if let Some(anchor) = self.selection_anchor {
                        let cursor = self.buf.cursor();
                        let (lo, hi) = if anchor <= cursor {
                            (anchor, cursor)
                        } else {
                            (cursor, anchor)
                        };
                        self.yank_reg = self.buf.as_str()[lo..hi].to_owned();
                        if matches!(action, EditorAction::DeleteSelection) {
                            self.buf.delete_range(lo, hi);
                            mutated = true;
                        }
                        self.selection_anchor = None;
                        self.mode = Mode::Normal;
                    }
                }

                EditorAction::SelectLine => {
                    let (lo, hi) = self.buf.current_line_range();
                    self.selection_anchor = Some(lo);
                    self.buf.set_cursor(hi);
                }

                EditorAction::YankMotion(motion) => {
                    let start = self.buf.cursor();
                    self.apply_motion(&motion);
                    let end = self.buf.cursor();
                    let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
                    self.yank_reg = self.buf.as_str()[lo..hi].to_owned();
                    self.buf.set_cursor(start); // restore
                }

                EditorAction::YankLine => {
                    let (lo, hi) = self.buf.current_line_range();
                    self.yank_reg = self.buf.as_str()[lo..hi].to_owned();
                }

                EditorAction::PasteAfter => {
                    let pos = self.buf.cursor() + 1;
                    let text = self.yank_reg.clone();
                    self.buf.replace_range(pos, pos, &text);
                    mutated = true;
                }

                EditorAction::PasteBefore => {
                    let pos = self.buf.cursor();
                    let text = self.yank_reg.clone();
                    self.buf.replace_range(pos, pos, &text);
                    mutated = true;
                }

                EditorAction::Undo => {
                    if self.undo_pos > 0 {
                        self.undo_pos -= 1;
                        if let Some((snap, cur)) =
                            self.undo_stack.get(self.undo_pos).cloned()
                        {
                            self.buf.set_content(snap);
                            self.buf.set_cursor(cur);
                            mutated = true;
                        }
                    }
                }
            }
        }
        mutated
    }

    // ── Motion execution ──────────────────────────────────────────────────────

    fn apply_motion(&mut self, motion: &Motion) {
        match motion {
            Motion::Left(n) => self.buf.move_left(*n),
            Motion::Right(n) => self.buf.move_right(*n),
            Motion::Up(n) => self.buf.move_up(*n),
            Motion::Down(n) => self.buf.move_down(*n),
            Motion::WordForward(n) => self.buf.move_word_forward(*n),
            Motion::WordBackward(n) => self.buf.move_word_backward(*n),
            Motion::WordEnd => self.buf.move_word_end(1),
            Motion::LineStart => self.buf.move_to_line_start(),
            Motion::LineEnd => self.buf.move_to_line_end(),
            Motion::FirstNonBlank => self.buf.move_to_first_nonblank(),
            Motion::FirstLine => self.buf.move_to_first_line(),
            Motion::LastLine => self.buf.move_to_last_line(),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn vim_engine(s: &str) -> EditorEngine {
        EditorEngine::new(s.into(), MotionMode::Vim)
    }

    fn keys(engine: &mut EditorEngine, seq: &[Key]) {
        for k in seq {
            engine.process_key(k.clone());
        }
    }

    #[test]
    fn vim_insert_then_esc() {
        let mut e = vim_engine("");
        keys(
            &mut e,
            &[
                Key::Char('i'),
                Key::Char('h'),
                Key::Char('i'),
                Key::Escape,
            ],
        );
        assert_eq!(e.buf.as_str(), "hi");
        assert_eq!(e.mode, Mode::Normal);
    }

    #[test]
    fn vim_dd_deletes_line() {
        let mut e = vim_engine("hello\nworld");
        keys(&mut e, &[Key::Char('d'), Key::Char('d')]);
        assert!(!e.buf.as_str().contains("hello\n"));
    }

    #[test]
    fn vim_word_forward() {
        let mut e = vim_engine("hello world foo");
        keys(&mut e, &[Key::Char('w')]);
        assert_eq!(e.buf.cursor(), 6);
    }

    #[test]
    fn vim_undo() {
        let mut e = vim_engine("hi");
        e.buf.set_cursor(2);
        keys(
            &mut e,
            &[Key::Char('i'), Key::Char('!'), Key::Escape],
        );
        assert_eq!(e.buf.as_str(), "hi!");
        keys(&mut e, &[Key::Char('u')]);
        assert_eq!(e.buf.as_str(), "hi");
    }
}
