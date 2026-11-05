//! When an output draws. Frames follow the vblank: one in flight at a time, the next one right
//! after it or, if nothing has changed, an estimated vblank later so that frame callbacks keep
//! their rhythm. A frame also knows whether it was drawn under the session lock, because `locked`
//! must not go out for a frame queued before the lock began.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    /// Nothing is going on.
    #[default]
    Idle,
    /// A render is on its way through the event loop.
    Scheduled,
    /// A frame waits for its vblank.
    Queued { locked: bool },
    /// The last render changed nothing, a timer stands in for the vblank.
    Estimating,
}

#[derive(Debug, Default)]
pub struct Pacing {
    phase: Phase,
    redraw: bool,
    /// What is on the screen was drawn while the session was locked.
    shows_locked: bool,
}

/// What a vblank, real or estimated, means for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presented {
    /// The screen now shows a frame that may confirm the lock.
    pub locked: bool,
    /// Another render is wanted, schedule it.
    pub render: bool,
}

impl Pacing {
    /// Something changed. Returns whether the caller has to schedule a render now; otherwise the
    /// next vblank takes care of it.
    pub fn request(&mut self) -> bool {
        self.redraw = true;
        if self.phase == Phase::Idle {
            self.phase = Phase::Scheduled;
            return true;
        }
        false
    }

    /// A scheduled render came up. Returns whether it is still wanted.
    pub fn start(&mut self) -> bool {
        let wanted = self.phase == Phase::Scheduled;
        if wanted {
            self.redraw = false;
        }
        wanted
    }

    /// A frame was queued for the next vblank.
    pub fn queued(&mut self, locked: bool) {
        self.phase = Phase::Queued { locked };
    }

    /// The render found nothing to do, so the screen shows what the scene looks like right now.
    pub fn unchanged(&mut self, locked: bool) {
        self.shows_locked = locked;
        self.phase = Phase::Estimating;
    }

    /// The real vblank arrived.
    pub fn vblank(&mut self) -> Presented {
        if let Phase::Queued { locked } = self.phase {
            self.shows_locked = locked;
        }
        self.tick()
    }

    /// The timer of `Estimating` ran out.
    pub fn estimated(&mut self) -> Option<Presented> {
        (self.phase == Phase::Estimating).then(|| self.tick())
    }

    fn tick(&mut self) -> Presented {
        self.phase = if self.redraw {
            Phase::Scheduled
        } else {
            Phase::Idle
        };
        Presented {
            locked: self.shows_locked,
            render: self.redraw,
        }
    }

    /// The device went away and came back: forget what was in flight.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_request_schedules_later_ones_wait_for_the_vblank() {
        let mut pacing = Pacing::default();
        assert!(pacing.request());
        assert!(!pacing.request());
        assert!(pacing.start());
        pacing.queued(false);
        assert!(!pacing.request());
        let presented = pacing.vblank();
        assert!(presented.render);
        assert!(pacing.start());
    }

    #[test]
    fn nothing_requested_goes_idle_after_the_vblank() {
        let mut pacing = Pacing::default();
        pacing.request();
        pacing.start();
        pacing.queued(false);
        assert!(!pacing.vblank().render);
        assert!(pacing.request());
    }

    #[test]
    fn a_stale_render_is_dropped() {
        let mut pacing = Pacing::default();
        pacing.request();
        pacing.reset();
        assert!(!pacing.start());
    }

    /// The hint of session 04: a frame queued before the lock must not confirm it.
    #[test]
    fn a_frame_queued_before_the_lock_does_not_confirm_it() {
        let mut pacing = Pacing::default();
        pacing.request();
        pacing.start();
        pacing.queued(false);
        // The lock begins here, and asks for a redraw.
        pacing.request();
        let presented = pacing.vblank();
        assert!(!presented.locked);
        assert!(presented.render);
        assert!(pacing.start());
        pacing.queued(true);
        assert!(pacing.vblank().locked);
    }

    #[test]
    fn a_locked_scene_equal_to_the_screen_confirms_after_the_estimate() {
        let mut pacing = Pacing::default();
        pacing.request();
        pacing.start();
        pacing.unchanged(true);
        assert!(pacing.estimated().unwrap().locked);
    }

    #[test]
    fn an_old_timer_does_nothing() {
        let mut pacing = Pacing::default();
        pacing.request();
        pacing.start();
        pacing.queued(false);
        assert_eq!(pacing.estimated(), None);
    }

    #[test]
    fn unlocking_stops_counting() {
        let mut pacing = Pacing::default();
        pacing.request();
        pacing.start();
        pacing.queued(true);
        assert!(pacing.vblank().locked);
        pacing.request();
        pacing.start();
        pacing.queued(false);
        assert!(!pacing.vblank().locked);
    }
}
