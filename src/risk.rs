//! Risk limits. Phase A implements only the per-trade capital cap, which is enforced
//! inside the depth walker so no reported size can ever exceed it.
//!
//! Phase B adds total exposure, max open positions and the kill switch (CLAUDE.md).

use rust_decimal::Decimal;

use crate::config::RiskConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskLimits {
    per_trade_cap_usd: Decimal,
}

impl RiskLimits {
    pub fn new(per_trade_cap_usd: Decimal) -> Self {
        Self { per_trade_cap_usd }
    }

    pub fn from_config(cfg: &RiskConfig) -> Self {
        Self::new(cfg.per_trade_cap_usd)
    }

    pub fn per_trade_cap_usd(&self) -> Decimal {
        self.per_trade_cap_usd
    }

    /// True when a position costing `capital` is inside the per-trade cap.
    pub fn allows_capital(&self, capital: Decimal) -> bool {
        capital <= self.per_trade_cap_usd
    }

    /// Largest share count affordable at `cost_per_share` under the per-trade cap.
    /// Returns `None` for a non-positive price (a book we should not trade against).
    pub fn max_shares_at(&self, cost_per_share: Decimal) -> Option<Decimal> {
        if cost_per_share <= Decimal::ZERO {
            return None;
        }
        self.per_trade_cap_usd.checked_div(cost_per_share)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn per_trade_cap_is_inclusive_and_blocks_overshoot() {
        let r = RiskLimits::new(dec!(50));
        assert!(r.allows_capital(dec!(49.99)));
        assert!(r.allows_capital(dec!(50)));
        assert!(!r.allows_capital(dec!(50.01)));
    }

    #[test]
    fn max_shares_respects_cap() {
        let r = RiskLimits::new(dec!(50));
        // A $0.98 share set: 50 / 0.98 = 51.02... shares.
        let s = r.max_shares_at(dec!(0.98)).expect("positive price");
        assert!(s > dec!(51.02) && s < dec!(51.03));
        assert_eq!(r.max_shares_at(dec!(0)), None);
    }
}
