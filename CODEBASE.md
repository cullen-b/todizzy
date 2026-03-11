# Todizzy — Codebase Context

Minimal macOS menu-bar notes app. Written in pure Rust using `objc2 0.5` + AppKit bindings (no Electron, no webview). ~3700 lines total across 10 source files.

---

## High-level architecture

```
main.rs
  └─ app::run_app(data_dir)
       ├─ AppDelegate          (NSObject + NSApplicationDelegate + NSWindowDelegate)
       │    ├─ NSStatusItem    menu-bar icon; left-click toggles window
       │    ├─ NSPanel         floating editor window
       │    ├─ EditorView      custom NSTextView subclass (owns engine + gesture state)
       │    ├─ NoteStore       persistence (one .txt file per note)
       │    ├─ Settings        JSON config
       │    └─ settings_panel  lazily-created secondary window
       └─ EditorView
            ├─ EditorEngine    pure-Rust Vim/Helix state machine
            └─ SwipeDetector   pure-Rust trackpad gesture accumulator
```

**Cross-layer communication**: `EditorView` posts `NSNotification`s (`TodizzyTextChanged`, `TodizzySwipeLeft/Right`, `TodizzyModeChanged`, `TodizzyHideWindow`, `TodizzyOpenSettings`) to `NSNotificationCenter`. `AppDelegate` observes them. This keeps `EditorView` free of a back-reference to `AppDelegate`, avoiding retain cycles.

---

## File-by-file

### `src/main.rs` (33 lines)
Entry point. Resolves data directory (`~/Library/Application Support/todizzy/` or `$TODIZZY_DATA_DIR` override) then calls `app::run_app`. Nothing else.

---

### `src/app/mod.rs` (~1925 lines) — the biggest file
All AppKit/ObjC code lives here. Three ObjC subclasses defined with the `declare_class!` macro:

**`PageDotsView`** — tiny `NSView` subclass that draws the row of page indicator dots. Ivars: `count: Cell<usize>`, `current: Cell<usize>`. Exposes `set_page(count, current)` which calls `setNeedsDisplay`.

**`EditorView`** — `NSTextView` subclass. Core of the editing experience.
- Ivars: `engine: RefCell<EditorEngine>`, `swipe: RefCell<SwipeDetector>`, `formatting: Cell<bool>` (re-entrancy guard), `last_escape_normal: RefCell<Option<Instant>>` (double-Escape → hide), `base_font_size: Cell<f64>` (stable reference used by markdown formatter).
- `configure(font_size)` — called once after construction; sets up font, colors, spell-check off, insertion-point color.
- `keyDown` override — translates `NSEvent` → `Key`, feeds to `engine.process_key()`, then calls `sync_text_view()` and `apply_nsview_cursor()`.
- `scrollWheel` override — routes events through `SwipeDetector`; posts swipe notifications on threshold.
- `didChangeText` — posts `TodizzyTextChanged`.
- `sync_text_view()` — pushes engine buffer contents into `NSTextView` when the buffer was mutated; calls `apply_markdown_formatting()` afterwards.
- `apply_nsview_cursor(utf16_pos)` — sets `NSTextView.selectedRange` to show the block cursor in Normal mode:
  - Non-newline char → `(pos, 1)` — 1-char selection shows block highlight.
  - `\n` or EOB with previous non-`\n` char on same line → step back, show block on last visible char.
  - Empty line or start of buffer → `(pos, 0)` with `updateInsertionPointStateAndRestartTimer:true`.
  - Insert/Visual → `(pos, 0)` — standard caret.
- `apply_markdown_formatting()` — per-line markdown renderer:
  - Iterates lines; resets each line's attributes to `def_dict` (default font/color, no strikethrough, no obliqueness).
  - Applies line-level pattern: `# ` → H1 (bold, +2pt), `=` at start → bold, `>` at start → gray.
  - Scans inline patterns **within that line's byte slice only**: `**…**` bold, `*…*` italic, `` `…` `` teal, `~~…~~` strikethrough.
  - Uses `base_font_size` (set in `configure`) — never reads `[self font].pointSize` which would grow unboundedly after heading formatting.
  - Calls `setTypingAttributes: def_dict` after `endEditing` so the next keystroke never inherits heading/bold attrs.
  - Wrapped in `formatting: Cell<bool>` guard to prevent re-entrancy from `setAttributes:` triggering `didChangeText`.

**`AppDelegate`** — owns everything at the app level.
- Ivars: `core: RefCell<AppCore>` (NoteStore + Settings + current_note index + data_dir), `panel`, `editor_view`, `status_item`, `page_dots`, `mode_label`, `nav_buttons`, `settings_panel`, `saved_window_origin: RefCell<Option<NSPoint>>`, `global_monitor`.
- `show_window()` — makes the panel visible. On first open (no saved origin) calls `position_window_below_status_bar()`; on subsequent opens restores `saved_window_origin` only if it falls within the same screen as the status bar item (multi-monitor aware). Triggers git pull if `git_sync` enabled.
- `hide_window()` — saves `panel.frame.origin` to `saved_window_origin`, calls `orderOut:`, triggers git push if `git_sync` enabled.
- `position_window_below_status_bar()` — reads status item button's screen frame and places the panel just below the menu bar on that screen.
- `status_bar_screen_frame()` — returns the `NSRect` of the screen containing the status bar button (for multi-monitor positioning).
- `load_note(idx)` — loads note from `NoteStore`, sets engine content, refreshes UI.
- `save_current_note()` — writes engine buffer to `NoteStore`.
- Notification handlers: `on_text_changed`, `on_swipe_left/right` (page navigation), `on_mode_changed` (updates N/I/V label), `on_hide_window` (double-Escape), `on_open_settings`.

---

### `src/editor/mod.rs` (6 lines)
Re-exports `EditorEngine`, `Key`, `Mode` from submodules. `pub mod buffer, engine, helix, vim`.

---

### `src/editor/engine.rs` (~537 lines)
Pure-Rust state machine. No ObjC, no I/O — fully unit-testable.

**`Key`** enum — platform-normalised key events: `Char(char)`, `Escape`, `Backspace`, `Delete`, `Enter`, `Tab`, arrow keys, `Home`/`End`/`PageUp`/`PageDown`.

**`Mode`** enum — `Normal`, `Insert`, `Visual { line: bool }`.

**`EditorAction`** enum — commands produced by `VimHandler`/`HelixHandler` and interpreted by the engine: `Move(Motion)`, `MoveExtend(Motion)`, `InsertChar`, `DeleteBackward`, `DeleteCharForward`, `DeleteMotion`, `DeleteLine`, `DeleteSelection`, `YankMotion`, `YankLine`, `YankSelection`, `SelectLine`, `PasteAfter/Before`, `Undo`, `SetMode`.

**`MotionHandler`** trait — `handle_key(key, mode, pending, actions)` — implemented by `VimHandler` and `HelixHandler`.

**`EditorEngine`** — owns `Buffer`, `Mode`, `selection_anchor: Option<usize>` (byte offset), `VimPending`, yank register, undo stack (max 128 snapshots as `(String, cursor)` pairs).
- Helix invariant: `selection_anchor` is always `Some` when `mode == Normal` (engine enforces this at end of `process_key`).
- `process_key(key) -> bool` — dispatches to handler, applies resulting `EditorAction`s, returns `true` if buffer was mutated.
- `apply_actions` — interprets each `EditorAction`: mutates buffer, manages selection_anchor, pushes undo snapshots.

---

### `src/editor/buffer.rs` (~367 lines)
`Buffer` — plain `String` with a cursor byte-offset. All cursor arithmetic is in bytes; all methods clamp to valid char boundaries and never panic.

Key methods: `insert_char_at_cursor`, `delete_char_at_cursor`, `delete_backward`, `move_left/right/up/down`, `move_word_forward/backward/end`, `move_line_start/end`, `move_to_first/last_line`, `delete_line`, `line_start/end`, `byte_to_lc` (byte offset → (line, col)).

---

### `src/editor/vim.rs` (~309 lines)
`VimHandler` implements `MotionHandler` for Vim-style modal editing.

**`VimPending`** — state for multi-key sequences: `count_str` (accumulated digit prefix), `operator: Option<char>` (pending `d`/`c`/`y`), `g_prefix: bool` (waiting for second key of `gg`).

Handles: Normal mode (`h/j/k/l`, `w/b/e`, `0/^/$`, `gg/G`, `i/a/o/I/A/O`, `x/X`, `dd/cc/yy`, `d{motion}`, `p/P`, `u`, `v/V`, `Esc`), Insert mode (all chars, Backspace, Enter, Esc, arrows), Visual mode (`d/c/y`, motions, `Esc`).

---

### `src/editor/helix.rs` (~144 lines)
`HelixHandler` implements `MotionHandler` for Helix-style selection-first editing.

Key differences from Vim: `w/b/e` emit `MoveExtend` (extends selection from anchor); `d/c` operate on current selection without needing a motion; no separate Visual mode (selection is always active in Normal); `x` selects current line (`SelectLine`); `i` enters Insert at selection start, `a` at end.

Reuses `VimPending` for the count/g-prefix state; reuses `Motion` type from `vim.rs`.

---

### `src/gestures/mod.rs` (113 lines)
`SwipeDetector` — pure-Rust trackpad gesture accumulator.
- Call `began()` on `NSEventPhase::Began`, `changed(dx, dy)` on each delta, `ended()` on `Ended`/`Cancelled`.
- Direction lock: after 6pt of movement, commits to Horizontal or Vertical. Horizontal gestures are consumed (not passed to scroll view). Fires `Triggered(SwipeDir)` once the horizontal accumulator exceeds 60pt.
- Returns `SwipeOutcome`: `Triggered(dir)`, `Consumed`, or `PassThrough`.

---

### `src/storage/mod.rs` (148 lines)
`NoteStore` — note persistence.
- Each note → `note-<N>.txt` in `~/Library/Application Support/todizzy/notes/`.
- `index.json` in same directory stores note order and a monotonically-increasing ID counter.
- Writes are atomic (write to `.tmp`, then `rename`).
- `NoteId(u32)` — opaque ID type.
- Key methods: `open(dir)`, `load_note(id) -> String`, `save_note(id, content)`, `create_note() -> NoteId`, `delete_note(idx)`.

---

### `src/settings/mod.rs` (95 lines)
`Settings` — user-facing config, stored as pretty-printed JSON at `~/Library/Application Support/todizzy/settings.json`.

Fields: `motion_mode: MotionMode` (Vim/Helix/None), `close_on_focus_loss: bool`, `font_size: f64`, `window_width/height: f64`, `show_nav_arrows: bool`, `show_page_dots: bool`, `show_mode_indicator: bool`, `git_sync: bool`.

All fields `#[serde(default)]` — missing keys fall back to defaults without error. `Settings::load(path)` never fails (returns defaults on missing/corrupt file).

`MotionMode` enum — `Vim` (default), `Helix`, `None`.

---

## Key patterns and gotchas

**`declare_class!` macro**: ObjC subclasses require this macro. Each class has an `Ivars` struct (stored as associated data on the ObjC object). Access via `self.ivars()`. Interior mutability (`Cell`, `RefCell`) is required because `&self` is always shared in ObjC callbacks.

**UTF-8 vs UTF-16**: `Buffer` and `EditorEngine` work in UTF-8 byte offsets. `NSTextView` uses UTF-16 (NSString). `app/mod.rs` contains `utf8_to_utf16` and `utf16_to_utf8` helper functions that convert by iterating characters — called whenever bridging between the two layers.

**`objc2` crate version 0.5**: APIs differ significantly from `objc` or `objc2 0.4`. Feature flags on crate dependencies control which AppKit classes are available. Some classes require multiple features (e.g. `NSStatusItem::button` needs `NSButton + NSControl + NSResponder + NSStatusBarButton + NSView`).

**No `sendEvent` or `keyEquivalent` hacks**: Key handling in Normal mode goes through `keyDown` override in `EditorView`. The engine processes the key, produces actions, and the view syncs state back to `NSTextView`.

**Undo**: `apply_markdown_formatting` disables `undoManager` during attribute changes so formatting never appears in the undo stack. Only actual text mutations go through undo.

**Git sync**: `show_window` spawns a `git pull` thread; `hide_window` spawns a `git push` thread. Both are fire-and-forget (errors are silently ignored). The notes directory must already be a git repo with a remote configured.

---

## Build

```bash
cargo build          # debug
cargo test           # 13 pure-Rust unit tests (no ObjC needed)
make bundle          # builds Todizzy.app at target/release/Todizzy.app
make run             # bundle + open
make dev             # kill existing process, rebuild, relaunch
```

Data dir override for dev: `TODIZZY_DATA_DIR=/tmp/todizzy-dev make dev`
