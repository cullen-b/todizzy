/// Helix-style motion handler.
///
/// Helix uses a selection-first model: every motion extends or sets the
/// selection, then operators act on it.  Multi-cursor is not required yet;
/// we implement the core single-selection modal layer.
///
/// Key differences from Vim:
///   • `w`/`b`/`e` move AND select to the next word boundary.
///   • `c`/`d` operate on the current selection without needing a motion.
///   • There is no separate Visual mode — the selection is always visible.
///   • `i` enters Insert at the start of selection; `a` appends after end.
///   • `x` selects the current line (extend with `X`).

use super::engine::{EditorAction, Key, Mode, MotionHandler};
use super::vim::{Motion, VimPending};

pub struct HelixHandler;

impl MotionHandler for HelixHandler {
    fn handle_key(
        &self,
        key: Key,
        mode: &Mode,
        pending: &mut VimPending,
        actions: &mut Vec<EditorAction>,
    ) {
        match mode {
            Mode::Insert => handle_insert(key, actions),
            Mode::Normal | Mode::Visual { .. } => {
                handle_normal(key, pending, actions)
            }
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

// ── Normal / selection mode ───────────────────────────────────────────────────

fn handle_normal(key: Key, pending: &mut VimPending, actions: &mut Vec<EditorAction>) {
    // Count accumulation
    if let Key::Char(c) = key {
        if c.is_ascii_digit() && (c != '0' || !pending.count_str.is_empty()) {
            pending.count_str.push(c);
            return;
        }
    }
    let n = pending.count();

    // `g` prefix
    if pending.g_prefix {
        pending.g_prefix = false;
        if let Key::Char('g') = key {
            actions.push(EditorAction::Move(Motion::FirstLine));
        }
        pending.clear();
        return;
    }

    match key {
        // ── Movement (extends selection in Helix) ──────────────────────────
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
        Key::Char('$') => actions.push(EditorAction::Move(Motion::LineEnd)),
        Key::Char('G') => actions.push(EditorAction::Move(Motion::LastLine)),
        Key::Char('g') => {
            pending.g_prefix = true;
            return;
        }

        // ── Select current line (Helix `x`) ───────────────────────────────
        Key::Char('x') => actions.push(EditorAction::SelectLine),

        // ── Operators (act on current selection) ──────────────────────────
        Key::Char('d') => actions.push(EditorAction::DeleteSelection),
        Key::Char('c') => {
            actions.push(EditorAction::DeleteSelection);
            actions.push(EditorAction::SetMode(Mode::Insert));
        }
        Key::Char('y') => actions.push(EditorAction::YankSelection),

        // ── Enter insert ──────────────────────────────────────────────────
        // `i` = insert before selection start
        Key::Char('i') => actions.push(EditorAction::SetMode(Mode::Insert)),
        // `a` = append after selection end
        Key::Char('a') => {
            actions.push(EditorAction::Move(Motion::Right(1)));
            actions.push(EditorAction::SetMode(Mode::Insert));
        }
        Key::Char('o') => {
            actions.push(EditorAction::Move(Motion::LineEnd));
            actions.push(EditorAction::InsertChar('\n'));
            actions.push(EditorAction::SetMode(Mode::Insert));
        }

        // ── Paste ─────────────────────────────────────────────────────────
        Key::Char('p') => actions.push(EditorAction::PasteAfter),
        Key::Char('P') => actions.push(EditorAction::PasteBefore),

        // ── Undo ─────────────────────────────────────────────────────────
        Key::Char('u') => actions.push(EditorAction::Undo),

        Key::Escape => {
            actions.push(EditorAction::SetMode(Mode::Normal));
        }
        _ => {}
    }

    pending.clear();
}
