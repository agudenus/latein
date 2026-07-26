//! Configuration: `config/default.toml` + selected environment overrides.
//!
//! No secrets live here. Per CLAUDE.md, credentials only ever come from the environment,
//! and Phase A needs none.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::Category;

pub const DEFAULT_CONFIG_PATH: &str = "config/default.toml";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("environment variable {var} is not a valid {expected}: {value:?}")]
    Env {
        var: &'static str,
        expected: &'static str,
        value: String,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Only `dry-run` is implemented in Phase A. Live trading is a later, explicitly
    /// gated milestone (CLAUDE.md).
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub risk: RiskConfig,
    /// Category → taker fee rate. Verified against docs.polymarket.com 2026-07; kept in
    /// config because the protocol can change them.
    #[serde(default = "default_fee_rates")]
    pub fees: BTreeMap<String, Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    pub gamma_base_url: String,
    pub clob_base_url: String,
    pub request_timeout_secs: u64,
    pub max_retries: u32,
    /// Client-side throttle between HTTP requests to one host.
    pub min_request_interval_ms: u64,
    /// Token ids per `POST /books` call.
    pub books_batch_size: usize,
    pub user_agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    /// Gamma `/events` page size.
    pub page_size: usize,
    /// Stop paginating after this many events (rate-limit guard).
    pub max_events: usize,
    /// Skip NegRisk events with more outcomes than this (leg count blows up capital).
    pub max_negrisk_outcomes: usize,
    /// Smallest share count worth reporting.
    pub min_size_shares: Decimal,
    /// Binary-search resolution for the depth walker, in shares.
    pub size_search_tolerance: Decimal,
    /// Minimum net profit per share, by category, with a `default` key.
    pub net_floor: BTreeMap<String, Decimal>,
    /// Report gaps whose taker net dies to fees/slippage but whose maker net clears the
    /// floor. Maker capture is fee-free and rebate-subsidised, but carries legging risk.
    pub report_maker_only: bool,
    /// If non-empty, only scan these categories.
    pub include_categories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskConfig {
    /// Hard per-opportunity capital cap in USDC. Applied inside the depth walker.
    pub per_trade_cap_usd: Decimal,
}

fn default_mode() -> String {
    "dry-run".to_string()
}

fn default_fee_rates() -> BTreeMap<String, Decimal> {
    // Verified against docs.polymarket.com/trading/fees, 2026-07.
    [
        ("geopolitics", Decimal::new(0, 2)),
        ("politics", Decimal::new(4, 2)),
        ("finance", Decimal::new(4, 2)),
        ("tech", Decimal::new(4, 2)),
        ("mentions", Decimal::new(4, 2)),
        ("sports", Decimal::new(5, 2)),
        ("economics", Decimal::new(5, 2)),
        ("culture", Decimal::new(5, 2)),
        ("weather", Decimal::new(5, 2)),
        ("other", Decimal::new(5, 2)),
        ("crypto", Decimal::new(7, 2)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            gamma_base_url: "https://gamma-api.polymarket.com".to_string(),
            clob_base_url: "https://clob.polymarket.com".to_string(),
            request_timeout_secs: 20,
            max_retries: 3,
            min_request_interval_ms: 120,
            books_batch_size: 100,
            user_agent: concat!("polyarb/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            page_size: 100,
            max_events: 500,
            max_negrisk_outcomes: 30,
            min_size_shares: Decimal::new(5, 0),
            size_search_tolerance: Decimal::new(1, 2),
            net_floor: [
                ("default".to_string(), Decimal::new(5, 3)),
                ("geopolitics".to_string(), Decimal::new(3, 3)),
            ]
            .into_iter()
            .collect(),
            report_maker_only: true,
            include_categories: Vec::new(),
        }
    }
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            per_trade_cap_usd: Decimal::new(50, 0),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            api: ApiConfig::default(),
            scan: ScanConfig::default(),
            risk: RiskConfig::default(),
            fees: default_fee_rates(),
        }
    }
}

impl Config {
    /// Load from `path` (or `$POLYARB_CONFIG`, or the default path if it exists), then
    /// apply environment overrides and validate.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let explicit = path.map(PathBuf::from).or_else(|| {
            std::env::var("POLYARB_CONFIG")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
        });

        let mut cfg = match explicit {
            Some(p) => Self::from_file(&p)?,
            None => {
                let default_path = PathBuf::from(DEFAULT_CONFIG_PATH);
                if default_path.exists() {
                    Self::from_file(&default_path)?
                } else {
                    Self::default()
                }
            }
        };

        cfg.apply_env_overrides()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn from_toml_str(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    fn apply_env_overrides(&mut self) -> Result<(), ConfigError> {
        if let Some(v) = env_str("POLYARB_GAMMA_BASE_URL") {
            self.api.gamma_base_url = v;
        }
        if let Some(v) = env_str("POLYARB_CLOB_BASE_URL") {
            self.api.clob_base_url = v;
        }
        if let Some(v) = env_decimal("POLYARB_PER_TRADE_CAP_USD")? {
            self.risk.per_trade_cap_usd = v;
        }
        if let Some(v) = env_decimal("POLYARB_NET_FLOOR_DEFAULT")? {
            self.scan.net_floor.insert("default".to_string(), v);
        }
        if let Some(v) = env_str("POLYARB_MODE") {
            self.mode = v;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.mode != "dry-run" {
            return Err(ConfigError::Invalid(format!(
                "mode must be \"dry-run\" in Phase A (got {:?}); live trading is not implemented",
                self.mode
            )));
        }
        if self.risk.per_trade_cap_usd <= Decimal::ZERO {
            return Err(ConfigError::Invalid(
                "risk.per_trade_cap_usd must be > 0".into(),
            ));
        }
        if self.scan.min_size_shares <= Decimal::ZERO {
            return Err(ConfigError::Invalid(
                "scan.min_size_shares must be > 0".into(),
            ));
        }
        if self.scan.size_search_tolerance <= Decimal::ZERO {
            return Err(ConfigError::Invalid(
                "scan.size_search_tolerance must be > 0".into(),
            ));
        }
        if self.scan.page_size == 0 || self.api.books_batch_size == 0 {
            return Err(ConfigError::Invalid(
                "scan.page_size and api.books_batch_size must be > 0".into(),
            ));
        }
        if !self.scan.net_floor.contains_key("default") {
            return Err(ConfigError::Invalid(
                "scan.net_floor must contain a \"default\" entry".into(),
            ));
        }
        if !self.fees.contains_key(Category::OTHER) {
            return Err(ConfigError::Invalid(
                "fees must contain an \"other\" entry (used as the fallback rate)".into(),
            ));
        }
        Ok(())
    }

    /// Minimum acceptable net profit per share for a category.
    pub fn net_floor_for(&self, category: &Category) -> Decimal {
        self.scan
            .net_floor
            .get(category.as_str())
            .or_else(|| self.scan.net_floor.get("default"))
            .copied()
            .unwrap_or(Decimal::ZERO)
    }

    pub fn category_included(&self, category: &Category) -> bool {
        self.scan.include_categories.is_empty()
            || self
                .scan
                .include_categories
                .iter()
                .any(|c| c.eq_ignore_ascii_case(category.as_str()))
    }
}

fn env_str(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn env_decimal(var: &'static str) -> Result<Option<Decimal>, ConfigError> {
    match env_str(var) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<Decimal>()
            .map(Some)
            .map_err(|_| ConfigError::Env {
                var,
                expected: "decimal",
                value: raw,
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const SHIPPED: &str = include_str!("../config/default.toml");

    #[test]
    fn shipped_default_config_parses_and_validates() {
        let cfg = Config::from_toml_str(SHIPPED).expect("config/default.toml must parse");
        cfg.validate().expect("config/default.toml must validate");
        assert_eq!(cfg.mode, "dry-run");
        assert_eq!(cfg.risk.per_trade_cap_usd, dec!(50));
        assert_eq!(cfg.fees["geopolitics"], dec!(0));
        assert_eq!(cfg.fees["politics"], dec!(0.04));
        assert_eq!(cfg.fees["sports"], dec!(0.05));
        assert_eq!(cfg.fees["crypto"], dec!(0.07));
    }

    #[test]
    fn net_floor_falls_back_to_default() {
        let cfg = Config::default();
        assert_eq!(
            cfg.net_floor_for(&Category::new("geopolitics")),
            dec!(0.003)
        );
        assert_eq!(cfg.net_floor_for(&Category::new("politics")), dec!(0.005));
        assert_eq!(cfg.net_floor_for(&Category::new("nonsense")), dec!(0.005));
    }

    #[test]
    fn live_mode_is_rejected_in_phase_a() {
        let cfg = Config {
            mode: "live".into(),
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unknown_config_keys_are_rejected_loudly() {
        let err = Config::from_toml_str("mode = \"dry-run\"\nnot_a_key = 1\n");
        assert!(err.is_err(), "typos in config must not be silently ignored");
    }

    #[test]
    fn category_filter_defaults_to_everything() {
        let mut cfg = Config::default();
        assert!(cfg.category_included(&Category::new("weather")));
        cfg.scan.include_categories = vec!["politics".into()];
        assert!(cfg.category_included(&Category::new("Politics")));
        assert!(!cfg.category_included(&Category::new("weather")));
    }
}
