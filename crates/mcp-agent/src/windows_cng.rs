use anyhow::{Context as _, Result, bail};
use rustls::pki_types::SubjectPublicKeyInfoDer;
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{Error, SignatureAlgorithm, SignatureScheme};
use rustls_pki_types::CertificateDer;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

const KEY_PREFIX: &str = "tools-mcp-device:";
const MAX_HELPER_OUTPUT: usize = 64 * 1024;
const MAX_SIGNATURE_BYTES: usize = 80;

/// Returns the persistent CNG key name for a validated device identifier.
///
/// # Errors
///
/// Fails when `device_id` is not a bounded portable identifier.
pub fn key_name(device_id: &str) -> Result<String> {
    validate_device_id(device_id)?;
    Ok(format!("{KEY_PREFIX}{device_id}"))
}

/// Generates a non-exportable CNG P-256 key and its PKCS#10 CSR.
///
/// # Errors
///
/// Fails when the audited native helper rejects key creation or emits an
/// invalid or oversized response.
pub fn generate_csr(device_id: &str, helper: &Path) -> Result<(String, String)> {
    let key_name = key_name(device_id)?;
    let output = helper_output(helper, "generate", &key_name, None, MAX_HELPER_OUTPUT)?;
    let csr = String::from_utf8(output).context("native CNG helper returned a non-UTF-8 CSR")?;
    if !csr.starts_with("-----BEGIN CERTIFICATE REQUEST-----\n")
        || !csr.ends_with("-----END CERTIFICATE REQUEST-----\n")
    {
        bail!("native CNG helper returned a malformed CSR")
    }
    Ok((csr, key_name))
}

/// Builds a rustls client-auth key backed by the non-exportable CNG key.
///
/// # Errors
///
/// Fails when the key is absent/exportable, helper output is malformed, or
/// the certificate leaf does not contain the enrolled key's public key.
pub fn certified_key(
    certificate_chain: Vec<CertificateDer<'static>>,
    key_name: &str,
    helper: &Path,
) -> Result<CertifiedKey> {
    validate_key_name(key_name)?;
    if certificate_chain.is_empty() {
        bail!("device certificate chain is empty")
    }
    let spki = helper_output(helper, "public-key", key_name, None, MAX_HELPER_OUTPUT)?;
    if spki.is_empty() {
        bail!("native CNG helper returned an empty public key")
    }
    let certified = CertifiedKey::new(
        certificate_chain,
        Arc::new(CngSigningKey {
            helper: helper.to_path_buf(),
            key_name: key_name.to_owned(),
            spki,
        }),
    );
    certified
        .keys_match()
        .context("signed certificate does not match the enrolled CNG key")?;
    Ok(certified)
}

fn validate_device_id(device_id: &str) -> Result<()> {
    if device_id.is_empty()
        || device_id.len() > 128
        || !device_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("device ID must contain 1-128 ASCII letters, digits, '-', '_', or '.'")
    }
    Ok(())
}

fn validate_key_name(name: &str) -> Result<()> {
    let device_id = name
        .strip_prefix(KEY_PREFIX)
        .context("invalid tools-mcp CNG key name")?;
    validate_device_id(device_id)
}

fn helper_output(
    helper: &Path,
    command: &str,
    key_name: &str,
    input: Option<&[u8]>,
    maximum: usize,
) -> Result<Vec<u8>> {
    let mut child = Command::new(helper)
        .args([command, key_name])
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch native CNG helper {}", helper.display()))?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("native CNG helper stdin is unavailable")?
            .write_all(input)
            .context("write native CNG signing input")?;
    }
    let output = child
        .wait_with_output()
        .context("wait for native CNG helper")?;
    if !output.status.success() {
        bail!("native CNG helper rejected the operation")
    }
    if output.stdout.len() > maximum {
        bail!("native CNG helper output exceeds its protocol bound")
    }
    Ok(output.stdout)
}

struct CngSigningKey {
    helper: PathBuf,
    key_name: String,
    spki: Vec<u8>,
}

impl fmt::Debug for CngSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CngSigningKey")
            .finish_non_exhaustive()
    }
}

impl SigningKey for CngSigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| {
                Box::new(CngSigner {
                    helper: self.helper.clone(),
                    key_name: self.key_name.clone(),
                }) as Box<dyn Signer>
            })
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(SubjectPublicKeyInfoDer::from(self.spki.as_slice()))
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

struct CngSigner {
    helper: PathBuf,
    key_name: String,
}

impl fmt::Debug for CngSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("CngSigner").finish_non_exhaustive()
    }
}

impl Signer for CngSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let signature = helper_output(
            &self.helper,
            "sign",
            &self.key_name,
            Some(message),
            MAX_SIGNATURE_BYTES,
        )
        .map_err(|_| Error::General("native CNG signature failed".to_owned()))?;
        if signature.is_empty() {
            return Err(Error::General("native CNG signature was empty".to_owned()));
        }
        Ok(signature)
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}
