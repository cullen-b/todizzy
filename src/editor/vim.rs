/// Vim motion handler.
///
/// Parses key sequences in Normal mode and returns `EditorAction`s.
/// Supports:
///   Movement : h j k l  w b e  0 ^ $  gg G
///   Operators : d (with motion), c (with motion), y (with motion)
///   Editing  : i a o I A O  x X  dd  cc  yy  p P
///   Counts   : [n]motion  (e.g. 5j, 3w)
///   Visual   : v (character visual), V (line visual)
///   Escape   : Esc → Normal

use super::engine::{EditorAction, Key, Mode, MotionHandler};

// ── State ─────────────────────────────────────────────────────────────────────

/// Pending key sequence being built (e.g. "g" waiting for "g", count digits).
#[derive(Debug, Default, Clone)]
pub struct VimPending {
    pub(crate) count_str: String,
    /// Operator character (`d`, `c`, `y`, …) if one is pending.
    pub(crate) operator: Option<char>,
    /// `g` has been pressed; waiting for second char.
    pub(crate) g_prefix: bool,
}

impl VimPending {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn count(&self) -> usize {
        self.count_str.parse().unwrap_or(1).max(1)
    }
}

pub struct VimHandler;

impl MotionHandler for VimHandler {
    fn handle_key(
        &self,
        key: Key,
        mode: &Mode,
        pending: &mut VimPending,
        actions: &mut Vec<EditorAction>,
    ) {
        match mode {
            Mode::Insert => handle_insert(key, actions),
            Mode::Normal => handle_normal(key, pending, actions),
            Mode::Visual { line: _ } => handle_visual(key, pending, actions),
        }
    }
}

// ── Insert mode ───────────────────────────────────────────────────────────────

fn handle_insert(key: Key, actions: &mut Vec<EditorAction>) {
    match key {
        Key::Escape => actions.push(EditorAction::SetMode(Mode::Normal)),
        Key::Backspace => actions.push(EditorAction::DeleteBackward),
        Key::Enter => actions.push(EditorAction::InsertChar('\n')),
        Key::Char(c) => actions.push(EditorAction::InsertChar(c)),
        Key::Left => actions.push(EditorAction::Move(Motion::Left(1))),
        Key::Right => actions.push(EditorAction::Move(Motion::Right(1))),
        Key::Up => actions.push(EditorAction::Move(Motion::Up(1))),
        Key::Down => actions.push(EditorAction::Move(Motion::Down(1))),
        _ => {}
    }
}

// ── Normal mode ───────────────────────────────────────────────────────────────

fn handle_normal(key: Key, pending: &mut VimPending, actions: &mut Vec<EditorAction>) {
    // Collect count digits
    if let Key::Char(c) = key {
        if c.is_ascii_digit() && (c != '0' || !pending.count_str.is_empty()) {
            pending.count_str.push(c);
            return;
        }
    }

    let n = pending.count();

    // Handle `g` prefix (gg, gj, gk …)
    if pending.g_prefix {
        pending.g_prefix = false;
        match key {
            Key::Char('g') => {
                actions.push(EditorAction::Move(Motion::FirstLine));
            }
            Key::Char('e') => {
                for _ in 0..n {
                    actions.push(EditorAction::Move(Motion::WordEnd));
                }
            }
            _ => {}
        }
        pending.clear();
        return;
    }

    // Handle pending operator + motion
    if let Some(op) = pending.operator {
        let motion = key_to_motion(&key, n);
        if let Some(m) = motion {
            match op {
                'd' => actions.push(EditorAction::DeleteMotion(m)),
                'c' => {
                    actions.push(EditorAction::DeleteMotion(m.clone()));
                    actions.push(EditorAction::SetMode(Mode::Insert));
                }
                'y' => actions.push(EditorAction::YankMotion(m)),
                _ => {}
            }
            pending.clear();
            return;
        }
        // `dd`, `cc`, `yy`
        if let Key::Char(c) = key {
            if c == op {
                match op {
                    'd' => actions.push(EditorAction::DeleteLine),
                    'c' => {
                        actions.push(EditorAction::DeleteLine);
                        actions.push(EditorAction::SetMode(Mode::Insert));
                    }
                    'y' => actions.push(EditorAction::YankLine),
                    _ => {}
                }
                pending.clear();
                return;
            }
        }
        // Unknown — abort
        pending.clear();
        return;
    }

    match key {
        // Movement
        Key::Char('h') | Key::Left => {
            actions.push(EditorAction::Move(Motion::Left(n)))
        }
        Key::Char('l') | Key::Right => {
            actions.push(EditorAction::Move(Motion::Right(n)))
        }
        Key::Char('j') | Key::Down => {
            actions.push(EditorAction::Move(Motion::Down(n)))
        }
        Key::Char('k') | Key::Up => {
            actions.push(EditorAction::Move(Motion::Up(n)))
        }
        Key::Char('w') => actions.push(EditorAction::Move(Motion::WordForward(n))),
        Key::Char('b') => actions.push(EditorAction::Move(Motion::WordBackward(n))),
        Key::Char('e') => actions.push(EditorAction::Move(Motion::WordEnd)),
        Key::Char('0') => actions.push(EditorAction::Move(Motion::LineStart)),
        Key::Char('^') => actions.push(EditorAction::Move(Motion::FirstNonBlank)),
        Key::Char('$') => actions.push(EditorAction::Move(Motion::LineEnd)),
        Key::Char('G') => actions.push(EditorAction::Move(Motion::LastLine)),
        Key::Char('g') => {
            pending.g_prefix = true;
            return; // don't clear yet
        }

        // Enter insert mode
        Key::Char('i') => actions.push(EditorAction::SetMode(Mode::Insert)),
        Key::Char('I') => {
            actions.push(EditorAction::Move(Motion::FirstNonBlank));
            actions.push(EditorAction::SetMode(Mode::Insert));
        }
        Key::Char('a') => {
            actions.push(EditorAction::Move(Motion::Right(1)));
            actions.push(EditorAction::SetMode(Mode::Insert));
        }
        Key::Char('A') => {
            actions.push(EditorAction::Move(Motion::LineEnd));
            actions.push(EditorAction::SetMode(Mode::Insert));
        }
        Key::Char('o') => {
            actions.push(EditorAction::Move(Motion::LineEnd));
            actions.push(EditorAction::InsertChar('\n'));
            actions.push(EditorAction::SetMode(Mode::Insert));
        }
        Key::Char('O') => {
            actions.push(EditorAction::Move(Motion::LineStart));
            actions.push(EditorAction::InsertChar('\n'));
            actions.push(EditorAction::Move(Motion::Up(1)));
            actions.push(EditorAction::SetMode(Mode::Insert));
        }

        // Delete / change
        Key::Char('x') => actions.push(EditorAction::DeleteCharForward),
        Key::Char('X') => actions.push(EditorAction::DeleteBackward),
        Key::Char('d') => {
            pending.operator = Some('d');
            return;
        }
        Key::Char('c') => {
            pending.operator = Some('c');
            return;
        }
        Key::Char('y') => {
            pending.operator = Some('y');
            return;
        }

        // Paste
        Key::Char('p') => actions.push(EditorAction::PasteAfter),
        Key::Char('P') => actions.push(EditorAction::PasteBefore),

        // Visual modes
        Key::Char('v') => {
            actions.push(EditorAction::SetMode(Mode::Visual { line: false }))
        }
        Key::Char('V') => {
            actions.push(EditorAction::SetMode(Mode::Visual { line: true }))
        }

        // Enter: create a new line below current line, stay in Normal mode
        Key::Enter => {
            actions.push(EditorAction::Move(Motion::LineEnd));
            actions.push(EditorAction::InsertChar('\n'));
        }

        // Undo / redo
        Key::Char('u') => actions.push(EditorAction::Undo),

        Key::Escape => {
            // Already in normal; ensure clean state
            pending.clear();
        }
        _ => {}
    }

    pending.clear();
}

// ── Visual mode ───────────────────────────────────────────────────────────────

fn handle_visual(key: Key, pending: &mut VimPending, actions: &mut Vec<EditorAction>) {
    // Collect count digits
    if let Key::Char(c) = key {
        if c.is_ascii_digit() && (c != '0' || !pending.count_str.is_empty()) {
            pending.count_str.push(c);
            return;
        }
    }
    let n = pending.count();

    match key {
        Key::Escape => actions.push(EditorAction::SetMode(Mode::Normal)),
        Key::Char('h') | Key::Left => {
            actions.push(EditorAction::Move(Motion::Left(n)))
        }
        Key::Char('l') | Key::Right => {
            actions.push(EditorAction::Move(Motion::Right(n)))
        }
        Key::Char('j') | Key::Down => {
            actions.push(EditorAction::Move(Motion::Down(n)))
        }
        Key::Char('k') | Key::Up => {
            actions.push(EditorAction::Move(Motion::Up(n)))
        }
        Key::Char('w') => actions.push(EditorAction::Move(Motion::WordForward(n))),
        Key::Char('b') => actions.push(EditorAction::Move(Motion::WordBackward(n))),
        Key::Char('d') => actions.push(EditorAction::DeleteSelection),
        Key::Char('y') => actions.push(EditorAction::YankSelection),
        Key::Char('c') => {
            actions.push(EditorAction::DeleteSelection);
            actions.push(EditorAction::SetMode(Mode::Insert));
        }
        _ => {}
    }
    pending.clear();
}

// ── Motion helpers ────────────────────────────────────────────────────────────

fn key_to_motion(key: &Key, n: usize) -> Option<Motion> {
    match key {
        Key::Char('h') | Key::Left => Some(Motion::Left(n)),
        Key::Char('l') | Key::Right => Some(Motion::Right(n)),
        Key::Char('j') | Key::Down => Some(Motion::Down(n)),
        Key::Char('k') | Key::Up => Some(Motion::Up(n)),
        Key::Char('w') => Some(Motion::WordForward(n)),
        Key::Char('b') => Some(Motion::WordBackward(n)),
        Key::Char('e') => Some(Motion::WordEnd),
        Key::Char('0') => Some(Motion::LineStart),
        Key::Char('$') => Some(Motion::LineEnd),
        _ => None,
    }
}

// ── Motion enum (shared with Helix) ──────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Motion {
    Left(usize),
    Right(usize),
    Up(usize),
    Down(usize),
    WordForward(usize),
    WordBackward(usize),
    WordEnd,
    LineStart,
    LineEnd,
    FirstNonBlank,
    FirstLine,
    LastLine,
}
