use zeroclaw_config::schema::{Config, GatewayBridgeConfig};
use zeroclaw_config::tokens::hash_token;
use zeroclaw_runtime::security::pairing::{PairingGuard, generate_bridge_token};

#[test]
fn persisted_token_hashes_cross_config_and_runtime_owners() {
    const TOKEN: &str = "zc_pairing_contract_fixture_only";
    // Independently calculated with Python hashlib.sha256 over the literal above.
    const HISTORICAL_HASH: &str =
        "268099d88852e81cfbc7b1861b4e5a035686f7364609e8351f3b0d891da2e116";

    assert_eq!(hash_token(TOKEN), HISTORICAL_HASH);
    assert_eq!(PairingGuard::token_hash(TOKEN), HISTORICAL_HASH);

    let mut config: Config = toml::from_str(
        r#"
schema_version = 2

[gateway]
require_pairing = true
paired_tokens = ["268099d88852e81cfbc7b1861b4e5a035686f7364609e8351f3b0d891da2e116"]
"#,
    )
    .expect("historical prehashed configuration");
    let bridge_token = generate_bridge_token();
    config.gateway.bridges.insert(
        "fixture_bridge".to_string(),
        GatewayBridgeConfig {
            token_hash: hash_token(&bridge_token),
            session_prefix: Some("fixture:".to_string()),
            sessions: vec!["fixture-session".to_string()],
        },
    );

    let serialized = toml::to_string(&config).expect("serialize configured token hashes");
    assert!(!serialized.contains(TOKEN));
    assert!(!serialized.contains(&bridge_token));
    let restored: Config = toml::from_str(&serialized).expect("restore configured token hashes");
    let guard = PairingGuard::new(
        restored.gateway.require_pairing,
        &restored.gateway.paired_tokens,
    );

    assert_eq!(
        guard.authenticate_and_hash(TOKEN).as_deref(),
        Some(HISTORICAL_HASH)
    );
    assert!(!guard.is_authenticated(HISTORICAL_HASH));
    assert!(!guard.is_authenticated(&bridge_token));

    let (name, bridge) = restored
        .gateway
        .bridge_for_token(&bridge_token)
        .expect("runtime-minted bridge token resolves in restored config");
    assert_eq!(name, "fixture_bridge");
    assert!(bridge.allows_session("fixture-session"));
    assert!(bridge.allows_session("fixture:conversation"));
    assert!(!bridge.allows_session("outside-fixture"));
    assert!(restored.gateway.bridge_for_token(TOKEN).is_none());
}

#[tokio::test]
async fn guard_clones_share_lockout_code_consumption_and_revocation() {
    let guard = PairingGuard::new(true, &[]);
    let peer = guard.clone();
    let code = guard.pairing_code().expect("one pending pairing code");

    for _ in 0..4 {
        assert_eq!(
            guard
                .try_pair("invalid-code", "fixture-locked-client")
                .await,
            Ok(None)
        );
    }
    assert_eq!(
        peer.try_pair("invalid-code", "fixture-locked-client").await,
        Ok(None)
    );
    assert!(
        guard
            .try_pair(&code, "fixture-locked-client")
            .await
            .is_err()
    );

    let token = peer
        .try_pair(&code, "fixture-other-client")
        .await
        .expect("another client is not locked out")
        .expect("the shared code can still be redeemed by another client");
    let token_hash = hash_token(&token);
    assert!(guard.pairing_code().is_none());
    assert_eq!(
        guard.authenticate_and_hash(&token),
        Some(token_hash.clone())
    );
    assert_eq!(peer.tokens(), guard.tokens());

    assert!(peer.revoke_token_hash(&token_hash));
    assert!(!guard.is_authenticated(&token));
    assert!(guard.tokens().is_empty());
}
