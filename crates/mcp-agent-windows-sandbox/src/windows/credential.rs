use crate::signature_der::p256_fixed_to_der;
use rcgen::{CertificateParams, DistinguishedName, DnType, PublicKeyData as _};
use sha2::{Digest as _, Sha256};
use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Write as _};
use std::iter::once;
use std::os::windows::ffi::OsStrExt as _;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_ECCKEY_BLOB, BCRYPT_ECCPRIVATE_BLOB, BCRYPT_ECCPUBLIC_BLOB, BCRYPT_ECDSA_P256_ALGORITHM,
    BCRYPT_ECDSA_PUBLIC_P256_MAGIC, MS_KEY_STORAGE_PROVIDER, NCRYPT_EXPORT_POLICY_PROPERTY,
    NCRYPT_HANDLE, NCRYPT_KEY_HANDLE, NCRYPT_PROV_HANDLE, NCRYPT_SILENT_FLAG,
    NCryptCreatePersistedKey, NCryptDeleteKey, NCryptExportKey, NCryptFinalizeKey,
    NCryptFreeObject, NCryptGetProperty, NCryptOpenKey, NCryptOpenStorageProvider,
    NCryptSetProperty, NCryptSignHash,
};

const KEY_PREFIX: &str = "tools-mcp-device:";
const TEST_KEY_PREFIX: &str = "tools-mcp-device:test-";
const MAX_SIGNING_INPUT: u64 = 64 * 1024;

pub(super) fn is_command(first: Option<&OsString>) -> bool {
    first.is_some_and(|value| {
        matches!(
            value.to_str(),
            Some("generate" | "public-key" | "sign" | "verify-non-exportable" | "delete-test-key")
        )
    })
}

pub(super) fn run(mut arguments: impl Iterator<Item = OsString>) -> Result<(), String> {
    let command = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("missing CNG command")?;
    let key_name = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("missing CNG key name")?;
    validate_key_name(&key_name)?;
    if arguments.next().is_some() {
        return Err("unexpected CNG command argument".to_owned());
    }

    match command.as_str() {
        "generate" => generate(&key_name),
        "public-key" => write_public_key(&key_name),
        "sign" => sign_from_stdin(&key_name),
        "verify-non-exportable" => {
            CngKey::open(&key_name)?.verify_non_exportable()?;
            Ok(())
        }
        "delete-test-key" if key_name.starts_with(TEST_KEY_PREFIX) => {
            CngKey::open(&key_name)?.delete()
        }
        "delete-test-key" => Err("only bounded test keys may be deleted".to_owned()),
        _ => Err("unknown CNG command".to_owned()),
    }
}

fn validate_key_name(name: &str) -> Result<(), String> {
    let device = name
        .strip_prefix(KEY_PREFIX)
        .ok_or("invalid tools-mcp CNG key name")?;
    if device.is_empty()
        || device.len() > 128
        || !device
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("invalid tools-mcp CNG key name".to_owned());
    }
    Ok(())
}

fn generate(key_name: &str) -> Result<(), String> {
    let key = CngKey::create(key_name)?;
    let signing_key = RemoteCngKey::new(key)?;
    let mut params = CertificateParams::default();
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(
        DnType::CommonName,
        format!("tools-mcp device {}", &key_name[KEY_PREFIX.len()..]),
    );
    params.distinguished_name = distinguished_name;
    let result = params
        .serialize_request(&signing_key)
        .map_err(|_| "create CNG-backed device CSR".to_owned())
        .and_then(|csr| {
            csr.pem()
                .map_err(|_| "encode CNG-backed device CSR".to_owned())
        })
        .and_then(|pem| {
            std::io::stdout()
                .write_all(pem.as_bytes())
                .map_err(|error| format!("write device CSR: {error}"))
        });
    if let Err(error) = result {
        let _ = signing_key.delete();
        return Err(error);
    }
    Ok(())
}

fn write_public_key(key_name: &str) -> Result<(), String> {
    let key = RemoteCngKey::new(CngKey::open(key_name)?)?;
    std::io::stdout()
        .write_all(&key.subject_public_key_info())
        .map_err(|error| format!("write CNG public key: {error}"))
}

fn sign_from_stdin(key_name: &str) -> Result<(), String> {
    let mut message = Vec::new();
    std::io::stdin()
        .take(MAX_SIGNING_INPUT + 1)
        .read_to_end(&mut message)
        .map_err(|error| format!("read signing input: {error}"))?;
    if message.len() as u64 > MAX_SIGNING_INPUT {
        return Err("signing input exceeds 64 KiB".to_owned());
    }
    let signature = CngKey::open(key_name)?.sign_message(&message)?;
    std::io::stdout()
        .write_all(&signature)
        .map_err(|error| format!("write CNG signature: {error}"))
}

struct Provider(NCRYPT_PROV_HANDLE);

impl Provider {
    fn open() -> Result<Self, String> {
        let mut handle = 0;
        status(unsafe { NCryptOpenStorageProvider(&raw mut handle, MS_KEY_STORAGE_PROVIDER, 0) })?;
        if handle == 0 {
            return Err("CNG provider returned an invalid handle".to_owned());
        }
        Ok(Self(handle))
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        unsafe {
            let _ = NCryptFreeObject(self.0);
        }
    }
}

struct CngKey(NCRYPT_KEY_HANDLE);

impl CngKey {
    fn create(name: &str) -> Result<Self, String> {
        let provider = Provider::open()?;
        let name = wide_null(OsStr::new(name));
        let mut handle = 0;
        status(unsafe {
            NCryptCreatePersistedKey(
                provider.0,
                &raw mut handle,
                BCRYPT_ECDSA_P256_ALGORITHM,
                name.as_ptr(),
                0,
                NCRYPT_SILENT_FLAG,
            )
        })?;
        let key = Self(handle);
        let creation = (|| {
            let export_policy = 0_u32;
            status(unsafe {
                NCryptSetProperty(
                    key.0,
                    NCRYPT_EXPORT_POLICY_PROPERTY,
                    (&raw const export_policy).cast(),
                    u32::try_from(size_of::<u32>()).expect("DWORD size fits u32"),
                    0,
                )
            })?;
            status(unsafe { NCryptFinalizeKey(key.0, NCRYPT_SILENT_FLAG) })?;
            key.verify_non_exportable()
        })();
        if let Err(error) = creation {
            let _ = key.delete();
            return Err(error);
        }
        Ok(key)
    }

    fn open(name: &str) -> Result<Self, String> {
        let provider = Provider::open()?;
        let name = wide_null(OsStr::new(name));
        let mut handle = 0;
        status(unsafe {
            NCryptOpenKey(
                provider.0,
                &raw mut handle,
                name.as_ptr(),
                0,
                NCRYPT_SILENT_FLAG,
            )
        })?;
        let key = Self(handle);
        key.verify_non_exportable()?;
        Ok(key)
    }

    fn verify_non_exportable(&self) -> Result<(), String> {
        let mut export_policy = u32::MAX;
        let mut bytes = 0;
        status(unsafe {
            NCryptGetProperty(
                self.0,
                NCRYPT_EXPORT_POLICY_PROPERTY,
                (&raw mut export_policy).cast(),
                u32::try_from(size_of::<u32>()).expect("DWORD size fits u32"),
                &raw mut bytes,
                0,
            )
        })?;
        if bytes != u32::try_from(size_of::<u32>()).expect("DWORD size fits u32")
            || export_policy != 0
        {
            return Err("CNG key permits private-key export".to_owned());
        }
        let mut private_bytes = 0;
        let export_status = unsafe {
            NCryptExportKey(
                self.0,
                0,
                BCRYPT_ECCPRIVATE_BLOB,
                null(),
                null_mut(),
                0,
                &raw mut private_bytes,
                NCRYPT_SILENT_FLAG,
            )
        };
        if export_status == 0 {
            return Err("CNG provider exported private-key material".to_owned());
        }
        Ok(())
    }

    fn public_point(&self) -> Result<Vec<u8>, String> {
        let blob = self.export(BCRYPT_ECCPUBLIC_BLOB)?;
        let header_size = size_of::<BCRYPT_ECCKEY_BLOB>();
        if blob.len() < header_size {
            return Err("CNG public-key blob is truncated".to_owned());
        }
        let header =
            unsafe { std::ptr::read_unaligned(blob.as_ptr().cast::<BCRYPT_ECCKEY_BLOB>()) };
        let coordinate_bytes = usize::try_from(header.cbKey)
            .map_err(|_| "CNG public-key coordinate size is invalid")?;
        if header.dwMagic != BCRYPT_ECDSA_PUBLIC_P256_MAGIC
            || coordinate_bytes != 32
            || blob.len() != header_size + coordinate_bytes * 2
        {
            return Err("CNG key is not an ECDSA P-256 public key".to_owned());
        }
        let mut point = Vec::with_capacity(65);
        point.push(4);
        point.extend_from_slice(&blob[header_size..]);
        Ok(point)
    }

    fn export(&self, kind: windows_sys::core::PCWSTR) -> Result<Vec<u8>, String> {
        let mut bytes = 0;
        status(unsafe {
            NCryptExportKey(
                self.0,
                0,
                kind,
                null(),
                null_mut(),
                0,
                &raw mut bytes,
                NCRYPT_SILENT_FLAG,
            )
        })?;
        let mut output = vec![0_u8; bytes as usize];
        status(unsafe {
            NCryptExportKey(
                self.0,
                0,
                kind,
                null(),
                output.as_mut_ptr(),
                bytes,
                &raw mut bytes,
                NCRYPT_SILENT_FLAG,
            )
        })?;
        output.truncate(bytes as usize);
        Ok(output)
    }

    fn sign_message(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        let digest = Sha256::digest(message);
        let mut bytes = 0;
        status(unsafe {
            NCryptSignHash(
                self.0,
                null(),
                digest.as_ptr(),
                u32::try_from(digest.len()).expect("SHA-256 size fits u32"),
                null_mut(),
                0,
                &raw mut bytes,
                NCRYPT_SILENT_FLAG,
            )
        })?;
        let mut fixed = vec![0_u8; bytes as usize];
        status(unsafe {
            NCryptSignHash(
                self.0,
                null(),
                digest.as_ptr(),
                u32::try_from(digest.len()).expect("SHA-256 size fits u32"),
                fixed.as_mut_ptr(),
                bytes,
                &raw mut bytes,
                NCRYPT_SILENT_FLAG,
            )
        })?;
        fixed.truncate(bytes as usize);
        p256_fixed_to_der(&fixed).map_err(str::to_owned)
    }

    fn delete(mut self) -> Result<(), String> {
        let handle = std::mem::replace(&mut self.0, 0);
        status(unsafe { NCryptDeleteKey(handle, NCRYPT_SILENT_FLAG) })
    }
}

impl Drop for CngKey {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                let _ = NCryptFreeObject(self.0 as NCRYPT_HANDLE);
            }
        }
    }
}

struct RemoteCngKey {
    key: CngKey,
    public_point: Vec<u8>,
}

impl RemoteCngKey {
    fn new(key: CngKey) -> Result<Self, String> {
        let public_point = key.public_point()?;
        Ok(Self { key, public_point })
    }

    fn delete(self) -> Result<(), String> {
        self.key.delete()
    }
}

impl rcgen::PublicKeyData for RemoteCngKey {
    fn der_bytes(&self) -> &[u8] {
        &self.public_point
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl rcgen::SigningKey for RemoteCngKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        self.key
            .sign_message(message)
            .map_err(|_| rcgen::Error::RemoteKeyError)
    }
}

fn status(value: i32) -> Result<(), String> {
    if value == 0 {
        Ok(())
    } else {
        Err(format!(
            "Windows CNG operation failed: 0x{:08x}",
            value.cast_unsigned()
        ))
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(once(0)).collect()
}

use std::mem::size_of;
