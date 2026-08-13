use mcp_agent_server::http::MCP_ENDPOINT;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cli {
    pub bind: SocketAddr,
    pub public_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub release_dir: Option<PathBuf>,
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
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8000)),
            public_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            release_dir: None,
        }
    }
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
                _ => return Err(CliError::UnknownArgument(argument)),
            }
        }
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
