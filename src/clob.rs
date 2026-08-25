//! CLOB API client — order book snapshots.
//!
//! `POST https://clob.polymarket.com/books` with a body of `[{"token_id": "..."}]`
//! returns one book per token: `{asset_id, bids: [{price, size}], asks: [{price, size}]}`
//! with decimal *strings*. Books are normalised (bids descending, asks ascending, junk
//! levels dropped) before anything downstream touches them — the API's ordering is not a
//! contract and the whole cost model depends on "best" meaning best.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::http::{ApiError, HttpClient};
use crate::types::{de_decimal, BookMap, OrderBook, PriceLevel, TokenId};

#[derive(Debug, Serialize)]
struct BookQuery<'a> {
    token_id: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
struct RawLevel {
    #[serde(deserialize_with = "de_decimal")]
    price: rust_decimal::Decimal,
    #[serde(deserialize_with = "de_decimal")]
    size: rust_decimal::Decimal,
}

#[derive(Debug, Clone, Deserialize)]
struct RawBook {
    /// TODO(verify-live): the batch endpoint returns `asset_id`; the singular `/book`
    /// endpoint has also been seen with `assetId`. Both spellings are accepted.
    #[serde(alias = "assetId", alias = "asset_id")]
    asset_id: String,
    #[serde(default)]
    bids: Vec<RawLevel>,
    #[serde(default)]
    asks: Vec<RawLevel>,
}

/// Parse a `/books` response body into normalised books.
pub fn parse_books(url: &str, body: &str) -> Result<Vec<OrderBook>, ApiError> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Payload {
        Many(Vec<RawBook>),
        One(RawBook),
    }

    let raw = match serde_json::from_str::<Payload>(body) {
        Ok(Payload::Many(v)) => v,
        Ok(Payload::One(b)) => vec![b],
        Err(source) => {
            return Err(ApiError::Decode {
                url: url.to_string(),
                source,
            })
        }
    };

    Ok(raw
        .into_iter()
        .map(|b| {
            OrderBook::new(
                TokenId::new(b.asset_id),
                b.bids
                    .into_iter()
                    .map(|l| PriceLevel::new(l.price, l.size))
                    .collect(),
                b.asks
                    .into_iter()
                    .map(|l| PriceLevel::new(l.price, l.size))
                    .collect(),
            )
            .normalized()
        })
        .collect())
}

// ---------------------------------------------------------------------------------
// Liquidity-rewards parameters (R1)
// ---------------------------------------------------------------------------------

/// The qualification parameters and pool size of one reward-eligible market.
///
/// Units are the load-bearing detail and are stated in every field: `max_spread` is in
/// **cents** while every book price in this codebase is a fraction of a dollar, and
/// `min_size` is in **shares** and is distinct from the CLOB's own `minimum_order_size`.
/// Confusing either is a documented 100×/zero-score bug class in ported implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewardMarket {
    pub condition_id: String,
    pub neg_risk: bool,
    /// `rewards.min_size`, in **shares**. An order below it scores nothing.
    pub min_size: Option<rust_decimal::Decimal>,
    /// `rewards.max_spread`, in **cents** from the adjusted midpoint.
    pub max_spread_cents: Option<rust_decimal::Decimal>,
    /// `Σ rewards.rates[].rewards_daily_rate`, USD per day. A configured **cap**, not a
    /// promised payout (see `research/liquidity-rewards-2026-08.md` §2.2).
    pub daily_rate: Option<rust_decimal::Decimal>,
    pub tokens: Vec<TokenId>,
    pub minimum_order_size: Option<rust_decimal::Decimal>,
    pub minimum_tick_size: Option<rust_decimal::Decimal>,
}

/// One page of `GET /sampling-markets`, plus the cursor that asks for the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewardMarketPage {
    pub markets: Vec<RewardMarket>,
    /// `next_cursor`. The API signals the end with `"LTE="` (base64 of `-1`) or by omitting
    /// it; both land here as `None`.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawRewardRate {
    #[serde(default, deserialize_with = "crate::types::de_opt_decimal")]
    rewards_daily_rate: Option<rust_decimal::Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawRewards {
    #[serde(default)]
    rates: Option<Vec<RawRewardRate>>,
    #[serde(default, deserialize_with = "crate::types::de_opt_decimal")]
    min_size: Option<rust_decimal::Decimal>,
    #[serde(default, deserialize_with = "crate::types::de_opt_decimal")]
    max_spread: Option<rust_decimal::Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawRewardToken {
    #[serde(default)]
    token_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawSamplingMarket {
    #[serde(default)]
    condition_id: Option<String>,
    #[serde(default)]
    neg_risk: Option<bool>,
    #[serde(default)]
    rewards: Option<RawRewards>,
    #[serde(default)]
    tokens: Vec<RawRewardToken>,
    #[serde(default, deserialize_with = "crate::types::de_opt_decimal")]
    minimum_order_size: Option<rust_decimal::Decimal>,
    #[serde(default, deserialize_with = "crate::types::de_opt_decimal")]
    minimum_tick_size: Option<rust_decimal::Decimal>,
}

/// The cursor value the CLOB uses for "no more pages" (base64 `"-1"`).
const CURSOR_END: &str = "LTE=";

/// Parse a `/sampling-markets` (or `/rewards/markets/current`) response body.
///
/// TODO(verify-live): this container cannot reach `clob.polymarket.com`, so the shape is
/// taken from the archived official TypeScript client's types plus a mirrored OpenAPI spec
/// and encoded in `tests/fixtures/clob_sampling_markets.json`. Both the wrapper
/// (`{data, next_cursor}` vs a bare array) and the `rewards` sub-object must be confirmed
/// against a live response before any number derived from them is trusted.
pub fn parse_sampling_markets(url: &str, body: &str) -> Result<RewardMarketPage, ApiError> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Payload {
        Wrapped {
            data: Vec<RawSamplingMarket>,
            #[serde(default)]
            next_cursor: Option<String>,
        },
        Bare(Vec<RawSamplingMarket>),
    }

    let (raw, next_cursor) = match serde_json::from_str::<Payload>(body) {
        Ok(Payload::Wrapped { data, next_cursor }) => (data, next_cursor),
        Ok(Payload::Bare(v)) => (v, None),
        Err(source) => {
            return Err(ApiError::Decode {
                url: url.to_string(),
                source,
            })
        }
    };

    let markets = raw
        .into_iter()
        .filter_map(|m| {
            let condition_id = m.condition_id.filter(|c| !c.trim().is_empty())?;
            let rewards = m.rewards;
            // Sum the configured rates. An absent `rates` array is "the API said nothing",
            // which stays `None`; an empty one is a real zero.
            let daily_rate = rewards.as_ref().and_then(|r| {
                r.rates.as_ref().map(|rates| {
                    rates
                        .iter()
                        .filter_map(|rate| rate.rewards_daily_rate)
                        .filter(|rate| *rate >= rust_decimal::Decimal::ZERO)
                        .sum::<rust_decimal::Decimal>()
                })
            });
            Some(RewardMarket {
                condition_id,
                neg_risk: m.neg_risk.unwrap_or(false),
                min_size: rewards.as_ref().and_then(|r| r.min_size),
                max_spread_cents: rewards.as_ref().and_then(|r| r.max_spread),
                daily_rate,
                tokens: m
                    .tokens
                    .into_iter()
                    .filter_map(|t| t.token_id)
                    .filter(|t| !t.trim().is_empty())
                    .map(TokenId::new)
                    .collect(),
                minimum_order_size: m.minimum_order_size,
                minimum_tick_size: m.minimum_tick_size,
            })
        })
        .collect();

    Ok(RewardMarketPage {
        markets,
        next_cursor: next_cursor
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty() && c != CURSOR_END),
    })
}

pub struct ClobClient<'a> {
    http: &'a HttpClient,
    base_url: String,
    batch_size: usize,
}

impl<'a> ClobClient<'a> {
    pub fn new(http: &'a HttpClient, cfg: &Config) -> Self {
        Self {
            http,
            base_url: cfg.api.clob_base_url.trim_end_matches('/').to_string(),
            batch_size: cfg.api.books_batch_size.max(1),
        }
    }

    /// Fetch books for every token, in batches. Tokens the API omits (no book yet) are
    /// simply absent from the result; detectors treat a missing leg as "not priceable".
    pub async fn fetch_books(&self, tokens: &[TokenId]) -> Result<BookMap, ApiError> {
        self.fetch_books_batched(tokens, |_| {}).await
    }

    /// [`fetch_books`](Self::fetch_books), with each batch handed to `on_batch` the moment
    /// it lands.
    ///
    /// A full sweep of the live universe is 160+ batches over 30–70 s, so "the state when
    /// the sweep finished" and "the state when this batch was answered" are very different
    /// things. The stream's divergence cross-check needs the second one, plus the instant
    /// the request went out, to tell a stale local book from a book that simply moved while
    /// the request was in flight.
    pub async fn fetch_books_batched<F>(
        &self,
        tokens: &[TokenId],
        mut on_batch: F,
    ) -> Result<BookMap, ApiError>
    where
        F: FnMut(BookBatch),
    {
        let url = format!("{}/books", self.base_url);
        let mut out = BookMap::with_capacity(tokens.len());

        for chunk in tokens.chunks(self.batch_size) {
            let payload: Vec<BookQuery<'_>> = chunk
                .iter()
                .map(|t| BookQuery {
                    token_id: t.as_str(),
                })
                .collect();
            let requested_at = Instant::now();
            let body = self.http.post_json(&url, &payload).await?;
            let books = parse_books(&url, &body)?;
            tracing::debug!(
                requested = chunk.len(),
                returned = books.len(),
                "fetched clob book batch"
            );
            on_batch(BookBatch {
                books: &books,
                requested_at,
            });
            for book in books {
                out.insert(book.asset_id.clone(), book);
            }
        }

        Ok(out)
    }
}

impl ClobClient<'_> {
    /// Every reward-eligible market, with its qualification parameters (R1).
    ///
    /// `GET /sampling-markets` returns *only* markets that currently carry a reward pool, so
    /// this is the candidate universe for the farming simulator — several hundred markets
    /// rather than the ~70 000 of the scanner's universe.
    ///
    /// `max_pages` is a rate-limit backstop like `scan.max_events`: when it is what stops
    /// us the candidate set is incomplete, and that is logged rather than swallowed.
    pub async fn fetch_reward_markets(
        &self,
        max_pages: usize,
    ) -> Result<Vec<RewardMarket>, ApiError> {
        let url = format!("{}/sampling-markets", self.base_url);
        let mut out: Vec<RewardMarket> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;

        loop {
            if pages >= max_pages.max(1) {
                tracing::warn!(
                    max_pages,
                    markets = out.len(),
                    "reward-market discovery stopped at rewardsim.max_candidate_pages — the \
                     candidate set is incomplete"
                );
                break;
            }
            let query: Vec<(&str, String)> = match cursor.as_ref() {
                Some(c) => vec![("next_cursor", c.clone())],
                None => Vec::new(),
            };
            let body = self.http.get_json(&url, &query).await?;
            let page = parse_sampling_markets(&url, &body)?;
            pages += 1;
            let got = page.markets.len();
            out.extend(page.markets);
            tracing::debug!(
                page = pages,
                got,
                total = out.len(),
                "fetched sampling markets"
            );
            match page.next_cursor {
                // A cursor that does not advance would page forever; stop instead.
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                _ => break,
            }
        }
        Ok(out)
    }
}

/// One `/books` batch, handed to the callback the moment it lands (so "now" inside the
/// callback *is* the batch's return time — there is no need to carry it).
#[derive(Debug, Clone, Copy)]
pub struct BookBatch<'a> {
    pub books: &'a [OrderBook],
    /// When the request went out. Local state touched after this is newer than the answer,
    /// which is what lets the stream tell drift from in-flight market movement.
    pub requested_at: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Side;
    use rust_decimal_macros::dec;

    const BOOKS: &str = include_str!("../tests/fixtures/clob_books.json");

    #[test]
    fn parses_a_realistic_books_response() {
        let books = parse_books("test", BOOKS).expect("fixture must parse");
        assert_eq!(books.len(), 4);

        let yes = &books[0];
        assert_eq!(yes.asset_id.as_str(), "1001");
        // The fixture lists levels in API order (worst-first on the ask side); the parser
        // must re-sort so `best_ask` really is the best.
        assert_eq!(yes.best_ask(), Some(dec!(0.41)));
        assert_eq!(yes.best_bid(), Some(dec!(0.40)));
        assert_eq!(yes.spread(), Some(dec!(0.01)));
        assert_eq!(yes.depth(Side::Ask), dec!(1800));
        // 500 @ 0.41 then 500 @ 0.43 → (205 + 215) / 1000 = 0.42
        assert_eq!(yes.vwap_for_size(Side::Ask, dec!(1000)), Some(dec!(0.42)));
    }

    #[test]
    fn zero_size_levels_and_empty_sides_are_handled() {
        let books = parse_books("test", BOOKS).expect("fixture must parse");
        // Token 1004 has a zero-size ask level that must not become the best ask.
        let b = books
            .iter()
            .find(|b| b.asset_id.as_str() == "1004")
            .unwrap();
        assert_eq!(b.best_ask(), Some(dec!(0.59)));
        // Token 2002 has no asks at all.
        let empty = books
            .iter()
            .find(|b| b.asset_id.as_str() == "2002")
            .unwrap();
        assert_eq!(empty.best_ask(), None);
        assert_eq!(empty.vwap_for_size(Side::Ask, dec!(1)), None);
    }

    #[test]
    fn single_book_payload_is_accepted() {
        let body =
            r#"{"market":"0xabc","asset_id":"9","bids":[{"price":"0.5","size":"10"}],"asks":[]}"#;
        let books = parse_books("test", body).expect("single book");
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].best_bid(), Some(dec!(0.5)));
    }

    #[test]
    fn malformed_body_names_the_endpoint() {
        let err = parse_books("https://clob/books", "{oops").expect_err("must fail");
        assert!(err.to_string().contains("https://clob/books"));
    }

    const SAMPLING: &str = include_str!("../tests/fixtures/clob_sampling_markets.json");

    /// R1 — the reward-parameter fixture, field by field, because every one of these is a
    /// unit trap: `max_spread` is cents, `min_size` is shares, `daily_rate` is a configured
    /// cap summed across funded configs.
    #[test]
    fn parses_the_sampling_markets_reward_parameters() {
        let page = parse_sampling_markets("test", SAMPLING).expect("fixture must parse");
        // The id-less market is dropped rather than guessed at.
        assert_eq!(page.markets.len(), 3);
        assert_eq!(page.next_cursor.as_deref(), Some("MTAw"));

        let first = &page.markets[0];
        assert_eq!(first.min_size, Some(dec!(50)));
        assert_eq!(first.max_spread_cents, Some(dec!(3.5)));
        assert_eq!(first.daily_rate, Some(dec!(30)));
        assert!(first.neg_risk);
        assert_eq!(first.tokens.len(), 2);
        assert_eq!(first.tokens[0].as_str(), "7001");
        assert_eq!(first.minimum_order_size, Some(dec!(5)));

        // Two funded configs on one market sum into one pool.
        assert_eq!(page.markets[1].daily_rate, Some(dec!(150)));
        assert_eq!(page.markets[1].max_spread_cents, Some(dec!(1.5)));

        // `rates: null` is "the API said nothing", not a zero pool.
        assert_eq!(page.markets[2].daily_rate, None);
    }

    #[test]
    fn the_sampling_cursor_terminates_on_the_end_sentinel_and_on_absence() {
        let end = r#"{"data":[],"next_cursor":"LTE="}"#;
        assert_eq!(
            parse_sampling_markets("test", end)
                .expect("parses")
                .next_cursor,
            None,
            "LTE= is the CLOB's end-of-pages sentinel and must not be paged on"
        );
        let bare =
            r#"[{"condition_id":"0x1","rewards":{"rates":[],"min_size":20,"max_spread":3}}]"#;
        let page = parse_sampling_markets("test", bare).expect("bare array");
        assert_eq!(page.next_cursor, None);
        // An empty (but present) rates array is a real zero pool.
        assert_eq!(page.markets[0].daily_rate, Some(dec!(0)));
    }

    #[test]
    fn float_prices_are_still_parsed_as_decimals() {
        // Defensive: the API sends strings today, but a float must not silently become an
        // f64 anywhere in the money path.
        let body = r#"[{"asset_id":"9","bids":[{"price":0.07,"size":3}],"asks":[]}]"#;
        let books = parse_books("test", body).expect("numeric form");
        assert_eq!(books[0].best_bid(), Some(dec!(0.07)));
    }
}
