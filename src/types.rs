//! Core domain types. All money / price / size values are [`Decimal`] — never `f64`.

use std::collections::HashMap;
use std::fmt;

use rust_decimal::Decimal;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

/// A CLOB outcome-token id (ERC-1155 token id, decimal string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TokenId(String);

impl TokenId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Fee/behaviour category of an event. Normalised to a lowercase canonical name so the
/// fee table stays config-owned (rates are set by the protocol and change).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Category(String);

impl Category {
    pub const OTHER: &'static str = "other";

    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into().trim().to_ascii_lowercase())
    }

    pub fn other() -> Self {
        Self(Self::OTHER.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Map free-form Gamma tag/category text onto a canonical fee category.
    ///
    /// TODO(verify-live): the alias table is inferred from Polymarket's public taxonomy;
    /// confirm the exact tag vocabulary against live `/events` payloads before relying on
    /// anything other than the `other` fallback.
    pub fn from_text(text: &str) -> Option<Self> {
        let t = text.trim().to_ascii_lowercase();
        let canonical = match t.as_str() {
            "geopolitics" | "world" | "geopolitical" => "geopolitics",
            "politics" | "election" | "elections" | "us-politics" | "us politics"
            | "politics-us" => "politics",
            "finance" | "business" | "stocks" | "markets" => "finance",
            "tech" | "technology" | "ai" => "tech",
            "mentions" | "mention" => "mentions",
            "sports" | "nba" | "nfl" | "mlb" | "nhl" | "soccer" | "football" | "tennis"
            | "epl" | "ufc" => "sports",
            "economics" | "economy" | "econ" | "inflation" | "fed" => "economics",
            "culture" | "pop-culture" | "pop culture" | "entertainment" | "movies" | "music" => {
                "culture"
            }
            "weather" | "climate" => "weather",
            "crypto" | "cryptocurrency" | "bitcoin" | "ethereum" | "crypto-prices" => "crypto",
            "other" => "other",
            _ => return None,
        };
        Some(Self(canonical.to_string()))
    }

    /// First recognised category among an event's category field and its tags.
    pub fn resolve(category_field: Option<&str>, tags: &[String]) -> Self {
        category_field
            .and_then(Self::from_text)
            .or_else(|| tags.iter().find_map(|t| Self::from_text(t)))
            .unwrap_or_else(Self::other)
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One aggregated level of an order book.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceLevel {
    #[serde(deserialize_with = "de_decimal")]
    pub price: Decimal,
    #[serde(deserialize_with = "de_decimal")]
    pub size: Decimal,
}

impl PriceLevel {
    pub fn new(price: Decimal, size: Decimal) -> Self {
        Self { price, size }
    }
}

/// Which side of the book we consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Resting buy orders — where we sell into.
    Bid,
    /// Resting sell orders — where we buy from.
    Ask,
}

/// A CLOB order book snapshot for a single outcome token.
///
/// Invariant after [`OrderBook::normalized`]: `bids` sorted descending by price, `asks`
/// ascending, and all levels have strictly positive price and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBook {
    pub asset_id: TokenId,
    #[serde(default)]
    pub bids: Vec<PriceLevel>,
    #[serde(default)]
    pub asks: Vec<PriceLevel>,
}

impl OrderBook {
    pub fn new(asset_id: TokenId, bids: Vec<PriceLevel>, asks: Vec<PriceLevel>) -> Self {
        Self {
            asset_id,
            bids,
            asks,
        }
    }

    /// Defensive normalisation: the API's ordering is not contractual, so we drop junk
    /// levels and re-sort both sides ourselves rather than trusting the response.
    pub fn normalized(mut self) -> Self {
        self.bids
            .retain(|l| l.price > Decimal::ZERO && l.size > Decimal::ZERO);
        self.asks
            .retain(|l| l.price > Decimal::ZERO && l.size > Decimal::ZERO);
        self.bids.sort_by(|a, b| b.price.cmp(&a.price));
        self.asks.sort_by(|a, b| a.price.cmp(&b.price));
        self
    }

    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.first().map(|l| l.price)
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.price)
    }

    pub fn spread(&self) -> Option<Decimal> {
        Some(self.best_ask()? - self.best_bid()?)
    }

    pub fn levels(&self, side: Side) -> &[PriceLevel] {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    pub fn depth(&self, side: Side) -> Decimal {
        self.levels(side).iter().map(|l| l.size).sum()
    }

    /// Size-weighted average fill price for consuming `size` shares from `side`.
    ///
    /// Returns `None` when the book cannot fill `size` (this is the depth constraint the
    /// research says dominates: top-of-book prices are not executable at size).
    pub fn vwap_for_size(&self, side: Side, size: Decimal) -> Option<Decimal> {
        if size <= Decimal::ZERO {
            return None;
        }
        let mut remaining = size;
        let mut notional = Decimal::ZERO;
        for level in self.levels(side) {
            let take = remaining.min(level.size);
            notional += take * level.price;
            remaining -= take;
            if remaining <= Decimal::ZERO {
                return notional.checked_div(size);
            }
        }
        None
    }
}

/// A tracked market: one binary condition with exactly two outcome tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedMarket {
    pub condition_id: String,
    pub question: String,
    /// Outcome labels, index-aligned with `token_ids` (Polymarket convention: `["Yes","No"]`).
    pub outcomes: [String; 2],
    pub token_ids: [TokenId; 2],
}

impl TrackedMarket {
    /// Index of the outcome whose label looks like "Yes". Defaults to 0 when neither
    /// label matches, which matches Polymarket's `["Yes","No"]` ordering.
    pub fn yes_index(&self) -> usize {
        if self.outcomes[0].trim().eq_ignore_ascii_case("yes") {
            0
        } else if self.outcomes[1].trim().eq_ignore_ascii_case("yes") {
            1
        } else {
            0
        }
    }

    pub fn yes_token(&self) -> &TokenId {
        &self.token_ids[self.yes_index()]
    }

    pub fn no_token(&self) -> &TokenId {
        &self.token_ids[1 - self.yes_index()]
    }

    pub fn yes_label(&self) -> &str {
        &self.outcomes[self.yes_index()]
    }

    pub fn no_label(&self) -> &str {
        &self.outcomes[1 - self.yes_index()]
    }
}

/// A tracked event: either a NegRisk (mutually exclusive multi-outcome) group or a
/// standalone group of binary markets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedEvent {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub neg_risk: bool,
    pub category: Category,
    pub markets: Vec<TrackedMarket>,
}

/// The full set of markets the scanner watches on a tick.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Universe {
    pub events: Vec<TrackedEvent>,
}

impl Universe {
    pub fn market_count(&self) -> usize {
        self.events.iter().map(|e| e.markets.len()).sum()
    }

    pub fn neg_risk_event_count(&self) -> usize {
        self.events.iter().filter(|e| e.neg_risk).count()
    }

    /// Every token id in the universe, de-duplicated, in stable order.
    pub fn token_ids(&self) -> Vec<TokenId> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for event in &self.events {
            for market in &event.markets {
                for token in &market.token_ids {
                    if seen.insert(token.clone()) {
                        out.push(token.clone());
                    }
                }
            }
        }
        out
    }
}

/// Book snapshots keyed by token id.
pub type BookMap = HashMap<TokenId, OrderBook>;

/// Which structural mispricing produced an opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpportunityKind {
    /// One condition: buy YES and NO, `ask(YES) + ask(NO) < $1`.
    BinaryYesNo,
    /// NegRisk event: buy YES on every outcome, `Σ ask(YESᵢ) < $1`.
    NegRiskYesSide,
    /// NegRisk event: buy NO on every outcome, `Σ ask(NOᵢ) < $(N−1)`.
    NegRiskNoSide,
}

impl OpportunityKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::BinaryYesNo => "binary YES+NO",
            Self::NegRiskYesSide => "negrisk YES-side",
            Self::NegRiskNoSide => "negrisk NO-side",
        }
    }
}

/// True arbitrage locks the payout at resolution regardless of outcome; relative value
/// only pays if prices converge. Never report the second as risk-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    TrueArb,
    RelativeValue,
}

impl Label {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TrueArb => "true-arb",
            Self::RelativeValue => "relative-value",
        }
    }
}

/// One buy leg of an opportunity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leg {
    pub token_id: TokenId,
    pub condition_id: String,
    pub question: String,
    pub outcome: String,
    pub best_ask: Decimal,
    pub best_bid: Option<Decimal>,
    /// Depth-walked average fill price at `size`.
    pub vwap: Decimal,
    pub size: Decimal,
    /// Total shares resting on the ask side.
    pub ask_depth: Decimal,
}

/// A detected, costed, depth-sized opportunity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opportunity {
    pub kind: OpportunityKind,
    pub label: Label,
    pub event_slug: String,
    pub event_title: String,
    pub category: Category,
    pub fee_rate: Decimal,
    /// Guaranteed payout per share-set at resolution: `$1` for binary / NegRisk YES,
    /// `$(N−1)` for the NegRisk NO-side construction.
    pub payout: Decimal,
    pub legs: Vec<Leg>,

    // ---- per-share cost breakdown -------------------------------------------------
    /// `payout − Σ best_ask` — the gap on the executable side at top of book. Never mid.
    pub gross_gap: Decimal,
    /// `Σ vwap − Σ best_ask` — extra cost of walking the book to `executable_size`.
    pub slippage_cost: Decimal,
    /// `Σ (best_ask − best_bid)`. Already embedded in `gross_gap` (which is ask-based);
    /// it is what a maker recovers by resting instead of crossing, hence
    /// `net_maker = gross_gap + spread_cost`.
    pub spread_cost: Option<Decimal>,
    /// `Σ rate · vwapᵢ · (1 − vwapᵢ)`, per share. Takers only; makers pay zero.
    pub fee_taker: Decimal,
    /// `gross_gap − slippage_cost − fee_taker`.
    pub net_taker: Decimal,
    /// `payout − Σ best_bid`; `None` when a leg has no bid. Carries fill/legging risk.
    pub net_maker: Option<Decimal>,

    // ---- sized totals -------------------------------------------------------------
    pub executable_size: Decimal,
    pub capital_required: Decimal,
    pub net_taker_total: Decimal,
    pub net_maker_total: Option<Decimal>,

    /// Execution / resolution caveats a human must read before trusting the number.
    pub resolution_flags: Vec<String>,
    /// NegRisk NO-side: capital efficiency depends on the NegRisk adapter conversion path.
    pub conversion_required: bool,
    /// Taker net was below the floor; only the maker construction clears it.
    pub maker_only: bool,
}

/// Accepts both `"0.42"` and `0.42` — Polymarket sends decimal strings, but the shape is
/// not contractual and a float here would silently corrupt money math.
pub fn de_decimal<'de, D>(deserializer: D) -> Result<Decimal, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Str(String),
        Num(serde_json::Number),
    }

    match Raw::deserialize(deserializer)? {
        Raw::Str(s) => s.trim().parse::<Decimal>().map_err(de::Error::custom),
        Raw::Num(n) => n.to_string().parse::<Decimal>().map_err(de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn book() -> OrderBook {
        // Deliberately unsorted input to prove `normalized` does not trust the API.
        OrderBook::new(
            TokenId::new("t1"),
            vec![
                PriceLevel::new(dec!(0.40), dec!(100)),
                PriceLevel::new(dec!(0.45), dec!(50)),
            ],
            vec![
                PriceLevel::new(dec!(0.55), dec!(200)),
                PriceLevel::new(dec!(0.50), dec!(100)),
                PriceLevel::new(dec!(0.60), dec!(0)), // zero size is dropped
            ],
        )
        .normalized()
    }

    #[test]
    fn normalizes_and_sorts_defensively() {
        let b = book();
        assert_eq!(b.best_bid(), Some(dec!(0.45)));
        assert_eq!(b.best_ask(), Some(dec!(0.50)));
        assert_eq!(b.spread(), Some(dec!(0.05)));
        assert_eq!(b.asks.len(), 2);
        assert_eq!(b.depth(Side::Ask), dec!(300));
    }

    #[test]
    fn vwap_walks_multiple_levels() {
        let b = book();
        // 100 @ 0.50 -> exactly top level.
        assert_eq!(b.vwap_for_size(Side::Ask, dec!(100)), Some(dec!(0.50)));
        // 200 shares = 100@0.50 + 100@0.55 = 105 / 200 = 0.525
        assert_eq!(b.vwap_for_size(Side::Ask, dec!(200)), Some(dec!(0.525)));
        // 300 shares = 50 + 110 = 160 / 300 = 0.5333...
        let v = b.vwap_for_size(Side::Ask, dec!(300)).expect("full depth");
        assert!((v - dec!(0.533333)).abs() < dec!(0.000001));
        // beyond depth
        assert_eq!(b.vwap_for_size(Side::Ask, dec!(301)), None);
        assert_eq!(b.vwap_for_size(Side::Ask, dec!(0)), None);
    }

    #[test]
    fn category_resolution_falls_back_to_other() {
        assert_eq!(
            Category::resolve(Some("Politics"), &[]).as_str(),
            "politics"
        );
        assert_eq!(
            Category::resolve(None, &["Sports".into(), "NBA".into()]).as_str(),
            "sports"
        );
        assert_eq!(
            Category::resolve(Some("Unknown Thing"), &["also-unknown".into()]).as_str(),
            "other"
        );
    }

    #[test]
    fn yes_no_token_selection_respects_outcome_labels() {
        let m = TrackedMarket {
            condition_id: "c".into(),
            question: "q".into(),
            outcomes: ["No".into(), "Yes".into()],
            token_ids: [TokenId::new("a"), TokenId::new("b")],
        };
        assert_eq!(m.yes_token().as_str(), "b");
        assert_eq!(m.no_token().as_str(), "a");
    }

    #[test]
    fn decimal_deserializes_from_string_and_number() {
        #[derive(Deserialize)]
        struct W {
            #[serde(deserialize_with = "de_decimal")]
            v: Decimal,
        }
        let a: W = serde_json::from_str(r#"{"v":"0.031"}"#).expect("string form");
        let b: W = serde_json::from_str(r#"{"v":0.031}"#).expect("number form");
        assert_eq!(a.v, dec!(0.031));
        assert_eq!(b.v, dec!(0.031));
    }
}
