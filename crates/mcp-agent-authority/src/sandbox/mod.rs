#[allow(dead_code, clippy::unnecessary_wraps)]
mod linux;
#[allow(dead_code)]
mod macos;
mod preflight;
#[allow(dead_code, clippy::unnecessary_wraps)]
mod windows;

use crate::{AuthorityError, CapabilitySnapshot, WorkspaceAuthority};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output};
use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

pub use preflight::PreflightReceipt;

pub const CAPABILITY_PROTOCOL: &str = "mcp-agent-workspace-write/v1";
pub const PINNED_CODEX_COMMIT: &str = "8cabf5a6cf103cebe338d46346e43e3201e64f41";
pub const PREFLIGHT_CANARY_BYTES: &[u8] = b"mcp-agent workspace-write sandbox preflight canary\n";
const MANIFEST_FILE: &str = "sandbox-manifest.json";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_POLICY_BYTES: u64 = 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
#[doc(hidden)]
pub const INTERNAL_SANDBOX_CHILD_FLAG: &str = "--__mcp-agent-sandbox-child";
const INTERNAL_SANDBOX_DELIMITER: &str = "--";
const INTERNAL_SANDBOX_TOKEN_ENV: &str = "MCP_AGENT_INTERNAL_SANDBOX_TOKEN";
const INTERNAL_SANDBOX_GUARD_ENV: &str = "MCP_AGENT_INTERNAL_SANDBOX_ACTIVE";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxManifest {
    pub capability_protocol: String,
    pub upstream_commit: String,
    pub target: String,
    pub artifact_path: PathBuf,
    pub artifact_sha256: String,
    pub policy_path: PathBuf,
    pub policy_sha256: String,
    pub canary_path: PathBuf,
    pub canary_sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox manifest is missing from the installed release")]
    ManifestMissing,
    #[error("sandbox manifest is invalid")]
    ManifestInvalid,
    #[error("sandbox capability protocol mismatch: expected {expected}, got {actual}")]
    ProtocolMismatch { expected: String, actual: String },
    #[error("sandbox release target does not match this process")]
    TargetMismatch,
    #[error("sandbox helper or policy is missing")]
    BackendMissing,
    #[error("sandbox artifact changed after verification")]
    ArtifactReplaced,
    #[error("sandbox authority rejected a path")]
    Authority(#[from] AuthorityError),
    #[error("sandbox operation failed")]
    Io(#[from] std::io::Error),
    #[error("sandbox preflight failed: {0}")]
    Preflight(String),
    #[error("sandbox child adapter rejected launch: {0}")]
    ChildAdapter(String),
}

#[derive(Clone, Debug)]
pub struct Sandbox {
    authority: WorkspaceAuthority,
    release: PathBuf,
    manifest: SandboxManifest,
    manifest_sha256: String,
    backend: Arc<VerifiedBackend>,
    reexec: Option<Arc<VerifiedBackend>>,
    reexec_token: Option<Arc<str>>,
}

/// A sandbox that has passed the native read/write/network startup proof.
#[derive(Clone, Debug)]
pub struct VerifiedSandbox {
    sandbox: Sandbox,
    receipt: PreflightReceipt,
}

/// A command whose only launch path is through a verified native sandbox.
#[derive(Debug)]
pub struct SandboxCommand {
    sandbox: Sandbox,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
}

#[derive(Debug)]
struct VerifiedBackend {
    file: File,
    path: PathBuf,
    identity: FileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl SandboxManifest {
    #[must_use]
    pub fn release_path(release: &Path) -> PathBuf {
        release.join(MANIFEST_FILE)
    }

    pub fn write_release_relative(&self, release: &Path) -> Result<(), SandboxError> {
        fs::create_dir_all(release.join("sandbox"))?;
        materialize_backend(release, self)?;
        fs::write(
            Self::release_path(release),
            serde_json::to_vec_pretty(self).map_err(|_| SandboxError::ManifestInvalid)?,
        )?;
        Ok(())
    }
}

pub fn expected_manifest() -> Result<SandboxManifest, SandboxError> {
    let artifact_path = backend_artifact_path();
    let policy_path = PathBuf::from("sandbox/workspace-write.policy");
    let canary_path = PathBuf::from("sandbox/preflight-canary");
    let artifact_sha256 = expected_artifact_digest()?;
    let policy_bytes = native_policy_bytes();
    Ok(SandboxManifest {
        capability_protocol: CAPABILITY_PROTOCOL.to_owned(),
        upstream_commit: PINNED_CODEX_COMMIT.to_owned(),
        target: current_target(),
        artifact_path,
        artifact_sha256,
        policy_path,
        policy_sha256: digest(policy_bytes),
        canary_path,
        canary_sha256: digest(PREFLIGHT_CANARY_BYTES),
    })
}

impl Sandbox {
    pub fn load(authority: WorkspaceAuthority, release: &Path) -> Result<Self, SandboxError> {
        // Normalize the caller's spelling once. Platform temporary roots often
        // contain an OS-owned alias (for example /var -> /private/var on
        // macOS); asset components below this canonical release must still be
        // direct, non-symlink entries.
        let release = release.canonicalize()?;
        let manifest_path = SandboxManifest::release_path(&release);
        let bytes =
            read_bounded(&manifest_path, MAX_MANIFEST_BYTES).map_err(|error| match error {
                SandboxError::BackendMissing => SandboxError::ManifestMissing,
                other => other,
            })?;
        let manifest: SandboxManifest =
            serde_json::from_slice(&bytes).map_err(|_| SandboxError::ManifestInvalid)?;
        if manifest.capability_protocol != CAPABILITY_PROTOCOL {
            return Err(SandboxError::ProtocolMismatch {
                expected: CAPABILITY_PROTOCOL.to_owned(),
                actual: manifest.capability_protocol,
            });
        }
        if manifest.upstream_commit != PINNED_CODEX_COMMIT || manifest.target != current_target() {
            return Err(SandboxError::TargetMismatch);
        }
        validate_relative_asset(&manifest.artifact_path)?;
        validate_relative_asset(&manifest.policy_path)?;
        validate_relative_asset(&manifest.canary_path)?;
        // The release manifest is descriptive, not a trust root. Every field
        // that controls executable code or policy must match the build-owned
        // expectation before any release file is trusted.
        let expected = expected_manifest()?;
        if manifest != expected {
            return Err(SandboxError::ArtifactReplaced);
        }
        verify_file_digest(
            &release.join(&manifest.artifact_path),
            &manifest.artifact_sha256,
            MAX_ARTIFACT_BYTES,
        )?;
        verify_file_digest(
            &release.join(&manifest.policy_path),
            &manifest.policy_sha256,
            MAX_POLICY_BYTES,
        )?;
        verify_file_digest(
            &release.join(&manifest.canary_path),
            &manifest.canary_sha256,
            MAX_POLICY_BYTES,
        )?;
        let backend = Arc::new(open_verified_backend(&release)?);
        Ok(Self {
            authority,
            release,
            manifest,
            manifest_sha256: digest(&bytes),
            backend,
            reexec: None,
            reexec_token: None,
        })
    }

    /// Loads the sandbox and pins the already-verified server executable as
    /// the only adapter through which workload children may be launched.
    #[doc(hidden)]
    pub fn load_with_reexec(
        authority: WorkspaceAuthority,
        release: &Path,
        executable: &Path,
    ) -> Result<Self, SandboxError> {
        let mut sandbox = Self::load(authority, release)?;
        let executable = executable.canonicalize()?;
        let file = open_verified_file(&executable, true)?;
        let identity = FileIdentity::from_metadata(&file.metadata()?);
        sandbox.reexec = Some(Arc::new(VerifiedBackend {
            file,
            path: executable,
            identity,
        }));
        sandbox.reexec_token = Some(Arc::from(random_reexec_token()?));
        Ok(sandbox)
    }

    pub fn render_native_policy(&self) -> Result<String, SandboxError> {
        self.reverify()?;
        #[cfg(target_os = "macos")]
        return macos::render_policy(self);
        #[cfg(not(target_os = "macos"))]
        Ok(String::from_utf8_lossy(native_policy_bytes()).into_owned())
    }

    pub fn preflight(self) -> Result<(VerifiedSandbox, PreflightReceipt), SandboxError> {
        let receipt = preflight::run(&self)?;
        let verified = VerifiedSandbox {
            sandbox: self,
            receipt: receipt.clone(),
        };
        Ok((verified, receipt))
    }

    fn reverify(&self) -> Result<(), SandboxError> {
        let manifest = read_bounded(
            &SandboxManifest::release_path(&self.release),
            MAX_MANIFEST_BYTES,
        )?;
        if digest(&manifest) != self.manifest_sha256 {
            return Err(SandboxError::ArtifactReplaced);
        }
        verify_file_digest(
            &self.release.join(&self.manifest.artifact_path),
            &self.manifest.artifact_sha256,
            MAX_ARTIFACT_BYTES,
        )?;
        verify_file_digest(
            &self.release.join(&self.manifest.policy_path),
            &self.manifest.policy_sha256,
            MAX_POLICY_BYTES,
        )?;
        verify_file_digest(
            &self.release.join(&self.manifest.canary_path),
            &self.manifest.canary_sha256,
            MAX_POLICY_BYTES,
        )?;
        self.backend.reverify()
    }

    fn build_command(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> Result<Command, SandboxError> {
        self.reverify()?;
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let command = platform_command(self, &self.backend.launch_path(), program, &args, cwd)?;
        match (&self.reexec, &self.reexec_token) {
            (Some(adapter), Some(token)) => {
                adapter.reverify()?;
                Ok(wrap_with_reexec(&command, &adapter.path, token))
            }
            (None, None) => Ok(command),
            _ => Err(SandboxError::ChildAdapter(
                "incomplete reexec authority".to_owned(),
            )),
        }
    }

    fn command_unverified(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<SandboxCommand, SandboxError> {
        let cwd = self.authority.command().resolve_cwd(cwd)?;
        Ok(SandboxCommand {
            sandbox: self.clone(),
            program: program.to_owned(),
            args: args.iter().map(|value| (*value).to_owned()).collect(),
            cwd,
        })
    }
}

/// Dispatches the hidden authenticated sandbox adapter before ordinary CLI
/// parsing. A successful dispatch replaces the process image and never
/// returns. Direct, malformed, or recursive selection fails closed.
#[doc(hidden)]
pub fn dispatch_internal_sandbox_child() -> Result<bool, SandboxError> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_none_or(|value| value != INTERNAL_SANDBOX_CHILD_FLAG)
    {
        return Ok(false);
    }
    let token = std::env::var_os(INTERNAL_SANDBOX_TOKEN_ENV);
    let guarded = std::env::var_os(INTERNAL_SANDBOX_GUARD_ENV).is_some();
    let launch = parse_internal_sandbox_child(&arguments, token.as_deref(), guarded)?;
    let verified = open_verified_backend(Path::new("."))?;
    if launch.program != verified.path {
        return Err(SandboxError::ChildAdapter(
            "sandbox launcher is not the verified native backend".to_owned(),
        ));
    }
    close_inherited_descriptors()?;

    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .env_remove(INTERNAL_SANDBOX_TOKEN_ENV)
        .env(INTERNAL_SANDBOX_GUARD_ENV, "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(SandboxError::Io(error))
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        Err(SandboxError::ChildAdapter(format!(
            "native sandbox launcher returned {status}"
        )))
    }
}

struct InternalSandboxLaunch {
    program: PathBuf,
    args: Vec<OsString>,
}

fn parse_internal_sandbox_child(
    arguments: &[OsString],
    token_environment: Option<&OsStr>,
    guarded: bool,
) -> Result<InternalSandboxLaunch, SandboxError> {
    if guarded {
        return Err(SandboxError::ChildAdapter(
            "recursive sandbox adapter selection".to_owned(),
        ));
    }
    if arguments.len() < 5
        || arguments[0] != INTERNAL_SANDBOX_CHILD_FLAG
        || arguments[2] != INTERNAL_SANDBOX_DELIMITER
        || token_environment != Some(arguments[1].as_os_str())
        || arguments[1].is_empty()
    {
        return Err(SandboxError::ChildAdapter(
            "malformed or unauthenticated adapter arguments".to_owned(),
        ));
    }
    let program = PathBuf::from(&arguments[3]);
    if program != Path::new("/usr/bin/sandbox-exec") || arguments[4] != "-p" {
        return Err(SandboxError::ChildAdapter(
            "unsandboxed launcher selection is forbidden".to_owned(),
        ));
    }
    Ok(InternalSandboxLaunch {
        program,
        args: arguments[4..].to_vec(),
    })
}

fn wrap_with_reexec(command: &Command, adapter: &Path, token: &str) -> Command {
    let mut wrapped = Command::new(adapter);
    wrapped
        .arg(INTERNAL_SANDBOX_CHILD_FLAG)
        .arg(token)
        .arg(INTERNAL_SANDBOX_DELIMITER)
        .arg(command.get_program())
        .args(command.get_args())
        .env(INTERNAL_SANDBOX_TOKEN_ENV, token)
        .env_remove(INTERNAL_SANDBOX_GUARD_ENV);
    if let Some(cwd) = command.get_current_dir() {
        wrapped.current_dir(cwd);
    }
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => {
                wrapped.env(key, value);
            }
            None => {
                wrapped.env_remove(key);
            }
        }
    }
    wrapped
}

fn random_reexec_token() -> Result<String, SandboxError> {
    #[cfg(unix)]
    {
        let mut bytes = [0_u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let mut token = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(token, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(token)
    }
    #[cfg(not(unix))]
    {
        let seed = format!("{}:{:?}", std::process::id(), std::time::SystemTime::now());
        Ok(digest(seed.as_bytes()))
    }
}

#[cfg(unix)]
fn close_inherited_descriptors() -> Result<(), SandboxError> {
    let (maximum, _) =
        nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
            .map_err(|error| SandboxError::Io(std::io::Error::from_raw_os_error(error as i32)))?;
    let maximum = std::os::fd::RawFd::try_from(maximum).unwrap_or(std::os::fd::RawFd::MAX);
    for descriptor in 3..maximum {
        let _ = nix::unistd::close(descriptor);
    }
    Ok(())
}

#[cfg(not(unix))]
fn close_inherited_descriptors() -> Result<(), SandboxError> {
    Ok(())
}

impl VerifiedSandbox {
    #[must_use]
    pub fn preflight_receipt(&self) -> &PreflightReceipt {
        &self.receipt
    }

    #[must_use]
    pub fn capabilities(&self) -> Option<&Arc<CapabilitySnapshot>> {
        self.sandbox.authority.capabilities()
    }

    pub fn command(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<SandboxCommand, SandboxError> {
        self.sandbox.command_unverified(program, args, cwd)
    }

    pub fn render_native_policy(&self) -> Result<String, SandboxError> {
        self.sandbox.render_native_policy()
    }
}

impl SandboxCommand {
    /// Revalidates the packaged sandbox and returns the fixed native launch
    /// command. Runtime adapters may configure stdio or attach a PTY, but may
    /// not replace the program, arguments, or working-directory authority.
    pub fn into_std_command(self) -> Result<Command, SandboxError> {
        self.sandbox
            .build_command(&self.program, &self.args, &self.cwd)
    }

    pub fn spawn(self) -> Result<Child, SandboxError> {
        Ok(self.into_std_command()?.spawn()?)
    }

    pub fn output(self) -> Result<Output, SandboxError> {
        Ok(self.into_std_command()?.output()?)
    }

    pub fn status(self) -> Result<ExitStatus, SandboxError> {
        Ok(self.into_std_command()?.status()?)
    }
}

impl VerifiedBackend {
    fn launch_path(&self) -> PathBuf {
        #[cfg(target_os = "linux")]
        return PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        return self.path.clone();
        #[allow(unreachable_code)]
        self.path.clone()
    }

    fn reverify(&self) -> Result<(), SandboxError> {
        let held = FileIdentity::from_metadata(&self.file.metadata()?);
        if held != self.identity {
            return Err(SandboxError::ArtifactReplaced);
        }
        let current = open_verified_file(&self.path, false)?;
        if FileIdentity::from_metadata(&current.metadata()?) != self.identity {
            return Err(SandboxError::ArtifactReplaced);
        }
        Ok(())
    }
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }
}

fn platform_command(
    sandbox: &Sandbox,
    launcher: &Path,
    program: &str,
    args: &[&str],
    cwd: &Path,
) -> Result<Command, SandboxError> {
    #[cfg(target_os = "macos")]
    return macos::command(sandbox, launcher, program, args, cwd);
    #[cfg(target_os = "linux")]
    return linux::command(sandbox, launcher, program, args, cwd);
    #[cfg(target_os = "windows")]
    return windows::command(sandbox, launcher, program, args, cwd);
    #[allow(unreachable_code)]
    Err(SandboxError::BackendMissing)
}

fn materialize_backend(release: &Path, manifest: &SandboxManifest) -> Result<(), SandboxError> {
    fs::write(release.join(&manifest.policy_path), native_policy_bytes())?;
    fs::write(release.join(&manifest.canary_path), PREFLIGHT_CANARY_BYTES)?;
    #[cfg(target_os = "macos")]
    fs::write(
        release.join(&manifest.artifact_path),
        macos::artifact_marker(),
    )?;
    #[cfg(target_os = "linux")]
    {
        let source = linux::packaging_source().ok_or(SandboxError::BackendMissing)?;
        fs::copy(source, release.join(&manifest.artifact_path))?;
    }
    #[cfg(target_os = "windows")]
    {
        let source = windows::packaging_source().ok_or(SandboxError::BackendMissing)?;
        fs::copy(source, release.join(&manifest.artifact_path))?;
    }
    Ok(())
}

fn backend_artifact_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    return PathBuf::from("sandbox/macos-seatbelt.marker");
    #[cfg(target_os = "linux")]
    return PathBuf::from("sandbox/bwrap");
    #[cfg(target_os = "windows")]
    return PathBuf::from("sandbox/mcp-agent-windows-sandbox.exe");
    #[allow(unreachable_code)]
    PathBuf::from("sandbox/unsupported")
}

fn expected_artifact_digest() -> Result<String, SandboxError> {
    #[cfg(target_os = "macos")]
    return Ok(digest(&macos::artifact_marker()));
    #[cfg(target_os = "linux")]
    return trusted_build_digest(option_env!("MCP_AGENT_BWRAP_SHA256"));
    #[cfg(target_os = "windows")]
    return trusted_build_digest(option_env!("MCP_AGENT_WINDOWS_SANDBOX_HELPER_SHA256"));
    #[allow(unreachable_code)]
    Err(SandboxError::BackendMissing)
}

fn native_policy_bytes() -> &'static [u8] {
    #[cfg(target_os = "macos")]
    return macos::POLICY.as_bytes();
    #[cfg(target_os = "linux")]
    return linux::POLICY_DESCRIPTION.as_bytes();
    #[cfg(target_os = "windows")]
    return windows::POLICY_DESCRIPTION.as_bytes();
    #[allow(unreachable_code)]
    b"unsupported"
}

fn current_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

fn validate_relative_asset(path: &Path) -> Result<(), SandboxError> {
    if path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SandboxError::ManifestInvalid);
    }
    Ok(())
}

fn verify_file_digest(path: &Path, expected: &str, max_bytes: u64) -> Result<(), SandboxError> {
    if digest_file(path, max_bytes)? == expected {
        Ok(())
    } else {
        Err(SandboxError::ArtifactReplaced)
    }
}

fn digest_file(path: &Path, max_bytes: u64) -> Result<String, SandboxError> {
    let mut file = open_verified_file(path, false)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > max_bytes {
            return Err(SandboxError::ArtifactReplaced);
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, SandboxError> {
    let file = open_verified_file(path, false)?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(SandboxError::ArtifactReplaced);
    }
    Ok(bytes)
}

fn map_missing_backend(error: std::io::Error) -> SandboxError {
    if error.kind() == std::io::ErrorKind::NotFound {
        SandboxError::BackendMissing
    } else {
        SandboxError::Io(error)
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn trusted_build_digest(value: Option<&'static str>) -> Result<String, SandboxError> {
    let value = value.ok_or(SandboxError::BackendMissing)?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SandboxError::BackendMissing);
    }
    Ok(value.to_ascii_lowercase())
}

fn open_verified_backend(release: &Path) -> Result<VerifiedBackend, SandboxError> {
    #[cfg(target_os = "macos")]
    let _ = release;
    #[cfg(target_os = "macos")]
    let path = PathBuf::from("/usr/bin/sandbox-exec");
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let path = release.join(backend_artifact_path());
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let path = release.join("sandbox/unsupported");

    let file = open_verified_file(&path, true)?;
    let identity = FileIdentity::from_metadata(&file.metadata()?);
    Ok(VerifiedBackend {
        file,
        path,
        identity,
    })
}

fn open_verified_file(path: &Path, executable_lock: bool) -> Result<File, SandboxError> {
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path).map_err(map_missing_backend)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SandboxError::ArtifactReplaced);
    }

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    if executable_lock {
        // Allow the image loader to read while denying writers and renames for
        // the entire verify-to-CreateProcess interval.
        options.share_mode(1); // FILE_SHARE_READ
    }
    #[cfg(not(windows))]
    let _ = executable_lock;
    let file = options.open(path).map_err(map_missing_backend)?;
    reject_symlink_components(path)?;
    Ok(file)
}

fn reject_symlink_components(path: &Path) -> Result<(), SandboxError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SandboxError::ArtifactReplaced);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(SandboxError::Io(error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        INTERNAL_SANDBOX_CHILD_FLAG, INTERNAL_SANDBOX_DELIMITER, SandboxError,
        parse_internal_sandbox_child,
    };
    use std::ffi::{OsStr, OsString};

    fn valid_arguments() -> Vec<OsString> {
        [
            INTERNAL_SANDBOX_CHILD_FLAG,
            "internal-token",
            INTERNAL_SANDBOX_DELIMITER,
            "/usr/bin/sandbox-exec",
            "-p",
            "(version 1)",
            "/bin/true",
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    #[test]
    fn internal_child_requires_authenticated_delimited_sandbox_launch() {
        let arguments = valid_arguments();
        let launch =
            parse_internal_sandbox_child(&arguments, Some(OsStr::new("internal-token")), false)
                .unwrap();

        assert_eq!(
            launch.program,
            std::path::Path::new("/usr/bin/sandbox-exec")
        );
        assert_eq!(launch.args.first(), Some(&OsString::from("-p")));
    }

    #[test]
    fn internal_child_rejects_malformed_recursive_and_unsandboxed_selection() {
        let valid = valid_arguments();
        assert!(matches!(
            parse_internal_sandbox_child(&valid, None, false),
            Err(SandboxError::ChildAdapter(_))
        ));
        assert!(matches!(
            parse_internal_sandbox_child(&valid, Some(OsStr::new("internal-token")), true),
            Err(SandboxError::ChildAdapter(_))
        ));

        let mut unsandboxed = valid;
        unsandboxed[3] = OsString::from("/bin/sh");
        assert!(matches!(
            parse_internal_sandbox_child(&unsandboxed, Some(OsStr::new("internal-token")), false),
            Err(SandboxError::ChildAdapter(_))
        ));
    }
}
