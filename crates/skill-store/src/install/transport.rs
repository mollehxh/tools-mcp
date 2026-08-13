use super::{
    InstallLimits, SkillInstallError,
    controlled_http::{validate_https_hop, validate_redirect_hop},
    source::is_non_public_ip,
};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) struct InstallDeadline {
    deadline: Instant,
}

impl InstallDeadline {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
        }
    }

    pub(crate) fn check(self) -> Result<(), SkillInstallError> {
        self.remaining().map(|_| ())
    }

    pub(crate) fn remaining(self) -> Result<Duration, SkillInstallError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(SkillInstallError::LimitExceeded)
    }
}

#[derive(Debug)]
pub(crate) struct TransferBudget {
    deadline: Instant,
    limit: usize,
    used: usize,
}

impl TransferBudget {
    pub(crate) fn new(limits: &InstallLimits) -> Self {
        Self::with_deadline(limits, InstallDeadline::new(limits.timeout))
    }

    pub(crate) fn with_deadline(limits: &InstallLimits, deadline: InstallDeadline) -> Self {
        Self {
            deadline: deadline.deadline,
            limit: limits.max_transport_bytes,
            used: 0,
        }
    }

    pub(crate) fn accept(&mut self, requested: usize) -> Result<(), SkillInstallError> {
        if Instant::now() >= self.deadline || self.used.saturating_add(requested) > self.limit {
            return Err(SkillInstallError::LimitExceeded);
        }
        self.used += requested;
        Ok(())
    }

    fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

#[derive(Clone, Debug)]
pub struct TransportHop {
    url: String,
    admission_addresses: Vec<IpAddr>,
    connect_addresses: Vec<IpAddr>,
    redirect: Option<String>,
    chunks: Vec<(Duration, Vec<u8>)>,
}

impl TransportHop {
    #[must_use]
    pub fn redirect(
        url: impl Into<String>,
        admission_addresses: Vec<IpAddr>,
        connect_addresses: Vec<IpAddr>,
        redirect: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            admission_addresses,
            connect_addresses,
            redirect: Some(redirect.into()),
            chunks: Vec::new(),
        }
    }

    #[must_use]
    pub fn success(
        url: impl Into<String>,
        admission_addresses: Vec<IpAddr>,
        connect_addresses: Vec<IpAddr>,
        chunks: Vec<Vec<u8>>,
    ) -> Self {
        Self::success_with_delays(
            url,
            admission_addresses,
            connect_addresses,
            chunks
                .into_iter()
                .map(|chunk| (Duration::ZERO, chunk))
                .collect(),
        )
    }

    #[must_use]
    pub fn success_with_delays(
        url: impl Into<String>,
        admission_addresses: Vec<IpAddr>,
        connect_addresses: Vec<IpAddr>,
        chunks: Vec<(Duration, Vec<u8>)>,
    ) -> Self {
        Self {
            url: url.into(),
            admission_addresses,
            connect_addresses,
            redirect: None,
            chunks,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransportScript {
    hops: Vec<TransportHop>,
    connected_hops: Arc<AtomicUsize>,
    bytes_read: Arc<AtomicUsize>,
}

impl TransportScript {
    #[must_use]
    pub fn new(hops: Vec<TransportHop>) -> Self {
        Self {
            hops,
            connected_hops: Arc::new(AtomicUsize::new(0)),
            bytes_read: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[must_use]
    pub fn connected_hops(&self) -> usize {
        self.connected_hops.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn bytes_read(&self) -> usize {
        self.bytes_read.load(Ordering::Acquire)
    }
}

/// Runs deterministic hops through the same redirect, resolution, deadline,
/// and byte-budget rules used by the production controlled connector.
///
/// # Errors
///
/// Returns an error before connection for a non-public resolution and while
/// reading for deadline or transport-byte exhaustion.
pub fn evaluate_transport_script(
    script: &TransportScript,
    limits: &InstallLimits,
) -> Result<Vec<u8>, SkillInstallError> {
    let mut budget = TransferBudget::new(limits);
    let mut expected_url = script
        .hops
        .first()
        .map(|hop| hop.url.clone())
        .ok_or(SkillInstallError::FetchFailed)?;
    let mut output = Vec::new();
    for hop in &script.hops {
        if hop.url != expected_url {
            return Err(SkillInstallError::InvalidSource);
        }
        validate_https_hop(&hop.url)?;
        validate_addresses(&hop.admission_addresses)?;
        validate_addresses(&hop.connect_addresses)?;
        script.connected_hops.fetch_add(1, Ordering::AcqRel);
        if let Some(redirect) = &hop.redirect {
            validate_redirect_hop(&hop.url, redirect)?;
            expected_url.clone_from(redirect);
            continue;
        }
        for (delay, chunk) in &hop.chunks {
            if !delay.is_zero() {
                std::thread::sleep(*delay);
            }
            if budget.accept(chunk.len()).is_err() {
                if budget.is_expired() {
                    return Err(SkillInstallError::LimitExceeded);
                }
                let remaining = limits.max_transport_bytes.saturating_sub(output.len());
                let accepted = remaining.min(chunk.len());
                output.extend_from_slice(&chunk[..accepted]);
                script.bytes_read.fetch_add(accepted, Ordering::AcqRel);
                return Err(SkillInstallError::LimitExceeded);
            }
            output.extend_from_slice(chunk);
            script.bytes_read.fetch_add(chunk.len(), Ordering::AcqRel);
        }
        return Ok(output);
    }
    Err(SkillInstallError::FetchFailed)
}

pub(crate) fn validate_addresses(addresses: &[IpAddr]) -> Result<(), SkillInstallError> {
    if addresses.is_empty() || addresses.iter().copied().any(is_non_public_ip) {
        return Err(SkillInstallError::NonPublicSource);
    }
    Ok(())
}
