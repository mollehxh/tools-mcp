use anyhow::{Context as _, Result, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher as _, SaltString};
use mcp_agent_gateway::{AuthConfig, AuthStore, DeviceAuthority, PendingDeviceEnrollment};
use mcp_agent_relay::certificate_fingerprint;
use rand::RngCore as _;
use rustls_pki_types::CertificateDer;
use sha2::{Digest as _, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn main() -> Result<()> {
    if !cfg!(unix) {
        bail!("tools-mcp-admin is supported only on the Linux gateway host")
    }
    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        Some("hash-owner-secret") => {
            let source = required_argument(&mut arguments, "secret file")?;
            let destination = required_argument(&mut arguments, "hash file")?;
            ensure_finished(arguments)?;
            hash_owner_secret(Path::new(&source), Path::new(&destination))
        }
        Some("init") => {
            ensure_finished(arguments)?;
            auth_store().map(drop)
        }
        Some("approve-device") => {
            let certificate = required_argument(&mut arguments, "certificate")?;
            let device_id = required_argument(&mut arguments, "device id")?;
            let platform = required_argument(&mut arguments, "platform")?;
            let expires_unix = required_argument(&mut arguments, "expiry")?.parse()?;
            ensure_finished(arguments)?;
            approve_device(Path::new(&certificate), device_id, platform, expires_unix)
        }
        Some("approve-device-csr") => {
            let csr = required_argument(&mut arguments, "CSR")?;
            let certificate = required_argument(&mut arguments, "certificate output")?;
            let device_id = required_argument(&mut arguments, "device id")?;
            let platform = required_argument(&mut arguments, "platform")?;
            let expires_unix = required_argument(&mut arguments, "expiry")?.parse()?;
            ensure_finished(arguments)?;
            approve_device_csr(
                Path::new(&csr),
                Path::new(&certificate),
                device_id,
                platform,
                expires_unix,
            )
        }
        Some("revoke-device") => {
            let device_id = required_argument(&mut arguments, "device id")?;
            ensure_finished(arguments)?;
            if !auth_store()?.revoke_device(&device_id)? {
                bail!("device was absent or already revoked")
            }
            Ok(())
        }
        Some("revoke-grant") => {
            let grant_id = required_argument(&mut arguments, "grant id")?;
            ensure_finished(arguments)?;
            if !auth_store()?.revoke_grant(&grant_id)? {
                bail!("grant was absent or already revoked")
            }
            Ok(())
        }
        Some("stale-restore-revoke-all") => {
            ensure_finished(arguments)?;
            let store = auth_store()?;
            store.revoke_all().context("revoke OAuth families")?;
            store
                .revoke_all_devices()
                .context("revoke relay identities")
        }
        _ => bail!(
            "usage: tools-mcp-admin <hash-owner-secret|init|approve-device|approve-device-csr|revoke-device|revoke-grant|stale-restore-revoke-all>"
        ),
    }
}

fn hash_owner_secret(source: &Path, destination: &Path) -> Result<()> {
    let secret_bytes = std::fs::read(source).context("read owner secret")?;
    let secret = secret_bytes.strip_suffix(b"\n").unwrap_or(&secret_bytes);
    let secret = secret.strip_suffix(b"\r").unwrap_or(secret);
    if secret.len() < 32 || secret.len() > 256 {
        bail!("owner secret must contain 32 to 256 bytes")
    }
    let mut salt = [0_u8; 16];
    rand::rng().fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt)
        .map_err(|error| anyhow::anyhow!("encode Argon2 salt: {error}"))?;
    let hash = Argon2::default()
        .hash_password(secret, &salt)
        .map_err(|error| anyhow::anyhow!("hash owner secret: {error}"))?
        .to_string();
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .context("create owner hash file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        output.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    output.write_all(hash.as_bytes())?;
    output.write_all(b"\n")?;
    Ok(())
}

fn approve_device(
    certificate_path: &Path,
    device_id: String,
    platform: String,
    expires_unix: i64,
) -> Result<()> {
    if platform != "linux-vps" {
        bail!("approve-device is reserved for the host-local VPS bootstrap; use approve-device-csr")
    }
    let certificate = first_certificate(certificate_path)?;
    auth_store()?.approve_device(&DeviceAuthority {
        owner_id: "owner".to_owned(),
        device_id,
        platform,
        certificate_fingerprint: certificate_fingerprint(&certificate),
        expires_unix,
    })?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep the approval ceremony linear and auditable.
fn approve_device_csr(
    csr_path: &Path,
    certificate_path: &Path,
    device_id: String,
    platform: String,
    expires_unix: i64,
) -> Result<()> {
    if !valid_device_id(&device_id) || !matches!(platform.as_str(), "macos" | "windows") {
        bail!("device id or local platform is invalid")
    }
    if certificate_path.exists() {
        bail!("certificate output already exists")
    }
    let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let lifetime = expires_unix
        .checked_sub(now)
        .context("device expiry overflow")?;
    if lifetime <= 0 || lifetime > 90 * 24 * 60 * 60 {
        bail!("device expiry must be within the next 90 days")
    }
    let issuer_cert = required_path("TOOLS_MCP_DEVICE_CA_ISSUER_CERT")?;
    let issuer_key = required_path("TOOLS_MCP_DEVICE_CA_ISSUER_KEY")?;
    require_root_private_key(&issuer_key)?;
    let temporary = tempfile::tempdir().context("create CSR approval workspace")?;
    let csr_der = temporary.path().join("request.der");
    let csr_public_pem = temporary.path().join("request-public.pem");
    let csr_public_der = temporary.path().join("request-public.der");
    openssl(
        Command::new("openssl")
            .args(["req", "-verify", "-noout", "-in"])
            .arg(csr_path),
        "verify CSR signature",
    )?;
    openssl(
        Command::new("openssl")
            .args(["req", "-in"])
            .arg(csr_path)
            .args(["-outform", "DER", "-out"])
            .arg(&csr_der),
        "normalize CSR",
    )?;
    openssl(
        Command::new("openssl")
            .args(["req", "-in"])
            .arg(csr_path)
            .args(["-pubkey", "-noout", "-out"])
            .arg(&csr_public_pem),
        "extract CSR public key",
    )?;
    normalize_public_key(&csr_public_pem, &csr_public_der)?;
    let csr_fingerprint = file_sha256(&csr_der)?;
    let public_key_fingerprint = file_sha256(&csr_public_der)?;
    let pending = PendingDeviceEnrollment {
        owner_id: "owner".to_owned(),
        device_id: device_id.clone(),
        platform: platform.clone(),
        csr_fingerprint: csr_fingerprint.clone(),
        public_key_fingerprint: public_key_fingerprint.clone(),
        expires_unix,
    };
    let store = auth_store()?;
    store.begin_device_enrollment(&pending)?;

    let extensions = temporary.path().join("device.ext");
    std::fs::write(
        &extensions,
        format!(
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\nsubjectAltName=URI:urn:tools-mcp:owner:owner,URI:urn:tools-mcp:device:{device_id},URI:urn:tools-mcp:platform:{platform}\n"
        ),
    )?;
    let signed = temporary.path().join("device.pem");
    let days = (lifetime + 86_399) / 86_400;
    openssl(
        Command::new("openssl")
            .args(["x509", "-req", "-sha256", "-in"])
            .arg(csr_path)
            .args(["-CA"])
            .arg(&issuer_cert)
            .args(["-CAkey"])
            .arg(&issuer_key)
            .args(["-CAcreateserial", "-days", &days.to_string(), "-extfile"])
            .arg(&extensions)
            .args(["-subj", &format!("/CN={device_id}"), "-out"])
            .arg(&signed),
        "sign device CSR",
    )?;
    openssl(
        Command::new("openssl")
            .args(["verify", "-purpose", "sslclient", "-CAfile"])
            .arg(&issuer_cert)
            .arg(&signed),
        "verify signed device certificate",
    )?;
    let certificate_public_pem = temporary.path().join("certificate-public.pem");
    let certificate_public_der = temporary.path().join("certificate-public.der");
    openssl(
        Command::new("openssl")
            .args(["x509", "-in"])
            .arg(&signed)
            .args(["-pubkey", "-noout", "-out"])
            .arg(&certificate_public_pem),
        "extract certificate public key",
    )?;
    normalize_public_key(&certificate_public_pem, &certificate_public_der)?;
    if file_sha256(&certificate_public_der)? != public_key_fingerprint {
        bail!("signed certificate public key does not match the approved CSR")
    }
    let certificate = first_certificate(&signed)?;
    let authority = DeviceAuthority {
        owner_id: "owner".to_owned(),
        device_id,
        platform,
        certificate_fingerprint: certificate_fingerprint(&certificate),
        expires_unix,
    };
    store.complete_device_enrollment(&csr_fingerprint, &public_key_fingerprint, &authority)?;
    let mut destination = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(certificate_path)
        .context("create approved certificate")?;
    destination.write_all(&std::fs::read(&signed)?)?;
    Ok(())
}

fn openssl(command: &mut Command, action: &str) -> Result<()> {
    let status = command.status().with_context(|| action.to_owned())?;
    if !status.success() {
        bail!("{action} failed")
    }
    Ok(())
}

fn normalize_public_key(source: &Path, destination: &Path) -> Result<()> {
    openssl(
        Command::new("openssl")
            .args(["pkey", "-pubin", "-in"])
            .arg(source)
            .args(["-outform", "DER", "-out"])
            .arg(destination),
        "normalize public key",
    )
}

fn file_sha256(path: &Path) -> Result<String> {
    Ok(Sha256::digest(std::fs::read(path)?).iter().fold(
        String::with_capacity(64),
        |mut output, byte| {
            use std::fmt::Write as _;
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        },
    ))
}

fn valid_device_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn require_root_private_key(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = path.metadata().context("read device CA key metadata")?;
        if metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
            bail!("device CA key must be root-owned and inaccessible to group/other")
        }
    }
    Ok(())
}

fn auth_store() -> Result<AuthStore> {
    let issuer = required_env("TOOLS_MCP_ISSUER")?;
    let client_id = required_env("TOOLS_MCP_CLIENT_ID")?;
    let owner_hash = std::fs::read_to_string(required_path("TOOLS_MCP_OWNER_HASH_FILE")?)
        .context("read owner hash")?;
    AuthStore::open(
        &required_path("TOOLS_MCP_DATABASE")?,
        AuthConfig {
            client_id,
            resource: format!("{}/mcp", issuer.trim_end_matches('/')),
            owner_secret_phc: owner_hash.trim().to_owned(),
            token_hash_key: std::fs::read(required_path("TOOLS_MCP_TOKEN_HASH_KEY_FILE")?)
                .context("read OAuth token hash key")?,
            access_lifetime: Duration::from_mins(5),
            refresh_lifetime: Duration::from_hours(24 * 30),
        },
    )
    .context("open gateway authentication state")
}

fn first_certificate(path: &Path) -> Result<CertificateDer<'static>> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::certs(&mut reader)
        .next()
        .transpose()?
        .context("PEM certificate file is empty")
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("required environment variable {name} is missing"))
}

fn required_path(name: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(required_env(name)?))
}

fn required_argument(arguments: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    arguments.next().with_context(|| format!("missing {name}"))
}

fn ensure_finished(mut arguments: impl Iterator<Item = String>) -> Result<()> {
    if arguments.next().is_some() {
        bail!("unexpected extra argument")
    }
    Ok(())
}
