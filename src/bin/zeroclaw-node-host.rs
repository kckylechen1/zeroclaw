#[path = "../node_host.rs"]
mod node_host;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    node_host::run().await
}
