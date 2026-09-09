use mcp_agent::cli::Command;

fn main() -> anyhow::Result<()> {
    mcp_agent_authority::sandbox::dispatch_internal_sandbox_child()?;
    match Command::parse_env()? {
        Command::Run(cli) => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(mcp_agent::startup::run(cli)),
        command => mcp_agent::enrollment::run(&command),
    }
}
