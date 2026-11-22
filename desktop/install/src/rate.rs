//! Install prompts are rate limited: one dialog at a time (a link that arrives while one is open is dropped),
//! and at most `per_minute` dialogs in any minute.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const PER_MINUTE: usize = 5;
const WINDOW: Duration = Duration::from_secs(60);

/// How long a bot's offer (its version, URL and SHA-256, as the catalogue gave them) can be accepted.
pub const OFFER_TTL: Duration = Duration::from_secs(5 * 60);

pub fn offer_expired(read_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(read_at) >= OFFER_TTL
}

pub struct Gate {
    open: bool,
    opened: VecDeque<Instant>,
    per_minute: usize,
}

impl Default for Gate {
    fn default() -> Self {
        Gate::new(PER_MINUTE)
    }
}

impl Gate {
    pub fn new(per_minute: usize) -> Gate {
        Gate { open: false, opened: VecDeque::new(), per_minute }
    }

    /// Whether a dialog may open now; if so, it counts as open until `close`.
    pub fn try_open(&mut self, now: Instant) -> bool {
        if self.open {
            return false;
        }
        while self.opened.front().is_some_and(|t| now.saturating_duration_since(*t) >= WINDOW) {
            self.opened.pop_front();
        }
        if self.opened.len() >= self.per_minute {
            return false;
        }
        self.opened.push_back(now);
        self.open = true;
        true
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_expire() {
        let t = Instant::now();
        assert!(!offer_expired(t, t));
        assert!(!offer_expired(t, t + OFFER_TTL - Duration::from_millis(1)));
        assert!(offer_expired(t, t + OFFER_TTL));
    }

    #[test]
    fn one_at_a_time_and_n_per_minute() {
        let t0 = Instant::now();
        let mut g = Gate::new(3);
        assert!(g.try_open(t0));
        assert!(!g.try_open(t0), "a second dialog while one is open");
        g.close();
        assert!(g.try_open(t0 + Duration::from_secs(1)));
        g.close();
        assert!(g.try_open(t0 + Duration::from_secs(2)));
        g.close();
        assert!(!g.try_open(t0 + Duration::from_secs(3)), "a fourth within the minute");
        assert!(!g.try_open(t0 + Duration::from_secs(59)));
        assert!(g.try_open(t0 + Duration::from_secs(60)), "the first one has left the window");
    }
}
