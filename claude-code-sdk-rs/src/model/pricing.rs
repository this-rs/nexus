//! The single price table (contract §12, decisions A1 and A21).
//!
//! Cost is `usage × price`, computed here and nowhere else. A model without a
//! price gives `None`: never a zero, never an invented figure. The table is empty
//! by default; the host fills it (configuration, catalogue, user input).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::agent::{Cost, CostBasis, ModelPrice, Usage};

/// Prices by model identifier, in USD per million tokens.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PriceTable {
    prices: BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces the price of `model`.
    pub fn insert(&mut self, model: impl Into<String>, price: ModelPrice) -> &mut Self {
        self.prices.insert(model.into(), price);
        self
    }

    /// Builder form of [`PriceTable::insert`].
    pub fn with(mut self, model: impl Into<String>, price: ModelPrice) -> Self {
        self.insert(model, price);
        self
    }

    /// The price of `model`, when known (exact identifier match).
    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        self.prices.get(model)
    }

    /// Number of priced models.
    pub fn len(&self) -> usize {
        self.prices.len()
    }

    /// Whether no model is priced.
    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    /// Cost in USD of `usage` on `model`, `None` when the model has no price or
    /// when the input or output token count is unknown.
    ///
    /// `Usage::input_tokens` is read as the tokens that were **not** served from
    /// the cache; cache reads and writes are charged at their own price when the
    /// table has one, at the input price otherwise.
    pub fn cost_usd(&self, model: &str, usage: &Usage) -> Option<f64> {
        let price = self.get(model)?;
        let input = usage.input_tokens?;
        let output = usage.output_tokens?;
        let per_token = |tokens: u64, per_mtok: f64| tokens as f64 * per_mtok / 1_000_000.0;
        let cache_read_price = price.cache_read_per_mtok.unwrap_or(price.input_per_mtok);
        let cache_write_price = price.cache_write_per_mtok.unwrap_or(price.input_per_mtok);
        Some(
            per_token(input, price.input_per_mtok)
                + per_token(output, price.output_per_mtok)
                + per_token(usage.cache_read_tokens.unwrap_or(0), cache_read_price)
                + per_token(usage.cache_creation_tokens.unwrap_or(0), cache_write_price),
        )
    }

    /// Cost of `usage` on `model` for an instance whose cost model is `basis`:
    /// a free endpoint costs zero and says so, a subscription has no marginal
    /// cost, anything else is priced from the table, and `None` without a price.
    pub fn cost(&self, model: &str, usage: &Usage, basis: CostBasis) -> Cost {
        match basis {
            CostBasis::Free => Cost::free(),
            CostBasis::Subscription => Cost {
                usd: Some(0.0),
                basis: CostBasis::Subscription,
            },
            _ => match self.cost_usd(model, usage) {
                Some(usd) => Cost {
                    usd: Some(usd),
                    basis: CostBasis::Priced,
                },
                None => Cost::unknown(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> ModelPrice {
        ModelPrice {
            input_per_mtok: 2.0,
            output_per_mtok: 10.0,
            cache_read_per_mtok: Some(0.5),
            cache_write_per_mtok: None,
        }
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            ..Usage::default()
        }
    }

    #[test]
    fn empty_table_prices_nothing() {
        let table = PriceTable::default();
        assert!(table.is_empty());
        let cost = table.cost("any", &usage(1000, 1000), CostBasis::Priced);
        assert_eq!(cost.usd, None);
        assert_eq!(cost.basis, CostBasis::Unknown);
    }

    #[test]
    fn model_without_price_is_none_not_zero() {
        let table = PriceTable::new().with("known", price());
        assert_eq!(table.cost_usd("other", &usage(10, 10)), None);
        assert_eq!(
            table.cost("other", &usage(10, 10), CostBasis::Reported).usd,
            None
        );
    }

    #[test]
    fn cost_is_usage_times_price() {
        let table = PriceTable::new().with("m", price());
        let mut u = usage(1_000_000, 500_000);
        u.cache_read_tokens = Some(2_000_000);
        // 1M*2 + 0.5M*10 + 2M*0.5 = 2 + 5 + 1
        let cost = table.cost("m", &u, CostBasis::Priced);
        assert!((cost.usd.unwrap() - 8.0).abs() < 1e-9);
        assert_eq!(cost.basis, CostBasis::Priced);
    }

    #[test]
    fn cache_write_falls_back_to_the_input_price() {
        let table = PriceTable::new().with("m", price());
        let mut u = usage(0, 0);
        u.cache_creation_tokens = Some(1_000_000);
        assert!((table.cost_usd("m", &u).unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_token_counts_give_none() {
        let table = PriceTable::new().with("m", price());
        assert_eq!(table.cost_usd("m", &Usage::default()), None);
        let mut half = usage(10, 10);
        half.output_tokens = None;
        assert_eq!(table.cost_usd("m", &half), None);
    }

    #[test]
    fn free_and_subscription_bases_do_not_need_a_price() {
        let table = PriceTable::new();
        let free = table.cost("m", &Usage::default(), CostBasis::Free);
        assert_eq!((free.usd, free.basis), (Some(0.0), CostBasis::Free));
        let sub = table.cost("m", &Usage::default(), CostBasis::Subscription);
        assert_eq!((sub.usd, sub.basis), (Some(0.0), CostBasis::Subscription));
    }

    #[test]
    fn table_serialises_as_a_plain_map() {
        let table = PriceTable::new().with("m", price());
        let json = serde_json::to_value(&table).unwrap();
        assert_eq!(json["m"]["input_per_mtok"], 2.0);
        assert_eq!(serde_json::from_value::<PriceTable>(json).unwrap(), table);
    }
}
