#![cfg(windows)]

use mcp_agent::windows_cng;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::SignatureScheme;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn helper() -> PathBuf {
    std::env::var_os("MCP_AGENT_WINDOWS_SANDBOX_HELPER_PATH")
        .map(PathBuf::from)
        .expect("Windows CI must provide the audited native helper path")
}

struct TestKey {
    helper: PathBuf,
    key_name: String,
}

impl Drop for TestKey {
    fn drop(&mut self) {
        let _ = Command::new(&self.helper)
            .args(["delete-test-key", &self.key_name])
            .status();
    }
}

fn issuer() -> Issuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
    ];
    Issuer::new(params, KeyPair::generate().unwrap())
}

fn unique_device_id() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("test-{}-{nonce}", std::process::id())
}

#[test]
fn cng_enrollment_key_matches_certificate_and_signs_for_rustls() {
    let helper = helper();
    let device_id = unique_device_id();
    let (csr, key_name) = windows_cng::generate_csr(&device_id, &helper).unwrap();
    let key = TestKey {
        helper: helper.clone(),
        key_name: key_name.clone(),
    };
    let request = CertificateSigningRequestParams::from_pem(&csr).unwrap();
    let certificate = request.signed_by(&issuer()).unwrap().der().clone();
    let certified = windows_cng::certified_key(vec![certificate], &key_name, &helper).unwrap();

    let signer = certified
        .key
        .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
        .unwrap();
    let signature = signer.sign(b"windows CNG rustls proof").unwrap();
    assert!(!signature.is_empty());

    drop(key);
}

#[test]
fn certificate_substitution_is_rejected() {
    let helper = helper();
    let device_id = unique_device_id();
    let (_, key_name) = windows_cng::generate_csr(&device_id, &helper).unwrap();
    let key = TestKey {
        helper: helper.clone(),
        key_name: key_name.clone(),
    };
    let unrelated_key = KeyPair::generate().unwrap();
    let unrelated_certificate = CertificateParams::default()
        .self_signed(&unrelated_key)
        .unwrap()
        .der()
        .clone();

    let error =
        windows_cng::certified_key(vec![unrelated_certificate], &key_name, &helper).unwrap_err();
    assert!(error.to_string().contains("does not match"));

    drop(key);
}
