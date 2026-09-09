use axum::Json;
use axum::body::Body;
use axum::extract::{Form, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, PRAGMA, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use url::Url;

const AUTHORIZATION_LIFETIME: Duration = Duration::from_mins(5);
const CODE_LIFETIME: Duration = Duration::from_mins(1);
const ACCESS_LIFETIME: Duration = Duration::from_secs(30);
const REFRESH_LIFETIME: Duration = Duration::from_hours(8);
const REQUIRED_SCOPE: &str = "offline_access";

#[derive(Clone, Debug)]
pub struct OAuthSpikeConfig {
    issuer: Url,
    resource: Url,
    owner_secret: String,
    allowed_client_id: String,
    allowed_redirect_uri: Url,
}

impl OAuthSpikeConfig {
    pub fn new(
        issuer: &str,
        owner_secret: String,
        allowed_client_id: String,
        allowed_redirect_uri: &str,
    ) -> anyhow::Result<Self> {
        let issuer = parse_https_url("issuer", issuer)?;
        anyhow::ensure!(
            issuer.path() == "/" && issuer.query().is_none() && issuer.fragment().is_none(),
            "U1 issuer must be an HTTPS origin without a path, query, or fragment"
        );
        anyhow::ensure!(
            owner_secret.len() >= 16,
            "U1 owner secret must contain at least 128 bits of operator-provided entropy"
        );
        let client_id = parse_https_url("client ID", &allowed_client_id)?;
        anyhow::ensure!(
            client_id.path() != "/" && client_id.fragment().is_none(),
            "U1 CIMD client ID must use a non-root path and no fragment"
        );
        let allowed_redirect_uri = parse_https_url("redirect URI", allowed_redirect_uri)?;
        anyhow::ensure!(
            allowed_redirect_uri.fragment().is_none(),
            "U1 redirect URI must not contain a fragment"
        );
        let resource = issuer.join("mcp")?;
        Ok(Self {
            issuer,
            resource,
            owner_secret,
            allowed_client_id,
            allowed_redirect_uri,
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
}

fn parse_https_url(kind: &str, value: &str) -> anyhow::Result<Url> {
    let parsed =
        Url::parse(value).map_err(|error| anyhow::anyhow!("invalid U1 {kind}: {error}"))?;
    anyhow::ensure!(
        parsed.scheme() == "https" && parsed.host_str().is_some(),
        "U1 {kind} must be an absolute HTTPS URL"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "U1 {kind} must not contain userinfo"
    );
    Ok(parsed)
}

#[derive(Clone)]
pub struct OAuthSpike {
    config: Arc<OAuthSpikeConfig>,
    state: Arc<Mutex<OAuthState>>,
}

impl OAuthSpike {
    #[must_use]
    pub fn new(config: OAuthSpikeConfig) -> Self {
        Self {
            config: Arc::new(config),
            state: Arc::new(Mutex::new(OAuthState::default())),
        }
    }

    #[must_use]
    pub fn config(&self) -> &OAuthSpikeConfig {
        &self.config
    }

    fn lock(&self) -> MutexGuard<'_, OAuthState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn validate_access_token(&self, token: &str) -> bool {
        let now = Instant::now();
        let hash = token_hash(token);
        let mut state = self.lock();
        state
            .access_tokens
            .retain(|_, grant| grant.expires_at > now);
        state.access_tokens.get(&hash).is_some_and(|grant| {
            grant.resource == self.config.resource.as_str()
                && grant.client_id == self.config.allowed_client_id
        })
    }
}

#[derive(Default)]
struct OAuthState {
    authorizations: HashMap<String, PendingAuthorization>,
    codes: HashMap<String, AuthorizationCode>,
    access_tokens: HashMap<[u8; 32], AccessGrant>,
    refresh_tokens: HashMap<[u8; 32], RefreshGrant>,
    consumed_refresh_tokens: HashMap<[u8; 32], String>,
    revoked_families: HashSet<String>,
}

struct PendingAuthorization {
    client_id: String,
    redirect_uri: String,
    resource: String,
    scope: String,
    state: Option<String>,
    code_challenge: String,
    expires_at: Instant,
}

struct AuthorizationCode {
    client_id: String,
    redirect_uri: String,
    resource: String,
    scope: String,
    code_challenge: String,
    expires_at: Instant,
}

struct AccessGrant {
    client_id: String,
    resource: String,
    expires_at: Instant,
}

#[derive(Clone)]
struct RefreshGrant {
    family: String,
    client_id: String,
    resource: String,
    scope: String,
    expires_at: Instant,
}

#[derive(Serialize)]
pub struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Vec<String>,
    bearer_methods_supported: Vec<String>,
}

#[derive(Serialize)]
pub struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    response_types_supported: Vec<String>,
    grant_types_supported: Vec<String>,
    code_challenge_methods_supported: Vec<String>,
    scopes_supported: Vec<String>,
    token_endpoint_auth_methods_supported: Vec<String>,
    client_id_metadata_document_supported: bool,
}

#[derive(Serialize)]
pub struct ClientMetadataDocument {
    client_id: String,
    client_name: &'static str,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: &'static str,
    grant_types: Vec<&'static str>,
    response_types: Vec<&'static str>,
    scope: &'static str,
}

#[derive(Deserialize)]
pub struct AuthorizationQuery {
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
pub struct ConsentForm {
    request_id: String,
    owner_secret: String,
}

#[derive(Deserialize)]
pub struct TokenForm {
    grant_type: String,
    client_id: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: String,
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
    refresh_token: String,
    scope: String,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    error_description: &'a str,
}

pub async fn protected_resource_metadata(
    State(oauth): State<OAuthSpike>,
) -> Json<ProtectedResourceMetadata> {
    Json(ProtectedResourceMetadata {
        resource: oauth.config.resource.to_string(),
        authorization_servers: vec![oauth.config.issuer().to_owned()],
        scopes_supported: vec![REQUIRED_SCOPE.to_owned()],
        bearer_methods_supported: vec!["header".to_owned()],
    })
}

pub async fn authorization_server_metadata(
    State(oauth): State<OAuthSpike>,
) -> Json<AuthorizationServerMetadata> {
    Json(AuthorizationServerMetadata {
        issuer: oauth.config.issuer().to_owned(),
        authorization_endpoint: format!("{}/authorize", oauth.config.issuer()),
        token_endpoint: format!("{}/token", oauth.config.issuer()),
        response_types_supported: vec!["code".to_owned()],
        grant_types_supported: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
        code_challenge_methods_supported: vec!["S256".to_owned()],
        scopes_supported: vec![REQUIRED_SCOPE.to_owned()],
        token_endpoint_auth_methods_supported: vec!["none".to_owned()],
        client_id_metadata_document_supported: true,
    })
}

pub async fn client_metadata_document(
    State(oauth): State<OAuthSpike>,
) -> Json<ClientMetadataDocument> {
    Json(ClientMetadataDocument {
        client_id: oauth.config.allowed_client_id.clone(),
        client_name: "tools-mcp ChatGPT U1",
        redirect_uris: vec![oauth.config.allowed_redirect_uri.to_string()],
        token_endpoint_auth_method: "none",
        grant_types: vec!["authorization_code", "refresh_token"],
        response_types: vec!["code"],
        scope: REQUIRED_SCOPE,
    })
}

pub async fn authorize(
    State(oauth): State<OAuthSpike>,
    Query(query): Query<AuthorizationQuery>,
) -> Result<Html<String>, OAuthHttpError> {
    validate_authorization_query(&oauth.config, &query)?;
    let request_id = random_token();
    let pending = PendingAuthorization {
        client_id: query.client_id,
        redirect_uri: query.redirect_uri,
        resource: query.resource,
        scope: normalize_scope(&query.scope),
        state: query.state,
        code_challenge: query.code_challenge,
        expires_at: Instant::now() + AUTHORIZATION_LIFETIME,
    };
    oauth
        .lock()
        .authorizations
        .insert(request_id.clone(), pending);
    Ok(Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"referrer\" content=\"no-referrer\"><title>Authorize tools-mcp</title></head><body><h1>Authorize tools-mcp</h1><p>Allow this ChatGPT client to use the five coding tools.</p><form method=\"post\" action=\"/authorize\"><input type=\"hidden\" name=\"request_id\" value=\"{request_id}\"><label>Owner secret <input type=\"password\" name=\"owner_secret\" autocomplete=\"current-password\" required></label><button type=\"submit\">Authorize</button></form></body></html>"
    )))
}

pub async fn consent(
    State(oauth): State<OAuthSpike>,
    Form(form): Form<ConsentForm>,
) -> Result<Redirect, OAuthHttpError> {
    if !constant_time_equal(&form.owner_secret, &oauth.config.owner_secret) {
        return Err(OAuthHttpError::access_denied("owner authentication failed"));
    }
    let now = Instant::now();
    let pending = oauth
        .lock()
        .authorizations
        .remove(&form.request_id)
        .filter(|pending| pending.expires_at > now)
        .ok_or_else(|| OAuthHttpError::invalid_request("authorization request expired"))?;
    let code = random_token();
    let mut redirect = Url::parse(&pending.redirect_uri)
        .map_err(|_| OAuthHttpError::invalid_request("stored redirect URI is invalid"))?;
    {
        let mut pairs = redirect.query_pairs_mut();
        pairs.append_pair("code", &code);
        if let Some(state) = &pending.state {
            pairs.append_pair("state", state);
        }
        pairs.append_pair("iss", oauth.config.issuer());
    }
    oauth.lock().codes.insert(
        code,
        AuthorizationCode {
            client_id: pending.client_id,
            redirect_uri: pending.redirect_uri,
            resource: pending.resource,
            scope: pending.scope,
            code_challenge: pending.code_challenge,
            expires_at: now + CODE_LIFETIME,
        },
    );
    Ok(Redirect::to(redirect.as_str()))
}

pub async fn token(State(oauth): State<OAuthSpike>, Form(form): Form<TokenForm>) -> Response {
    let result = match form.grant_type.as_str() {
        "authorization_code" => exchange_code(&oauth, &form),
        "refresh_token" => exchange_refresh(&oauth, &form),
        _ => Err(OAuthHttpError::unsupported_grant()),
    };
    match result {
        Ok(tokens) => token_response(tokens),
        Err(error) => error.into_response(),
    }
}

fn validate_authorization_query(
    config: &OAuthSpikeConfig,
    query: &AuthorizationQuery,
) -> Result<(), OAuthHttpError> {
    eprintln!(
        "U1 authorization identifiers: client_id={} redirect_uri={}",
        query.client_id.escape_debug(),
        query.redirect_uri.escape_debug()
    );
    if query.response_type != "code" {
        return Err(OAuthHttpError::invalid_request(
            "response_type must be code",
        ));
    }
    if query.client_id != config.allowed_client_id {
        return Err(OAuthHttpError::invalid_client(
            "client_id is not allowlisted",
        ));
    }
    if query.redirect_uri != config.allowed_redirect_uri.as_str() {
        return Err(OAuthHttpError::invalid_request(
            "redirect_uri does not match",
        ));
    }
    if query.resource != config.resource.as_str() {
        return Err(OAuthHttpError::invalid_target(
            "resource does not match MCP endpoint",
        ));
    }
    if query.code_challenge_method != "S256" || !valid_pkce_value(&query.code_challenge) {
        return Err(OAuthHttpError::invalid_request("PKCE S256 is required"));
    }
    if !query
        .scope
        .split_ascii_whitespace()
        .any(|scope| scope == REQUIRED_SCOPE)
    {
        return Err(OAuthHttpError::invalid_scope("offline_access is required"));
    }
    Ok(())
}

fn exchange_code(oauth: &OAuthSpike, form: &TokenForm) -> Result<TokenResponse, OAuthHttpError> {
    validate_token_binding(oauth, form)?;
    let code = form
        .code
        .as_deref()
        .ok_or_else(|| OAuthHttpError::invalid_request("code is required"))?;
    let redirect_uri = form
        .redirect_uri
        .as_deref()
        .ok_or_else(|| OAuthHttpError::invalid_request("redirect_uri is required"))?;
    let verifier = form
        .code_verifier
        .as_deref()
        .ok_or_else(|| OAuthHttpError::invalid_request("code_verifier is required"))?;
    if !valid_pkce_value(verifier) {
        return Err(OAuthHttpError::invalid_grant("invalid code_verifier"));
    }
    let now = Instant::now();
    let grant = oauth
        .lock()
        .codes
        .remove(code)
        .filter(|grant| grant.expires_at > now)
        .ok_or_else(|| OAuthHttpError::invalid_grant("authorization code is invalid or expired"))?;
    if grant.client_id != form.client_id
        || grant.redirect_uri != redirect_uri
        || grant.resource != form.resource
        || !constant_time_equal(&grant.code_challenge, &pkce_challenge(verifier))
    {
        return Err(OAuthHttpError::invalid_grant(
            "authorization code binding failed",
        ));
    }
    Ok(issue_tokens(
        oauth,
        &grant.client_id,
        &grant.resource,
        &grant.scope,
        None,
    ))
}

fn exchange_refresh(oauth: &OAuthSpike, form: &TokenForm) -> Result<TokenResponse, OAuthHttpError> {
    validate_token_binding(oauth, form)?;
    let token = form
        .refresh_token
        .as_deref()
        .ok_or_else(|| OAuthHttpError::invalid_request("refresh_token is required"))?;
    let hash = token_hash(token);
    let now = Instant::now();
    let mut state = oauth.lock();
    if let Some(family) = state.consumed_refresh_tokens.get(&hash).cloned() {
        state.revoked_families.insert(family.clone());
        state
            .refresh_tokens
            .retain(|_, grant| grant.family != family);
        return Err(OAuthHttpError::invalid_grant(
            "refresh token reuse revoked its family",
        ));
    }
    let grant = state
        .refresh_tokens
        .remove(&hash)
        .filter(|grant| grant.expires_at > now)
        .ok_or_else(|| OAuthHttpError::invalid_grant("refresh token is invalid or expired"))?;
    if state.revoked_families.contains(&grant.family)
        || grant.client_id != form.client_id
        || grant.resource != form.resource
    {
        return Err(OAuthHttpError::invalid_grant(
            "refresh token binding failed",
        ));
    }
    state
        .consumed_refresh_tokens
        .insert(hash, grant.family.clone());
    drop(state);
    Ok(issue_tokens(
        oauth,
        &grant.client_id,
        &grant.resource,
        &grant.scope,
        Some(grant.family),
    ))
}

fn validate_token_binding(oauth: &OAuthSpike, form: &TokenForm) -> Result<(), OAuthHttpError> {
    if form.client_id != oauth.config.allowed_client_id {
        return Err(OAuthHttpError::invalid_client(
            "client_id is not allowlisted",
        ));
    }
    if form.resource != oauth.config.resource.as_str() {
        return Err(OAuthHttpError::invalid_target(
            "resource does not match MCP endpoint",
        ));
    }
    Ok(())
}

fn issue_tokens(
    oauth: &OAuthSpike,
    client_id: &str,
    resource: &str,
    scope: &str,
    family: Option<String>,
) -> TokenResponse {
    let access_token = random_token();
    let refresh_token = random_token();
    let family = family.unwrap_or_else(random_token);
    let now = Instant::now();
    let mut state = oauth.lock();
    state.access_tokens.insert(
        token_hash(&access_token),
        AccessGrant {
            client_id: client_id.to_owned(),
            resource: resource.to_owned(),
            expires_at: now + ACCESS_LIFETIME,
        },
    );
    state.refresh_tokens.insert(
        token_hash(&refresh_token),
        RefreshGrant {
            family,
            client_id: client_id.to_owned(),
            resource: resource.to_owned(),
            scope: scope.to_owned(),
            expires_at: now + REFRESH_LIFETIME,
        },
    );
    TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ACCESS_LIFETIME.as_secs(),
        refresh_token,
        scope: scope.to_owned(),
    }
}

fn token_response(tokens: TokenResponse) -> Response {
    (
        StatusCode::OK,
        [
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (PRAGMA, HeaderValue::from_static("no-cache")),
        ],
        Json(tokens),
    )
        .into_response()
}

pub async fn require_bearer(
    State(oauth): State<OAuthSpike>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let valid = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| oauth.validate_access_token(token));
    if valid {
        return next.run(request).await;
    }
    let metadata = format!(
        "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
        oauth.config.issuer()
    );
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(ErrorBody {
            error: "invalid_token",
            error_description: "a valid bearer token is required",
        }),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&metadata) {
        response.headers_mut().insert(WWW_AUTHENTICATE, value);
    }
    response
}

#[derive(Debug)]
pub struct OAuthHttpError {
    status: StatusCode,
    error: &'static str,
    description: &'static str,
}

impl OAuthHttpError {
    fn invalid_request(description: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", description)
    }

    fn invalid_client(description: &'static str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "invalid_client", description)
    }

    fn invalid_grant(description: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_grant", description)
    }

    fn invalid_scope(description: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_scope", description)
    }

    fn invalid_target(description: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_target", description)
    }

    fn unsupported_grant() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "grant_type is unsupported",
        )
    }

    fn access_denied(description: &'static str) -> Self {
        Self::new(StatusCode::FORBIDDEN, "access_denied", description)
    }

    const fn new(status: StatusCode, error: &'static str, description: &'static str) -> Self {
        Self {
            status,
            error,
            description,
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

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn valid_pkce_value(value: &str) -> bool {
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

fn constant_time_equal(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}
