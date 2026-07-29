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

    #[test]
    fn float_prices_are_still_parsed_as_decimals() {
        // Defensive: the API sends strings today, but a float must not silently become an
        // f64 anywhere in the money path.
        let body = r#"[{"asset_id":"9","bids":[{"price":0.07,"size":3}],"asks":[]}]"#;
        let books = parse_books("test", body).expect("numeric form");
        assert_eq!(books[0].best_bid(), Some(dec!(0.07)));
    }
}
