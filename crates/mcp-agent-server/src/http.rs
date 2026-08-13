use crate::{AgentHandler, ApplicationContext};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, ORIGIN};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use futures_util::StreamExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub const MCP_ENDPOINT: &str = "/mcp";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpConfig {
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub max_request_body_bytes: usize,
    pub max_header_bytes: usize,
    pub max_header_count: usize,
    pub max_in_flight_requests: usize,
    pub max_sse_responses: usize,
    pub upload_idle_timeout: Duration,
    pub response_idle_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
            allowed_origins: Vec::new(),
            max_request_body_bytes: 4 * 1024 * 1024,
            max_header_bytes: 64 * 1024,
            max_header_count: 100,
            max_in_flight_requests: 32,
            max_sse_responses: 16,
            upload_idle_timeout: Duration::from_secs(15),
            response_idle_timeout: Duration::from_mins(2),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HttpConfigError {
    #[error("public Host validation cannot contain a wildcard")]
    WildcardHost,
    #[error("Origin validation cannot contain a wildcard")]
    WildcardOrigin,
    #[error("HTTP admission limits must be nonzero")]
    ZeroLimit,
}

impl HttpConfig {
    /// Validates security-sensitive allowlists and admission bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for wildcard validation or a zero admission limit.
    pub fn validate(&self) -> Result<(), HttpConfigError> {
        if self.allowed_hosts.iter().any(|host| host.trim() == "*") {
            return Err(HttpConfigError::WildcardHost);
        }
        if self
            .allowed_origins
            .iter()
            .any(|origin| origin.trim() == "*")
        {
            return Err(HttpConfigError::WildcardOrigin);
        }
        if self.allowed_hosts.is_empty()
            || self.max_request_body_bytes == 0
            || self.max_header_bytes == 0
            || self.max_header_count == 0
            || self.max_in_flight_requests == 0
            || self.max_sse_responses == 0
            || self.upload_idle_timeout.is_zero()
            || self.response_idle_timeout.is_zero()
        {
            return Err(HttpConfigError::ZeroLimit);
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Admission {
    requests: Arc<Semaphore>,
    sse: Arc<Semaphore>,
    max_header_bytes: usize,
    max_header_count: usize,
    max_request_body_bytes: usize,
    upload_idle_timeout: Duration,
    response_idle_timeout: Duration,
    allowed_origins: Arc<[String]>,
}

/// Builds the fixed `/mcp` Streamable HTTP router.
///
/// # Errors
///
/// Returns an error when the HTTP security configuration is unsafe.
pub fn router(
    context: Arc<ApplicationContext>,
    config: HttpConfig,
    cancellation_token: CancellationToken,
) -> Result<Router, HttpConfigError> {
    config.validate()?;
    let rmcp_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_max_request_body_bytes(config.max_request_body_bytes)
        .with_allowed_hosts(config.allowed_hosts)
        .with_allowed_origins(config.allowed_origins.clone())
        .with_cancellation_token(cancellation_token);
    let service: StreamableHttpService<AgentHandler, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(AgentHandler::new(Arc::clone(&context))),
            Arc::new(LocalSessionManager::default()),
            rmcp_config,
        );
    let admission = Admission {
        requests: Arc::new(Semaphore::new(config.max_in_flight_requests)),
        sse: Arc::new(Semaphore::new(config.max_sse_responses)),
        max_header_bytes: config.max_header_bytes,
        max_header_count: config.max_header_count,
        max_request_body_bytes: config.max_request_body_bytes,
        upload_idle_timeout: config.upload_idle_timeout,
        response_idle_timeout: config.response_idle_timeout,
        allowed_origins: config.allowed_origins.into(),
    };
    Ok(Router::new()
        .nest_service(MCP_ENDPOINT, service)
        .layer(from_fn_with_state(admission, enforce_admission)))
}

async fn enforce_admission(
    State(admission): State<Admission>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let header_bytes = headers
        .iter()
        .map(|(name, value)| name.as_str().len().saturating_add(value.as_bytes().len()))
        .sum::<usize>();
    if headers.len() > admission.max_header_count || header_bytes > admission.max_header_bytes {
        return plain_response(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "request headers exceed the configured limit",
        );
    }
    if let Some(origin) = headers.get(ORIGIN) {
        let allowed = origin.to_str().ok().is_some_and(|origin| {
            admission
                .allowed_origins
                .iter()
                .any(|allowed| allowed == origin)
        });
        if !allowed {
            return plain_response(StatusCode::FORBIDDEN, "request Origin is not allowed");
        }
    }
    let Ok(request_permit) = Arc::clone(&admission.requests).try_acquire_owned() else {
        return plain_response(
            StatusCode::TOO_MANY_REQUESTS,
            "request concurrency limit reached",
        );
    };
    let sse_permit = None;
    let request = match buffer_request_body(
        request,
        admission.max_request_body_bytes,
        admission.upload_idle_timeout,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    let Ok(response) =
        tokio::time::timeout(admission.response_idle_timeout, next.run(request)).await
    else {
        return plain_response(StatusCode::GATEWAY_TIMEOUT, "response timed out");
    };
    let is_sse = response.headers().get(CONTENT_TYPE).is_some_and(|value| {
        value
            .to_str()
            .ok()
            .is_some_and(|value| value.starts_with("text/event-stream"))
    });
    let sse_permit = if is_sse && sse_permit.is_none() {
        match Arc::clone(&admission.sse).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => return too_many_sse_responses(),
        }
    } else {
        sse_permit
    };
    hold_permits_and_enforce_response_idle(
        response,
        request_permit,
        sse_permit,
        admission.response_idle_timeout,
    )
}

async fn buffer_request_body(
    request: Request,
    max_bytes: usize,
    idle_timeout: Duration,
) -> Result<Request, Response> {
    if request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > max_bytes)
    {
        return Err(payload_too_large());
    }
    let (parts, body) = request.into_parts();
    let mut stream = body.into_data_stream();
    let mut buffered = Vec::new();
    loop {
        match tokio::time::timeout(idle_timeout, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                if buffered.len().saturating_add(chunk.len()) > max_bytes {
                    return Err(payload_too_large());
                }
                buffered.extend_from_slice(&chunk);
            }
            Ok(Some(Err(_))) => {
                return Err(plain_response(
                    StatusCode::BAD_REQUEST,
                    "request body could not be read",
                ));
            }
            Ok(None) => return Ok(Request::from_parts(parts, Body::from(buffered))),
            Err(_) => {
                return Err(plain_response(
                    StatusCode::REQUEST_TIMEOUT,
                    "request upload timed out",
                ));
            }
        }
    }
}

fn hold_permits_and_enforce_response_idle(
    response: Response,
    request_permit: tokio::sync::OwnedSemaphorePermit,
    sse_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    idle_timeout: Duration,
) -> Response {
    let (parts, body) = response.into_parts();
    let stream = Box::pin(body.into_data_stream());
    let state = (stream, Some((request_permit, sse_permit)), false);
    let stream =
        futures_util::stream::unfold(state, move |(mut stream, permits, terminated)| async move {
            if terminated {
                return None;
            }
            match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    Some((Ok::<_, axum::BoxError>(chunk), (stream, permits, false)))
                }
                Ok(Some(Err(error))) => Some((
                    Err::<axum::body::Bytes, _>(error.into()),
                    (stream, permits, true),
                )),
                Ok(None) => None,
                Err(_) => Some((
                    Err::<axum::body::Bytes, _>(
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "response body idle timeout",
                        )
                        .into(),
                    ),
                    (stream, permits, true),
                )),
            }
        });
    Response::from_parts(parts, Body::from_stream(stream))
}

fn payload_too_large() -> Response {
    plain_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "request body exceeds the configured limit",
    )
}

fn plain_response(status: StatusCode, message: &'static str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(message))
        .expect("static HTTP response is valid")
}

fn too_many_sse_responses() -> Response {
    plain_response(
        StatusCode::TOO_MANY_REQUESTS,
        "SSE concurrency limit reached",
    )
}

#[cfg(test)]
mod tests {
    use super::{hold_permits_and_enforce_response_idle, too_many_sse_responses};
    use axum::body::{Body, Bytes, to_bytes};
    use axum::http::StatusCode;
    use axum::response::Response;
    use futures_util::StreamExt;
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn response_body_idle_timeout_releases_request_and_sse_permits() {
        let requests = Arc::new(Semaphore::new(1));
        let sse = Arc::new(Semaphore::new(1));
        let request_permit = Arc::clone(&requests).acquire_owned().await.unwrap();
        let sse_permit = Arc::clone(&sse).acquire_owned().await.unwrap();
        let body = futures_util::stream::once(async {
            Ok::<_, Infallible>(Bytes::from_static(b"event: ready\n\n"))
        })
        .chain(futures_util::stream::pending());
        let response = Response::new(Body::from_stream(body));
        let response = hold_permits_and_enforce_response_idle(
            response,
            request_permit,
            Some(sse_permit),
            Duration::from_millis(20),
        );

        assert!(Arc::clone(&requests).try_acquire_owned().is_err());
        assert!(Arc::clone(&sse).try_acquire_owned().is_err());
        let body_error =
            tokio::time::timeout(Duration::from_secs(1), to_bytes(response.into_body(), 1024))
                .await
                .expect("the body wrapper must enforce its own timeout")
                .expect_err("a stalled response body must terminate with an error");
        assert!(
            body_error
                .to_string()
                .contains("response body idle timeout")
        );
        assert_eq!(requests.available_permits(), 1);
        assert_eq!(sse.available_permits(), 1);
        assert_eq!(
            too_many_sse_responses().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
