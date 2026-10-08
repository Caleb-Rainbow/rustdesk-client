//! Bounded reconnect delays. Live desktop traffic and server heartbeat replies
//! are not delayed by this policy.

#[derive(Default)]
pub(crate) struct ReconnectBackoff {
    failures: u32,
}

impl ReconnectBackoff {
    pub(crate) fn reset(&mut self) {
        self.failures = 0;
    }

    pub(crate) fn next_delay_ms(&mut self, random: u64) -> u64 {
        let cap = (1_000u64 << self.failures.min(5)).min(30_000);
        self.failures = self.failures.saturating_add(1);
        cap / 2 + random % (cap / 2 + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_grow_then_remain_bounded() {
        let mut backoff = ReconnectBackoff::default();
        for cap in [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000] {
            let delay = backoff.next_delay_ms(u64::MAX);
            assert!((cap / 2..=cap).contains(&delay));
        }
        for _ in 0..1_000 {
            assert!((15_000..=30_000).contains(&backoff.next_delay_ms(0)));
        }
    }

    #[test]
    fn manual_restart_or_stable_connection_resets_delay() {
        let mut backoff = ReconnectBackoff::default();
        for _ in 0..10 {
            backoff.next_delay_ms(0);
        }
        backoff.reset();
        assert_eq!(backoff.next_delay_ms(0), 500);
        backoff.reset();
        assert_eq!(backoff.next_delay_ms(500), 1_000);
    }
}
