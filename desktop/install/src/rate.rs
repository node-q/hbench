//! Links are rate limited: one at a time (an install link that arrives while its dialog is open is dropped, and
//! so is a run link while the last one waits for the page), and at most `per_minute` in any minute.

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

    /// Close the open one if it opened `ttl` or more ago: a run link the page never took doesn't block the next
    /// for good.
    pub fn expire(&mut self, now: Instant, ttl: Duration) {
        if self.open && self.opened.back().is_none_or(|t| now.saturating_duration_since(*t) >= ttl) {
            self.open = false;
        }
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

    #[test]
    fn a_stale_open_one_expires() {
        let t0 = Instant::now();
        let ttl = Duration::from_secs(120);
        let mut g = Gate::new(5);
        assert!(g.try_open(t0));
        g.expire(t0 + ttl - Duration::from_millis(1), ttl);
        assert!(g.is_open(), "still waiting for the page");
        assert!(!g.try_open(t0 + Duration::from_secs(1)));
        g.expire(t0 + ttl, ttl);
        assert!(!g.is_open());
        assert!(g.try_open(t0 + ttl));
        // Expiring doesn't lift the per-minute limit.
        let mut g = Gate::new(1);
        assert!(g.try_open(t0));
        g.expire(t0 + Duration::from_secs(30), Duration::from_secs(10));
        assert!(!g.try_open(t0 + Duration::from_secs(30)));
    }
}
