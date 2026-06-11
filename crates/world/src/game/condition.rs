#![forbid(unsafe_code)]
#[derive(Debug, Clone)]
pub(crate) struct ConditionRegeneration {
    pub expires_at_ms: u64,
    pub health_gain: i32,
    pub health_interval_ms: u64,
    pub last_health_tick: u64,
    pub total_heal: i32,
    pub total_heal_cap: i32,
}
#[allow(dead_code)]
impl ConditionRegeneration {
    pub fn new(now: u64, dur: u64, hg: i32, hi: u64, cap: i32) -> Self {
        Self {
            expires_at_ms: now + dur,
            health_gain: hg,
            health_interval_ms: hi,
            last_health_tick: now,
            total_heal: 0,
            total_heal_cap: cap,
        }
    }
    pub fn extend(&mut self, extra: u64, now: u64) {
        self.expires_at_ms = self.expires_at_ms.saturating_add(extra);
        self.last_health_tick = now;
    }
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at_ms
    }
    pub fn remaining_ms(&self, now: u64) -> u64 {
        self.expires_at_ms.saturating_sub(now)
    }
    pub fn execute_tick(&mut self, now: u64) -> i32 {
        if self.health_gain > 0
            && now.saturating_sub(self.last_health_tick) >= self.health_interval_ms
        {
            let a = self
                .health_gain
                .min(self.total_heal_cap.saturating_sub(self.total_heal));
            if a > 0 {
                self.total_heal += a;
                self.last_health_tick = now;
                return a;
            }
        }
        0
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tick_heals() {
        let mut c = ConditionRegeneration::new(0, 60000, 1, 6000, 100);
        assert_eq!(c.execute_tick(6000), 1);
    }
    #[test]
    fn cap_stops() {
        let mut c = ConditionRegeneration::new(0, 60000, 1, 6000, 2);
        c.execute_tick(6000);
        c.execute_tick(12000);
        assert_eq!(c.execute_tick(18000), 0);
    }
}
