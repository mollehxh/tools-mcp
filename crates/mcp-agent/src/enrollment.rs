use crate::cli::Command;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use anyhow::Context as _;
use anyhow::{Result, bail};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use mcp_agent_authority::release::{verify_release, verify_release_assets};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::fs::{File, OpenOptions};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::io::{BufReader, Write as _};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::path::{Path, PathBuf};

/// Executes a human-only local device enrollment command.
///
/// # Errors
///
/// Fails closed on unsupported platforms, invalid release assets, an existing
/// output file, malformed certificate data, or native Keychain failure.
pub fn run(command: &Command) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        run_macos(command)
    }
    #[cfg(target_os = "windows")]
    {
        run_windows(command)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = command;
        bail!("local device enrollment is not implemented for this platform")
    }
}

#[cfg(target_os = "macos")]
fn run_macos(command: &Command) -> Result<()> {
    match command {
        Command::EnrollDevice {
            device_id,
            csr_output,
            release_dir,
        } => {
            let release = verified_release(release_dir.as_deref())?;
            let helper = release.join("tools-mcp-keygen");
            let (csr, label) = crate::macos_keychain::generate_csr(device_id, &helper)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(csr_output)
                .with_context(|| format!("create CSR output {}", csr_output.display()))?;
            output
                .write_all(csr.as_bytes())
                .context("write device CSR")?;
            output.sync_all().context("sync device CSR")?;
            eprintln!(
                "created non-exportable device key {label}; approve {} over SSH",
                csr_output.display()
            );
            Ok(())
        }
        Command::InstallDeviceCertificate {
            device_id,
            certificate,
            release_dir: _,
        } => {
            let label = crate::macos_keychain::keychain_label(device_id)?;
            let leaf = first_certificate(certificate)?;
            crate::macos_keychain::install_certificate(leaf.as_ref(), &label)?;
            eprintln!(
                "installed certificate for {device_id}; keep {} as the relay certificate chain",
                certificate.display()
            );
            Ok(())
        }
        Command::Run(_) => bail!("run mode is not an enrollment command"),
    }
}

#[cfg(target_os = "windows")]
fn run_windows(command: &Command) -> Result<()> {
    match command {
        Command::EnrollDevice {
            device_id,
            csr_output,
            release_dir,
        } => {
            let release = verified_release(release_dir.as_deref())?;
            let helper = release.join("tools-mcp-keygen.exe");
            let (csr, key_name) = crate::windows_cng::generate_csr(device_id, &helper)?;
            write_new_csr(csr_output, &csr)?;
            eprintln!(
                "created non-exportable device key {key_name}; approve {} over SSH",
                csr_output.display()
            );
            Ok(())
        }
        Command::InstallDeviceCertificate {
            device_id,
            certificate,
            release_dir,
        } => {
            let release = verified_release(release_dir.as_deref())?;
            let helper = release.join("tools-mcp-keygen.exe");
            let key_name = crate::windows_cng::key_name(device_id)?;
            let chain = certificates(certificate)?;
            crate::windows_cng::certified_key(chain, &key_name, &helper)?;
            eprintln!(
                "validated certificate for {device_id}; keep {} as the relay certificate chain",
                certificate.display()
            );
            Ok(())
        }
        Command::Run(_) => bail!("run mode is not an enrollment command"),
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn verified_release(configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(release) = configured {
        let release = release
            .canonicalize()
            .context("configured release directory is unavailable")?;
        verify_release_assets(&release, env!("CARGO_PKG_VERSION"))
            .context("enrollment helper release verification failed")?;
        return Ok(release);
    }
    let executable = std::env::current_exe().context("executable path is unavailable")?;
    let release = executable
        .parent()
        .map(Path::to_path_buf)
        .context("executable has no release directory")?;
    verify_release(&release, &executable, env!("CARGO_PKG_VERSION"))
        .context("installed enrollment release verification failed")?;
    Ok(release)
}

#[cfg(target_os = "macos")]
fn first_certificate(path: &Path) -> Result<rustls_pki_types::CertificateDer<'static>> {
    let mut reader = BufReader::new(
        File::open(path).with_context(|| format!("open certificate {}", path.display()))?,
    );
    rustls_pemfile::certs(&mut reader)
        .next()
        .transpose()
        .context("read PEM certificate")?
        .context("certificate file is empty")
}

#[cfg(target_os = "windows")]
fn certificates(path: &Path) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>> {
    let mut reader = BufReader::new(
        File::open(path).with_context(|| format!("open certificate {}", path.display()))?,
    );
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<std::io::Result<Vec<_>>>()
        .context("read PEM certificate chain")?;
    if certificates.is_empty() {
        bail!("certificate file is empty")
    }
    Ok(certificates)
}

#[cfg(target_os = "windows")]
fn write_new_csr(path: &Path, csr: &str) -> Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create CSR output {}", path.display()))?;
    output
        .write_all(csr.as_bytes())
        .context("write device CSR")?;
    output.sync_all().context("sync device CSR")
}
