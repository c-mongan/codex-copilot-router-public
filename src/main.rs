#[tokio::main]
async fn main() -> anyhow::Result<()> {
    codex_code_router::cli::run().await
}
