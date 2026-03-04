/// Swipe-gesture detector.
///
/// Tracks horizontal scroll-wheel / trackpad events and fires a `SwipeDir`
/// when the accumulated delta crosses the threshold.
///
/// This is pure Rust state; the ObjC layer feeds it `NSEvent` delta values.
/// Using scroll-wheel events rather than `NSPanGestureRecognizer` gives us
/// access to the precise per-phase delta on any trackpad.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeDir {
    Left,
    Right,
}

/// Minimum accumulated horizontal scroll (in AppKit points) to trigger a
/// page switch.  Intentionally generous so normal vertical scrolling never
/// trips the page-switch by accident.
const THRESHOLD: f64 = 120.0;

#[derive(Debug, Default)]
pub struct SwipeDetector {
    accumulated: f64,
    active: bool,
}

impl SwipeDetector {
    /// Call when `NSEventPhase::Began` is received.
    pub fn began(&mut self) {
        self.accumulated = 0.0;
        self.active = true;
    }

    /// Call with `NSEvent::scrollingDeltaX()` for each `Changed` event.
    /// Returns `Some(dir)` when the threshold is exceeded — caller should
    /// then switch note pages and call `reset()`.
    pub fn changed(&mut self, delta_x: f64) -> Option<SwipeDir> {
        if !self.active {
            return None;
        }
        self.accumulated += delta_x;
        if self.accumulated.abs() >= THRESHOLD {
            let dir = if self.accumulated < 0.0 {
                SwipeDir::Left
            } else {
                SwipeDir::Right
            };
            self.reset();
            return Some(dir);
        }
        None
    }

    /// Call when `NSEventPhase::Ended` or `Cancelled`.
    pub fn ended(&mut self) {
        self.reset();
    }

    pub fn reset(&mut self) {
        self.accumulated = 0.0;
        self.active = false;
    }
}
