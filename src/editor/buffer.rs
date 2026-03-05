/// In-memory text buffer with cursor tracking.
///
/// Stores the note as a plain `String` (UTF-8).
/// The cursor is a *byte* offset into the string.
/// All motion methods clamp to valid positions — they never panic.
///
/// For notes (typically <100 KB) a single String is faster than a rope and
/// simpler to reason about.  Upgrade to a gap buffer or rope if needed.

#[derive(Debug, Clone)]
pub struct Buffer {
    content: String,
    /// Cursor byte offset.  Always on a char boundary.
    cursor: usize,
}

impl Buffer {
    pub fn new(content: String) -> Self {
        let cursor = 0;
        Self { content, cursor }
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    pub fn as_str(&self) -> &str {
        &self.content
    }

    pub fn len(&self) -> usize {
        self.content.len()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_cursor(&mut self, byte_offset: usize) {
        self.cursor = self.clamp(byte_offset);
    }

    /// Current (line, col) — 0-indexed.
    pub fn cursor_lc(&self) -> (usize, usize) {
        self.byte_to_lc(self.cursor)
    }

    pub fn line_count(&self) -> usize {
        self.content.lines().count().max(1)
    }

    /// Byte offset of the start of `line` (0-indexed).
    pub fn line_start(&self, line: usize) -> usize {
        let mut offset = 0;
        for (i, l) in self.content.split('\n').enumerate() {
            if i == line {
                return offset;
            }
            offset += l.len() + 1; // +1 for '\n'
        }
        self.content.len()
    }

    /// Byte offset just past the last non-newline char of `line`.
    pub fn line_end(&self, line: usize) -> usize {
        let start = self.line_start(line);
        let rest = &self.content[start..];
        start + rest.find('\n').unwrap_or(rest.len())
    }

    // ── Mutations ─────────────────────────────────────────────────────────────

    /// Insert `text` at the current cursor position and advance the cursor.
    pub fn insert(&mut self, text: &str) {
        let pos = self.cursor;
        self.content.insert_str(pos, text);
        self.cursor += text.len();
    }

    /// Delete the character to the left of the cursor (backspace).
    pub fn delete_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let c = self.prev_char_boundary(self.cursor);
        self.content.drain(c..self.cursor);
        self.cursor = c;
    }

    /// Delete the character under the cursor (Vim `x`).
    pub fn delete_char_forward(&mut self) {
        if self.cursor >= self.content.len() {
            return;
        }
        let next = self.next_char_boundary(self.cursor);
        self.content.drain(self.cursor..next);
        self.cursor = self.clamp(self.cursor);
    }

    /// Delete from `start` to `end` (byte offsets, exclusive end).
    pub fn delete_range(&mut self, start: usize, end: usize) {
        let start = start.min(self.content.len());
        let end = end.min(self.content.len());
        if start >= end {
            return;
        }
        self.content.drain(start..end);
        self.cursor = self.clamp(start);
    }

    /// Replace `range` with `text`.
    pub fn replace_range(&mut self, start: usize, end: usize, text: &str) {
        let start = start.min(self.content.len());
        let end = end.min(self.content.len());
        self.content.replace_range(start..end, text);
        self.cursor = self.clamp(start + text.len());
    }

    pub fn set_content(&mut self, text: String) {
        self.content = text;
        self.cursor = 0;
    }

    /// Append a newline at the very end without moving the cursor.
    /// Used when `j` is pressed on the last line.
    pub fn push_newline_at_end(&mut self) {
        self.content.push('\n');
    }

    // ── Cursor motions ────────────────────────────────────────────────────────

    pub fn move_left(&mut self, n: usize) {
        for _ in 0..n {
            if self.cursor == 0 {
                break;
            }
            self.cursor = self.prev_char_boundary(self.cursor);
            // Don't cross line in normal mode — callers that want EOL wrap should
            // use move_to_end_of_prev_line themselves.
        }
    }

    pub fn move_right(&mut self, n: usize) {
        for _ in 0..n {
            if self.cursor >= self.content.len() {
                break;
            }
            let next = self.next_char_boundary(self.cursor);
            // Don't move onto the newline — vim `l` stops before \n
            if self.content.as_bytes().get(next).copied() == Some(b'\n') {
                // already at line end; stop
                break;
            }
            self.cursor = next;
        }
    }

    pub fn move_up(&mut self, n: usize) {
        let (line, col) = self.cursor_lc();
        let target_line = line.saturating_sub(n);
        let start = self.line_start(target_line);
        let end = self.line_end(target_line);
        self.cursor = (start + col).min(end);
    }

    pub fn move_down(&mut self, n: usize) {
        let (line, col) = self.cursor_lc();
        let max_line = self.line_count().saturating_sub(1);
        let target_line = (line + n).min(max_line);
        let start = self.line_start(target_line);
        let end = self.line_end(target_line);
        self.cursor = (start + col).min(end);
    }

    /// Move to start of current line (Vim `0`).
    pub fn move_to_line_start(&mut self) {
        let (line, _) = self.cursor_lc();
        self.cursor = self.line_start(line);
    }

    /// Move to end of current line (Vim `$`).
    pub fn move_to_line_end(&mut self) {
        let (line, _) = self.cursor_lc();
        self.cursor = self.line_end(line);
    }

    /// Move to first non-blank of current line (Vim `^`).
    pub fn move_to_first_nonblank(&mut self) {
        let (line, _) = self.cursor_lc();
        let start = self.line_start(line);
        let end = self.line_end(line);
        let line_str = &self.content[start..end];
        let offset = line_str
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.cursor = start + offset;
    }

    /// Move to first line (Vim `gg`).
    pub fn move_to_first_line(&mut self) {
        self.cursor = 0;
    }

    /// Move to last line (Vim `G`).
    pub fn move_to_last_line(&mut self) {
        let last = self.line_count().saturating_sub(1);
        self.cursor = self.line_start(last);
    }

    /// Move to start of next word (Vim `w`).
    pub fn move_word_forward(&mut self, n: usize) {
        for _ in 0..n {
            self.cursor = self.next_word_start(self.cursor);
        }
    }

    /// Move to start of previous word (Vim `b`).
    pub fn move_word_backward(&mut self, n: usize) {
        for _ in 0..n {
            self.cursor = self.prev_word_start(self.cursor);
        }
    }

    /// Move to end of word (Vim `e`).
    pub fn move_word_end(&mut self, n: usize) {
        for _ in 0..n {
            self.cursor = self.word_end(self.cursor);
        }
    }

    // ── Line-level helpers (used by engine) ───────────────────────────────────

    /// Byte range of the current line (including newline if present).
    pub fn current_line_range(&self) -> (usize, usize) {
        let (line, _) = self.cursor_lc();
        let start = self.line_start(line);
        let end = self.line_end(line);
        let end_with_nl = (end + 1).min(self.content.len());
        (start, end_with_nl)
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn clamp(&self, offset: usize) -> usize {
        let max = self.content.len();
        let mut o = offset.min(max);
        // Back off to char boundary
        while o > 0 && !self.content.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    fn prev_char_boundary(&self, from: usize) -> usize {
        let mut o = from.saturating_sub(1);
        while o > 0 && !self.content.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    fn next_char_boundary(&self, from: usize) -> usize {
        let mut o = from + 1;
        while o < self.content.len() && !self.content.is_char_boundary(o) {
            o += 1;
        }
        o.min(self.content.len())
    }

    fn byte_to_lc(&self, byte: usize) -> (usize, usize) {
        let before = &self.content[..byte.min(self.content.len())];
        let line = before.chars().filter(|&c| c == '\n').count();
        let col = before.rfind('\n').map(|p| byte - p - 1).unwrap_or(byte);
        (line, col)
    }

    fn next_word_start(&self, from: usize) -> usize {
        let bytes = self.content.as_bytes();
        let mut i = from;
        // Skip current word chars
        while i < bytes.len() && Self::is_word_char(bytes[i]) {
            i += 1;
        }
        // Skip whitespace
        while i < bytes.len() && !Self::is_word_char(bytes[i]) {
            i += 1;
        }
        i
    }

    fn prev_word_start(&self, from: usize) -> usize {
        let bytes = self.content.as_bytes();
        let mut i = from;
        if i == 0 {
            return 0;
        }
        i -= 1;
        // Skip whitespace
        while i > 0 && !Self::is_word_char(bytes[i]) {
            i -= 1;
        }
        // Skip word chars
        while i > 0 && Self::is_word_char(bytes[i - 1]) {
            i -= 1;
        }
        i
    }

    fn word_end(&self, from: usize) -> usize {
        let bytes = self.content.as_bytes();
        let mut i = from;
        if i + 1 >= bytes.len() {
            return i;
        }
        i += 1;
        while i < bytes.len() && !Self::is_word_char(bytes[i]) {
            i += 1;
        }
        while i + 1 < bytes.len() && Self::is_word_char(bytes[i + 1]) {
            i += 1;
        }
        i
    }

    fn is_word_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_cursor() {
        let mut b = Buffer::new(String::new());
        b.insert("hello");
        assert_eq!(b.as_str(), "hello");
        assert_eq!(b.cursor(), 5);
    }

    #[test]
    fn delete_backward() {
        let mut b = Buffer::new("hello".into());
        b.set_cursor(5);
        b.delete_backward();
        assert_eq!(b.as_str(), "hell");
    }

    #[test]
    fn move_word() {
        let mut b = Buffer::new("hello world foo".into());
        b.move_word_forward(1);
        assert_eq!(b.cursor(), 6); // start of "world"
    }

    #[test]
    fn line_navigation() {
        let mut b = Buffer::new("line1\nline2\nline3".into());
        b.move_down(1);
        assert_eq!(b.cursor_lc(), (1, 0));
        b.move_to_line_end();
        assert_eq!(b.cursor_lc(), (1, 5));
    }
}
