//! Fetches the ZEC price. The price list it fills is `zingolib_common::price::PriceList`.
//!
//! Currently only supports USD.

use serde::Deserialize;
use zingolib_common::price::Price;

/// Errors with price requests and parsing.
// TODO: remove unused when historical data is implemented
#[derive(Debug, thiserror::Error)]
pub enum PriceError {
    /// Request failed.
    #[error("request failed. {0}")]
    RequestFailed(#[from] reqwest::Error),
    /// Deserialization failed.
    #[error("deserialization failed. {0}")]
    DeserializationFailed(#[from] serde_json::Error),
    /// Parse error.
    #[error("parse error. {0}")]
    ParseError(#[from] std::num::ParseFloatError),
    /// Price list start time not set. Call `PriceList::set_start_time`.
    #[error("price list start time has not been set.")]
    PriceListNotInitialized,
    /// Decimal conversion error.
    #[error("decimal conversion error. {0}")]
    DecimalError(#[from] rust_decimal::Error),
    /// Invalid price.
    #[error("invalid price.")]
    InvalidPrice,
}

#[derive(Debug, Deserialize)]
struct CurrentPriceResponse {
    price: String,
    timestamp: u32,
}

/// Get current price of ZEC in USD
pub async fn get_current_price() -> Result<Price, PriceError> {
    let httpget = reqwest::get("https://api.gemini.com/v1/trades/zecusd?limit_trades=11").await?;
    let mut trades = httpget
        .json::<Vec<CurrentPriceResponse>>()
        .await?
        .iter()
        .map(|response| {
            let price_usd: f32 = response.price.parse()?;
            if !price_usd.is_finite() {
                return Err(PriceError::InvalidPrice);
            }

            Ok(Price {
                price_usd,
                time: response.timestamp,
            })
        })
        .collect::<Result<Vec<Price>, PriceError>>()?;

    trades.sort_by(|a, b| {
        a.price_usd
            .partial_cmp(&b.price_usd)
            .expect("trades are checked to be finite and comparable")
    });

    Ok(trades[5])
}
