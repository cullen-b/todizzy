/// Swipe-gesture detector.
///
/// Tracks horizontal scroll-wheel / trackpad events and fires a `SwipeDir`
/// when the accumulated delta crosses the threshold.
///
/// Direction is committed after the first 6 pts of movement: if the gesture
/// is primarily horizontal it becomes a page-swipe; if primarily vertical it
/// passes through to normal scrolling.  This prevents accidental page-switches
/// during vertical scrolling and prevents the gesture from flickering between
/// the two interpretations mid-gesture.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeDir {
    Left,
    Right,
}

/// What the caller should do after calling [`SwipeDetector::changed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeOutcome {
    /// A swipe was detected — switch pages.
    Triggered(SwipeDir),
    /// We are tracking a horizontal swipe; caller should NOT pass to super.
    Consumed,
    /// Vertical gesture or detector idle; caller should pass event to super.
    PassThrough,
}

/// Accumulated distance before we lock in a gesture direction.
const LOCK_DIST: f64 = 6.0;
/// Horizontal distance (after direction lock) required to fire a swipe.
const THRESHOLD: f64 = 60.0;

#[derive(Debug, Default, PartialEq, Eq)]
enum GestureDir {
    #[default]
    Unknown,
    Horizontal,
    Vertical,
}

#[derive(Debug, Default)]
pub struct SwipeDetector {
    accum_x: f64,
    accum_y: f64,
    direction: GestureDir,
    active: bool,
    fired: bool,
}

impl SwipeDetector {
    /// Call when `NSEventPhase::Began` is received.
    pub fn began(&mut self) {
        *self = Self::default();
        self.active = true;
    }

    /// Call with both delta axes for each `Changed` event.
    /// Returns a [`SwipeOutcome`] telling the caller how to handle the event.
    pub fn changed(&mut self, dx: f64, dy: f64) -> SwipeOutcome {
        // Auto-begin if we somehow missed the Began phase.
        if !self.active {
            self.began();
        }

        // After we have already fired a swipe this gesture, eat remaining events.
        if self.fired {
            return SwipeOutcome::Consumed;
        }

        self.accum_x += dx;
        self.accum_y += dy;

        // Commit to a direction once enough movement has accumulated.
        if self.direction == GestureDir::Unknown {
            let dist = (self.accum_x * self.accum_x + self.accum_y * self.accum_y).sqrt();
            if dist < LOCK_DIST {
                // Not enough data yet — hold on but don't pass through (prevents
                // the scroll view from consuming the first few events of a swipe).
                return SwipeOutcome::Consumed;
            }
            self.direction = if self.accum_x.abs() >= self.accum_y.abs() {
                GestureDir::Horizontal
            } else {
                GestureDir::Vertical
            };
        }

        match self.direction {
            GestureDir::Horizontal => {
                if self.accum_x.abs() >= THRESHOLD {
                    let dir = if self.accum_x < 0.0 {
                        SwipeDir::Left
                    } else {
                        SwipeDir::Right
                    };
                    self.fired = true;
                    SwipeOutcome::Triggered(dir)
                } else {
                    SwipeOutcome::Consumed
                }
            }
            // Vertical gesture: pass all events to the scroll view.
            GestureDir::Vertical => SwipeOutcome::PassThrough,
            GestureDir::Unknown => unreachable!(),
        }
    }

    /// Call when `NSEventPhase::Ended` or `Cancelled`.
    pub fn ended(&mut self) {
        *self = Self::default();
    }
}
