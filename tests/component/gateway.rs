//! Gateway component tests.

// ═════════════════════════════════════════════════════════════════════════════
// Gateway constants and configuration validation
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn gateway_rate_limit_window_is_60s() {
    assert_eq!(
        zeroclaw::gateway::RATE_LIMIT_WINDOW_SECS,
        60,
        "Rate limit window should be 60 seconds"
    );
}
