use mcp_agent::cli::Cli;

fn main() -> anyhow::Result<()> {
    if mcp_agent_authority::sandbox::dispatch_internal_sandbox_child()? {
        unreachable!("sandbox child dispatch replaces the process image");
    }
    let cli = Cli::parse_env()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(mcp_agent::startup::run(cli))
}
