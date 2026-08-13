use mcp_agent::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse_env()?;
    mcp_agent::startup::run(cli).await
}
