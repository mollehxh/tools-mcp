use super::{
    InstallLimits,
    transport::{InstallDeadline, TransferBudget, validate_addresses},
};
use gix_features::io::pipe;
use gix_transport::client::blocking_io::http::{self, PostBodyDataKind};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::any::Any;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::{Arc, Mutex, mpsc};

#[derive(Clone, Debug)]
struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let result = resolve_public(name.as_str(), 443).map(|addresses| {
            let addresses: Addrs = Box::new(addresses.into_iter());
            addresses
        });
        Box::pin(std::future::ready(result.map_err(|error| {
            Box::new(error) as Box<dyn std::error::Error + Send + Sync>
        })))
    }
}

pub(crate) struct ControlledHttp {
    request: mpsc::SyncSender<Request>,
    response: mpsc::Receiver<Response>,
    redirected_base_url: Arc<Mutex<Option<String>>>,
}

#[derive(Debug)]
struct Request {
    url: String,
    base_url: String,
    headers: reqwest::header::HeaderMap,
    upload: Option<PostBodyDataKind>,
}

struct Response {
    headers: pipe::Reader,
    body: pipe::Reader,
    upload: pipe::Writer,
}

impl ControlledHttp {
    pub(crate) fn new(
        limits: InstallLimits,
        deadline: InstallDeadline,
    ) -> Result<Self, super::SkillInstallError> {
        let (request_tx, request_rx) = mpsc::sync_channel::<Request>(0);
        let (response_tx, response_rx) = mpsc::sync_channel(0);
        let redirected_base_url: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let worker_redirect = Arc::clone(&redirected_base_url);
        let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            let target = attempt.url();
            let Some(host) = target.host_str() else {
                return attempt.error("redirect has no host");
            };
            let port = target.port_or_known_default().unwrap_or(443);
            if let Some(previous) = attempt.previous().last()
                && validate_redirect_hop(previous.as_str(), target.as_str()).is_err()
            {
                return attempt.error("unsafe Git redirect");
            }
            match resolve_public(host, port) {
                Ok(_) => attempt.follow(),
                Err(_) => attempt.error("redirect resolves to a non-public destination"),
            }
        });
        let client = reqwest::blocking::ClientBuilder::new()
            .dns_resolver(Arc::new(PublicResolver))
            .redirect(redirect_policy)
            .no_proxy()
            .timeout(limits.timeout)
            .connect_timeout(limits.timeout)
            .build()
            .map_err(|_| super::SkillInstallError::FetchFailed)?;
        spawn_worker(
            limits,
            deadline,
            client,
            request_rx,
            response_tx,
            worker_redirect,
        )?;
        Ok(Self {
            request: request_tx,
            response: response_rx,
            redirected_base_url,
        })
    }

    fn request(
        &mut self,
        url: &str,
        base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        upload: Option<PostBodyDataKind>,
    ) -> Result<http::PostResponse<pipe::Reader, pipe::Reader, pipe::Writer>, http::Error> {
        let mut header_map = reqwest::header::HeaderMap::new();
        for header in headers {
            insert_header(&mut header_map, header.as_ref());
        }
        self.request
            .send(Request {
                url: url.to_string(),
                base_url: base_url.to_string(),
                headers: header_map,
                upload,
            })
            .map_err(|_| http::Error::Detail {
                description: "controlled HTTPS worker stopped".to_string(),
            })?;
        let response = self.response.recv().map_err(|_| http::Error::Detail {
            description: "controlled HTTPS worker stopped".to_string(),
        })?;
        Ok(http::PostResponse {
            headers: response.headers,
            body: response.body,
            post_body: response.upload,
        })
    }
}

fn spawn_worker(
    limits: InstallLimits,
    deadline: InstallDeadline,
    client: reqwest::blocking::Client,
    request_rx: mpsc::Receiver<Request>,
    response_tx: mpsc::SyncSender<Response>,
    redirected_base_url: Arc<Mutex<Option<String>>>,
) -> Result<(), super::SkillInstallError> {
    std::thread::Builder::new()
        .name("skill-git-https".to_string())
        .spawn(move || {
            run_worker(
                &limits,
                deadline,
                &client,
                request_rx,
                &response_tx,
                &redirected_base_url,
            );
        })
        .map(|_| ())
        .map_err(|_| super::SkillInstallError::FetchFailed)
}

fn run_worker(
    limits: &InstallLimits,
    deadline: InstallDeadline,
    client: &reqwest::blocking::Client,
    request_rx: mpsc::Receiver<Request>,
    response_tx: &mpsc::SyncSender<Response>,
    redirected_base_url: &Mutex<Option<String>>,
) {
    let mut transfer_budget = TransferBudget::with_deadline(limits, deadline);
    for request in request_rx {
        let (upload_tx, mut upload_rx) = pipe::unidirectional(0);
        let (mut body_tx, body_rx) = pipe::unidirectional(0);
        let (mut headers_tx, headers_rx) = pipe::unidirectional(0);
        if response_tx
            .send(Response {
                headers: headers_rx,
                body: body_rx,
                upload: upload_tx,
            })
            .is_err()
        {
            break;
        }
        let mut upload = Vec::new();
        if request.upload.is_some() {
            if upload_rx
                .by_ref()
                .take((limits.max_transport_bytes + 1) as u64)
                .read_to_end(&mut upload)
                .is_err()
            {
                send_error(&mut headers_tx, "upload failed");
                continue;
            }
            if transfer_budget.accept(upload.len()).is_err() {
                send_error(&mut headers_tx, "transport budget exceeded");
                continue;
            }
        }
        let effective_url = redirected_base_url
            .lock()
            .ok()
            .and_then(|redirect| redirect.clone())
            .map_or_else(
                || request.url.clone(),
                |base| swap_tails(&base, &request.base_url, &request.url),
            );
        let builder = if request.upload.is_some() {
            client.post(&effective_url).body(upload)
        } else {
            client.get(&effective_url)
        }
        .headers(request.headers.clone());
        let Ok(remaining) = deadline.remaining() else {
            send_error(&mut headers_tx, "installation deadline exceeded");
            continue;
        };
        let response = builder
            .timeout(remaining)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status);
        let Ok(mut response) = response else {
            send_error(&mut headers_tx, "controlled HTTPS request failed");
            continue;
        };
        record_redirect(redirected_base_url, &request, &response, &effective_url);
        write_headers(&mut headers_tx, &response);
        drop(headers_tx);
        if let Err(error) = copy_bounded(
            &mut response,
            &mut body_tx,
            &mut transfer_budget,
            limits.max_objects,
        ) {
            let _ = body_tx.channel.send(Err(error));
        }
    }
}

fn record_redirect(
    redirected_base_url: &Mutex<Option<String>>,
    request: &Request,
    response: &reqwest::blocking::Response,
    effective_url: &str,
) {
    if response.url().as_str() != effective_url
        && let Some(base) =
            derive_redirected_base(response.url().as_str(), &request.base_url, &request.url)
        && let Ok(mut redirected) = redirected_base_url.lock()
    {
        *redirected = Some(base);
    }
}

fn write_headers(writer: &mut pipe::Writer, response: &reqwest::blocking::Response) {
    for (name, value) in response.headers() {
        if writer.write_all(name.as_str().as_bytes()).is_err()
            || writer.write_all(b":").is_err()
            || writer.write_all(value.as_bytes()).is_err()
            || writer.write_all(b"\n").is_err()
        {
            break;
        }
    }
}

pub(crate) fn validate_https_hop(value: &str) -> Result<(), super::SkillInstallError> {
    let url = url::Url::parse(value).map_err(|_| super::SkillInstallError::InvalidSource)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        return Err(super::SkillInstallError::InvalidSource);
    }
    Ok(())
}

pub(crate) fn validate_redirect_hop(
    previous: &str,
    target: &str,
) -> Result<(), super::SkillInstallError> {
    validate_https_hop(previous)?;
    validate_https_hop(target)?;
    let previous =
        url::Url::parse(previous).map_err(|_| super::SkillInstallError::InvalidSource)?;
    let target = url::Url::parse(target).map_err(|_| super::SkillInstallError::InvalidSource)?;
    if !same_git_tail(&previous, &target) {
        return Err(super::SkillInstallError::InvalidSource);
    }
    Ok(())
}

impl std::fmt::Debug for ControlledHttp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlledHttp")
            .finish_non_exhaustive()
    }
}

impl http::Http for ControlledHttp {
    type Headers = pipe::Reader;
    type ResponseBody = pipe::Reader;
    type PostBody = pipe::Writer;

    fn get(
        &mut self,
        url: &str,
        base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<http::GetResponse<Self::Headers, Self::ResponseBody>, http::Error> {
        self.request(url, base_url, headers, None).map(Into::into)
    }

    fn post(
        &mut self,
        url: &str,
        base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        body: PostBodyDataKind,
    ) -> Result<http::PostResponse<Self::Headers, Self::ResponseBody, Self::PostBody>, http::Error>
    {
        self.request(url, base_url, headers, Some(body))
    }

    fn configure(
        &mut self,
        _config: &dyn Any,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        // Ambient Git HTTP configuration must not change the controlled client.
        Ok(())
    }

    fn redirected_base_url(&self) -> Option<String> {
        self.redirected_base_url
            .lock()
            .ok()
            .and_then(|url| url.clone())
    }
}

fn resolve_public(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    let addresses = (host, port).to_socket_addrs()?.collect::<Vec<_>>();
    let ips = addresses
        .iter()
        .map(SocketAddr::ip)
        .collect::<Vec<IpAddr>>();
    validate_addresses(&ips).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "destination is not public",
        )
    })?;
    Ok(addresses)
}

fn same_git_tail(previous: &reqwest::Url, target: &reqwest::Url) -> bool {
    const TAILS: [&str; 2] = ["/info/refs", "/git-upload-pack"];
    TAILS
        .iter()
        .any(|tail| previous.path().ends_with(tail) && target.path().ends_with(tail))
}

fn copy_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    budget: &mut TransferBudget,
    max_objects: usize,
) -> std::io::Result<()> {
    let mut buffer = [0u8; 16 * 1024];
    let mut body = Vec::new();
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            validate_pack_headers(&body, max_objects)?;
            return writer.write_all(&body);
        }
        budget
            .accept(count)
            .map_err(|_| std::io::Error::other("transport budget exceeded"))?;
        body.extend_from_slice(&buffer[..count]);
    }
}

fn validate_pack_headers(body: &[u8], max_objects: usize) -> std::io::Result<()> {
    validate_pack_headers_in_stream(body, max_objects)?;
    let pkt_line_stream = pkt_line_data_stream(body);
    validate_pack_headers_in_stream(&pkt_line_stream, max_objects)
}

fn validate_pack_headers_in_stream(bytes: &[u8], max_objects: usize) -> std::io::Result<()> {
    const HEADER_BYTES: usize = 12;
    for magic in bytes
        .windows(4)
        .enumerate()
        .filter_map(|(offset, window)| (window == b"PACK").then_some(offset))
    {
        let Some(header) = bytes.get(magic..magic + HEADER_BYTES) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated pack header",
            ));
        };
        let version = u32::from_be_bytes(header[4..8].try_into().expect("fixed slice"));
        let objects = u32::from_be_bytes(header[8..12].try_into().expect("fixed slice"));
        if matches!(version, 2 | 3)
            && usize::try_from(objects).map_or(true, |objects| objects > max_objects)
        {
            return Err(std::io::Error::other("pack object limit exceeded"));
        }
    }
    Ok(())
}

fn pkt_line_data_stream(body: &[u8]) -> Vec<u8> {
    let mut offset = 0usize;
    let mut stream = Vec::new();
    while let Some(prefix) = body.get(offset..offset.saturating_add(4)) {
        let Ok(prefix) = std::str::from_utf8(prefix) else {
            break;
        };
        let Ok(length) = usize::from_str_radix(prefix, 16) else {
            break;
        };
        if length <= 2 {
            offset += 4;
            continue;
        }
        if length < 4 {
            break;
        }
        let Some(packet) = body.get(offset + 4..offset.saturating_add(length)) else {
            break;
        };
        match packet.first().copied() {
            Some(1) => stream.extend_from_slice(&packet[1..]),
            Some(2 | 3) => {}
            _ => stream.extend_from_slice(packet),
        }
        offset += length;
    }
    stream
}

fn insert_header(headers: &mut reqwest::header::HeaderMap, line: &str) {
    let Some((name, value)) = line.split_once(':') else {
        return;
    };
    if let Some((name, value)) = reqwest::header::HeaderName::from_str(name)
        .ok()
        .zip(reqwest::header::HeaderValue::try_from(value.trim()).ok())
    {
        headers.append(name, value);
    }
}

fn send_error(writer: &mut pipe::Writer, message: &str) {
    let _ = writer.channel.send(Err(std::io::Error::other(message)));
}

fn swap_tails(redirected_base: &str, base: &str, url: &str) -> String {
    url.strip_prefix(base).map_or_else(
        || url.to_string(),
        |tail| format!("{redirected_base}{tail}"),
    )
}

fn derive_redirected_base(redirect: &str, base: &str, original: &str) -> Option<String> {
    let tail = original.strip_prefix(base)?;
    redirect
        .strip_suffix(tail)
        .map(std::string::ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::validate_pack_headers;

    #[test]
    fn pack_guard_rejects_oversized_header_before_forwarding_it() {
        let mut header = Vec::from(*b"PACK");
        header.extend_from_slice(&2u32.to_be_bytes());
        header.extend_from_slice(&17u32.to_be_bytes());

        assert_eq!(header.len(), 12);
        assert!(validate_pack_headers(&header, 16).is_err());
    }

    #[test]
    fn pack_guard_reconstructs_headers_split_across_sideband_packets() {
        let packet = |payload: &[u8]| {
            let mut packet = format!("{:04x}", payload.len() + 5).into_bytes();
            packet.push(1);
            packet.extend_from_slice(payload);
            packet
        };
        let mut body = packet(b"PACK");
        let mut tail = 2u32.to_be_bytes().to_vec();
        tail.extend_from_slice(&17u32.to_be_bytes());
        body.extend_from_slice(&packet(&tail));
        body.extend_from_slice(b"0000");

        assert!(validate_pack_headers(&body, 16).is_err());
    }

    #[test]
    fn pack_guard_accepts_bounded_raw_and_sideband_headers() {
        let mut raw = Vec::from(*b"PACK");
        raw.extend_from_slice(&2u32.to_be_bytes());
        raw.extend_from_slice(&16u32.to_be_bytes());
        assert!(validate_pack_headers(&raw, 16).is_ok());

        let mut framed = b"0009\x01PACK000d\x01".to_vec();
        framed.extend_from_slice(&2u32.to_be_bytes());
        framed.extend_from_slice(&16u32.to_be_bytes());
        assert!(validate_pack_headers(&framed, 16).is_ok());
    }

    #[test]
    fn pack_guard_rejects_oversized_version_three_header() {
        let mut header = Vec::from(*b"PACK");
        header.extend_from_slice(&3u32.to_be_bytes());
        header.extend_from_slice(&17u32.to_be_bytes());

        assert!(validate_pack_headers(&header, 16).is_err());
    }
}
