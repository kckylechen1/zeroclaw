//! Security component tests.

use zeroclaw::config::Config;

// ═════════════════════════════════════════════════════════════════════════════
// Autonomy configuration defaults and validation
// ═════════════════════════════════════════════════════════════════════════════

// ═════════════════════════════════════════════════════════════════════════════
// Security configuration
// ═════════════════════════════════════════════════════════════════════════════

// ═════════════════════════════════════════════════════════════════════════════
// Autonomy level serialization round-trip
// ═════════════════════════════════════════════════════════════════════════════

// ═════════════════════════════════════════════════════════════════════════════
// Credential pattern validation (via config/schema)
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn security_config_secret_property_readback_masks_api_key() {
    let mut config = Config::default();
    let path = "providers.models.openrouter.default.api_key";
    let secret = "sk-1234567890abcdef";

    assert!(
        Config::prop_is_secret(path),
        "{path} should be classified as a secret config property"
    );
    config
        .providers
        .models
        .ensure("openrouter", "default")
        .expect("openrouter provider entry should be creatable");
    config
        .set_prop(path, secret)
        .expect("secret config property should be settable");

    let readback = config
        .get_prop(path)
        .expect("secret config property should be readable");
    assert_ne!(
        readback, secret,
        "secret config property readback must not expose the raw API key"
    );
    assert!(
        readback.contains("****"),
        "secret config property readback should use a masked placeholder, got {readback:?}"
    );
}
