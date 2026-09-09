fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    match command.as_str() {
        "upstream-verify" => xtask::upstream::verify(),
        "conformance" => xtask::conformance::run(),
        "inspector-smoke" => xtask::inspector::run(),
        "package" => xtask::package::run(),
        "npm-package" => xtask::npm_package::run(),
        "oauth-conformance" => xtask::hybrid::oauth_conformance(),
        "hybrid-smoke" => xtask::hybrid::hybrid_smoke(),
        "vps-smoke" => xtask::hybrid::vps_smoke(),
        "vps-package" => xtask::vps_package::run(),
        "transport-spike" => xtask::transport_spike::run(),
        "transport-spike-verify-chatgpt" => {
            let count = xtask::transport_spike::verify_chatgpt_checkpoint()?;
            println!("verified ChatGPT checkpoint with {count} tools");
            Ok(())
        }
        "transport-spike-serve" => {
            let bind = args.next().unwrap_or_else(|| "127.0.0.1:3000".to_owned());
            let public_host = args.next();
            anyhow::ensure!(
                args.next().is_none(),
                "usage: cargo run -p xtask -- transport-spike-serve [ADDRESS] [PUBLIC_HOST]"
            );
            xtask::transport_spike::serve(&bind, public_host.as_deref())
        }
        "transport-spike-oauth-serve" => {
            let bind = args.next().unwrap_or_else(|| "0.0.0.0:8443".to_owned());
            let issuer = args
                .next()
                .unwrap_or_else(|| "https://vpn.play2go.cloud:8443".to_owned());
            let certificate = args.next().map(std::path::PathBuf::from).ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: cargo run -p xtask -- transport-spike-oauth-serve [ADDRESS] [ISSUER] CERT_PEM KEY_PEM"
                )
            })?;
            let private_key = args.next().map(std::path::PathBuf::from).ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: cargo run -p xtask -- transport-spike-oauth-serve [ADDRESS] [ISSUER] CERT_PEM KEY_PEM"
                )
            })?;
            anyhow::ensure!(
                args.next().is_none(),
                "usage: cargo run -p xtask -- transport-spike-oauth-serve [ADDRESS] [ISSUER] CERT_PEM KEY_PEM"
            );
            xtask::transport_spike::serve_oauth(&bind, &issuer, &certificate, &private_key)
        }
        _ => anyhow::bail!(
            "expected `upstream-verify`, `conformance`, `package`, `npm-package`, `inspector-smoke`, `oauth-conformance`, `hybrid-smoke`, `vps-smoke`, `vps-package`, `transport-spike`, `transport-spike-verify-chatgpt`, `transport-spike-serve`, or `transport-spike-oauth-serve`"
        ),
    }
}
