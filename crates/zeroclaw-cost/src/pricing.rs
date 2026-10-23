use zeroclaw_api::cost::TokenUsage;

fn sanitize_price(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

/// Price one request using the resolved rates, in USD per million tokens.
/// Cached input is a subset of input; an absent cache rate uses the input rate.
pub fn calculate_token_usage(
    model: impl Into<String>,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    input_price_per_million: f64,
    output_price_per_million: f64,
    cached_input_price_per_million: f64,
) -> TokenUsage {
    let model = model.into();
    let input_price_per_million = sanitize_price(input_price_per_million);
    let output_price_per_million = sanitize_price(output_price_per_million);
    let cached_input_price_per_million = sanitize_price(cached_input_price_per_million);
    let cached_input_tokens = cached_input_tokens.min(input_tokens);
    let billable_uncached_input = input_tokens.saturating_sub(cached_input_tokens);
    let total_tokens = input_tokens.saturating_add(output_tokens);

    // Calculate cost: (tokens / 1M) * price_per_million for each band.
    // Cached subset uses its own rate when set, else falls back to the
    // standard input rate so providers without a cache-rate aren't
    // charged $0 for the cached portion.
    let cached_rate = if cached_input_price_per_million > 0.0 {
        cached_input_price_per_million
    } else {
        input_price_per_million
    };
    let input_cost = (billable_uncached_input as f64 / 1_000_000.0) * input_price_per_million;
    let cached_cost = (cached_input_tokens as f64 / 1_000_000.0) * cached_rate;
    let output_cost = (output_tokens as f64 / 1_000_000.0) * output_price_per_million;
    let cost_usd = input_cost + cached_cost + output_cost;

    TokenUsage {
        model,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        total_tokens,
        cost_usd,
        pricing_available: true,
        timestamp: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_calculation() {
        let usage = calculate_token_usage("test/model", 1000, 500, 0, 3.0, 15.0, 0.0);

        // Expected: (1000/1M)*3 + (500/1M)*15 = 0.003 + 0.0075 = 0.0105
        assert!((usage.cost_usd - 0.0105).abs() < 0.0001);
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.cached_input_tokens, 0);
        assert_eq!(usage.billable_input_tokens(), 1000);
        assert_eq!(usage.output_tokens, 500);
        assert_eq!(usage.total_tokens, 1500);
    }

    #[test]
    fn token_usage_cached_input_billed_at_cached_rate() {
        // 200 cached input @ 0.5/Mtok + 800 uncached input @ 3/Mtok + 500 output @ 15/Mtok
        // = (200/1e6)*0.5 + (800/1e6)*3 + (500/1e6)*15
        // = 0.0001 + 0.0024 + 0.0075 = 0.01
        let usage = calculate_token_usage("test/model", 1000, 500, 200, 3.0, 15.0, 0.5);
        assert!((usage.cost_usd - 0.01).abs() < 1e-6, "{}", usage.cost_usd);
        assert_eq!(usage.cached_input_tokens, 200);
    }

    #[test]
    fn token_usage_zero_cached_rate_falls_back_to_input_rate() {
        // Cached rate 0 means "no discount" — bill cached subset at the
        // standard input rate so providers without a published cache rate
        // still produce a sane total.
        let with_cache = calculate_token_usage("test/model", 1000, 500, 200, 3.0, 15.0, 0.0);
        let without_cache = calculate_token_usage("test/model", 1000, 500, 0, 3.0, 15.0, 0.0);
        assert!((with_cache.cost_usd - without_cache.cost_usd).abs() < 1e-9);
    }

    #[test]
    fn token_usage_cache_aware_calculation() {
        let usage = calculate_token_usage("test/model", 1_000, 500, 800, 3.0, 15.0, 0.3);

        // Expected: uncached=(200/1M)*3 + cached=(800/1M)*0.3 + output=(500/1M)*15
        let expected = 0.0006 + 0.00024 + 0.0075;
        assert!((usage.cost_usd - expected).abs() < 0.0001);
        assert_eq!(usage.cached_input_tokens, 800);
        assert_eq!(usage.billable_input_tokens(), 200);
        assert_eq!(usage.total_tokens, 1500);
    }

    #[test]
    fn token_usage_zero_tokens() {
        let usage = calculate_token_usage("test/model", 0, 0, 0, 3.0, 15.0, 0.0);
        assert!(usage.cost_usd.abs() < f64::EPSILON);
        assert_eq!(usage.total_tokens, 0);
    }

    #[test]
    fn token_usage_negative_or_non_finite_prices_are_clamped() {
        let usage =
            calculate_token_usage("test/model", 1000, 1000, 0, -3.0, f64::NAN, f64::INFINITY);
        assert!(usage.cost_usd.abs() < f64::EPSILON);
        assert_eq!(usage.total_tokens, 2000);
    }
}
