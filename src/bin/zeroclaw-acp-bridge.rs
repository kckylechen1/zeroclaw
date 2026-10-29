//! Bounded ACP v1 stdio projection of the gateway Conversation, not an Agent.
#[path = "acp_bridge/mod.rs"]
mod acp_bridge;

#[tokio::main]
async fn main() {
    if let Err(code) = acp_bridge::run().await {
        eprintln!("{code}");
        std::process::exit(1);
    }
}
