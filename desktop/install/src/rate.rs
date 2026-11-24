//! Links are rate limited: one of each kind at a time (an install link that arrives while its dialog is open is
//! dropped, and so is a run link while the last one waits for the page), and at most `per_minute` in any minute,
//! install and run links together.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const PER_MINUTE: usize = 5;
const WINDOW: Duration = Duration::from_secs(60);

/// How long a bot's offer (its version, URL and SHA-256, as the catalogue gave them) can be accepted.
pub const OFFER_TTL: Duration = Duration::from_secs(5 * 60);

pub fn offer_expired(read_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(read_at) >= OFFER_TTL
}

/// The per-minute budget all `actuaria:` links share, install and run links alike: at most `per_minute` are
/// acted on in any minute.
pub struct Budget {
    taken: VecDeque<Instant>,
    per_minute: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new(PER_MINUTE)
    }
}

impl Budget {
    pub fn new(per_minute: usize) -> Budget {
        Budget { taken: VecDeque::new(), per_minute }
    }

    /// Whether one more link fits in the last minute; if so, it counts.
    pub fn try_take(&mut self, now: Instant) -> bool {
        while self.taken.front().is_some_and(|t| now.saturating_duration_since(*t) >= WINDOW) {
            self.taken.pop_front();
        }
        if self.taken.len() >= self.per_minute {
            return false;
        }
        self.taken.push_back(now);
        true
    }
}

/// One link of a kind at a time: an install dialog, or a run link waiting for the page.
#[derive(Default)]
pub struct Gate {
    opened_at: Option<Instant>,
}

impl Gate {
    /// Whether a link may open now: none of this kind is open, and the shared budget has room (the budget is
    /// only charged when the gate is free). If so, it counts as open until `close`.
    pub fn try_open(&mut self, budget: &mut Budget, now: Instant) -> bool {
        if self.opened_at.is_some() || !budget.try_take(now) {
            return false;
        }
        self.opened_at = Some(now);
        true
    }

    /// Close the open one if it opened `ttl` or more ago: a run link the page never took doesn't block the next
    /// for good.
    pub fn expire(&mut self, now: Instant, ttl: Duration) {
        if self.opened_at.is_some_and(|t| now.saturating_duration_since(t) >= ttl) {
            self.opened_at = None;
        }
    }

    pub fn close(&mut self) {
        self.opened_at = None;
    }

    pub fn is_open(&self) -> bool {
        self.opened_at.is_some()
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
        let mut b = Budget::new(3);
        let mut g = Gate::default();
        assert!(g.try_open(&mut b, t0));
        assert!(!g.try_open(&mut b, t0), "a second dialog while one is open");
        g.close();
        assert!(g.try_open(&mut b, t0 + Duration::from_secs(1)));
        g.close();
        assert!(g.try_open(&mut b, t0 + Duration::from_secs(2)));
        g.close();
        assert!(!g.try_open(&mut b, t0 + Duration::from_secs(3)), "a fourth within the minute");
        assert!(!g.try_open(&mut b, t0 + Duration::from_secs(59)));
        assert!(g.try_open(&mut b, t0 + Duration::from_secs(60)), "the first one has left the window");
    }

    #[test]
    fn install_and_run_links_share_one_budget() {
        let t0 = Instant::now();
        let mut budget = Budget::new(PER_MINUTE);
        let (mut install, mut run) = (Gate::default(), Gate::default());
        // Alternate the kinds, closing each: five in all, whatever the mix.
        for n in 0..PER_MINUTE {
            let t = t0 + Duration::from_secs(n as u64);
            let g = if n % 2 == 0 { &mut install } else { &mut run };
            assert!(g.try_open(&mut budget, t), "link {n}");
            g.close();
        }
        let t = t0 + Duration::from_secs(10);
        assert!(!install.try_open(&mut budget, t), "a sixth install link in the minute");
        assert!(!run.try_open(&mut budget, t), "a sixth run link in the minute");
        assert!(run.try_open(&mut budget, t0 + Duration::from_secs(60)));
    }

    #[test]
    fn a_link_dropped_while_its_kind_is_open_costs_nothing() {
        let t0 = Instant::now();
        let mut budget = Budget::new(2);
        let (mut install, mut run) = (Gate::default(), Gate::default());
        assert!(run.try_open(&mut budget, t0));
        for _ in 0..10 {
            assert!(!run.try_open(&mut budget, t0), "dropped: the last run link is still waiting");
        }
        // One at a time is per kind: an install link may still open, and uses the last of the budget.
        assert!(install.try_open(&mut budget, t0));
        install.close();
        assert!(!install.try_open(&mut budget, t0), "the budget is spent");
    }

    #[test]
    fn a_stale_open_one_expires() {
        let t0 = Instant::now();
        let ttl = Duration::from_secs(120);
        let mut b = Budget::new(5);
        let mut g = Gate::default();
        assert!(g.try_open(&mut b, t0));
        g.expire(t0 + ttl - Duration::from_millis(1), ttl);
        assert!(g.is_open(), "still waiting for the page");
        assert!(!g.try_open(&mut b, t0 + Duration::from_secs(1)));
        g.expire(t0 + ttl, ttl);
        assert!(!g.is_open());
        assert!(g.try_open(&mut b, t0 + ttl));
        // Expiring doesn't lift the per-minute limit.
        let mut b = Budget::new(1);
        let mut g = Gate::default();
        assert!(g.try_open(&mut b, t0));
        g.expire(t0 + Duration::from_secs(30), Duration::from_secs(10));
        assert!(!g.try_open(&mut b, t0 + Duration::from_secs(30)));
    }
}
