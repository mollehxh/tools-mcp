use mcp_agent_server::http::MCP_ENDPOINT;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Run(Cli),
    EnrollDevice {
        device_id: String,
        csr_output: PathBuf,
        release_dir: Option<PathBuf>,
    },
    InstallDeviceCertificate {
        device_id: String,
        certificate: PathBuf,
        release_dir: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cli {
    pub bind: SocketAddr,
    pub public_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub release_dir: Option<PathBuf>,
    pub relay: Option<RelayCli>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayCli {
    pub url: String,
    pub ca: PathBuf,
    pub device_cert: PathBuf,
    pub device_key: DeviceKeySource,
    pub device_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceKeySource {
    Pem(PathBuf),
    MacosKeychain(String),
    WindowsCng(String),
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CliError {
    #[error("missing value for {0}")]
    MissingValue(String),
    #[error("unrecognized argument: {0}")]
    UnknownArgument(String),
    #[error("--bind must be a valid socket address")]
    Bind,
    #[error("mcp-agent binds only to a loopback address; use an external tunnel")]
    NonLoopbackBind,
    #[error("the MCP endpoint is fixed at /mcp")]
    Endpoint,
    #[error("Host and Origin entries must not be empty or wildcard values")]
    UnsafeAllowlist,
    #[error(
        "relay mode requires --relay-url, --relay-ca, --device-cert, exactly one of --device-key, --device-keychain-label, or --device-cng-key-name, and --device-id together"
    )]
    IncompleteRelay,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8000)),
            public_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            release_dir: None,
            relay: None,
        }
    }
}

impl Command {
    /// Parses the process command, including human-only enrollment commands.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown commands, missing arguments, or malformed
    /// run-mode configuration.
    pub fn parse_env() -> Result<Self, CliError> {
        Self::parse_from(std::env::args_os())
    }

    /// Parses a process command from an explicit iterator.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown commands, missing arguments, or malformed
    /// run-mode configuration.
    pub fn parse_from<I, S>(arguments: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arguments = arguments
            .into_iter()
            .map(|argument| argument.as_ref().to_os_string())
            .collect::<Vec<_>>();
        match arguments.get(1).and_then(|argument| argument.to_str()) {
            Some("enroll-device") => parse_enroll_device(&arguments),
            Some("install-device-certificate") => parse_install_certificate(&arguments),
            _ => Cli::parse_from(arguments).map(Self::Run),
        }
    }
}

fn parse_enroll_device(arguments: &[std::ffi::OsString]) -> Result<Command, CliError> {
    let mut device_id = None;
    let mut csr_output = None;
    let mut release_dir = None;
    parse_pairs(arguments, |flag, value| {
        match flag {
            "--device-id" => device_id = Some(value.to_owned()),
            "--csr-output" => csr_output = Some(PathBuf::from(value)),
            "--release-dir" => release_dir = Some(PathBuf::from(value)),
            _ => return Err(CliError::UnknownArgument(flag.to_owned())),
        }
        Ok(())
    })?;
    Ok(Command::EnrollDevice {
        device_id: device_id.ok_or_else(|| CliError::MissingValue("--device-id".to_owned()))?,
        csr_output: csr_output.ok_or_else(|| CliError::MissingValue("--csr-output".to_owned()))?,
        release_dir,
    })
}

fn parse_install_certificate(arguments: &[std::ffi::OsString]) -> Result<Command, CliError> {
    let mut device_id = None;
    let mut certificate = None;
    let mut release_dir = None;
    parse_pairs(arguments, |flag, value| {
        match flag {
            "--device-id" => device_id = Some(value.to_owned()),
            "--device-cert" => certificate = Some(PathBuf::from(value)),
            "--release-dir" => release_dir = Some(PathBuf::from(value)),
            _ => return Err(CliError::UnknownArgument(flag.to_owned())),
        }
        Ok(())
    })?;
    Ok(Command::InstallDeviceCertificate {
        device_id: device_id.ok_or_else(|| CliError::MissingValue("--device-id".to_owned()))?,
        certificate: certificate
            .ok_or_else(|| CliError::MissingValue("--device-cert".to_owned()))?,
        release_dir,
    })
}

fn parse_pairs(
    arguments: &[std::ffi::OsString],
    mut accept: impl FnMut(&str, &str) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut pairs = arguments[2..].chunks_exact(2);
    for pair in &mut pairs {
        let flag = pair[0].to_string_lossy();
        let value = pair[1].to_string_lossy();
        accept(&flag, &value)?;
    }
    if let Some(flag) = pairs.remainder().first() {
        return Err(CliError::MissingValue(flag.to_string_lossy().into_owned()));
    }
    Ok(())
}

impl Cli {
    /// Parses command-line arguments from the current process.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed or security-incompatible arguments.
    pub fn parse_env() -> Result<Self, CliError> {
        Self::parse_from(std::env::args_os())
    }

    /// Parses command-line arguments from an explicit iterator.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed or security-incompatible arguments.
    pub fn parse_from<I, S>(arguments: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cli = Self::default();
        let mut relay_url = None;
        let mut relay_ca = None;
        let mut device_cert = None;
        let mut device_key = None;
        let mut device_keychain_label = None;
        let mut device_cng_key_name = None;
        let mut device_id = None;
        let mut arguments = arguments.into_iter();
        let _program = arguments.next();
        while let Some(argument) = arguments.next() {
            let argument = argument.as_ref().to_string_lossy().into_owned();
            let value = |arguments: &mut I::IntoIter| {
                arguments
                    .next()
                    .map(|value| value.as_ref().to_string_lossy().into_owned())
                    .ok_or_else(|| CliError::MissingValue(argument.clone()))
            };
            match argument.as_str() {
                "--bind" => {
                    cli.bind = value(&mut arguments)?.parse().map_err(|_| CliError::Bind)?;
                }
                "--endpoint" => {
                    if value(&mut arguments)? != MCP_ENDPOINT {
                        return Err(CliError::Endpoint);
                    }
                }
                "--public-host" => cli.public_hosts.push(value(&mut arguments)?),
                "--origin" => cli.allowed_origins.push(value(&mut arguments)?),
                "--release-dir" => cli.release_dir = Some(PathBuf::from(value(&mut arguments)?)),
                "--relay-url" => relay_url = Some(value(&mut arguments)?),
                "--relay-ca" => relay_ca = Some(PathBuf::from(value(&mut arguments)?)),
                "--device-cert" => device_cert = Some(PathBuf::from(value(&mut arguments)?)),
                "--device-key" => device_key = Some(PathBuf::from(value(&mut arguments)?)),
                "--device-keychain-label" => {
                    device_keychain_label = Some(value(&mut arguments)?);
                }
                "--device-cng-key-name" => {
                    device_cng_key_name = Some(value(&mut arguments)?);
                }
                "--device-id" => device_id = Some(value(&mut arguments)?),
                _ => return Err(CliError::UnknownArgument(argument)),
            }
        }
        let device_key = match (device_key, device_keychain_label, device_cng_key_name) {
            (Some(path), None, None) => Some(DeviceKeySource::Pem(path)),
            (None, Some(label), None) => Some(DeviceKeySource::MacosKeychain(label)),
            (None, None, Some(name)) => Some(DeviceKeySource::WindowsCng(name)),
            (None, None, None) => None,
            _ => return Err(CliError::IncompleteRelay),
        };
        cli.relay = match (relay_url, relay_ca, device_cert, device_key, device_id) {
            (None, None, None, None, None) => None,
            (Some(url), Some(ca), Some(device_cert), Some(device_key), Some(device_id)) => {
                Some(RelayCli {
                    url,
                    ca,
                    device_cert,
                    device_key,
                    device_id,
                })
            }
            _ => return Err(CliError::IncompleteRelay),
        };
        cli.validate()?;
        Ok(cli)
    }

    fn validate(&self) -> Result<(), CliError> {
        if !self.bind.ip().is_loopback() {
            return Err(CliError::NonLoopbackBind);
        }
        if self
            .public_hosts
            .iter()
            .chain(&self.allowed_origins)
            .any(|value| value.trim().is_empty() || value.trim() == "*")
        {
            return Err(CliError::UnsafeAllowlist);
        }
        Ok(())
    }
}
