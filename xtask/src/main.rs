fn main() -> anyhow::Result<()> {
    let command = std::env::args().nth(1).unwrap_or_default();
    match command.as_str() {
        "upstream-verify" => xtask::upstream::verify(),
        "transport-spike" => xtask::transport_spike::run(),
        _ => anyhow::bail!("expected `upstream-verify` or `transport-spike`"),
    }
}
