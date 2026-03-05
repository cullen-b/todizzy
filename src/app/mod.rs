/// macOS AppKit integration layer.
///
/// Architecture
/// ────────────
/// AppDelegate (NSObject + NSApplicationDelegate + NSWindowDelegate)
///   ├─ owns NSStatusItem     (menu-bar button — left-click toggles, right-click menu)
///   ├─ owns NSPanel          (floating editor window, no traffic-light buttons)
///   ├─ owns EditorView       (custom NSTextView subclass)
///   ├─ owns NoteStore        (persistence)
///   ├─ owns Settings
///   ├─ owns global_monitor   (⌥Space shortcut token)
///   └─ owns settings_panel   (lazily-created settings window)
///
/// EditorView (NSTextView subclass)
///   ├─ owns EditorEngine  (Vim / Helix state machine — pure Rust)
///   └─ owns SwipeDetector (trackpad gesture state — pure Rust)
///
/// Communication: EditorView → AppDelegate via NSNotificationCenter.
/// This keeps EditorView free of a back-reference (no retain cycle).

use std::{cell::{Cell, RefCell}, path::PathBuf};

use objc2::{
    declare_class, msg_send, msg_send_id,
    mutability::MainThreadOnly,
    rc::Retained,
    runtime::{AnyObject, Sel},
    ClassType, DeclaredClass,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSButton,
    NSColor, NSEvent, NSEventMask, NSEventPhase, NSFont, NSMenu, NSMenuItem,
    NSPanel, NSScrollView, NSSegmentedControl, NSStatusBar, NSStatusItem,
    NSText, NSTextField, NSTextView, NSView, NSWindow, NSWindowButton,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSNotification, NSNotificationCenter,
    NSNotificationName, NSObject, NSObjectProtocol, NSPoint, NSRange, NSRect,
    NSSize, NSString,
};

use crate::{
    editor::{EditorEngine, Key, Mode},
    gestures::{SwipeDetector, SwipeDir, SwipeOutcome},
    settings::{MotionMode, Settings},
    storage::NoteStore,
};

// ── Notification names ────────────────────────────────────────────────────────

fn notif_text_changed() -> &'static NSNotificationName {
    ns_string!("TodizzyTextChanged")
}
fn notif_swipe_left() -> &'static NSNotificationName {
    ns_string!("TodizzySwipeLeft")
}
fn notif_swipe_right() -> &'static NSNotificationName {
    ns_string!("TodizzySwipeRight")
}
fn notif_open_settings() -> &'static NSNotificationName {
    ns_string!("TodizzyOpenSettings")
}
fn notif_mode_changed() -> &'static NSNotificationName {
    ns_string!("TodizzyModeChanged")
}
fn notif_hide_window() -> &'static NSNotificationName {
    ns_string!("TodizzyHideWindow")
}

// ─────────────────────────────────────────────────────────────────────────────
// PageDotsView  (NSView subclass — draws page-indicator dots)
// ─────────────────────────────────────────────────────────────────────────────

struct PageDotsViewIvars {
    count:   Cell<usize>,
    current: Cell<usize>,
}

declare_class!(
    struct PageDotsView;

    unsafe impl ClassType for PageDotsView {
        type Super = NSView;
        type Mutability = MainThreadOnly;
        const NAME: &'static str = "TodizzyPageDotsView";
    }

    impl DeclaredClass for PageDotsView {
        type Ivars = PageDotsViewIvars;
    }

    unsafe impl NSObjectProtocol for PageDotsView {}

    unsafe impl PageDotsView {
        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            let count   = self.ivars().count.get();
            let current = self.ivars().current.get();
            if count == 0 { return; }

            let bounds: NSRect = unsafe { msg_send![self, bounds] };
            let dot_d  = 6.0f64;
            let gap    = 5.0f64;
            let step   = dot_d + gap;
            let total_w = count as f64 * dot_d + count.saturating_sub(1) as f64 * gap;
            let x0 = (bounds.size.width  - total_w) / 2.0;
            let y0 = (bounds.size.height - dot_d)   / 2.0;

            for i in 0..count {
                let x = x0 + i as f64 * step;
                let rect = NSRect::new(NSPoint::new(x, y0), NSSize::new(dot_d, dot_d));
                let (r, g, b, a): (f64, f64, f64, f64) = if i == current {
                    (0.25, 0.50, 0.95, 1.00) // solid blue — active page
                } else {
                    (0.60, 0.75, 1.00, 0.55) // light translucent — inactive
                };
                unsafe {
                    let color: Retained<AnyObject> = msg_send_id![
                        objc2::class!(NSColor),
                        colorWithRed: r green: g blue: b alpha: a
                    ];
                    let _: () = msg_send![&*color, set];
                    let path: Retained<AnyObject> = msg_send_id![
                        objc2::class!(NSBezierPath),
                        bezierPathWithOvalInRect: rect
                    ];
                    let _: () = msg_send![&*path, fill];
                }
            }
        }

        /// Pass mouse events through so the window remains draggable.
        #[method_id(hitTest:)]
        fn hit_test(&self, _pt: NSPoint) -> Option<Retained<NSView>> {
            None
        }
    }
);

impl PageDotsView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let ivars = PageDotsViewIvars {
            count:   Cell::new(1),
            current: Cell::new(0),
        };
        let this = mtm.alloc::<Self>().set_ivars(ivars);
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }

    pub fn set_state(&self, count: usize, current: usize) {
        self.ivars().count.set(count);
        self.ivars().current.set(current);
        unsafe { let _: () = msg_send![self, setNeedsDisplay: true]; }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EditorView  (NSTextView subclass)
// ─────────────────────────────────────────────────────────────────────────────

struct EditorViewIvars {
    engine:              RefCell<EditorEngine>,
    swipe:               RefCell<SwipeDetector>,
    formatting:          Cell<bool>,
    last_escape_normal:  RefCell<Option<std::time::Instant>>,
}

declare_class!(
    struct EditorView;

    unsafe impl ClassType for EditorView {
        type Super = NSTextView;
        type Mutability = MainThreadOnly;
        const NAME: &'static str = "TodizzyEditorView";
    }

    impl DeclaredClass for EditorView {
        type Ivars = EditorViewIvars;
    }

    unsafe impl NSObjectProtocol for EditorView {}

    unsafe impl EditorView {
        /// Intercept every key event.
        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            self.handle_key_event(event);
        }

        /// Intercept scroll wheel for note-page swiping.
        #[method(scrollWheel:)]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.handle_scroll_wheel(event);
        }

        /// Called after NSTextView finishes a text change.
        #[method(didChangeText)]
        fn did_change_text(&self) {
            let _: () = unsafe { msg_send![super(self), didChangeText] };
            self.on_text_changed();
        }

        /// Draw a block-shaped cursor in Normal mode; fall back to the default
        /// thin insertion point in Insert / Visual mode.
        #[method(drawInsertionPointInRect:color:turnedOn:)]
        fn draw_insertion_point_in_rect(
            &self,
            rect:      NSRect,
            color:     &NSColor,
            turned_on: bool,
        ) {
            let is_normal = {
                let eng = self.ivars().engine.borrow();
                eng.mode == Mode::Normal
            };

            if !is_normal {
                let _: () = unsafe {
                    msg_send![super(self), drawInsertionPointInRect: rect
                                                              color: color
                                                           turnedOn: turned_on]
                };
                return;
            }

            // Measure one character width using the font's maximum advance.
            let char_w: f64 = unsafe {
                let tv = self as &NSTextView;
                let font: *mut AnyObject = msg_send![tv, font];
                if !font.is_null() {
                    let adv: NSSize = msg_send![font, maximumAdvancement];
                    adv.width.max(1.0)
                } else {
                    8.4
                }
            };

            let block = NSRect::new(rect.origin, NSSize::new(char_w, rect.size.height));

            unsafe {
                if turned_on {
                    let _: () = msg_send![color, set];
                    let _: () = msg_send![objc2::class!(NSBezierPath), fillRect: block];
                } else {
                    // Invalidate the full block area so the text beneath is
                    // redrawn cleanly without the cursor ghost.
                    let _: () = msg_send![self, setNeedsDisplayInRect: block];
                }
            }
        }

    }
);

impl EditorView {
    fn new(mtm: MainThreadMarker, frame: NSRect, motion_mode: MotionMode) -> Retained<Self> {
        let ivars = EditorViewIvars {
            engine:             RefCell::new(EditorEngine::new(String::new(), motion_mode)),
            swipe:              RefCell::new(SwipeDetector::default()),
            formatting:         Cell::new(false),
            last_escape_normal: RefCell::new(None),
        };
        let this = mtm.alloc::<Self>().set_ivars(ivars);
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }

    // ── Key event dispatch ────────────────────────────────────────────────────

    fn handle_key_event(&self, event: &NSEvent) {
        let key = match nsevent_to_key(event) {
            Some(k) => k,
            None => {
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
                return;
            }
        };

        let in_insert = {
            let eng = self.ivars().engine.borrow();
            eng.mode == Mode::Insert && key != Key::Escape
        };

        // Double-Escape in Normal mode → hide the window without touching the mouse.
        if key == Key::Escape && !in_insert {
            let is_normal = self.ivars().engine.borrow().mode == Mode::Normal;
            if is_normal {
                let now = std::time::Instant::now();
                let prev = self.ivars().last_escape_normal.borrow().clone();
                if let Some(t) = prev {
                    if now.duration_since(t).as_millis() < 400 {
                        *self.ivars().last_escape_normal.borrow_mut() = None;
                        let nc = unsafe { NSNotificationCenter::defaultCenter() };
                        unsafe { nc.postNotificationName_object(notif_hide_window(), None::<&AnyObject>) };
                        return;
                    }
                }
                *self.ivars().last_escape_normal.borrow_mut() = Some(now);
            }
        }

        if in_insert {
            let _: () = unsafe { msg_send![super(self), keyDown: event] };
            self.sync_buffer_from_nsview();
        } else {
            let (content_changed, new_content, cursor_utf16) = {
                let mut eng = self.ivars().engine.borrow_mut();
                let before = eng.buf.as_str().to_owned();
                eng.process_key(key);
                let after = eng.buf.as_str().to_owned();
                let changed = after != before;
                let cursor_utf16 = utf8_to_utf16(&after, eng.buf.cursor());
                (changed, after, cursor_utf16)
            };

            if content_changed {
                self.apply_nsview_string(&new_content);
                // setString: fires didChangeText → sync_buffer_from_nsview, which
                // reads NSTextView's selection (reset to 0 by setString:) and sets
                // eng.buf.cursor = 0.  Restore the position the engine computed.
                let byte_pos = utf16_to_utf8(&new_content, cursor_utf16);
                self.ivars().engine.borrow_mut().buf.set_cursor(byte_pos);
            }
            self.apply_nsview_cursor(cursor_utf16);
        }

        // Keep the mode indicator in sync after every key press.
        self.post_mode_notification();
    }

    // ── Swipe / scroll-wheel ──────────────────────────────────────────────────

    fn handle_scroll_wheel(&self, event: &NSEvent) {
        let precise = unsafe { event.hasPreciseScrollingDeltas() };
        if !precise {
            let _: () = unsafe { msg_send![super(self), scrollWheel: event] };
            return;
        }

        let dx = unsafe { event.scrollingDeltaX() };
        let dy = unsafe { event.scrollingDeltaY() };
        let phase = unsafe { event.phase() };

        if phase.contains(NSEventPhase::Began) {
            self.ivars().swipe.borrow_mut().began();
            // Pass Began through so NSScrollView can set up its own state.
            let _: () = unsafe { msg_send![super(self), scrollWheel: event] };
            return;
        }

        if phase.contains(NSEventPhase::Ended) || phase.contains(NSEventPhase::Cancelled) {
            self.ivars().swipe.borrow_mut().ended();
            let _: () = unsafe { msg_send![super(self), scrollWheel: event] };
            return;
        }

        if phase.contains(NSEventPhase::Changed) {
            let outcome = self.ivars().swipe.borrow_mut().changed(dx, dy);
            match outcome {
                SwipeOutcome::Triggered(dir) => {
                    self.post_swipe_notification(dir);
                    // Don't forward — we consumed this gesture.
                }
                SwipeOutcome::Consumed => {
                    // Horizontal swipe building up — swallow the event so the
                    // scroll view doesn't partially scroll the text.
                }
                SwipeOutcome::PassThrough => {
                    let _: () = unsafe { msg_send![super(self), scrollWheel: event] };
                }
            }
        }
    }

    fn post_mode_notification(&self) {
        let nc = unsafe { NSNotificationCenter::defaultCenter() };
        unsafe { nc.postNotificationName_object(notif_mode_changed(), None::<&AnyObject>) };
    }

    fn post_swipe_notification(&self, dir: SwipeDir) {
        let nc = unsafe { NSNotificationCenter::defaultCenter() };
        let name = match dir {
            SwipeDir::Left  => notif_swipe_left(),
            SwipeDir::Right => notif_swipe_right(),
        };
        unsafe { nc.postNotificationName_object(name, None::<&AnyObject>) };
    }

    // ── Text-changed notification ─────────────────────────────────────────────

    fn on_text_changed(&self) {
        self.sync_buffer_from_nsview();
        self.apply_markdown_formatting();
        let nc = unsafe { NSNotificationCenter::defaultCenter() };
        unsafe { nc.postNotificationName_object(notif_text_changed(), None::<&AnyObject>) };
    }

    // ── NSTextView ↔ Buffer sync ──────────────────────────────────────────────

    fn sync_buffer_from_nsview(&self) {
        let content: String = unsafe {
            let tv = self as &NSTextView;
            let t  = tv as &NSText;
            t.string().to_string()
        };
        let range: NSRange = unsafe {
            let t = self as &NSTextView as &NSText;
            t.selectedRange()
        };
        let cursor = utf16_to_utf8(&content, range.location);

        let mut eng = self.ivars().engine.borrow_mut();
        eng.buf.set_content(content);
        eng.buf.set_cursor(cursor);
    }

    fn apply_nsview_string(&self, text: &str) {
        let ns = NSString::from_str(text);
        unsafe {
            let t = self as &NSTextView as &NSText;
            t.setString(&ns);
        }
    }

    fn apply_nsview_cursor(&self, utf16_pos: usize) {
        // In Normal mode show a block cursor (1-char selection) or, in Helix mode,
        // highlight the full anchor→cursor selection.
        // We skip newlines: selecting '\n' makes NSTextView highlight the entire
        // line width, which looks wrong — fall back to thin insertion point instead.
        let (loc, len): (usize, usize) = {
            let eng = self.ivars().engine.borrow();
            match eng.mode {
                Mode::Normal => {
                    let text = eng.buf.as_str();
                    let cursor_byte = utf16_to_utf8(text, utf16_pos);
                    if eng.motion_mode() == MotionMode::Helix {
                        let anchor = eng.selection_anchor.unwrap_or(cursor_byte);
                        let (lo, hi) = if anchor <= cursor_byte {
                            (anchor, cursor_byte)
                        } else {
                            (cursor_byte, anchor)
                        };
                        // Always show at least 1 char even on collapsed selection.
                        let hi_adj = if lo == hi {
                            text[lo..].chars().next()
                                .map(|c| lo + c.len_utf8())
                                .unwrap_or(lo)
                        } else {
                            hi
                        };
                        let lo_u16 = utf8_to_utf16(text, lo);
                        let hi_u16 = utf8_to_utf16(text, hi_adj);
                        (lo_u16, hi_u16.saturating_sub(lo_u16))
                    } else {
                        // Vim: 1-char block cursor, skip newlines.
                        match text[cursor_byte..].chars().next() {
                            Some(c) if c != '\n' => (utf16_pos, 1),
                            _ => (utf16_pos, 0),
                        }
                    }
                }
                _ => (utf16_pos, 0),
            }
        };
        let range = NSRange { location: loc, length: len };
        unsafe { (self as &NSTextView).setSelectedRange(range) };
    }

    // ── Public interface used by AppDelegate ──────────────────────────────────

    pub fn load_content(&self, text: &str) {
        {
            let mut eng = self.ivars().engine.borrow_mut();
            eng.set_content(text.to_owned());
        }
        self.apply_nsview_string(text);
        // apply_nsview_cursor reads the engine mode, so the block-cursor
        // selection is set correctly even on initial load.
        self.apply_nsview_cursor(0);
        self.apply_markdown_formatting();
    }

    pub fn current_content(&self) -> String {
        self.ivars().engine.borrow().buf.as_str().to_owned()
    }

    pub fn current_mode(&self) -> Mode {
        self.ivars().engine.borrow().mode.clone()
    }

    pub fn set_motion_mode(&self, mode: MotionMode) {
        self.ivars().engine.borrow_mut().set_motion_mode(mode);
    }

    pub fn configure(&self, font_size: f64) {
        unsafe {
            let tv = self as &NSTextView;

            let font = NSFont::monospacedSystemFontOfSize_weight(font_size, 0.0);
            let _: () = msg_send![tv, setFont: &*font];

            let _: () = msg_send![tv, setBackgroundColor: &*NSColor::textBackgroundColor()];
            let _: () = msg_send![tv, setTextColor: &*NSColor::textColor()];
            let _: () = msg_send![tv, setTextContainerInset: NSSize::new(8.0, 8.0)];
            let _: () = msg_send![tv, setHorizontallyResizable: false];
            let _: () = msg_send![tv, setVerticallyResizable: true];
            tv.setAutoresizingMask(NSAutoresizingMaskOptions::NSViewWidthSizable);

            let _: () = msg_send![tv, setContinuousSpellCheckingEnabled: false];
            let _: () = msg_send![tv, setGrammarCheckingEnabled: false];
            let _: () = msg_send![tv, setAutomaticSpellingCorrectionEnabled: false];
            let _: () = msg_send![tv, setAutomaticDataDetectionEnabled: false];
            let _: () = msg_send![tv, setAutomaticLinkDetectionEnabled: false];

            if let Some(tc) = tv.textContainer() {
                tc.setWidthTracksTextView(true);
            }
        }
    }

    fn apply_markdown_formatting(&self) {
        // Guard against re-entrant calls.
        if self.ivars().formatting.get() { return; }
        self.ivars().formatting.set(true);

        let text = self.ivars().engine.borrow().buf.as_str().to_owned();
        if text.is_empty() {
            self.ivars().formatting.set(false);
            return;
        }

        let spans = markdown_spans(&text);

        unsafe {
            let ts: *mut AnyObject = msg_send![self as &NSTextView, textStorage];
            if ts.is_null() {
                self.ivars().formatting.set(false);
                return;
            }

            let full_u16 = utf8_to_utf16(&text, text.len());
            if full_u16 == 0 {
                self.ivars().formatting.set(false);
                return;
            }

            // Disable undo so attribute changes don't pollute the undo stack.
            let win: *mut AnyObject = msg_send![self as &NSTextView, window];
            let undo: *mut AnyObject = if !win.is_null() {
                msg_send![win, undoManager]
            } else {
                std::ptr::null_mut()
            };
            if !undo.is_null() {
                let _: () = msg_send![undo, disableUndoRegistration];
            }

            let _: () = msg_send![ts, beginEditing];

            // Determine the current font size from the text view.
            let font_pt: f64 = {
                let f: *mut AnyObject = msg_send![self as &NSTextView, font];
                if !f.is_null() { msg_send![f, pointSize] } else { 14.0 }
            };

            // Build default attribute dict: resets all spans on re-format.
            let def_font  = NSFont::monospacedSystemFontOfSize_weight(font_pt, 0.0);
            let def_color = NSColor::textColor();
            let num_zero: Retained<AnyObject>  = msg_send_id![objc2::class!(NSNumber), numberWithInt: 0i32];
            let flt_zero: Retained<AnyObject>  = msg_send_id![objc2::class!(NSNumber), numberWithDouble: 0.0f64];
            let def_dict: Retained<AnyObject>  = msg_send_id![objc2::class!(NSMutableDictionary), new];
            let _: () = msg_send![&*def_dict, setObject: &*def_font  forKey: ns_string!("NSFont")];
            let _: () = msg_send![&*def_dict, setObject: &*def_color forKey: ns_string!("NSColor")];
            let _: () = msg_send![&*def_dict, setObject: &*num_zero  forKey: ns_string!("NSStrikethrough")];
            let _: () = msg_send![&*def_dict, setObject: &*flt_zero  forKey: ns_string!("NSObliqueness")];

            let full_range = NSRange { location: 0, length: full_u16 };
            let _: () = msg_send![ts, setAttributes: &*def_dict range: full_range];

            // Pre-build span resources.
            let bold_font = NSFont::monospacedSystemFontOfSize_weight(font_pt, 0.5);
            let h1_font   = NSFont::monospacedSystemFontOfSize_weight(font_pt + 2.0, 0.5);
            let gray: Retained<AnyObject> = msg_send_id![
                objc2::class!(NSColor),
                colorWithRed: 0.50f64 green: 0.50f64 blue: 0.50f64 alpha: 1.0f64
            ];
            let teal: Retained<AnyObject> = msg_send_id![
                objc2::class!(NSColor),
                colorWithRed: 0.10f64 green: 0.60f64 blue: 0.60f64 alpha: 1.0f64
            ];
            let num_strike: Retained<AnyObject> = msg_send_id![objc2::class!(NSNumber), numberWithInt: 1i32];
            let num_italic: Retained<AnyObject> = msg_send_id![objc2::class!(NSNumber), numberWithDouble: 0.20f64];

            for span in &spans {
                let lo = utf8_to_utf16(&text, span.start);
                let hi = utf8_to_utf16(&text, span.end);
                if hi <= lo { continue; }
                let r = NSRange { location: lo, length: hi - lo };
                match span.kind {
                    MdSpanKind::H1 => {
                        let _: () = msg_send![ts, addAttribute: ns_string!("NSFont") value: &*h1_font range: r];
                    }
                    MdSpanKind::Bold => {
                        let _: () = msg_send![ts, addAttribute: ns_string!("NSFont") value: &*bold_font range: r];
                    }
                    MdSpanKind::Italic => {
                        let _: () = msg_send![ts, addAttribute: ns_string!("NSObliqueness") value: &*num_italic range: r];
                    }
                    MdSpanKind::Code => {
                        let _: () = msg_send![ts, addAttribute: ns_string!("NSColor") value: &*teal range: r];
                    }
                    MdSpanKind::Strike => {
                        let _: () = msg_send![ts, addAttribute: ns_string!("NSStrikethrough") value: &*num_strike range: r];
                    }
                    MdSpanKind::Quote => {
                        let _: () = msg_send![ts, addAttribute: ns_string!("NSColor") value: &*gray range: r];
                    }
                }
            }

            let _: () = msg_send![ts, endEditing];

            if !undo.is_null() {
                let _: () = msg_send![undo, enableUndoRegistration];
            }
        }

        self.ivars().formatting.set(false);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AppDelegate
// ─────────────────────────────────────────────────────────────────────────────

struct AppCore {
    store:        NoteStore,
    settings:     Settings,
    current_note: usize,
    data_dir:     PathBuf,
}

struct AppDelegateIvars {
    core:                RefCell<AppCore>,
    status_item:         RefCell<Option<Retained<NSStatusItem>>>,
    panel:               RefCell<Option<Retained<NSPanel>>>,
    editor:              RefCell<Option<Retained<EditorView>>>,
    settings_panel:      RefCell<Option<Retained<NSPanel>>>,
    mode_label:          RefCell<Option<Retained<NSTextField>>>,
    page_dots:           RefCell<Option<Retained<PageDotsView>>>,
    nav_prev_btn:        RefCell<Option<Retained<NSButton>>>,
    nav_next_btn:        RefCell<Option<Retained<NSButton>>>,
    saved_window_origin: RefCell<Option<NSPoint>>,
    pre_pull_content:    RefCell<Option<String>>,
}

declare_class!(
    struct AppDelegate;

    unsafe impl ClassType for AppDelegate {
        type Super = NSObject;
        type Mutability = MainThreadOnly;
        const NAME: &'static str = "TodizzyAppDelegate";
    }

    impl DeclaredClass for AppDelegate {
        type Ivars = AppDelegateIvars;
    }

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[method(applicationDidFinishLaunching:)]
        fn app_did_finish_launching(&self, _notif: &NSNotification) {
            self.setup();
        }

        #[method(applicationShouldTerminateAfterLastWindowClosed:)]
        fn should_quit_on_window_close(&self, _app: &NSApplication) -> bool {
            false
        }

        #[method(applicationWillTerminate:)]
        fn app_will_terminate(&self, _notif: &NSNotification) {
            self.save_current_note();
            self.git_push();
        }
    }

    unsafe impl NSWindowDelegate for AppDelegate {
        #[method(windowDidResignKey:)]
        fn window_did_resign_key(&self, _notif: &NSNotification) {
            let close = self.ivars().core.borrow().settings.close_on_focus_loss;
            if !close {
                return;
            }
            // Don't hide the editor if our own settings window is taking focus.
            let settings_becoming_key = self
                .ivars()
                .settings_panel
                .borrow()
                .as_ref()
                .map(|p| p.isKeyWindow())
                .unwrap_or(false);
            if !settings_becoming_key {
                self.hide_window();
            }
        }
    }

    unsafe impl AppDelegate {
        /// Left-click: toggle window.  Right-click: show context menu.
        #[method(handleStatusBarClick)]
        fn handle_status_bar_click_sel(&self) {
            self.handle_status_bar_click();
        }

        /// TodizzyTextChanged notification.
        #[method(onTextChanged:)]
        fn on_text_changed_notif(&self, _notif: &NSNotification) {
            self.save_current_note();
        }

        /// TodizzySwipeLeft → advance to next note.
        #[method(onSwipeLeft:)]
        fn on_swipe_left(&self, _notif: &NSNotification) {
            self.go_next_note();
        }

        /// TodizzySwipeRight → go to previous note.
        #[method(onSwipeRight:)]
        fn on_swipe_right(&self, _notif: &NSNotification) {
            self.go_prev_note();
        }

        /// TodizzyOpenSettings notification (from right-click menu).
        #[method(onOpenSettings:)]
        fn on_open_settings_notif(&self, _notif: &NSNotification) {
            self.open_settings_window();
        }

        /// TodizzyModeChanged notification.
        #[method(onModeChanged:)]
        fn on_mode_changed_notif(&self, _notif: &NSNotification) {
            self.update_mode_label();
        }

        /// TodizzyHideWindow notification — double-Escape in Normal mode.
        #[method(onHideWindow:)]
        fn on_hide_window_notif(&self, _notif: &NSNotification) {
            self.hide_window();
        }

        /// Called on the main thread after a background git pull completes.
        #[method(reloadNotesAfterPull)]
        fn reload_notes_after_pull_sel(&self) {
            self.reload_notes_after_pull();
        }

        /// "Quit" menu item action.
        #[method(quitApp:)]
        fn quit_app(&self, _sender: &AnyObject) {
            unsafe {
                let mtm = MainThreadMarker::new_unchecked();
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        }

        /// "Done" button in settings window.
        #[method(applySettings:)]
        fn apply_settings_sel(&self, _sender: &AnyObject) {
            self.apply_settings_from_panel();
        }

        /// ‹ button — go to previous note.
        #[method(prevNote:)]
        fn prev_note_action(&self, _sender: &AnyObject) {
            self.go_prev_note();
        }

        /// › button — go to next note.
        #[method(nextNote:)]
        fn next_note_action(&self, _sender: &AnyObject) {
            self.go_next_note();
        }
    }
);

// ── Git helpers (free functions — safe to call from background threads) ────────

fn run_git_push(dir: String) {
    let _ = std::process::Command::new("git")
        .args(["-C", &dir, "add", "-A"]).output();
    // "nothing to commit" returns non-zero — that's fine, push continues.
    let _ = std::process::Command::new("git")
        .args(["-C", &dir, "commit", "-m", "todizzy: sync"]).output();
    let _ = std::process::Command::new("git")
        .args(["-C", &dir, "push"]).output();
}

fn run_git_pull(dir: String) {
    let _ = std::process::Command::new("git")
        .args(["-C", &dir, "pull"]).output();
}

// ─────────────────────────────────────────────────────────────────────────────

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker, data_dir: PathBuf) -> Retained<Self> {
        let store = NoteStore::open(data_dir.join("notes")).expect("open note store");
        let settings = Settings::load(&data_dir.join("settings.json"));

        let ivars = AppDelegateIvars {
            core: RefCell::new(AppCore {
                store,
                settings,
                current_note: 0,
                data_dir,
            }),
            status_item:         RefCell::new(None),
            panel:               RefCell::new(None),
            editor:              RefCell::new(None),
            settings_panel:      RefCell::new(None),
            mode_label:          RefCell::new(None),
            page_dots:           RefCell::new(None),
            nav_prev_btn:        RefCell::new(None),
            nav_next_btn:        RefCell::new(None),
            saved_window_origin: RefCell::new(None),
            pre_pull_content:    RefCell::new(None),
        };
        let this = mtm.alloc::<Self>().set_ivars(ivars);
        unsafe { msg_send_id![super(this), init] }
    }

    // ── Full setup ────────────────────────────────────────────────────────────

    fn setup(&self) {
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        self.setup_status_bar(mtm);
        self.setup_window(mtm);
        self.subscribe_notifications();
        self.load_current_note();
    }

    // ── Status bar ────────────────────────────────────────────────────────────

    fn setup_status_bar(&self, mtm: MainThreadMarker) {
        let bar = unsafe { NSStatusBar::systemStatusBar() };
        let item = unsafe { bar.statusItemWithLength(-1.0f64) };

        if let Some(button) = unsafe { item.button(mtm) } {
            unsafe {
                let title = NSString::from_str("🗒");
                button.setTitle(&title);

                // Fire action on both left- and right-mouse-down.
                // NSEventMaskLeftMouseDown = 1<<1 = 2
                // NSEventMaskRightMouseDown = 1<<3 = 8
                let _: () = msg_send![&*button, sendActionOn: NSEventMask(2 | 8)];

                let _: () = msg_send![&*button, setTarget: self as *const Self as *const AnyObject];
                let _: () = msg_send![&*button, setAction: Self::sel_status_click()];
            }
        }

        *self.ivars().status_item.borrow_mut() = Some(item);
    }

    fn sel_status_click() -> Sel {
        objc2::sel!(handleStatusBarClick)
    }

    fn handle_status_bar_click(&self) {
        // Detect right-click via NSApp.currentEvent
        let is_right = unsafe {
            let mtm = MainThreadMarker::new_unchecked();
            let app = NSApplication::sharedApplication(mtm);
            if let Some(ev) = app.currentEvent() {
                // NSEventType::RightMouseDown = 3
                let raw_type: usize = msg_send![&*ev, type];
                raw_type == 3
            } else {
                false
            }
        };

        if is_right {
            self.show_status_menu();
        } else if self.window_is_visible() {
            self.hide_window();
        } else {
            self.show_window();
        }
    }

    fn show_status_menu(&self) {
        let mtm = unsafe { MainThreadMarker::new_unchecked() };

        let menu = unsafe { NSMenu::initWithTitle(mtm.alloc(), ns_string!("")) };
        unsafe {
            // Settings item
            let settings_title = NSString::from_str("Settings…");
            let settings_item: Retained<NSMenuItem> = msg_send_id![
                mtm.alloc::<NSMenuItem>(),
                initWithTitle: &*settings_title
                action: objc2::sel!(onOpenSettings:)
                keyEquivalent: ns_string!("")
            ];
            let _: () = msg_send![&*settings_item, setTarget: self as *const Self as *const AnyObject];
            menu.addItem(&settings_item);

            // Separator
            let sep = NSMenuItem::separatorItem(mtm);
            menu.addItem(&sep);

            // Quit item
            let quit_title = NSString::from_str("Quit Todizzy");
            let quit_item: Retained<NSMenuItem> = msg_send_id![
                mtm.alloc::<NSMenuItem>(),
                initWithTitle: &*quit_title
                action: objc2::sel!(quitApp:)
                keyEquivalent: ns_string!("q")
            ];
            let _: () = msg_send![&*quit_item, setTarget: self as *const Self as *const AnyObject];
            menu.addItem(&quit_item);
        }

        // Pop the menu up under the status bar button
        if let Some(item) = self.ivars().status_item.borrow().as_ref() {
            unsafe {
                let _: () = msg_send![&**item, popUpStatusItemMenu: &*menu];
            }
        }
    }

    // ── Window ────────────────────────────────────────────────────────────────

    fn setup_window(&self, mtm: MainThreadMarker) {
        let (w, h, font_size, motion_mode) = {
            let core = self.ivars().core.borrow();
            (
                core.settings.window_width,
                core.settings.window_height,
                core.settings.font_size,
                core.settings.motion_mode,
            )
        };

        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;

        let panel: Retained<NSPanel> = unsafe {
            msg_send_id![
                mtm.alloc::<NSPanel>(),
                initWithContentRect: rect
                styleMask: style
                backing: NSBackingStoreType::NSBackingStoreBuffered
                defer: false
            ]
        };

        unsafe {
            panel.setReleasedWhenClosed(false);
            panel.setTitlebarAppearsTransparent(true);
            panel.setMovableByWindowBackground(true);
            // Clear the title — we want a clean, chrome-free surface.
            panel.setTitle(ns_string!(""));
            panel.setLevel(objc2_app_kit::NSFloatingWindowLevel);
            let _: () = msg_send![&*panel, setDelegate: self as *const Self as *const AnyObject];

            // ── Hide traffic-light buttons ────────────────────────────────────
            let win = &*panel as &NSWindow;
            for btn_kind in [
                NSWindowButton::NSWindowCloseButton,
                NSWindowButton::NSWindowMiniaturizeButton,
                NSWindowButton::NSWindowZoomButton,
            ] {
                if let Some(btn) = win.standardWindowButton(btn_kind) {
                    let _: () = msg_send![&*btn, setHidden: true];
                }
            }
        }

        // ── Scroll view ───────────────────────────────────────────────────────
        let content_view: &NSView = unsafe {
            let cv: *mut NSView = msg_send![&*panel, contentView];
            &*cv
        };
        let cv_frame: NSRect = unsafe { msg_send![content_view, bounds] };

        let scroll = unsafe { NSScrollView::initWithFrame(mtm.alloc(), cv_frame) };
        unsafe {
            let _: () = msg_send![&*scroll, setHasVerticalScroller: false];
            let _: () = msg_send![&*scroll, setHasHorizontalScroller: false];
            scroll.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable
                    | NSAutoresizingMaskOptions::NSViewHeightSizable,
            );
        }

        let editor_frame: NSRect = unsafe {
            let clip: *mut NSView = msg_send![&*scroll, contentView];
            msg_send![&*clip, bounds]
        };

        let editor = EditorView::new(mtm, editor_frame, motion_mode);
        editor.configure(font_size);

        unsafe {
            let _: () = msg_send![&*editor, setMinSize: NSSize::new(0.0, h)];
            let _: () = msg_send![&*editor, setMaxSize: NSSize::new(f64::MAX, f64::MAX)];
            let _: () = msg_send![&*editor, setDelegate: self as *const Self as *const AnyObject];
            let _: () = msg_send![&*scroll, setDocumentView: &*editor];
            let _: () = msg_send![content_view, addSubview: &*scroll];
        }

        *self.ivars().editor.borrow_mut() = Some(editor);

        // ── Navigation buttons (‹ ›) in the title-bar area ───────────────────
        // FullSizeContentView means the content_view covers the whole window,
        // including the (transparent) title bar at the top.  We place two small
        // buttons in that area, pinned to the top-right with autoresizing.
        let bw = 26.0f64;
        let bh = 22.0f64;
        let right_pad = 6.0f64;
        let top_pad   = 5.0f64;
        let cv_h = cv_frame.size.height;
        let cv_w = cv_frame.size.width;

        // "›" (next) — rightmost
        let next_frame = NSRect::new(
            NSPoint::new(cv_w - right_pad - bw, cv_h - top_pad - bh),
            NSSize::new(bw, bh),
        );
        // "‹" (prev) — just left of next
        let prev_frame = NSRect::new(
            NSPoint::new(cv_w - right_pad - bw * 2.0 - 4.0, cv_h - top_pad - bh),
            NSSize::new(bw, bh),
        );

        // NSViewMinXMargin (1) | NSViewMinYMargin (8) = pin to top-right corner.
        let pin_top_right = NSAutoresizingMaskOptions(1 | 8);

        // Helper: create a nav button with corners matching the window (~9 pt).
        let make_nav_btn = |title_str: &str, frame: NSRect| -> Retained<NSButton> {
            let btn: Retained<NSButton> = unsafe {
                msg_send_id![mtm.alloc::<NSButton>(), initWithFrame: frame]
            };
            unsafe {
                let title = NSString::from_str(title_str);
                let _: () = msg_send![&*btn, setTitle: &*title];
                // NSBezelStyleRegularSquare = 2 — rectangular bezel we can
                // clip to the desired corner radius via the layer.
                let _: () = msg_send![&*btn, setBezelStyle: 2usize];
                let _: () = msg_send![&*btn, setWantsLayer: true];
                let layer: *mut AnyObject = msg_send![&*btn, layer];
                if !layer.is_null() {
                    let _: () = msg_send![layer, setCornerRadius: 9.0f64];
                    let _: () = msg_send![layer, setMasksToBounds: true];
                }
                btn.setAutoresizingMask(pin_top_right);
            }
            btn
        };

        let prev_btn = make_nav_btn("‹", prev_frame);
        unsafe {
            let _: () = msg_send![&*prev_btn, setTarget: self as *const Self as *const AnyObject];
            let _: () = msg_send![&*prev_btn, setAction: objc2::sel!(prevNote:)];
            let _: () = msg_send![content_view, addSubview: &*prev_btn];
        }
        *self.ivars().nav_prev_btn.borrow_mut() = Some(prev_btn);

        let next_btn = make_nav_btn("›", next_frame);
        unsafe {
            let _: () = msg_send![&*next_btn, setTarget: self as *const Self as *const AnyObject];
            let _: () = msg_send![&*next_btn, setAction: objc2::sel!(nextNote:)];
            let _: () = msg_send![content_view, addSubview: &*next_btn];
        }
        *self.ivars().nav_next_btn.borrow_mut() = Some(next_btn);

        // ── Mode indicator (N / I / V) — top-left ────────────────────────────
        let mode_frame = NSRect::new(
            NSPoint::new(8.0, cv_h - top_pad - bh),
            NSSize::new(22.0, bh),
        );
        let mode_label: Retained<NSTextField> = unsafe {
            msg_send_id![mtm.alloc::<NSTextField>(), initWithFrame: mode_frame]
        };
        unsafe {
            let s = NSString::from_str("N");
            let _: () = msg_send![&*mode_label, setStringValue: &*s];
            let _: () = msg_send![&*mode_label, setEditable: false];
            let _: () = msg_send![&*mode_label, setBordered: false];
            let _: () = msg_send![&*mode_label, setDrawsBackground: false];
            // Bold monospaced font at 14pt
            let font = NSFont::monospacedSystemFontOfSize_weight(14.0, 0.5);
            let _: () = msg_send![&*mode_label, setFont: &*font];
            // Initial color: blue for Normal
            let nc: Retained<AnyObject> = msg_send_id![
                objc2::class!(NSColor),
                colorWithRed: 0.30f64 green: 0.55f64 blue: 0.95f64 alpha: 1.0f64
            ];
            let _: () = msg_send![&*mode_label, setTextColor: &*nc];
            // NSViewMaxXMargin (4) | NSViewMinYMargin (8) = pin to top-left
            mode_label.setAutoresizingMask(NSAutoresizingMaskOptions(4 | 8));
            let _: () = msg_send![content_view, addSubview: &*mode_label];
        }
        *self.ivars().mode_label.borrow_mut() = Some(mode_label);

        // ── Page dots — top-center, full width ───────────────────────────────
        let dots_frame = NSRect::new(
            NSPoint::new(0.0, cv_h - top_pad - bh),
            NSSize::new(cv_w, bh),
        );
        let dots = PageDotsView::new(mtm, dots_frame);
        unsafe {
            // NSViewWidthSizable (2) | NSViewMinYMargin (8) = full-width, pins to top
            dots.setAutoresizingMask(NSAutoresizingMaskOptions(2 | 8));
            let _: () = msg_send![content_view, addSubview: &*dots];
        }
        *self.ivars().page_dots.borrow_mut() = Some(dots);

        *self.ivars().panel.borrow_mut() = Some(panel);

        // Apply initial visibility from loaded settings.
        self.apply_visibility_settings();
    }

    // ── Notification subscriptions ────────────────────────────────────────────

    fn subscribe_notifications(&self) {
        let nc = unsafe { NSNotificationCenter::defaultCenter() };
        let observer = self as *const Self as *const AnyObject;
        unsafe {
            nc.addObserver_selector_name_object(
                &*observer,
                objc2::sel!(onTextChanged:),
                Some(notif_text_changed()),
                None,
            );
            nc.addObserver_selector_name_object(
                &*observer,
                objc2::sel!(onSwipeLeft:),
                Some(notif_swipe_left()),
                None,
            );
            nc.addObserver_selector_name_object(
                &*observer,
                objc2::sel!(onSwipeRight:),
                Some(notif_swipe_right()),
                None,
            );
            nc.addObserver_selector_name_object(
                &*observer,
                objc2::sel!(onOpenSettings:),
                Some(notif_open_settings()),
                None,
            );
            nc.addObserver_selector_name_object(
                &*observer,
                objc2::sel!(onModeChanged:),
                Some(notif_mode_changed()),
                None,
            );
            nc.addObserver_selector_name_object(
                &*observer,
                objc2::sel!(onHideWindow:),
                Some(notif_hide_window()),
                None,
            );
        }
    }

    // ── Window show / hide / position ─────────────────────────────────────────

    fn show_window(&self) {
        // Show immediately with current local content — no blocking.
        self.load_current_note();

        // Restore previous position if the user dragged the window; otherwise
        // default to below the status bar icon (first open).
        {
            let saved = *self.ivars().saved_window_origin.borrow();
            if let Some(origin) = saved {
                if let Some(panel) = self.ivars().panel.borrow().as_ref() {
                    unsafe { let _: () = msg_send![&**panel, setFrameOrigin: origin]; }
                }
            } else {
                self.position_window_below_status_bar();
            }
        }
        if let Some(panel) = self.ivars().panel.borrow().as_ref() {
            unsafe {
                // For a menu-bar accessory app, we must explicitly activate
                // so the panel can receive key events on every show cycle.
                let mtm = MainThreadMarker::new_unchecked();
                let app = NSApplication::sharedApplication(mtm);
                let _: () = msg_send![&*app, activateIgnoringOtherApps: true];

                let null: *const AnyObject = std::ptr::null();
                let _: () = msg_send![&**panel, makeKeyAndOrderFront: null];
            }
            if let Some(editor) = self.ivars().editor.borrow().as_ref() {
                unsafe {
                    let _: () = msg_send![&**panel, makeFirstResponder: &**editor];
                }
            }
        }

        // Background pull: capture pre-pull content, pull, then reload on main
        // thread if the user hasn't started typing.
        let git_sync = self.ivars().core.borrow().settings.git_sync;
        if git_sync {
            let pre = self.ivars().editor.borrow().as_ref()
                .map(|e| e.current_content())
                .unwrap_or_default();
            *self.ivars().pre_pull_content.borrow_mut() = Some(pre);

            let dir = self.notes_dir().to_string_lossy().into_owned();
            // Safety: AppDelegate is owned by NSApp and lives for the process lifetime.
            let self_ptr = self as *const AppDelegate as usize;
            std::thread::spawn(move || {
                run_git_pull(dir);
                unsafe {
                    let null: *const AnyObject = std::ptr::null();
                    let _: () = msg_send![
                        self_ptr as *mut AnyObject,
                        performSelectorOnMainThread: objc2::sel!(reloadNotesAfterPull)
                        withObject: null
                        waitUntilDone: false
                    ];
                }
            });
        }
    }

    fn hide_window(&self) {
        if let Some(panel) = self.ivars().panel.borrow().as_ref() {
            // Save current position so we can restore it on next show.
            let frame: NSRect = unsafe { msg_send![&**panel, frame] };
            *self.ivars().saved_window_origin.borrow_mut() = Some(frame.origin);
            unsafe {
                let null: *const AnyObject = std::ptr::null();
                let _: () = msg_send![&**panel, orderOut: null];
            }
        }
        // Save the note synchronously (fast, local disk), then push in background.
        self.save_current_note();
        let git_sync = self.ivars().core.borrow().settings.git_sync;
        if git_sync {
            let dir = self.notes_dir().to_string_lossy().into_owned();
            std::thread::spawn(move || run_git_push(dir));
        }
    }

    fn window_is_visible(&self) -> bool {
        self.ivars()
            .panel
            .borrow()
            .as_ref()
            .map(|p| p.isVisible())
            .unwrap_or(false)
    }

    fn position_window_below_status_bar(&self) {
        let origin = self.status_bar_bottom_left().unwrap_or_else(|| {
            NSPoint::new(100.0, 800.0)
        });

        if let Some(panel) = self.ivars().panel.borrow().as_ref() {
            let panel_height: f64 = {
                let f: NSRect = unsafe { msg_send![&**panel, frame] };
                f.size.height
            };
            let window_origin = NSPoint::new(origin.x, origin.y - panel_height);
            unsafe {
                let _: () = msg_send![&**panel, setFrameOrigin: window_origin];
            }
        }
    }

    fn status_bar_bottom_left(&self) -> Option<NSPoint> {
        let item_ref = self.ivars().status_item.borrow();
        let item = item_ref.as_ref()?;
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let button = unsafe { item.button(mtm) }?;
        unsafe {
            let win: *mut NSWindow = msg_send![&*button, window];
            if win.is_null() {
                return None;
            }
            let btn_frame: NSRect = msg_send![&*button, frame];
            let screen_frame: NSRect = msg_send!(&*win, convertRectToScreen: btn_frame);
            Some(NSPoint::new(screen_frame.origin.x, screen_frame.origin.y))
        }
    }

    // ── Git sync ──────────────────────────────────────────────────────────────

    fn notes_dir(&self) -> std::path::PathBuf {
        self.ivars().core.borrow().data_dir.join("notes")
    }

    /// Synchronous push — used only at app quit where we must finish before exit.
    fn git_push(&self) {
        let git_sync = self.ivars().core.borrow().settings.git_sync;
        if !git_sync { return; }
        run_git_push(self.notes_dir().to_string_lossy().into_owned());
    }

    /// Called on the main thread after the background pull thread finishes.
    /// Re-opens the NoteStore and refreshes the editor — but only if the user
    /// hasn't started typing since the window opened (to avoid clobbering edits).
    fn reload_notes_after_pull(&self) {
        // Re-open NoteStore to pick up any newly-pulled note files.
        if let Ok(store) = crate::storage::NoteStore::open(self.notes_dir()) {
            self.ivars().core.borrow_mut().store = store;
        }

        // Reload the editor only when the user hasn't modified the note yet.
        let pre = self.ivars().pre_pull_content.borrow_mut().take();
        if let Some(pre_content) = pre {
            let unchanged = self.ivars().editor.borrow().as_ref()
                .map(|e| e.current_content() == pre_content)
                .unwrap_or(false);
            if unchanged {
                self.load_current_note();
            }
        }
    }

    // ── Settings window ───────────────────────────────────────────────────────

    fn open_settings_window(&self) {
        let mtm = unsafe { MainThreadMarker::new_unchecked() };

        // Read current settings before building the panel
        let settings = {
            let core = self.ivars().core.borrow();
            core.settings.clone()
        };

        let panel = self.build_settings_panel(mtm, &settings);

        unsafe {
            let _: () = msg_send![&*panel, center];
            let null: *const AnyObject = std::ptr::null();
            let _: () = msg_send![&*panel, makeKeyAndOrderFront: null];
        }

        *self.ivars().settings_panel.borrow_mut() = Some(panel);
    }

    fn build_settings_panel(&self, mtm: MainThreadMarker, s: &Settings) -> Retained<NSPanel> {
        let w = 320.0f64;
        let h = 330.0f64;
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;

        let panel: Retained<NSPanel> = unsafe {
            msg_send_id![
                mtm.alloc::<NSPanel>(),
                initWithContentRect: rect
                styleMask: style
                backing: NSBackingStoreType::NSBackingStoreBuffered
                defer: false
            ]
        };
        unsafe {
            panel.setReleasedWhenClosed(false);
            panel.setTitle(ns_string!("Todizzy Settings"));
        }

        let cv: *mut NSView = unsafe { msg_send![&*panel, contentView] };

        // ── Mode label ────────────────────────────────────────────────────────
        let mode_lbl = make_label(
            mtm,
            "Editor Mode:",
            NSRect::new(NSPoint::new(20.0, 276.0), NSSize::new(100.0, 22.0)),
        );
        unsafe { let _: () = msg_send![cv, addSubview: &*mode_lbl]; }

        // ── Segmented control ─────────────────────────────────────────────────
        let seg_rect = NSRect::new(NSPoint::new(128.0, 272.0), NSSize::new(172.0, 28.0));
        let seg: Retained<NSSegmentedControl> = unsafe {
            msg_send_id![mtm.alloc::<NSSegmentedControl>(), initWithFrame: seg_rect]
        };
        let sel_idx: usize = match s.motion_mode {
            MotionMode::Vim   => 0,
            MotionMode::Helix => 1,
            MotionMode::None  => 2,
        };
        unsafe {
            let _: () = msg_send![&*seg, setSegmentCount: 3usize];
            for (i, lbl) in ["Vim", "Helix", "None"].iter().enumerate() {
                let ns = NSString::from_str(lbl);
                let _: () = msg_send![&*seg, setLabel: &*ns forSegment: i];
            }
            let _: () = msg_send![&*seg, setSelectedSegment: sel_idx];
            let _: () = msg_send![&*seg, setTag: 100isize];
            let _: () = msg_send![cv, addSubview: &*seg];
        }

        // ── Close-on-focus-loss checkbox ──────────────────────────────────────
        let chk_rect = NSRect::new(NSPoint::new(20.0, 232.0), NSSize::new(270.0, 22.0));
        let chk: Retained<NSButton> = unsafe {
            msg_send_id![mtm.alloc::<NSButton>(), initWithFrame: chk_rect]
        };
        unsafe {
            let _: () = msg_send![&*chk, setButtonType: 3usize]; // NSButtonTypeSwitch
            let title = NSString::from_str("Close on focus loss");
            let _: () = msg_send![&*chk, setTitle: &*title];
            let state: isize = if s.close_on_focus_loss { 1 } else { 0 };
            let _: () = msg_send![&*chk, setState: state];
            let _: () = msg_send![&*chk, setTag: 101isize];
            let _: () = msg_send![cv, addSubview: &*chk];
        }

        // ── Font size label + field ───────────────────────────────────────────
        let fs_lbl = make_label(
            mtm,
            "Font Size:",
            NSRect::new(NSPoint::new(20.0, 194.0), NSSize::new(80.0, 22.0)),
        );
        unsafe { let _: () = msg_send![cv, addSubview: &*fs_lbl]; }

        let fs_rect = NSRect::new(NSPoint::new(108.0, 194.0), NSSize::new(60.0, 22.0));
        let fs_field: Retained<NSTextField> = unsafe {
            msg_send_id![mtm.alloc::<NSTextField>(), initWithFrame: fs_rect]
        };
        unsafe {
            let val = NSString::from_str(&format!("{}", s.font_size as u32));
            let _: () = msg_send![&*fs_field, setStringValue: &*val];
            let _: () = msg_send![&*fs_field, setTag: 102isize];
            let _: () = msg_send![cv, addSubview: &*fs_field];
        }

        // ── Visibility checkboxes ─────────────────────────────────────────────
        let make_vis_chk = |title_str: &str, y: f64, tag: isize, on: bool| {
            let r = NSRect::new(NSPoint::new(20.0, y), NSSize::new(270.0, 22.0));
            let b: Retained<NSButton> = unsafe {
                msg_send_id![mtm.alloc::<NSButton>(), initWithFrame: r]
            };
            unsafe {
                let _: () = msg_send![&*b, setButtonType: 3usize];
                let t = NSString::from_str(title_str);
                let _: () = msg_send![&*b, setTitle: &*t];
                let st: isize = if on { 1 } else { 0 };
                let _: () = msg_send![&*b, setState: st];
                let _: () = msg_send![&*b, setTag: tag];
                let _: () = msg_send![cv, addSubview: &*b];
            }
        };

        make_vis_chk("Show nav arrows",        154.0, 103, s.show_nav_arrows);
        make_vis_chk("Show page dots",         124.0, 104, s.show_page_dots);
        make_vis_chk("Show mode indicator",     94.0, 105, s.show_mode_indicator);
        make_vis_chk("Git sync (pull/push)",    64.0, 106, s.git_sync);

        // ── Done button ───────────────────────────────────────────────────────
        let done_rect = NSRect::new(NSPoint::new(w - 100.0, 20.0), NSSize::new(80.0, 32.0));
        let done_btn: Retained<NSButton> = unsafe {
            msg_send_id![mtm.alloc::<NSButton>(), initWithFrame: done_rect]
        };
        unsafe {
            let title = NSString::from_str("Done");
            let _: () = msg_send![&*done_btn, setTitle: &*title];
            // NSBezelStyleRounded = 1
            let _: () = msg_send![&*done_btn, setBezelStyle: 1usize];
            let _: () = msg_send![&*done_btn, setTarget: self as *const Self as *const AnyObject];
            let _: () = msg_send![&*done_btn, setAction: objc2::sel!(applySettings:)];
            let _: () = msg_send![cv, addSubview: &*done_btn];
        }

        panel
    }

    fn apply_settings_from_panel(&self) {
        // Read values first, releasing the panel borrow before writing settings
        let (motion_mode, close_on_focus, font_size, show_arrows, show_dots, show_mode, git_sync) = {
            let panel_ref = self.ivars().settings_panel.borrow();
            let panel = match panel_ref.as_ref() {
                Some(p) => p,
                None => return,
            };
            let cv: *mut AnyObject = unsafe { msg_send![&**panel, contentView] };

            let seg: *mut AnyObject = unsafe { msg_send![cv, viewWithTag: 100isize] };
            let mode_idx: usize = if !seg.is_null() {
                unsafe { msg_send![seg, selectedSegment] }
            } else {
                0
            };
            let motion = match mode_idx {
                0 => MotionMode::Vim,
                1 => MotionMode::Helix,
                _ => MotionMode::None,
            };

            let chk: *mut AnyObject = unsafe { msg_send![cv, viewWithTag: 101isize] };
            let close: bool = if !chk.is_null() {
                let st: isize = unsafe { msg_send![chk, state] };
                st == 1
            } else {
                true
            };

            let fld: *mut AnyObject = unsafe { msg_send![cv, viewWithTag: 102isize] };
            let fsize: f64 = if !fld.is_null() {
                let s: Retained<NSString> = unsafe { msg_send_id![fld, stringValue] };
                s.to_string().parse::<f64>().unwrap_or(14.0).clamp(8.0, 72.0)
            } else {
                14.0
            };

            let read_chk = |tag: isize| -> bool {
                let v: *mut AnyObject = unsafe { msg_send![cv, viewWithTag: tag] };
                if !v.is_null() { let st: isize = unsafe { msg_send![v, state] }; st == 1 } else { true }
            };
            let arrows   = read_chk(103);
            let dots     = read_chk(104);
            let mode     = read_chk(105);
            let git_sync = read_chk(106);

            (motion, close, fsize, arrows, dots, mode, git_sync)
        }; // panel_ref borrow released here

        // Update and persist settings
        let settings_path = {
            let mut core = self.ivars().core.borrow_mut();
            core.settings.motion_mode         = motion_mode;
            core.settings.close_on_focus_loss  = close_on_focus;
            core.settings.font_size           = font_size;
            core.settings.show_nav_arrows     = show_arrows;
            core.settings.show_page_dots      = show_dots;
            core.settings.show_mode_indicator  = show_mode;
            core.settings.git_sync            = git_sync;
            core.data_dir.join("settings.json")
        };
        {
            let core = self.ivars().core.borrow();
            let _ = core.settings.save(&settings_path);
        }

        // Apply live to editor
        if let Some(editor) = self.ivars().editor.borrow().as_ref() {
            editor.set_motion_mode(motion_mode);
            editor.configure(font_size);
        }

        // Apply visibility live
        self.apply_visibility_settings();

        // Close the settings panel
        if let Some(p) = self.ivars().settings_panel.borrow().as_ref() {
            unsafe {
                let null: *const AnyObject = std::ptr::null();
                let _: () = msg_send![&**p, orderOut: null];
            }
        }
    }

    fn apply_visibility_settings(&self) {
        let (show_arrows, show_dots, show_mode) = {
            let core = self.ivars().core.borrow();
            (core.settings.show_nav_arrows, core.settings.show_page_dots, core.settings.show_mode_indicator)
        };
        if let Some(btn) = self.ivars().nav_prev_btn.borrow().as_ref() {
            unsafe { let _: () = msg_send![&**btn, setHidden: !show_arrows]; }
        }
        if let Some(btn) = self.ivars().nav_next_btn.borrow().as_ref() {
            unsafe { let _: () = msg_send![&**btn, setHidden: !show_arrows]; }
        }
        if let Some(dots) = self.ivars().page_dots.borrow().as_ref() {
            unsafe { let _: () = msg_send![&**dots, setHidden: !show_dots]; }
        }
        if let Some(label) = self.ivars().mode_label.borrow().as_ref() {
            unsafe { let _: () = msg_send![&**label, setHidden: !show_mode]; }
        }
    }

    // ── Note management ───────────────────────────────────────────────────────

    fn load_current_note(&self) {
        let content = {
            let core = self.ivars().core.borrow();
            let id   = core.store.id_at(core.current_note);
            core.store.load_note(id)
        };
        if let Some(editor) = self.ivars().editor.borrow().as_ref() {
            editor.load_content(&content);
        }
        self.update_page_dots();
        self.update_mode_label();
    }

    fn update_page_dots(&self) {
        let (count, current) = {
            let core = self.ivars().core.borrow();
            (core.store.len(), core.current_note)
        };
        if let Some(dots) = self.ivars().page_dots.borrow().as_ref() {
            dots.set_state(count, current);
        }
    }

    fn update_mode_label(&self) {
        let (text, r, g, b): (&str, f64, f64, f64) = {
            if let Some(editor) = self.ivars().editor.borrow().as_ref() {
                match editor.current_mode() {
                    Mode::Normal           => ("N", 0.30, 0.55, 0.95), // blue
                    Mode::Insert           => ("I", 0.90, 0.55, 0.20), // orange
                    Mode::Visual { .. }    => ("V", 0.65, 0.35, 0.90), // purple
                }
            } else {
                ("N", 0.30, 0.55, 0.95)
            }
        };
        if let Some(label) = self.ivars().mode_label.borrow().as_ref() {
            unsafe {
                let s = NSString::from_str(text);
                let _: () = msg_send![&**label, setStringValue: &*s];
                let color: Retained<AnyObject> = msg_send_id![
                    objc2::class!(NSColor),
                    colorWithRed: r green: g blue: b alpha: 1.0f64
                ];
                let _: () = msg_send![&**label, setTextColor: &*color];
            }
        }
    }

    fn save_current_note(&self) {
        let content = self
            .ivars()
            .editor
            .borrow()
            .as_ref()
            .map(|e| e.current_content())
            .unwrap_or_default();

        let core = self.ivars().core.borrow();
        let id = core.store.id_at(core.current_note);
        let _ = core.store.save_note(id, &content);
    }

    fn go_next_note(&self) {
        self.save_current_note();
        {
            let mut core = self.ivars().core.borrow_mut();
            let last = core.store.len().saturating_sub(1);
            if core.current_note >= last {
                // At the last note — create a fresh blank page.
                core.store.create_note();
                core.current_note = core.store.len() - 1;
            } else {
                core.current_note += 1;
            }
        }
        self.load_current_note();
    }

    fn go_prev_note(&self) {
        self.save_current_note();
        {
            let mut core = self.ivars().core.borrow_mut();
            if core.current_note > 0 {
                core.current_note -= 1;
            }
        }
        self.load_current_note();
    }

}

// ── Settings-window helper ────────────────────────────────────────────────────

fn make_label(mtm: MainThreadMarker, text: &str, rect: NSRect) -> Retained<NSTextField> {
    let field: Retained<NSTextField> = unsafe {
        msg_send_id![mtm.alloc::<NSTextField>(), initWithFrame: rect]
    };
    unsafe {
        let s = NSString::from_str(text);
        let _: () = msg_send![&*field, setStringValue: &*s];
        let _: () = msg_send![&*field, setEditable: false];
        let _: () = msg_send![&*field, setBordered: false];
        let _: () = msg_send![&*field, setDrawsBackground: false];
    }
    field
}

// ── Markdown span scanner ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum MdSpanKind {
    H1,     // `# heading` — bold + larger font
    Bold,   // `= line` or `**text**`
    Italic, // `*text*`
    Code,   // `` `text` ``
    Strike, // `~~text~~`
    Quote,  // `> text` — gray
}

struct MdSpan {
    start: usize,
    end:   usize,
    kind:  MdSpanKind,
}

fn markdown_spans(text: &str) -> Vec<MdSpan> {
    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let len   = bytes.len();

    // ── Line-level patterns ───────────────────────────────────────────────────
    let mut pos = 0usize;
    while pos < len {
        let line_end = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .unwrap_or(len);

        if line_end > pos {
            let ls = pos;
            let le = line_end;
            if bytes[ls] == b'#' {
                if ls + 1 < len && bytes[ls + 1] == b' ' {
                    spans.push(MdSpan { start: ls, end: le, kind: MdSpanKind::H1 });
                } else {
                    spans.push(MdSpan { start: ls, end: le, kind: MdSpanKind::Bold });
                }
            } else if bytes[ls] == b'=' {
                spans.push(MdSpan { start: ls, end: le, kind: MdSpanKind::Bold });
            } else if bytes[ls] == b'>' {
                spans.push(MdSpan { start: ls, end: le, kind: MdSpanKind::Quote });
            }
        }
        pos = line_end + 1;
    }

    // ── Inline patterns ───────────────────────────────────────────────────────
    let mut i = 0usize;
    while i < len {
        // **bold**  (check before single *)
        if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            if let Some(close) = find_marker(bytes, i + 2, b"**") {
                spans.push(MdSpan { start: i, end: close + 2, kind: MdSpanKind::Bold });
                i = close + 2;
                continue;
            }
        }
        // *italic*  (not **)
        if bytes[i] == b'*' && (i + 1 >= len || bytes[i + 1] != b'*') {
            if let Some(close) = find_same_line(bytes, i + 1, b'*') {
                spans.push(MdSpan { start: i, end: close + 1, kind: MdSpanKind::Italic });
                i = close + 1;
                continue;
            }
        }
        // `code`
        if bytes[i] == b'`' {
            if let Some(close) = find_same_line(bytes, i + 1, b'`') {
                spans.push(MdSpan { start: i, end: close + 1, kind: MdSpanKind::Code });
                i = close + 1;
                continue;
            }
        }
        // ~~strike~~
        if i + 1 < len && bytes[i] == b'~' && bytes[i + 1] == b'~' {
            if let Some(close) = find_marker(bytes, i + 2, b"~~") {
                spans.push(MdSpan { start: i, end: close + 2, kind: MdSpanKind::Strike });
                i = close + 2;
                continue;
            }
        }
        i += 1;
    }

    spans
}

fn find_marker(bytes: &[u8], from: usize, marker: &[u8]) -> Option<usize> {
    let mlen = marker.len();
    let mut i = from;
    while i + mlen <= bytes.len() {
        if &bytes[i..i + mlen] == marker { return Some(i); }
        i += 1;
    }
    None
}

fn find_same_line(bytes: &[u8], from: usize, ch: u8) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'\n' { return None; }
        if bytes[i] == ch   { return Some(i); }
        i += 1;
    }
    None
}

// ── Key-event translation ─────────────────────────────────────────────────────

fn nsevent_to_key(event: &NSEvent) -> Option<Key> {
    let kc: u16 = unsafe { event.keyCode() };
    match kc {
        36  => return Some(Key::Enter),
        48  => return Some(Key::Tab),
        51  => return Some(Key::Backspace),
        53  => return Some(Key::Escape),
        115 => return Some(Key::Home),
        116 => return Some(Key::PageUp),
        119 => return Some(Key::End),
        121 => return Some(Key::PageDown),
        123 => return Some(Key::Left),
        124 => return Some(Key::Right),
        125 => return Some(Key::Down),
        126 => return Some(Key::Up),
        _   => {}
    }

    let chars_ns: Option<Retained<NSString>> =
        unsafe { msg_send_id![event, characters] };
    let s = chars_ns?.to_string();
    let c = s.chars().next()?;

    if c.is_control() {
        return None;
    }
    Some(Key::Char(c))
}

// ── UTF-8 ↔ UTF-16 helpers ────────────────────────────────────────────────────

fn utf8_to_utf16(s: &str, byte: usize) -> usize {
    s[..byte.min(s.len())].chars().map(|c| c.len_utf16()).sum()
}

fn utf16_to_utf8(s: &str, utf16: usize) -> usize {
    let mut u16_pos = 0usize;
    let mut byte_pos = 0usize;
    for c in s.chars() {
        if u16_pos >= utf16 { break; }
        u16_pos += c.len_utf16();
        byte_pos += c.len_utf8();
    }
    byte_pos
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn run_app(data_dir: PathBuf) {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let delegate = AppDelegate::new(mtm, data_dir);
    app.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*delegate)));

    std::mem::forget(delegate);

    unsafe { app.run() };
}
