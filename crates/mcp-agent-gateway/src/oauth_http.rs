//! Self-hosted OAuth HTTP adapter over durable hashed grant storage.

use crate::{AuthError, AuthStore, AuthorizationGrant, Observability, TokenPair};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Form, Query, Request, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, COOKIE, PRAGMA, REFERRER_POLICY,
    SET_COOKIE, WWW_AUTHENTICATE, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq as _;
use tokio::sync::Semaphore;
use url::Url;

const REQUIRED_SCOPE: &str = "offline_access";
const AUTHORIZATION_LIFETIME: Duration = Duration::from_mins(5);
const CODE_LIFETIME: Duration = Duration::from_mins(1);
const MAX_PENDING_AUTHORIZATIONS: usize = 64;
const MAX_STATE_BYTES: usize = 512;
const MAX_OWNER_FAILURES: usize = 5;
const MAX_CONCURRENT_OWNER_VERIFICATIONS: usize = 2;

#[derive(Clone, Debug)]
pub struct OAuthHttpConfig {
    issuer: Url,
    resource: Url,
    client_id: String,
    redirect_uri: Url,
}

impl OAuthHttpConfig {
    /// Validates exact HTTPS OAuth bindings.
    ///
    /// # Errors
    ///
    /// Returns an error for non-HTTPS, userinfo-bearing, fragmented, or non-origin values.
    pub fn new(
        issuer: &str,
        client_id: String,
        redirect_uri: &str,
    ) -> Result<Self, url::ParseError> {
        let issuer = Url::parse(issuer)?;
        let redirect_uri = Url::parse(redirect_uri)?;
        let client = Url::parse(&client_id)?;
        if issuer.scheme() != "https"
            || issuer.host_str().is_none()
            || !valid_issuer_path(issuer.path())
            || issuer.query().is_some()
            || issuer.fragment().is_some()
            || !issuer.username().is_empty()
            || issuer.password().is_some()
        {
            return Err(url::ParseError::RelativeUrlWithoutBase);
        }
        if client.scheme() != "https"
            || client.host_str().is_none()
            || client.fragment().is_some()
            || !client.username().is_empty()
            || client.password().is_some()
        {
            return Err(url::ParseError::RelativeUrlWithoutBase);
        }
        if redirect_uri.scheme() != "https"
            || redirect_uri.host_str().is_none()
            || redirect_uri.fragment().is_some()
            || !redirect_uri.username().is_empty()
            || redirect_uri.password().is_some()
        {
            return Err(url::ParseError::RelativeUrlWithoutBase);
        }
        let resource = issuer.join("mcp")?;
        Ok(Self {
            issuer,
            resource,
            client_id,
            redirect_uri,
        })
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        self.issuer.as_str().trim_end_matches('/')
    }
    #[must_use]
    pub fn resource(&self) -> &str {
        self.resource.as_str()
    }

    fn endpoint(&self, name: &str) -> Url {
        self.issuer
            .join(name)
            .expect("validated relative OAuth endpoint is valid")
    }
}

fn valid_issuer_path(path: &str) -> bool {
    path.starts_with('/')
        && path.ends_with('/')
        && !path.contains("//")
        && path.split('/').filter(|part| !part.is_empty()).all(|part| {
            part.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
}

#[derive(Clone)]
pub struct OAuthHttpService {
    config: Arc<OAuthHttpConfig>,
    store: Arc<AuthStore>,
    state: Arc<Mutex<EphemeralState>>,
    owner_verifications: Arc<Semaphore>,
    observability: Option<Arc<Observability>>,
}

#[derive(Default)]
struct EphemeralState {
    authorizations: HashMap<String, PendingAuthorization>,
}

struct PendingAuthorization {
    client_id: String,
    redirect_uri: String,
    resource: String,
    scope: String,
    state: Option<String>,
    code_challenge: String,
    expires_at: Instant,
    owner_failures: usize,
}

impl OAuthHttpService {
    #[must_use]
    pub fn new(config: OAuthHttpConfig, store: Arc<AuthStore>) -> Self {
        Self {
            config: Arc::new(config),
            store,
            state: Arc::new(Mutex::new(EphemeralState::default())),
            owner_verifications: Arc::new(Semaphore::new(MAX_CONCURRENT_OWNER_VERIFICATIONS)),
            observability: None,
        }
    }

    #[must_use]
    pub fn with_observability(mut self, observability: Arc<Observability>) -> Self {
        self.observability = Some(observability);
        self
    }

    pub fn routes(&self) -> Router {
        Router::new()
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(authorization_server_metadata),
            )
            .route(
                "/.well-known/openid-configuration",
                get(|| async { StatusCode::NOT_FOUND }),
            )
            .route("/oauth-client/chatgpt.json", get(client_metadata_document))
            .route("/authorize", get(authorize).post(consent))
            .route("/token", post(token))
            .with_state(self.clone())
    }

    #[must_use]
    pub fn issuer(&self) -> &Url {
        &self.config.issuer
    }

    fn lock(&self) -> MutexGuard<'_, EphemeralState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Serialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Vec<String>,
    bearer_methods_supported: Vec<String>,
}
#[derive(Serialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    response_types_supported: Vec<String>,
    grant_types_supported: Vec<String>,
    code_challenge_methods_supported: Vec<String>,
    scopes_supported: Vec<String>,
    token_endpoint_auth_methods_supported: Vec<String>,
    client_id_metadata_document_supported: bool,
    authorization_response_iss_parameter_supported: bool,
}
#[derive(Serialize)]
struct ClientMetadataDocument {
    client_id: String,
    client_name: &'static str,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: &'static str,
    grant_types: Vec<&'static str>,
    response_types: Vec<&'static str>,
    scope: &'static str,
}

async fn protected_resource_metadata(
    State(service): State<OAuthHttpService>,
) -> Json<ProtectedResourceMetadata> {
    Json(ProtectedResourceMetadata {
        resource: service.config.resource().to_owned(),
        authorization_servers: vec![service.config.issuer().to_owned()],
        scopes_supported: vec![REQUIRED_SCOPE.to_owned()],
        bearer_methods_supported: vec!["header".to_owned()],
    })
}
async fn authorization_server_metadata(
    State(service): State<OAuthHttpService>,
) -> Json<AuthorizationServerMetadata> {
    Json(AuthorizationServerMetadata {
        issuer: service.config.issuer().to_owned(),
        authorization_endpoint: service.config.endpoint("authorize").to_string(),
        token_endpoint: service.config.endpoint("token").to_string(),
        response_types_supported: vec!["code".to_owned()],
        grant_types_supported: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
        code_challenge_methods_supported: vec!["S256".to_owned()],
        scopes_supported: vec![REQUIRED_SCOPE.to_owned()],
        token_endpoint_auth_methods_supported: vec!["none".to_owned()],
        client_id_metadata_document_supported: true,
        authorization_response_iss_parameter_supported: true,
    })
}
async fn client_metadata_document(
    State(service): State<OAuthHttpService>,
) -> Json<ClientMetadataDocument> {
    Json(ClientMetadataDocument {
        client_id: service.config.client_id.clone(),
        client_name: "tools-mcp",
        redirect_uris: vec![service.config.redirect_uri.to_string()],
        token_endpoint_auth_method: "none",
        grant_types: vec!["authorization_code", "refresh_token"],
        response_types: vec!["code"],
        scope: REQUIRED_SCOPE,
    })
}

#[derive(Deserialize)]
struct AuthorizationQuery {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    resource: String,
    scope: String,
    state: Option<String>,
    code_challenge: String,
    code_challenge_method: String,
}
#[derive(Deserialize)]
struct ConsentForm {
    request_id: String,
    #[serde(default)]
    owner_secret: Option<String>,
    decision: String,
}
#[derive(Deserialize)]
struct TokenForm {
    grant_type: String,
    client_id: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: String,
}

async fn authorize(
    State(service): State<OAuthHttpService>,
    Query(query): Query<AuthorizationQuery>,
) -> Result<Response, OAuthHttpError> {
    validate_authorization(&service.config, &query)?;
    let request_id = crate::auth::random_token();
    let mut state = service.lock();
    let now = Instant::now();
    state
        .authorizations
        .retain(|_, pending| pending.expires_at > now);
    if state.authorizations.len() >= MAX_PENDING_AUTHORIZATIONS {
        return Err(OAuthHttpError::rate_limited());
    }
    state.authorizations.insert(
        request_id.clone(),
        PendingAuthorization {
            client_id: query.client_id,
            redirect_uri: query.redirect_uri,
            resource: query.resource,
            scope: normalize_scope(&query.scope),
            state: query.state,
            code_challenge: query.code_challenge,
            expires_at: Instant::now() + AUTHORIZATION_LIFETIME,
            owner_failures: 0,
        },
    );
    drop(state);
    let authorization_path = service.config.endpoint("authorize").path().to_owned();
    let page = Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"referrer\" content=\"no-referrer\"><title>Authorize tools-mcp</title></head><body><h1>Authorize tools-mcp</h1><form method=\"post\" action=\"{authorization_path}\"><input type=\"hidden\" name=\"request_id\" value=\"{request_id}\"><label>Owner secret <input type=\"password\" name=\"owner_secret\" autocomplete=\"current-password\" required></label><button type=\"submit\" name=\"decision\" value=\"approve\">Authorize</button><button type=\"submit\" name=\"decision\" value=\"deny\" formnovalidate>Deny</button></form></body></html>"
    ));
    Ok((
        browser_security_headers(
            &request_id,
            &service.config.redirect_uri,
            &authorization_path,
        ),
        page,
    )
        .into_response())
}

async fn consent(
    State(service): State<OAuthHttpService>,
    headers: HeaderMap,
    Form(form): Form<ConsentForm>,
) -> Result<Response, OAuthHttpError> {
    let cookie_request = cookie_value(&headers, "tools_mcp_oauth");
    if !cookie_request.is_some_and(|value| {
        value.len() == form.request_id.len()
            && value.as_bytes().ct_eq(form.request_id.as_bytes()).into()
    }) {
        return Err(OAuthHttpError::invalid_request());
    }
    let now = Instant::now();
    if form.decision == "deny" {
        let pending = service
            .lock()
            .authorizations
            .remove(&form.request_id)
            .filter(|pending| pending.expires_at > now)
            .ok_or_else(OAuthHttpError::invalid_request)?;
        return denied_redirect(&service, pending);
    }
    if form.decision != "approve" {
        return Err(OAuthHttpError::invalid_request());
    }
    {
        let mut state = service.lock();
        let pending = state
            .authorizations
            .get_mut(&form.request_id)
            .filter(|pending| pending.expires_at > now);
        let Some(pending) = pending else {
            state.authorizations.remove(&form.request_id);
            return Err(OAuthHttpError::invalid_request());
        };
        if pending.owner_failures >= MAX_OWNER_FAILURES {
            return Err(OAuthHttpError::rate_limited());
        }
        pending.owner_failures += 1;
    }
    let _verification_permit = Arc::clone(&service.owner_verifications)
        .try_acquire_owned()
        .map_err(|_| OAuthHttpError::rate_limited())?;
    if !service
        .store
        .verify_owner_secret(form.owner_secret.as_deref().unwrap_or_default())
    {
        return Err(OAuthHttpError::access_denied());
    }
    let pending = service
        .lock()
        .authorizations
        .remove(&form.request_id)
        .filter(|pending| pending.expires_at > now)
        .ok_or_else(OAuthHttpError::invalid_request)?;
    let code = crate::auth::random_token();
    let mut redirect =
        Url::parse(&pending.redirect_uri).map_err(|_| OAuthHttpError::invalid_request())?;
    {
        let mut pairs = redirect.query_pairs_mut();
        pairs.append_pair("code", &code);
        if let Some(state) = &pending.state {
            pairs.append_pair("state", state);
        }
        pairs.append_pair("iss", service.config.issuer());
    }
    service
        .store
        .store_authorization_code(
            &code,
            &AuthorizationGrant {
                client_id: pending.client_id,
                redirect_uri: pending.redirect_uri,
                resource: pending.resource,
                scope: pending.scope,
                code_challenge: pending.code_challenge,
                expires_unix: unix_after(CODE_LIFETIME)?,
            },
        )
        .map_err(|error| OAuthHttpError::from_auth(&error))?;
    let mut response = Redirect::to(redirect.as_str()).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(SET_COOKIE, clear_cookie_header(&service.config));
    Ok(response)
}

fn denied_redirect(
    service: &OAuthHttpService,
    pending: PendingAuthorization,
) -> Result<Response, OAuthHttpError> {
    let mut redirect =
        Url::parse(&pending.redirect_uri).map_err(|_| OAuthHttpError::invalid_request())?;
    {
        let mut pairs = redirect.query_pairs_mut();
        pairs.append_pair("error", "access_denied");
        if let Some(state) = pending.state {
            pairs.append_pair("state", &state);
        }
        pairs.append_pair("iss", service.config.issuer());
    }
    let mut response = Redirect::to(redirect.as_str()).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(SET_COOKIE, clear_cookie_header(&service.config));
    Ok(response)
}

async fn token(State(service): State<OAuthHttpService>, Form(form): Form<TokenForm>) -> Response {
    let is_refresh = form.grant_type == "refresh_token";
    let result = match form.grant_type.as_str() {
        "authorization_code" => exchange_code(&service, &form),
        "refresh_token" => exchange_refresh(&service, &form),
        _ => Err(OAuthHttpError::unsupported_grant()),
    };
    if is_refresh && let Some(observability) = &service.observability {
        observability.record_refresh(
            result.is_ok(),
            result.as_ref().err().map(|error| error.error),
        );
    }
    match result {
        Ok(tokens) => token_response(tokens),
        Err(error) => error.into_response(),
    }
}

fn exchange_code(
    service: &OAuthHttpService,
    form: &TokenForm,
) -> Result<TokenPair, OAuthHttpError> {
    validate_binding(&service.config, form)?;
    let code = form
        .code
        .as_deref()
        .ok_or_else(OAuthHttpError::invalid_request)?;
    let redirect = form
        .redirect_uri
        .as_deref()
        .ok_or_else(OAuthHttpError::invalid_request)?;
    let verifier = form
        .code_verifier
        .as_deref()
        .filter(|value| valid_pkce(value))
        .ok_or_else(OAuthHttpError::invalid_grant)?;
    service
        .store
        .exchange_authorization_code(
            code,
            &form.client_id,
            redirect,
            &form.resource,
            &pkce_challenge(verifier),
        )
        .map_err(|error| OAuthHttpError::from_auth(&error))
}

fn exchange_refresh(
    service: &OAuthHttpService,
    form: &TokenForm,
) -> Result<TokenPair, OAuthHttpError> {
    validate_binding(&service.config, form)?;
    let refresh = form
        .refresh_token
        .as_deref()
        .ok_or_else(OAuthHttpError::invalid_request)?;
    service
        .store
        .refresh(refresh)
        .map_err(|error| OAuthHttpError::from_auth(&error))
}

fn validate_authorization(
    config: &OAuthHttpConfig,
    query: &AuthorizationQuery,
) -> Result<(), OAuthHttpError> {
    if query.response_type != "code"
        || query.client_id != config.client_id
        || query.redirect_uri != config.redirect_uri.as_str()
        || query.resource != config.resource()
        || query.code_challenge_method != "S256"
        || !valid_pkce(&query.code_challenge)
        || query.state.as_ref().is_some_and(|state| {
            state.len() > MAX_STATE_BYTES || state.chars().any(char::is_control)
        })
    {
        return Err(OAuthHttpError::invalid_request());
    }
    if normalize_scope(&query.scope) != REQUIRED_SCOPE {
        return Err(OAuthHttpError::invalid_scope());
    }
    Ok(())
}

fn validate_binding(config: &OAuthHttpConfig, form: &TokenForm) -> Result<(), OAuthHttpError> {
    if form.client_id != config.client_id {
        return Err(OAuthHttpError::invalid_client());
    }
    if form.resource != config.resource() {
        return Err(OAuthHttpError::invalid_target());
    }
    Ok(())
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
    refresh_token: String,
    scope: String,
}
fn token_response(pair: TokenPair) -> Response {
    (
        StatusCode::OK,
        [
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (PRAGMA, HeaderValue::from_static("no-cache")),
        ],
        Json(TokenResponse {
            access_token: pair.access_token,
            token_type: "Bearer",
            expires_in: pair.expires_in,
            refresh_token: pair.refresh_token,
            scope: pair.scope,
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    error_description: &'static str,
}
#[derive(Debug)]
struct OAuthHttpError {
    status: StatusCode,
    error: &'static str,
    description: &'static str,
}
impl OAuthHttpError {
    const fn invalid_request() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "OAuth request is invalid",
        )
    }
    const fn invalid_client() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "OAuth client is invalid",
        )
    }
    const fn invalid_grant() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "OAuth grant is invalid",
        )
    }
    const fn invalid_scope() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_scope",
            "offline_access is required",
        )
    }
    const fn invalid_target() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "OAuth resource is invalid",
        )
    }
    const fn unsupported_grant() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "OAuth grant type is unsupported",
        )
    }
    const fn access_denied() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "access_denied",
            "owner authentication failed",
        )
    }
    const fn rate_limited() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
            "too many owner authentication attempts",
        )
    }
    const fn new(status: StatusCode, error: &'static str, description: &'static str) -> Self {
        Self {
            status,
            error,
            description,
        }
    }
    fn from_auth(error: &AuthError) -> Self {
        match error {
            AuthError::InvalidGrant => Self::invalid_grant(),
            AuthError::AccessDenied => Self::access_denied(),
            AuthError::Storage(_) | AuthError::InvalidOwnerHash | AuthError::Clock => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                "OAuth state is unavailable",
            ),
        }
    }
}
impl IntoResponse for OAuthHttpError {
    fn into_response(self) -> Response {
        (
            self.status,
            [(CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            Json(ErrorBody {
                error: self.error,
                error_description: self.description,
            }),
        )
            .into_response()
    }
}

/// Protects MCP with the durable opaque bearer-token store.
pub async fn require_bearer(
    State(service): State<OAuthHttpService>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let authority = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| service.store.resolve_access(token).ok().flatten());
    if let Some(authority) = authority {
        request
            .extensions_mut()
            .insert(mcp_agent_server::http::AuthenticatedPrincipal {
                principal_fingerprint: authority.principal_fingerprint(),
            });
        return next.run(request).await;
    }
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(ErrorBody {
            error: "invalid_token",
            error_description: "a valid bearer token is required",
        }),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&format!(
        "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
        service.config.issuer()
    )) {
        response.headers_mut().insert(WWW_AUTHENTICATE, value);
    }
    response
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
fn valid_pkce(value: &str) -> bool {
    (43..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}
fn normalize_scope(scope: &str) -> String {
    let mut scopes = scope.split_ascii_whitespace().collect::<Vec<_>>();
    scopes.sort_unstable();
    scopes.dedup();
    scopes.join(" ")
}

fn unix_after(duration: Duration) -> Result<i64, OAuthHttpError> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| OAuthHttpError::invalid_request())?
        .checked_add(duration)
        .ok_or_else(OAuthHttpError::invalid_request)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| OAuthHttpError::invalid_request())
}

fn browser_security_headers(
    request_id: &str,
    redirect_uri: &Url,
    authorization_path: &str,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let callback_origin = redirect_uri.origin().ascii_serialization();
    let content_security_policy = format!(
        "default-src 'none'; form-action 'self' {callback_origin}; frame-ancestors 'none'; base-uri 'none'"
    );
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&content_security_policy)
            .expect("validated HTTPS redirect origin is a valid CSP source"),
    );
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    let cookie = format!(
        "tools_mcp_oauth={request_id}; Path={authorization_path}; Max-Age=300; Secure; HttpOnly; SameSite=Strict"
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        headers.insert(SET_COOKIE, value);
    }
    headers
}

fn clear_cookie_header(config: &OAuthHttpConfig) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "tools_mcp_oauth=; Path={}; Max-Age=0; Secure; HttpOnly; SameSite=Strict",
        config.endpoint("authorize").path()
    ))
    .expect("validated OAuth path is a valid cookie path")
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(candidate, value)| (candidate == name).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::Argon2;
    use argon2::password_hash::{PasswordHasher, SaltString};
    use axum::body::to_bytes;
    use axum::http::header::LOCATION;
    use std::time::Duration;

    fn service() -> (tempfile::TempDir, OAuthHttpService) {
        let root = tempfile::tempdir().unwrap();
        let salt = SaltString::encode_b64(b"0123456789abcdef").unwrap();
        let owner_secret_phc = Argon2::default()
            .hash_password(b"owner-secret", &salt)
            .unwrap()
            .to_string();
        let client_id = "https://chatgpt.com/oauth/test/client.json".to_owned();
        let resource = "https://example.ngrok-free.dev/mcp".to_owned();
        let store = Arc::new(
            AuthStore::open(
                &root.path().join("gateway.sqlite3"),
                crate::AuthConfig {
                    client_id: client_id.clone(),
                    resource,
                    owner_secret_phc,
                    token_hash_key: vec![7; 32],
                    access_lifetime: Duration::from_mins(1),
                    refresh_lifetime: Duration::from_hours(1),
                },
            )
            .unwrap(),
        );
        let config = OAuthHttpConfig::new(
            "https://example.ngrok-free.dev/",
            client_id,
            "https://chatgpt.com/connector/oauth/test",
        )
        .unwrap();
        (root, OAuthHttpService::new(config, store))
    }

    fn prefixed_service() -> (tempfile::TempDir, OAuthHttpService) {
        let root = tempfile::tempdir().unwrap();
        let salt = SaltString::encode_b64(b"fedcba9876543210").unwrap();
        let owner_secret_phc = Argon2::default()
            .hash_password(b"friend-secret", &salt)
            .unwrap()
            .to_string();
        let client_id = "https://chatgpt.com/oauth/client.json".to_owned();
        let config = OAuthHttpConfig::new(
            "https://example.ngrok-free.dev/friend/",
            client_id.clone(),
            "https://chatgpt.com/connector_platform_oauth_redirect",
        )
        .unwrap();
        let store = Arc::new(
            AuthStore::open(
                &root.path().join("gateway.sqlite3"),
                crate::AuthConfig {
                    client_id,
                    resource: config.resource().to_owned(),
                    owner_secret_phc,
                    token_hash_key: vec![9; 32],
                    access_lifetime: Duration::from_mins(1),
                    refresh_lifetime: Duration::from_hours(1),
                },
            )
            .unwrap(),
        );
        (root, OAuthHttpService::new(config, store))
    }

    #[tokio::test]
    async fn metadata_advertises_issuer_parameter_returned_by_authorization() {
        let (_root, service) = service();
        let metadata =
            serde_json::to_value(authorization_server_metadata(State(service)).await.0).unwrap();

        assert_eq!(
            metadata["authorization_response_iss_parameter_supported"],
            true
        );
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn consent_code_refresh_flow_is_bound_and_one_time() {
        let (_root, service) = service();
        let observability = Arc::new(Observability::test_instance());
        let service = service.with_observability(Arc::clone(&observability));
        let verifier = "a".repeat(43);
        let challenge = pkce_challenge(&verifier);
        let response = authorize(
            State(service.clone()),
            Query(AuthorizationQuery {
                response_type: "code".to_owned(),
                client_id: service.config.client_id.clone(),
                redirect_uri: service.config.redirect_uri.to_string(),
                resource: service.config.resource().to_owned(),
                scope: "offline_access".to_owned(),
                state: Some("chatgpt-state".to_owned()),
                code_challenge: challenge,
                code_challenge_method: "S256".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[CONTENT_SECURITY_POLICY],
            "default-src 'none'; form-action 'self' https://chatgpt.com; frame-ancestors 'none'; base-uri 'none'"
        );
        let set_cookie = response.headers()[SET_COOKIE].to_str().unwrap();
        let request_id = set_cookie
            .strip_prefix("tools_mcp_oauth=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let form = ConsentForm {
            request_id: request_id.clone(),
            owner_secret: Some("owner-secret".to_owned()),
            decision: "approve".to_owned(),
        };
        assert!(
            consent(State(service.clone()), HeaderMap::new(), Form(form))
                .await
                .is_err()
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("tools_mcp_oauth={request_id}")).unwrap(),
        );
        let response = consent(
            State(service.clone()),
            headers,
            Form(ConsentForm {
                request_id,
                owner_secret: Some("owner-secret".to_owned()),
                decision: "approve".to_owned(),
            }),
        )
        .await
        .unwrap();
        let location = Url::parse(response.headers()[LOCATION].to_str().unwrap()).unwrap();
        let code = location
            .query_pairs()
            .find_map(|(key, value)| (key == "code").then(|| value.into_owned()))
            .unwrap();
        assert!(location.as_str().contains("state=chatgpt-state"));

        let exchange = || TokenForm {
            grant_type: "authorization_code".to_owned(),
            client_id: service.config.client_id.clone(),
            code: Some(code.clone()),
            redirect_uri: Some(service.config.redirect_uri.to_string()),
            code_verifier: Some(verifier.clone()),
            refresh_token: None,
            resource: service.config.resource().to_owned(),
        };
        let first = token(State(service.clone()), Form(exchange())).await;
        assert_eq!(first.status(), StatusCode::OK);
        let body = to_bytes(first.into_body(), 64 * 1024).await.unwrap();
        let pair: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let access_token = pair["access_token"].as_str().unwrap();
        let refresh_token = pair["refresh_token"].as_str().unwrap().to_owned();
        assert!(service.store.validate_access(access_token).unwrap());
        assert_eq!(
            token(State(service.clone()), Form(exchange()))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );

        let refresh = TokenForm {
            grant_type: "refresh_token".to_owned(),
            client_id: service.config.client_id.clone(),
            code: None,
            redirect_uri: None,
            code_verifier: None,
            refresh_token: Some(refresh_token.clone()),
            resource: service.config.resource().to_owned(),
        };
        assert_eq!(
            token(State(service.clone()), Form(refresh)).await.status(),
            StatusCode::OK
        );
        let replay = TokenForm {
            grant_type: "refresh_token".to_owned(),
            client_id: service.config.client_id.clone(),
            code: None,
            redirect_uri: None,
            code_verifier: None,
            refresh_token: Some(refresh_token),
            resource: service.config.resource().to_owned(),
        };
        assert_eq!(
            token(State(service), Form(replay)).await.status(),
            StatusCode::BAD_REQUEST
        );
        let metrics = observability.render_metrics(None).unwrap();
        assert!(metrics.contains("tools_mcp_oauth_refresh_total{outcome=\"success\"} 1"));
        assert!(metrics.contains("tools_mcp_oauth_refresh_total{outcome=\"failure\"} 1"));
    }

    #[test]
    fn rejects_non_origin_issuer_and_unsafe_redirects() {
        assert!(
            OAuthHttpConfig::new(
                "http://example.com/",
                "https://chatgpt.com/oauth/test/client.json".to_owned(),
                "https://chatgpt.com/connector/oauth/test"
            )
            .is_err()
        );
        assert!(
            OAuthHttpConfig::new(
                "https://example.com/path",
                "https://chatgpt.com/oauth/test/client.json".to_owned(),
                "https://chatgpt.com/connector/oauth/test"
            )
            .is_err()
        );
        assert!(
            OAuthHttpConfig::new(
                "https://example.com/",
                "https://chatgpt.com/oauth/test/client.json".to_owned(),
                "http://chatgpt.com/connector/oauth/test"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn path_prefixed_issuer_scopes_resource_form_cookie_and_metadata() {
        let (_root, service) = prefixed_service();
        assert_eq!(
            service.config.resource(),
            "https://example.ngrok-free.dev/friend/mcp"
        );
        let metadata = serde_json::to_value(
            authorization_server_metadata(State(service.clone()))
                .await
                .0,
        )
        .unwrap();
        assert_eq!(
            metadata["authorization_endpoint"],
            "https://example.ngrok-free.dev/friend/authorize"
        );
        assert_eq!(
            metadata["token_endpoint"],
            "https://example.ngrok-free.dev/friend/token"
        );

        let response = authorize(
            State(service.clone()),
            Query(AuthorizationQuery {
                response_type: "code".to_owned(),
                client_id: service.config.client_id.clone(),
                redirect_uri: service.config.redirect_uri.to_string(),
                resource: service.config.resource().to_owned(),
                scope: REQUIRED_SCOPE.to_owned(),
                state: None,
                code_challenge: "a".repeat(43),
                code_challenge_method: "S256".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert!(
            response.headers()[SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Path=/friend/authorize")
        );
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("action=\"/friend/authorize\"")
        );
    }

    #[tokio::test]
    async fn authorization_rejects_scope_state_substitution_and_bounds_pending_state() {
        let (_root, service) = service();
        let query = |scope: &str, state: Option<String>| AuthorizationQuery {
            response_type: "code".to_owned(),
            client_id: service.config.client_id.clone(),
            redirect_uri: service.config.redirect_uri.to_string(),
            resource: service.config.resource().to_owned(),
            scope: scope.to_owned(),
            state,
            code_challenge: "a".repeat(43),
            code_challenge_method: "S256".to_owned(),
        };

        assert_eq!(
            authorize(
                State(service.clone()),
                Query(query("offline_access admin", None)),
            )
            .await
            .unwrap_err()
            .error,
            "invalid_scope"
        );
        assert_eq!(
            authorize(
                State(service.clone()),
                Query(query(
                    "offline_access",
                    Some("x".repeat(MAX_STATE_BYTES + 1))
                )),
            )
            .await
            .unwrap_err()
            .error,
            "invalid_request"
        );

        for index in 0..MAX_PENDING_AUTHORIZATIONS {
            authorize(
                State(service.clone()),
                Query(query("offline_access", Some(format!("state-{index}")))),
            )
            .await
            .unwrap();
        }
        let saturated = authorize(
            State(service.clone()),
            Query(query("offline_access", Some("one-too-many".to_owned()))),
        )
        .await
        .unwrap_err();
        assert_eq!(saturated.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn denial_is_csrf_bound_consumes_the_transaction_and_preserves_state() {
        let (_root, service) = service();
        let response = authorize(
            State(service.clone()),
            Query(AuthorizationQuery {
                response_type: "code".to_owned(),
                client_id: service.config.client_id.clone(),
                redirect_uri: service.config.redirect_uri.to_string(),
                resource: service.config.resource().to_owned(),
                scope: REQUIRED_SCOPE.to_owned(),
                state: Some("deny-state".to_owned()),
                code_challenge: "a".repeat(43),
                code_challenge_method: "S256".to_owned(),
            }),
        )
        .await
        .unwrap();
        let request_id = response.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .strip_prefix("tools_mcp_oauth=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("tools_mcp_oauth={request_id}")).unwrap(),
        );
        let denied = consent(
            State(service.clone()),
            headers.clone(),
            Form(ConsentForm {
                request_id: request_id.clone(),
                owner_secret: None,
                decision: "deny".to_owned(),
            }),
        )
        .await
        .unwrap();
        let location = Url::parse(denied.headers()[LOCATION].to_str().unwrap()).unwrap();
        assert!(
            location
                .query_pairs()
                .any(|(key, value)| key == "error" && value == "access_denied")
        );
        assert!(
            location
                .query_pairs()
                .any(|(key, value)| key == "state" && value == "deny-state")
        );
        assert!(
            consent(
                State(service),
                headers,
                Form(ConsentForm {
                    request_id,
                    owner_secret: None,
                    decision: "deny".to_owned(),
                }),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn stale_consent_and_per_authorization_brute_force_fail_closed() {
        let (_root, service) = service();
        let pending = |expires_at| PendingAuthorization {
            client_id: service.config.client_id.clone(),
            redirect_uri: service.config.redirect_uri.to_string(),
            resource: service.config.resource().to_owned(),
            scope: REQUIRED_SCOPE.to_owned(),
            state: None,
            code_challenge: "a".repeat(43),
            expires_at,
            owner_failures: 0,
        };
        service.lock().authorizations.insert(
            "expired-request".to_owned(),
            pending(Instant::now().checked_sub(Duration::from_secs(1)).unwrap()),
        );
        let mut expired_headers = HeaderMap::new();
        expired_headers.insert(
            COOKIE,
            HeaderValue::from_static("tools_mcp_oauth=expired-request"),
        );
        assert!(
            consent(
                State(service.clone()),
                expired_headers,
                Form(ConsentForm {
                    request_id: "expired-request".to_owned(),
                    owner_secret: Some("owner-secret".to_owned()),
                    decision: "approve".to_owned(),
                }),
            )
            .await
            .is_err()
        );

        service.lock().authorizations.insert(
            "rate-limited-request".to_owned(),
            pending(Instant::now() + Duration::from_mins(1)),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("tools_mcp_oauth=rate-limited-request"),
        );
        for _ in 0..MAX_OWNER_FAILURES {
            let error = consent(
                State(service.clone()),
                headers.clone(),
                Form(ConsentForm {
                    request_id: "rate-limited-request".to_owned(),
                    owner_secret: Some("wrong-secret".to_owned()),
                    decision: "approve".to_owned(),
                }),
            )
            .await
            .unwrap_err();
            assert_eq!(error.status, StatusCode::FORBIDDEN);
        }
        let limited = consent(
            State(service.clone()),
            headers,
            Form(ConsentForm {
                request_id: "rate-limited-request".to_owned(),
                owner_secret: Some("owner-secret".to_owned()),
                decision: "approve".to_owned(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);

        service.lock().authorizations.insert(
            "independent-request".to_owned(),
            pending(Instant::now() + Duration::from_mins(1)),
        );
        let mut independent_headers = HeaderMap::new();
        independent_headers.insert(
            COOKIE,
            HeaderValue::from_static("tools_mcp_oauth=independent-request"),
        );
        let response = consent(
            State(service),
            independent_headers,
            Form(ConsentForm {
                request_id: "independent-request".to_owned(),
                owner_secret: Some("owner-secret".to_owned()),
                decision: "approve".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }
}
