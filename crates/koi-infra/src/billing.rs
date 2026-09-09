//! 模型调用用量与费用计算。
//!
//! 核心事件只保存供应商返回的原始 Token 用量；价格属于部署配置，因此放在基础设施层
//! 计算。这样既能保留每次调用的可审计用量，也不会让 koi-core 依赖某个价格表或货币。

use thiserror::Error;

use koi_core::domain::{EventEnvelope, ModelEvent, Usage, UsageTotals};

/// 一百万 Token 的价格，单位为美元。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BillingPricing {
    pub input_price_per_million_tokens: f64,
    pub cached_input_price_per_million_tokens: f64,
    pub output_price_per_million_tokens: f64,
}

impl Default for BillingPricing {
    fn default() -> Self {
        Self {
            input_price_per_million_tokens: 0.0,
            cached_input_price_per_million_tokens: 0.0,
            output_price_per_million_tokens: 0.0,
        }
    }
}

impl BillingPricing {
    /// 校验配置中的价格是否可以安全用于费用计算。
    ///
    /// 价格允许为零，便于本地测试或免费模型；不允许负数、NaN 和无穷大。
    pub fn validate(self) -> Result<Self, BillingPricingError> {
        for (name, value) in [
            (
                "input_price_per_million_tokens",
                self.input_price_per_million_tokens,
            ),
            (
                "cached_input_price_per_million_tokens",
                self.cached_input_price_per_million_tokens,
            ),
            (
                "output_price_per_million_tokens",
                self.output_price_per_million_tokens,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(BillingPricingError::InvalidPrice { field: name, value });
            }
        }
        Ok(self)
    }

    /// 计算一次模型调用的美元成本。
    ///
    /// 缓存命中 Token 是输入 Token 的子集，因此普通输入价格只应用于未命中的部分。
    #[must_use]
    pub fn cost_for_usage(self, usage: &Usage) -> f64 {
        let cached_input_tokens = usage
            .cached_input_tokens
            .unwrap_or_default()
            .min(usage.input_tokens);
        let uncached_input_tokens = usage.input_tokens - cached_input_tokens;
        cost(
            uncached_input_tokens,
            cached_input_tokens,
            usage.output_tokens,
            self,
        )
    }

    /// 计算一组累计用量的美元成本。
    #[must_use]
    pub fn cost_for_totals(self, totals: &UsageTotals) -> f64 {
        let cached_input_tokens = totals.cached_input_tokens.min(totals.input_tokens);
        let uncached_input_tokens = totals.input_tokens - cached_input_tokens;
        cost(
            uncached_input_tokens,
            cached_input_tokens,
            totals.output_tokens,
            self,
        )
    }
}

/// 从事件中提取模型调用完成时供应商返回的用量。
#[must_use]
pub fn usage_from_event(event: &EventEnvelope) -> Option<&Usage> {
    let koi_core::domain::AgentEvent::Model(model) = &event.payload else {
        return None;
    };
    match model.as_ref() {
        ModelEvent::Completed { usage, .. } => Some(usage),
        ModelEvent::CallStarted { .. } | ModelEvent::Delta { .. } | ModelEvent::Failed { .. } => {
            None
        }
    }
}

/// 汇总事件流中已经完成的模型调用用量。
#[must_use]
pub fn totals_from_events<'a>(events: impl IntoIterator<Item = &'a EventEnvelope>) -> UsageTotals {
    let mut totals = UsageTotals::default();
    for event in events {
        if let Some(usage) = usage_from_event(event) {
            totals.add_usage(usage);
        }
    }
    totals
}

fn cost(
    uncached_input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    pricing: BillingPricing,
) -> f64 {
    (uncached_input_tokens as f64 * pricing.input_price_per_million_tokens
        + cached_input_tokens as f64 * pricing.cached_input_price_per_million_tokens
        + output_tokens as f64 * pricing.output_price_per_million_tokens)
        / 1_000_000.0
}

#[derive(Debug, Error, PartialEq)]
pub enum BillingPricingError {
    #[error("价格配置 {field} 无效：{value}")]
    InvalidPrice { field: &'static str, value: f64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_input_uses_cached_price_and_is_not_double_charged() {
        let pricing = BillingPricing {
            input_price_per_million_tokens: 2.0,
            cached_input_price_per_million_tokens: 0.5,
            output_price_per_million_tokens: 8.0,
        };
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 250_000,
            cached_input_tokens: Some(400_000),
            reasoning_tokens: None,
        };

        let cost = pricing.cost_for_usage(&usage);
        assert!((cost - 3.4).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_prices_are_rejected() {
        let pricing = BillingPricing {
            output_price_per_million_tokens: -1.0,
            ..BillingPricing::default()
        };
        assert!(matches!(
            pricing.validate(),
            Err(BillingPricingError::InvalidPrice {
                field: "output_price_per_million_tokens",
                ..
            })
        ));
    }
}
