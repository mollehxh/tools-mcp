fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    match command.as_str() {
        "upstream-verify" => xtask::upstream::verify(),
        "conformance" => xtask::conformance::run(),
        "transport-spike" => xtask::transport_spike::run(),
        "transport-spike-serve" => {
            let bind = args.next().unwrap_or_else(|| "127.0.0.1:3000".to_owned());
            let public_host = args.next();
            anyhow::ensure!(
                args.next().is_none(),
                "usage: cargo run -p xtask -- transport-spike-serve [ADDRESS] [PUBLIC_HOST]"
            );
            xtask::transport_spike::serve(&bind, public_host.as_deref())
        }
        _ => anyhow::bail!(
            "expected `upstream-verify`, `conformance`, `transport-spike`, or `transport-spike-serve`"
        ),
    }
}
