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

use std::{cell::RefCell, path::PathBuf};

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
    gestures::{SwipeDetector, SwipeDir},
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

// ─────────────────────────────────────────────────────────────────────────────
// EditorView  (NSTextView subclass)
// ─────────────────────────────────────────────────────────────────────────────

struct EditorViewIvars {
    engine: RefCell<EditorEngine>,
    swipe:  RefCell<SwipeDetector>,
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

    }
);

impl EditorView {
    fn new(mtm: MainThreadMarker, frame: NSRect, motion_mode: MotionMode) -> Retained<Self> {
        let ivars = EditorViewIvars {
            engine: RefCell::new(EditorEngine::new(String::new(), motion_mode)),
            swipe:  RefCell::new(SwipeDetector::default()),
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
            }
            self.apply_nsview_cursor(cursor_utf16);
        }
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

        // Only treat as a swipe when horizontal motion clearly dominates (3:1).
        // This prevents accidental page-switches during normal vertical scrolling.
        if dx.abs() < dy.abs() * 3.0 {
            let _: () = unsafe { msg_send![super(self), scrollWheel: event] };
            return;
        }

        let phase = unsafe { event.phase() };

        if phase.contains(NSEventPhase::Began) {
            self.ivars().swipe.borrow_mut().began();
        } else if phase.contains(NSEventPhase::Changed) {
            let maybe = self.ivars().swipe.borrow_mut().changed(dx);
            if let Some(dir) = maybe {
                self.post_swipe_notification(dir);
            }
        } else if phase.contains(NSEventPhase::Ended)
            || phase.contains(NSEventPhase::Cancelled)
        {
            self.ivars().swipe.borrow_mut().ended();
        }
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
        // In Normal mode show a block cursor by "selecting" the character under
        // the cursor (length 1).  In Insert / Visual mode use the standard
        // zero-width insertion point.
        let sel_len: usize = {
            let eng = self.ivars().engine.borrow();
            match eng.mode {
                Mode::Normal => {
                    let byte_pos = utf16_to_utf8(eng.buf.as_str(), utf16_pos);
                    match eng.buf.as_str()[byte_pos..].chars().next() {
                        Some(c) if c != '\n' => 1,
                        _ => 0,
                    }
                }
                _ => 0,
            }
        };
        let range = NSRange { location: utf16_pos, length: sel_len };
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
    }

    pub fn current_content(&self) -> String {
        self.ivars().engine.borrow().buf.as_str().to_owned()
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
    core:           RefCell<AppCore>,
    status_item:    RefCell<Option<Retained<NSStatusItem>>>,
    panel:          RefCell<Option<Retained<NSPanel>>>,
    editor:         RefCell<Option<Retained<EditorView>>>,
    settings_panel: RefCell<Option<Retained<NSPanel>>>,
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
    }
);

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
            status_item:    RefCell::new(None),
            panel:          RefCell::new(None),
            editor:         RefCell::new(None),
            settings_panel: RefCell::new(None),
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
            let _: () = msg_send![&*scroll, setHasVerticalScroller: true];
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
        *self.ivars().panel.borrow_mut()  = Some(panel);
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
        }
    }

    // ── Window show / hide / position ─────────────────────────────────────────

    fn show_window(&self) {
        self.position_window_below_status_bar();
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
    }

    fn hide_window(&self) {
        if let Some(panel) = self.ivars().panel.borrow().as_ref() {
            unsafe {
                let null: *const AnyObject = std::ptr::null();
                let _: () = msg_send![&**panel, orderOut: null];
            }
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
        let h = 210.0f64;
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
            NSRect::new(NSPoint::new(20.0, 162.0), NSSize::new(100.0, 22.0)),
        );
        unsafe { let _: () = msg_send![cv, addSubview: &*mode_lbl]; }

        // ── Segmented control ─────────────────────────────────────────────────
        let seg_rect = NSRect::new(NSPoint::new(128.0, 158.0), NSSize::new(172.0, 28.0));
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
        let chk_rect = NSRect::new(NSPoint::new(20.0, 118.0), NSSize::new(270.0, 22.0));
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
            NSRect::new(NSPoint::new(20.0, 80.0), NSSize::new(80.0, 22.0)),
        );
        unsafe { let _: () = msg_send![cv, addSubview: &*fs_lbl]; }

        let fs_rect = NSRect::new(NSPoint::new(108.0, 80.0), NSSize::new(60.0, 22.0));
        let fs_field: Retained<NSTextField> = unsafe {
            msg_send_id![mtm.alloc::<NSTextField>(), initWithFrame: fs_rect]
        };
        unsafe {
            let val = NSString::from_str(&format!("{}", s.font_size as u32));
            let _: () = msg_send![&*fs_field, setStringValue: &*val];
            let _: () = msg_send![&*fs_field, setTag: 102isize];
            let _: () = msg_send![cv, addSubview: &*fs_field];
        }

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
        let (motion_mode, close_on_focus, font_size) = {
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

            (motion, close, fsize)
        }; // panel_ref borrow released here

        // Update and persist settings
        let settings_path = {
            let mut core = self.ivars().core.borrow_mut();
            core.settings.motion_mode = motion_mode;
            core.settings.close_on_focus_loss = close_on_focus;
            core.settings.font_size = font_size;
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

        // Close the settings panel
        if let Some(p) = self.ivars().settings_panel.borrow().as_ref() {
            unsafe {
                let null: *const AnyObject = std::ptr::null();
                let _: () = msg_send![&**p, orderOut: null];
            }
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
            let max = core.store.len().saturating_sub(1);
            core.current_note = (core.current_note + 1).min(max);
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
