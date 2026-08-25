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
    /// TODO(verify-live): the alias table is inferred from Polymarket's public taxonomy.
    /// The live `/events/keyset` capture (2026-07-30) confirms the *shape* — `tags[]` of
    /// `{label, slug}`, e.g. `{"label":"Politics","slug":"politics"}` — but two events are
    /// not the whole vocabulary, so anything beyond the `other` fallback is still a guess.
    /// Since that capture the fee rate no longer depends on this mapping wherever Gamma
    /// states one per market (see [`MarketFees::api_rate`]); the floors still do.
    pub fn from_text(text: &str) -> Option<Self> {
        let t = text.trim().to_ascii_lowercase();
        let canonical = match t.as_str() {
            "geopolitics" | "world" | "geopolitical" => "geopolitics",
            "politics" | "election" | "elections" | "us-politics" | "us politics"
            | "politics-us" => "politics",
            "finance" | "business" | "stocks" | "markets" => "finance",
            "tech" | "technology" | "ai" => "tech",
            "mentions" | "mention" => "mentions",
            "sports" | "nba" | "nfl" | "mlb" | "nhl" | "soccer" | "football" | "tennis" | "epl"
            | "ufc" => "sports",
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

    /// Apply one aggregated level update from a streaming delta (M6).
    ///
    /// A zero (or negative) size **removes** the level — that is how the CLOB channel says
    /// "this price is gone" — and any other size replaces it outright (levels are
    /// aggregate resting size, not increments). The [`normalized`](Self::normalized)
    /// invariant is maintained, so `best_bid`/`best_ask`/`vwap_for_size` stay correct after
    /// any sequence of updates without re-sorting the whole book.
    pub fn apply_level(&mut self, side: Side, price: Decimal, size: Decimal) {
        if price <= Decimal::ZERO {
            return; // same junk rule as `normalized`
        }
        let levels = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let existing = levels.iter().position(|l| l.price == price);
        if size <= Decimal::ZERO {
            if let Some(i) = existing {
                levels.remove(i);
            }
            return;
        }
        match existing {
            Some(i) => levels[i].size = size,
            None => {
                let at = match side {
                    Side::Bid => levels.iter().position(|l| l.price < price),
                    Side::Ask => levels.iter().position(|l| l.price > price),
                }
                .unwrap_or(levels.len());
                levels.insert(at, PriceLevel::new(price, size));
            }
        }
    }

    pub fn best_bid(&self) -> Option<Decimal> {
        self.best(Side::Bid)
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.best(Side::Ask)
    }

    pub fn best(&self, side: Side) -> Option<Decimal> {
        self.levels(side).first().map(|l| l.price)
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

/// Per-market fee data exactly as Gamma reports it.
///
/// Verified against the live `GET /events/keyset` response (2026-07-30): every market
/// object carries `feesEnabled` and `feeType`, plus a `feeSchedule`
/// `{exponent, rate, takerOnly, rebateRate}` whenever fees are on. The legacy `/events`
/// payload carries none of it, so every field is optional and "absent" means "the API said
/// nothing — ask the category table".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketFees {
    /// `feesEnabled`. `Some(false)` is an explicit "this market is fee-free".
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `feeType`, e.g. `"politics_fees"`. Informational: the rate is what we cost with.
    #[serde(default)]
    pub fee_type: Option<String>,
    /// `feeSchedule.rate` — the `rate` of `rate · p · (1 − p)`.
    #[serde(default)]
    pub rate: Option<Decimal>,
    /// `feeSchedule.exponent`. `1` is the documented curve we implement; anything else is
    /// a formula this build does not know, and is never guessed at.
    #[serde(default)]
    pub exponent: Option<Decimal>,
    /// `feeSchedule.takerOnly`. Our cost model already charges takers only; a `false` here
    /// would mean makers pay too, which is worth surfacing rather than assuming away.
    #[serde(default)]
    pub taker_only: Option<bool>,
    /// `feeSchedule.rebateRate` — maker rebate share. Captured for Phase B; unused today.
    #[serde(default)]
    pub rebate_rate: Option<Decimal>,
}

impl MarketFees {
    /// True when Gamma described a fee curve with an exponent other than the documented
    /// `1`. The formula is then unknown to this build, so the API rate must NOT be used.
    pub fn exponent_unsupported(&self) -> bool {
        self.enabled == Some(true)
            && self.rate.is_some()
            && self.exponent.is_some_and(|e| e != Decimal::ONE)
    }

    /// The taker fee rate the API states for this market, or `None` when the category
    /// table has to decide.
    ///
    /// Precedence, deliberately conservative — an API rate is only used when the API said
    /// something we can price exactly:
    ///
    /// * `feesEnabled = false` → `Some(0)`. Gamma says this market charges no taker fee
    ///   (observed on pre-deployment markets, and matching the documented fee-free tiers).
    /// * `feesEnabled = true` with a `rate` and `exponent` 1 (or absent, which is the
    ///   documented default curve) → `Some(rate)`.
    /// * `feesEnabled = true` with an exponent we do not implement → `None`: the category
    ///   table is used instead, and discovery counts it (never silently mispriced).
    /// * `feesEnabled = true` with no `rate` → `None`.
    /// * fields absent altogether (the legacy `/events` shape) → `None`.
    pub fn api_rate(&self) -> Option<Decimal> {
        match self.enabled {
            Some(false) => Some(Decimal::ZERO),
            Some(true) => {
                if self.exponent_unsupported() {
                    return None;
                }
                self.rate.filter(|r| *r >= Decimal::ZERO)
            }
            None => None,
        }
    }
}

/// Gamma's own per-market activity figures — a **pruning input only**.
///
/// M6.4. A live discovery pass tracks ~44 000 markets, and most of them cannot fill a $50
/// order; they still cost a WebSocket subscription, a slot in every REST sweep and a share
/// of the rate-limit budget. These numbers are how the universe is cut down to the markets
/// worth watching (see `scan.activity_floor`).
///
/// They are **never** used for sizing, profit or slippage math. CLAUDE.md is explicit on
/// why: reported volume is double-counted and was up to ~60% wash trading, and liquidity
/// must be measured from order book depth. A reported figure is good enough to answer "is
/// this market alive at all?" and nothing more.
///
/// Every field is optional: the legacy `/events` payload carries none of them, and absence
/// means "the API said nothing", which the floor treats as a keep — never as a zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketActivity {
    /// `liquidityClob`, else `liquidityNum`, else `liquidity` (see `gamma::market_activity`).
    #[serde(default)]
    pub liquidity: Option<Decimal>,
    /// `volume24hrClob`, else `volume24hr`.
    #[serde(default)]
    pub volume_24h: Option<Decimal>,
    /// `spread`. Captured for diagnostics; the floor does not read it (the executable
    /// spread comes from the book, not from a reported aggregate).
    #[serde(default)]
    pub spread: Option<Decimal>,
    #[serde(default)]
    pub best_bid: Option<Decimal>,
    #[serde(default)]
    pub best_ask: Option<Decimal>,
}

impl MarketActivity {
    /// True when Gamma reported none of these figures for this market.
    pub fn is_empty(&self) -> bool {
        self.liquidity.is_none()
            && self.volume_24h.is_none()
            && self.spread.is_none()
            && self.best_bid.is_none()
            && self.best_ask.is_none()
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
    /// What Gamma says about this market's fees. Empty for the legacy endpoint.
    #[serde(default)]
    pub fees: MarketFees,
    /// What Gamma says about this market's activity. Used only to decide whether the
    /// market is worth tracking at all — never in any money path.
    #[serde(default)]
    pub activity: MarketActivity,
    /// Order-book constraints Gamma publishes per market. Captured for Phase B execution
    /// (tick-size rounding, minimum order size) and for the liquidity-reward qualification
    /// parameters; nothing in Phase A reads them.
    #[serde(default)]
    pub trading: MarketTrading,
}

/// `orderPriceMinTickSize` / `orderMinSize` / `rewardsMinSize` / `rewardsMaxSpread`, as
/// published on every market object of the live `/events/keyset` response (2026-07-30).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketTrading {
    #[serde(default)]
    pub min_tick_size: Option<Decimal>,
    #[serde(default)]
    pub min_order_size: Option<Decimal>,
    /// `rewardsMinSize` — the smallest order, **in shares**, that scores at all. Distinct
    /// from `min_order_size` (the CLOB's own 5-share floor): an order can be perfectly legal
    /// and still score zero.
    #[serde(default)]
    pub rewards_min_size: Option<Decimal>,
    /// `rewardsMaxSpread` — the qualifying half-band **in cents** (e.g. `3.5`), while every
    /// book price in this codebase is a fraction of a dollar (`0.035`). The 100× confusion
    /// between the two is a documented bug class in ported implementations, so the unit is
    /// carried in the name of every conversion (see `rewardsim::cents_to_dollars`).
    #[serde(default)]
    pub rewards_max_spread: Option<Decimal>,
    /// Total configured reward pool for this market, USD per day: the sum of
    /// `clobRewards[].rewardsDailyRate` (R1).
    ///
    /// It is a **configured cap, not a payout** — the research is explicit that the sum of
    /// advertised daily rates across the venue is an order of magnitude above what is
    /// actually distributed, because a market that never reaches its LP-activity threshold
    /// pays out less than its rate. Never model income off it without saying so.
    #[serde(default)]
    pub rewards_daily_rate: Option<Decimal>,
}

impl MarketTrading {
    /// True when this market advertises a reward pool *and* both qualification parameters,
    /// which is the minimum needed to score a quote at all. The candidate builder in
    /// `dryrun` applies the same three tests through `reward_params`, which additionally
    /// converts them; this is the readable form of the predicate.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reward_eligible(&self) -> bool {
        self.rewards_daily_rate.is_some_and(|r| r > Decimal::ZERO)
            && self.rewards_max_spread.is_some_and(|v| v > Decimal::ZERO)
            && self.rewards_min_size.is_some_and(|s| s > Decimal::ZERO)
    }
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
    /// The markets we can actually price — a *subset* of the event's outcome set.
    pub markets: Vec<TrackedMarket>,
    /// How many markets Gamma listed for this event **before** any filtering: closed,
    /// inactive, order-book-disabled and unparseable ones all count.
    ///
    /// This is the denominator of the full-coverage guard. A NegRisk sweep only pays $1
    /// (YES-side) or $(N−1) (NO-side) when it spans *every* outcome of the event; if an
    /// outcome was dropped during discovery, buying the tracked subset leaves the dropped
    /// outcome uncovered and the position is not risk-free. Sweeps over a partial outcome
    /// set must never be labelled `true-arb`.
    pub total_outcomes: usize,
    /// When Gamma says the event closes, if it said. Used only to detect markets that live
    /// and die *between* universe refreshes (the fast-cycling crypto series); nothing in
    /// the detectors keys on it.
    #[serde(default)]
    pub end_date: Option<chrono::DateTime<chrono::Utc>>,
}

impl TrackedEvent {
    /// True when every outcome Gamma listed for this event is tracked and priceable.
    ///
    /// `total_outcomes == 0` means the count was never populated; treat that as unknown
    /// (i.e. *not* complete) rather than optimistically assuming full coverage.
    pub fn coverage_complete(&self) -> bool {
        self.total_outcomes > 0 && self.markets.len() == self.total_outcomes
    }

    /// Outcomes Gamma listed that we are not tracking.
    pub fn missing_outcomes(&self) -> usize {
        self.total_outcomes.saturating_sub(self.markets.len())
    }

    /// True when this event closes at or before `cutoff`. An unknown end time is never
    /// "short-lived": we do not guess.
    pub fn ends_by(&self, cutoff: chrono::DateTime<chrono::Utc>) -> bool {
        self.end_date.is_some_and(|end| end <= cutoff)
    }
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

    /// How many distinct token ids the universe holds — one WebSocket subscription and one
    /// `/books` slot each, so it is the number that actually drives the scan's cost.
    ///
    /// Counts without cloning: at live scale [`token_ids`](Self::token_ids) allocates tens
    /// of thousands of strings, which is far too much for a log line.
    pub fn token_count(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        self.events
            .iter()
            .flat_map(|e| &e.markets)
            .flat_map(|m| &m.token_ids)
            .filter(|token| seen.insert(*token))
            .count()
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

    /// Stable machine name — matches the serde representation, and is what the database
    /// and the dedupe key store.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BinaryYesNo => "binary_yes_no",
            Self::NegRiskYesSide => "neg_risk_yes_side",
            Self::NegRiskNoSide => "neg_risk_no_side",
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
    /// The taker fee rate this leg was costed at. Legs of one event normally share it
    /// (they share the event's `feeType`), but it is resolved and recorded per leg so a
    /// mixed event can never be costed at one leg's rate. `#[serde(default)]` so rows
    /// written before this field existed still deserialise.
    #[serde(default)]
    pub fee_rate: Decimal,
}

/// A detected, costed, depth-sized opportunity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opportunity {
    pub kind: OpportunityKind,
    pub label: Label,
    pub event_slug: String,
    pub event_title: String,
    pub category: Category,
    /// The highest taker fee rate applied to any leg — the headline number for the report
    /// and the CLI. The exact per-leg rates live on [`Leg::fee_rate`].
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

    /// `Some((tracked, total))` when this construction spans only part of the event's
    /// outcome set. Always `None` for a `true-arb` row: a partial sweep is relative value,
    /// never arbitrage, because the untracked outcomes can win and pay us nothing.
    pub partial_coverage: Option<(usize, usize)>,

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

/// Optional [`Decimal`], accepting `null`, `"0.04"` and `0.04`.
///
/// Anything unparseable becomes `None` rather than an error: these fields are advisory
/// (an absent fee rate falls back to the category table), and one malformed number must
/// not throw away a whole discovery page. It is never turned into a guessed value.
pub fn de_opt_decimal<'de, D>(deserializer: D) -> Result<Option<Decimal>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Str(String),
        Num(serde_json::Number),
    }

    Ok(match Option::<Raw>::deserialize(deserializer)? {
        None => None,
        Some(Raw::Str(s)) => s.trim().parse::<Decimal>().ok(),
        Some(Raw::Num(n)) => n.to_string().parse::<Decimal>().ok(),
    })
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
    fn apply_level_sets_replaces_and_removes_while_keeping_the_ordering() {
        let mut b = book(); // bids 0.45/50, 0.40/100 ; asks 0.50/100, 0.55/200

        // A new best bid slots in at the front.
        b.apply_level(Side::Bid, dec!(0.47), dec!(25));
        assert_eq!(b.best_bid(), Some(dec!(0.47)));
        assert_eq!(b.bids.len(), 3);

        // A new inner ask sorts ahead of the existing ones.
        b.apply_level(Side::Ask, dec!(0.52), dec!(10));
        assert_eq!(
            b.asks.iter().map(|l| l.price).collect::<Vec<_>>(),
            vec![dec!(0.50), dec!(0.52), dec!(0.55)]
        );

        // An existing level is replaced, not accumulated.
        b.apply_level(Side::Ask, dec!(0.50), dec!(7));
        assert_eq!(b.asks[0].size, dec!(7));
        assert_eq!(b.depth(Side::Ask), dec!(217));

        // Zero size removes.
        b.apply_level(Side::Ask, dec!(0.50), dec!(0));
        assert_eq!(b.best_ask(), Some(dec!(0.52)));
        assert_eq!(b.asks.len(), 2);

        // Removing a level that is not there, and junk prices, are no-ops.
        b.apply_level(Side::Bid, dec!(0.99), dec!(0));
        b.apply_level(Side::Bid, dec!(0), dec!(100));
        assert_eq!(b.bids.len(), 3);
        assert_eq!(b.best_bid(), Some(dec!(0.47)));

        // Emptying a side is allowed and leaves no phantom best price.
        for price in [dec!(0.47), dec!(0.45), dec!(0.40)] {
            b.apply_level(Side::Bid, price, Decimal::ZERO);
        }
        assert_eq!(b.best_bid(), None);
        assert_eq!(b.spread(), None);
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
            fees: MarketFees::default(),
            trading: MarketTrading::default(),
            activity: MarketActivity::default(),
        };
        assert_eq!(m.yes_token().as_str(), "b");
        assert_eq!(m.no_token().as_str(), "a");
    }

    /// R1 — reward eligibility needs all three parameters, and each one absent means "the
    /// API said nothing", never a zero or a default.
    #[test]
    fn reward_eligibility_needs_a_pool_a_band_and_a_minimum_size() {
        let full = MarketTrading {
            rewards_min_size: Some(dec!(50)),
            rewards_max_spread: Some(dec!(3.5)),
            rewards_daily_rate: Some(dec!(30)),
            ..MarketTrading::default()
        };
        assert!(full.reward_eligible());

        for missing in [
            MarketTrading {
                rewards_daily_rate: None,
                ..full.clone()
            },
            MarketTrading {
                rewards_max_spread: None,
                ..full.clone()
            },
            MarketTrading {
                rewards_min_size: None,
                ..full.clone()
            },
            // A market listed as reward-eligible with an unfunded pool pays nothing.
            MarketTrading {
                rewards_daily_rate: Some(Decimal::ZERO),
                ..full.clone()
            },
        ] {
            assert!(
                !missing.reward_eligible(),
                "{missing:?} must not be eligible"
            );
        }
        assert!(!MarketTrading::default().reward_eligible());
    }

    /// The precedence rules for the API-provided fee data, one case each. These decide
    /// real money: an over-stated rate hides opportunities, an under-stated one invents
    /// them.
    #[test]
    fn api_fee_rate_precedence() {
        // Fees explicitly off → zero, not the category table.
        let off = MarketFees {
            enabled: Some(false),
            ..MarketFees::default()
        };
        assert_eq!(off.api_rate(), Some(Decimal::ZERO));
        assert!(!off.exponent_unsupported());

        // The live politics shape: enabled, standard curve, rate stated.
        let politics = MarketFees {
            enabled: Some(true),
            fee_type: Some("politics_fees".into()),
            rate: Some(dec!(0.04)),
            exponent: Some(dec!(1)),
            taker_only: Some(true),
            rebate_rate: Some(dec!(0.25)),
        };
        assert_eq!(politics.api_rate(), Some(dec!(0.04)));
        assert!(!politics.exponent_unsupported());

        // An exponent we do not implement: fall back, never guess the formula.
        let exotic = MarketFees {
            exponent: Some(dec!(2)),
            ..politics.clone()
        };
        assert_eq!(exotic.api_rate(), None);
        assert!(exotic.exponent_unsupported());

        // Enabled but no rate stated → the category table decides.
        let no_rate = MarketFees {
            rate: None,
            ..politics.clone()
        };
        assert_eq!(no_rate.api_rate(), None);

        // An absent exponent is the documented default curve, so the rate is usable.
        let no_exponent = MarketFees {
            exponent: None,
            ..politics
        };
        assert_eq!(no_exponent.api_rate(), Some(dec!(0.04)));

        // The legacy shape: the API said nothing at all.
        assert_eq!(MarketFees::default().api_rate(), None);
        assert!(!MarketFees::default().exponent_unsupported());
    }

    #[test]
    fn optional_decimals_accept_strings_numbers_null_and_junk() {
        #[derive(Deserialize)]
        struct W {
            #[serde(default, deserialize_with = "de_opt_decimal")]
            v: Option<Decimal>,
        }
        let parse = |s: &str| serde_json::from_str::<W>(s).expect("parses").v;
        assert_eq!(parse(r#"{"v":"0.04"}"#), Some(dec!(0.04)));
        assert_eq!(parse(r#"{"v":0.04}"#), Some(dec!(0.04)));
        assert_eq!(parse(r#"{"v":null}"#), None);
        assert_eq!(parse("{}"), None);
        // Junk is "unknown", never a guessed number.
        assert_eq!(parse(r#"{"v":"soon"}"#), None);
    }

    /// The database and the dedupe key store `as_str()`; JSON stores the serde name.
    /// If they ever diverge, historical rows stop matching new ones.
    #[test]
    fn opportunity_kind_names_match_their_serde_form() {
        for kind in [
            OpportunityKind::BinaryYesNo,
            OpportunityKind::NegRiskYesSide,
            OpportunityKind::NegRiskNoSide,
        ] {
            let json = serde_json::to_string(&kind).expect("serialise");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
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
