//! Waiting for changes to settle before acting: the edit is saved half a
//! second after the last change, not on every frame of a drag.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Debounce {
    delay: Duration,
    deadline: Option<Instant>,
}

impl Debounce {
    pub fn new(delay: Duration) -> Debounce {
        Debounce {
            delay,
            deadline: None,
        }
    }

    /// Something changed at `now`: wait `delay` from here.
    pub fn poke(&mut self, now: Instant) {
        self.deadline = Some(now + self.delay);
    }

    /// True once, when `delay` has passed since the last change.
    pub fn fire(&mut self, now: Instant) -> bool {
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.deadline = None;
                true
            }
            _ => false,
        }
    }

    /// Time left before it fires, if a change is waiting.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline.map(|d| d.saturating_duration_since(now))
    }

    pub fn cancel(&mut self) {
        self.deadline = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_once_after_the_last_change() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut d = Debounce::new(ms(500));
        assert!(!d.fire(t0 + ms(1000)), "nothing changed");

        d.poke(t0);
        assert!(d.remaining(t0).is_some());
        assert!(!d.fire(t0 + ms(400)));
        // Another change pushes it back.
        d.poke(t0 + ms(400));
        assert!(!d.fire(t0 + ms(800)));
        assert_eq!(d.remaining(t0 + ms(800)), Some(ms(100)));
        assert!(d.fire(t0 + ms(900)));
        assert!(!d.fire(t0 + ms(1000)), "only once");
        assert!(d.remaining(t0 + ms(1000)).is_none());
        assert_eq!(d.remaining(t0), None);

        d.poke(t0);
        d.cancel();
        assert!(!d.fire(t0 + ms(5000)));
    }
}
