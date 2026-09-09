use anyhow::{Context as _, Result, anyhow, bail};
use rcgen::{CertificateParams, DistinguishedName, DnType};
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{Error, SignatureAlgorithm, SignatureScheme};
use rustls_pki_types::CertificateDer;
use security_framework::certificate::SecCertificate;
use security_framework::item::{
    AddRef, ItemAddOptions, ItemAddValue, ItemSearchOptions, KeyClass, Limit, Location, Reference,
    SearchResult,
};
use security_framework::key::{Algorithm, SecKey};
use std::fmt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

const LABEL_PREFIX: &str = "tools-mcp-device:";
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;

/// Generates a non-exportable P-256 Keychain key and returns a PKCS#10 CSR.
///
/// The audited native helper sets the immutable macOS `kSecAttrIsExtractable`
/// key attribute to false. Its private material therefore never enters Rust,
/// the filesystem, or the enrollment ceremony.
///
/// # Errors
///
/// Fails for an invalid or duplicate device ID, a rejected native helper, or
/// any key generation, public-key encoding, signing, or CSR encoding failure.
pub fn generate_csr(device_id: &str, keygen_helper: &Path) -> Result<(String, String)> {
    validate_device_id(device_id)?;
    let label = format!("{LABEL_PREFIX}{device_id}");
    if find_private_keys(&label)?.next().is_some() {
        bail!("a device key already exists for {device_id}")
    }

    let status = Command::new(keygen_helper)
        .args(["generate", &label])
        .arg(std::env::current_exe().context("resolve trusted tools-mcp executable")?)
        .status()
        .with_context(|| format!("launch native key generator {}", keygen_helper.display()))?;
    if !status.success() {
        bail!("native key generator rejected the enrollment request")
    }
    let private_key = unique_private_key(&label).context("load generated Keychain key")?;
    if private_key.external_representation().is_some() {
        let _ = private_key.delete();
        bail!("generated device key unexpectedly has an external representation")
    }

    let remote_key = match RemoteSigningKey::new(private_key) {
        Ok(remote_key) => remote_key,
        Err((private_key, error)) => {
            let _ = private_key.delete();
            return Err(error).context("read generated public key");
        }
    };
    let mut params = CertificateParams::default();
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, format!("tools-mcp device {device_id}"));
    params.distinguished_name = distinguished_name;
    let csr = match params.serialize_request(&remote_key) {
        Ok(csr) => csr,
        Err(error) => {
            let _ = remote_key.private_key.delete();
            let signing_error = remote_key
                .signing_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(detail) = signing_error {
                return Err(anyhow!(detail)).context("sign device CSR with macOS Keychain");
            }
            return Err(error).context("create Keychain CSR");
        }
    };
    Ok((csr.pem().context("encode device CSR")?, label))
}

/// Returns the canonical Keychain label for a validated device identifier.
///
/// # Errors
///
/// Fails when `device_id` is not a bounded portable identifier.
pub fn keychain_label(device_id: &str) -> Result<String> {
    validate_device_id(device_id)?;
    Ok(format!("{LABEL_PREFIX}{device_id}"))
}

/// Installs a signed device certificate beside its existing non-exportable Keychain key.
///
/// # Errors
///
/// Fails when the enrolled key is missing or ambiguous, the certificate is
/// invalid or belongs to another key, or Keychain refuses the insertion.
pub fn install_certificate(certificate_der: &[u8], label: &str) -> Result<()> {
    let private_key = unique_private_key(label)?;
    ensure_key_matches_certificate(&private_key, certificate_der)?;
    let certificate =
        SecCertificate::from_der(certificate_der).context("parse signed device certificate")?;
    ItemAddOptions::new(ItemAddValue::Ref(AddRef::Certificate(certificate)))
        .set_location(Location::DefaultFileKeychain)
        .set_label(label)
        .add()
        .context("install device certificate in the default file Keychain")
}

/// Loads a certificate-matching non-exportable Keychain key for rustls client auth.
///
/// # Errors
///
/// Fails when the chain is empty, the key is missing, ambiguous, exportable, or
/// does not match the leaf certificate.
pub fn certified_key(
    certificate_chain: Vec<CertificateDer<'static>>,
    label: &str,
) -> Result<CertifiedKey> {
    let leaf = certificate_chain
        .first()
        .context("device certificate chain is empty")?;
    let private_key = unique_private_key(label)?;
    ensure_key_matches_certificate(&private_key, leaf.as_ref())?;
    if private_key.external_representation().is_some() {
        bail!("device key is exportable; enrollment requires a non-exportable key")
    }

    Ok(CertifiedKey::new(
        certificate_chain,
        Arc::new(KeychainSigningKey { private_key }),
    ))
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

fn find_private_keys(label: &str) -> Result<impl Iterator<Item = SecKey>> {
    let results = ItemSearchOptions::new()
        .key_class(KeyClass::private())
        .label(label)
        .load_refs(true)
        .limit(Limit::All)
        .search();
    let results = match results {
        Ok(results) => results,
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Vec::new(),
        Err(error) => {
            return Err(anyhow!("search macOS Keychain device keys: {error}"));
        }
    };
    Ok(results.into_iter().filter_map(|result| match result {
        SearchResult::Ref(Reference::Key(key)) => Some(key),
        _ => None,
    }))
}

fn unique_private_key(label: &str) -> Result<SecKey> {
    let mut keys = find_private_keys(label)?;
    let key = keys.next().context("device key is not enrolled")?;
    if keys.next().is_some() {
        bail!("multiple device keys have the same Keychain label")
    }
    Ok(key)
}

fn ensure_key_matches_certificate(private_key: &SecKey, certificate_der: &[u8]) -> Result<()> {
    let certificate =
        SecCertificate::from_der(certificate_der).context("parse device certificate")?;
    let certificate_key = certificate
        .public_key()
        .context("read device certificate public key")?
        .external_representation()
        .context("certificate public key has no external representation")?;
    let enrolled_key = private_key
        .public_key()
        .context("read enrolled device public key")?
        .external_representation()
        .context("enrolled public key has no external representation")?;
    if certificate_key.as_ref() != enrolled_key.as_ref() {
        bail!("signed certificate does not match the enrolled device key")
    }
    Ok(())
}

struct RemoteSigningKey {
    private_key: SecKey,
    public_key: Vec<u8>,
    signing_error: Mutex<Option<String>>,
}

impl RemoteSigningKey {
    fn new(private_key: SecKey) -> std::result::Result<Self, (SecKey, anyhow::Error)> {
        let public_key = private_key
            .public_key()
            .context("Keychain key has no public key")
            .and_then(|key| {
                key.external_representation()
                    .context("Keychain public key cannot be encoded")
            });
        match public_key {
            Ok(public_key) => Ok(Self {
                private_key,
                public_key: public_key.to_vec(),
                signing_error: Mutex::new(None),
            }),
            Err(error) => Err((private_key, error)),
        }
    }
}

impl rcgen::PublicKeyData for RemoteSigningKey {
    fn der_bytes(&self) -> &[u8] {
        &self.public_key
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl rcgen::SigningKey for RemoteSigningKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        self.private_key
            .create_signature(Algorithm::ECDSASignatureMessageX962SHA256, message)
            .map_err(|error| {
                *self
                    .signing_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.to_string());
                rcgen::Error::RemoteKeyError
            })
    }
}

struct KeychainSigningKey {
    private_key: SecKey,
}

impl fmt::Debug for KeychainSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeychainSigningKey")
            .finish_non_exhaustive()
    }
}

impl SigningKey for KeychainSigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| {
                Box::new(KeychainSigner {
                    private_key: self.private_key.clone(),
                }) as Box<dyn Signer>
            })
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

struct KeychainSigner {
    private_key: SecKey,
}

impl fmt::Debug for KeychainSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeychainSigner")
            .finish_non_exhaustive()
    }
}

impl Signer for KeychainSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        self.private_key
            .create_signature(Algorithm::ECDSASignatureMessageX962SHA256, message)
            .map_err(|error| Error::General(format!("macOS Keychain signing failed: {error}")))
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngCore as _;
    use std::fs;
    use std::process::Command;

    struct KeyCleanup(Option<SecKey>);
    struct CertificateCleanup(Option<SecCertificate>);

    impl Drop for KeyCleanup {
        fn drop(&mut self) {
            if let Some(key) = self.0.take() {
                let _ = key.delete();
            }
        }
    }

    impl Drop for CertificateCleanup {
        fn drop(&mut self) {
            if let Some(certificate) = self.0.take() {
                let _ = certificate.delete();
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One linear native ceremony is easier to audit as a whole.
    fn native_keychain_csr_and_rustls_signer_keep_key_non_exportable() {
        let device_id = format!("native-test-{:016x}", rand::rng().next_u64());
        let temporary = tempfile::tempdir().expect("temporary native helper directory");
        let helper = temporary.path().join("tools-mcp-keygen");
        let helper_source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../native/macos-keychain-helper/main.swift");
        let module_cache = temporary.path().join("swift-module-cache");
        std::fs::create_dir(&module_cache).expect("create native helper module cache");
        let compilation = Command::new("xcrun")
            .args(["swiftc"])
            .env("CLANG_MODULE_CACHE_PATH", &module_cache)
            .env("SWIFT_MODULECACHE_PATH", &module_cache)
            .arg(&helper_source)
            .arg("-o")
            .arg(&helper)
            .output()
            .expect("compile native key helper");
        assert!(
            compilation.status.success(),
            "native helper compilation failed: {}",
            String::from_utf8_lossy(&compilation.stderr)
        );

        let (csr, label) = generate_csr(&device_id, &helper).expect("generate native CSR");
        let private_key = unique_private_key(&label).expect("load generated key");
        let _cleanup = KeyCleanup(Some(private_key.clone()));
        assert!(private_key.external_representation().is_none());

        let message = b"tools-mcp native credential proof";
        let signature = private_key
            .create_signature(Algorithm::ECDSASignatureMessageX962SHA256, message)
            .expect("sign with non-exportable Keychain key");
        assert!(
            private_key
                .public_key()
                .expect("public key")
                .verify_signature(
                    Algorithm::ECDSASignatureMessageX962SHA256,
                    message,
                    &signature,
                )
                .expect("verify native signature")
        );

        let csr_path = temporary.path().join("device.csr");
        fs::write(&csr_path, csr).expect("write temporary CSR");
        let output = Command::new("/usr/bin/openssl")
            .args(["req", "-in"])
            .arg(&csr_path)
            .args(["-verify", "-noout"])
            .output()
            .expect("run OpenSSL CSR verification");
        assert!(
            output.status.success(),
            "CSR verification failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let ca_key = temporary.path().join("ca.key");
        let ca_certificate = temporary.path().join("ca.pem");
        let certificate_path = temporary.path().join("device.der");
        let extensions = temporary.path().join("extensions.cnf");
        fs::write(
            &extensions,
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n",
        )
        .expect("write certificate extensions");
        let ca = Command::new("/usr/bin/openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes"])
            .arg("-keyout")
            .arg(&ca_key)
            .arg("-out")
            .arg(&ca_certificate)
            .args(["-subj", "/CN=tools-mcp-native-test-ca", "-days", "1"])
            .output()
            .expect("create temporary CA");
        assert!(ca.status.success(), "temporary CA generation failed");
        let signed = Command::new("/usr/bin/openssl")
            .args(["x509", "-req", "-in"])
            .arg(&csr_path)
            .arg("-CA")
            .arg(&ca_certificate)
            .arg("-CAkey")
            .arg(&ca_key)
            .arg("-CAcreateserial")
            .arg("-out")
            .arg(&certificate_path)
            .args(["-outform", "DER", "-days", "1", "-sha256", "-extfile"])
            .arg(&extensions)
            .output()
            .expect("sign temporary device certificate");
        assert!(
            signed.status.success(),
            "device certificate signing failed: {}",
            String::from_utf8_lossy(&signed.stderr)
        );

        let certificate_der = fs::read(&certificate_path).expect("read signed certificate");
        install_certificate(&certificate_der, &label).expect("install signed certificate");
        let _certificate_cleanup = CertificateCleanup(Some(
            SecCertificate::from_der(&certificate_der).expect("parse cleanup certificate"),
        ));
        let certified = certified_key(vec![CertificateDer::from(certificate_der.clone())], &label)
            .expect("load rustls Keychain signer");
        let rustls_signer = certified
            .key
            .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
            .expect("choose rustls ECDSA scheme");
        let rustls_message = b"tools-mcp rustls client-auth proof";
        let rustls_signature = rustls_signer
            .sign(rustls_message)
            .expect("sign through rustls adapter");
        let certificate = SecCertificate::from_der(&certificate_der).expect("parse certificate");
        assert!(
            certificate
                .public_key()
                .expect("certificate public key")
                .verify_signature(
                    Algorithm::ECDSASignatureMessageX962SHA256,
                    rustls_message,
                    &rustls_signature,
                )
                .expect("verify rustls adapter signature")
        );
    }
}
