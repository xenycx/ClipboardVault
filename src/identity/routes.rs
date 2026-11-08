//! Public `/api/auth/*` routes.
//!
//! Paths, request bodies and error codes are the ones `static/app.js` used with
//! Better Auth, so the browser code is unchanged. Errors are `{code, message}`
//! with an HTTP status; every state-changing route checks `Origin` first and is
//! rate limited per client address.

use std::net::{IpAddr, SocketAddr};

use axum::{
    Json,
    body::Bytes,
    extract::{ConnectInfo, FromRequestParts, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;

use crate::{
    AppState,
    db::{self},
    error::AppError,
    identity::{
        SESSION_COOKIE, admin::email_is_valid, approve_user_in, clear_session_cookie,
        constant_time_eq, cookie_value, create_session, new_id, oauth, password, rate_limit,
        server_settings, session_cookie, session_from_headers, sha256_hex,
    },
};

/// The TCP peer address, when the server was started with connect info.
pub struct PeerAddr(pub Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0.ip()),
        ))
    }
}

pub fn auth_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"code": code, "message": message}))).into_response()
}

fn too_many_requests() -> Response {
    auth_error(
        StatusCode::TOO_MANY_REQUESTS,
        "TOO_MANY_REQUESTS",
        "Too many requests. Please try again later.",
    )
}

fn internal(error: impl std::fmt::Display) -> Response {
    tracing::error!(%error, "authentication request failed");
    auth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL_SERVER_ERROR",
        "An internal error occurred",
    )
}

/// State-changing auth requests must come from this server's own pages, as Better
/// Auth's trusted-origin check required. `Referer` is the fallback for clients that
/// omit `Origin`; a request with neither is refused.
pub fn origin_is_trusted(public_base_url: &str, headers: &HeaderMap) -> bool {
    let Ok(expected) = url::Url::parse(public_base_url) else {
        return false;
    };
    let supplied = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| url::Url::parse(value).ok());
    supplied.is_some_and(|supplied| supplied.origin() == expected.origin())
}

/// `Some(refusal)` when the request did not come from this server's pages.
fn untrusted_origin(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    (!origin_is_trusted(&state.public_base_url, headers))
        .then(|| auth_error(StatusCode::FORBIDDEN, "INVALID_ORIGIN", "Invalid origin"))
}

fn client(state: &AppState, headers: &HeaderMap, peer: &PeerAddr) -> String {
    rate_limit::client_ip(headers, peer.0, state.trust_proxy_headers)
}

fn parse<T: DeserializeOwned>(body: &Bytes) -> Result<T, Box<Response>> {
    serde_json::from_slice(body).map_err(|_| {
        Box::new(auth_error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "The request body is not valid JSON",
        ))
    })
}

fn user_agent(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
}

fn signed_in(state: &AppState, token: &str, body: serde_json::Value) -> Response {
    let mut response = Json(body).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        session_cookie(token, state.secure_cookies),
    );
    response
}

#[derive(Deserialize)]
pub struct SignInBody {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
}

#[derive(sqlx::FromRow)]
struct Credentials {
    id: String,
    name: String,
    email: String,
    password_hash: Option<String>,
    banned: bool,
}

/// `POST /api/auth/sign-in/email`
pub async fn sign_in_email(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = untrusted_origin(&state, &headers) {
        return response;
    }
    let ip = client(&state, &headers, &peer);
    if !state.limiter.allow("sign-in", &ip, rate_limit::SIGN_IN) {
        return too_many_requests();
    }
    let input: SignInBody = match parse(&body) {
        Ok(input) => input,
        Err(response) => return *response,
    };
    let email = input.email.trim().to_lowercase();
    let account = match sqlx::query_as::<_, Credentials>(
        "SELECT id, name, email, password_hash, banned FROM users WHERE email = ?",
    )
    .bind(&email)
    .fetch_optional(&state.db.read)
    .await
    {
        Ok(account) => account,
        Err(error) => return internal(error),
    };
    let stored = account
        .as_ref()
        .and_then(|account| account.password_hash.clone());
    let verification = password::verify_blocking(stored, input.password.clone()).await;
    let (Some(account), password::Verification::Valid { rehash }) = (account, verification) else {
        return auth_error(
            StatusCode::UNAUTHORIZED,
            "INVALID_EMAIL_OR_PASSWORD",
            "Invalid email or password",
        );
    };
    if account.banned {
        return auth_error(
            StatusCode::FORBIDDEN,
            "BANNED_USER",
            "You have been banned from this application",
        );
    }
    let new_hash = if rehash {
        password::hash_blocking(input.password).await.ok()
    } else {
        None
    };
    let token = async {
        let mut tx = state.db.begin().await?;
        if let Some(hash) = new_hash {
            sqlx::query("UPDATE users SET password_hash = ?, updated_at = ? WHERE id = ?")
                .bind(hash)
                .bind(db::now())
                .bind(&account.id)
                .execute(&mut *tx)
                .await?;
        }
        let token = create_session(&mut tx, &account.id, Some(&ip), user_agent(&headers)).await?;
        tx.commit().await?;
        Ok::<_, AppError>(token)
    }
    .await;
    match token {
        Ok(token) => signed_in(
            &state,
            &token,
            json!({"redirect": false, "user": {"id": account.id, "name": account.name, "email": account.email}}),
        ),
        Err(error) => internal(error),
    }
}

#[derive(Deserialize)]
pub struct SignUpBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
}

/// `POST /api/auth/sign-up/email`. New accounts wait for approval unless an
/// administrator has turned approval off.
pub async fn sign_up_email(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = untrusted_origin(&state, &headers) {
        return response;
    }
    let ip = client(&state, &headers, &peer);
    if !state.limiter.allow("sign-up", &ip, rate_limit::SIGN_UP) {
        return too_many_requests();
    }
    if !server_settings(&state.db).await.registration_enabled {
        return auth_error(
            StatusCode::FORBIDDEN,
            "REGISTRATION_CLOSED",
            "Registration is closed on this server. Ask an administrator to create your account.",
        );
    }
    let input: SignUpBody = match parse(&body) {
        Ok(input) => input,
        Err(response) => return *response,
    };
    let name: String = input.name.trim().chars().take(80).collect();
    let email = input.email.trim().to_lowercase();
    if name.is_empty() {
        return auth_error(StatusCode::BAD_REQUEST, "INVALID_NAME", "Enter your name");
    }
    if !email_is_valid(&email) {
        return auth_error(StatusCode::BAD_REQUEST, "INVALID_EMAIL", "Invalid email");
    }
    let length = input.password.chars().count();
    if length < password::MIN_LENGTH {
        return auth_error(
            StatusCode::BAD_REQUEST,
            "PASSWORD_TOO_SHORT",
            "Password too short",
        );
    }
    if length > password::MAX_LENGTH {
        return auth_error(
            StatusCode::BAD_REQUEST,
            "PASSWORD_TOO_LONG",
            "Password too long",
        );
    }
    let hash = match password::hash_blocking(input.password).await {
        Ok(hash) => hash,
        Err(error) => return internal(error),
    };
    let result = async {
        let mut tx = state.db.begin().await?;
        let exists = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE email = ?)")
            .bind(&email)
            .fetch_one(&mut *tx)
            .await?;
        if exists {
            return Ok(None);
        }
        let id = new_id();
        let now = db::now();
        sqlx::query(
            "INSERT INTO users (id, name, email, password_hash, created_at, updated_at) VALUES (?,?,?,?,?,?)",
        )
        .bind(&id)
        .bind(&name)
        .bind(&email)
        .bind(&hash)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        let token = create_session(&mut tx, &id, Some(&ip), user_agent(&headers)).await?;
        tx.commit().await?;
        Ok::<_, AppError>(Some((id, token)))
    }
    .await;
    match result {
        Ok(Some((id, token))) => signed_in(
            &state,
            &token,
            json!({"token": null, "user": {"id": id, "name": name, "email": email}}),
        ),
        Ok(None) => auth_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "USER_ALREADY_EXISTS_USE_ANOTHER_EMAIL",
            "User already exists. Use another email.",
        ),
        Err(error) => internal(error),
    }
}

/// `POST /api/auth/sign-out`: deletes the session and clears its cookie.
pub async fn sign_out(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = untrusted_origin(&state, &headers) {
        return response;
    }
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE)
        && let Err(error) = sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(sha256_hex(token))
            .execute(&state.db.write)
            .await
    {
        return internal(error);
    }
    let mut response = Json(json!({"success": true})).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        clear_session_cookie(state.secure_cookies),
    );
    response
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetBody {
    #[serde(default)]
    token: String,
    #[serde(default)]
    new_password: String,
}

/// `POST /api/auth/reset-password`: redeems an administrator-issued link. Every
/// session of the account is revoked, as Better Auth did.
pub async fn reset_password(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = untrusted_origin(&state, &headers) {
        return response;
    }
    let ip = client(&state, &headers, &peer);
    if !state
        .limiter
        .allow("reset-password", &ip, rate_limit::RESET_PASSWORD)
    {
        return too_many_requests();
    }
    let input: ResetBody = match parse(&body) {
        Ok(input) => input,
        Err(response) => return *response,
    };
    if !password::length_is_valid(&input.new_password) {
        return auth_error(
            StatusCode::BAD_REQUEST,
            "PASSWORD_TOO_SHORT",
            "Passwords must be 8 to 128 characters.",
        );
    }
    let hash = match password::hash_blocking(input.new_password).await {
        Ok(hash) => hash,
        Err(error) => return internal(error),
    };
    let result = async {
        let mut tx = state.db.begin().await?;
        let now = db::now();
        let user_id = sqlx::query_scalar::<_, String>(
            "UPDATE password_resets SET used_at = ? WHERE token_hash = ? AND used_at IS NULL AND expires_at > ? RETURNING user_id",
        )
        .bind(&now)
        .bind(sha256_hex(input.token.trim()))
        .bind(&now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(user_id) = user_id else {
            return Ok(false);
        };
        sqlx::query("UPDATE users SET password_hash = ?, updated_at = ? WHERE id = ?")
            .bind(&hash)
            .bind(&now)
            .bind(&user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(&user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok::<_, AppError>(true)
    }
    .await;
    match result {
        Ok(true) => Json(json!({"status": true})).into_response(),
        Ok(false) => auth_error(
            StatusCode::BAD_REQUEST,
            "INVALID_TOKEN",
            "This reset link is invalid or has expired.",
        ),
        Err(error) => internal(error),
    }
}

#[derive(Deserialize)]
pub struct BootstrapBody {
    #[serde(default)]
    token: String,
}

/// `POST /api/auth/vault/bootstrap`: the signed-in account becomes the first
/// administrator when it presents `SETUP_TOKEN` and no administrator exists yet.
pub async fn bootstrap(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = untrusted_origin(&state, &headers) {
        return response;
    }
    let ip = client(&state, &headers, &peer);
    if !state.limiter.allow("setup", &ip, rate_limit::SETUP) {
        return too_many_requests();
    }
    let session = match session_from_headers(&state.db, &headers, None).await {
        Ok(session) => session,
        Err(AppError::Unauthorized) => {
            return auth_error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Sign in first");
        }
        Err(error) => return internal(error),
    };
    let input: BootstrapBody = match parse(&body) {
        Ok(input) => input,
        Err(response) => return *response,
    };
    if !constant_time_eq(input.token.trim(), &state.setup_token) {
        return auth_error(
            StatusCode::FORBIDDEN,
            "INVALID_SETUP_TOKEN",
            "Invalid setup token",
        );
    }
    let result = async {
        let mut tx = state.db.begin().await?;
        let admins = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT count(*) FROM users WHERE {}",
            crate::identity::ADMIN_ROLE_SQL
        ))
        .fetch_one(&mut *tx)
        .await?;
        if admins > 0 {
            return Ok(false);
        }
        sqlx::query("UPDATE users SET role = 'admin', updated_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(&session.user.id)
            .execute(&mut *tx)
            .await?;
        approve_user_in(&mut tx, &session.user.id, None).await?;
        tx.commit().await?;
        Ok::<_, AppError>(true)
    }
    .await;
    match result {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => auth_error(
            StatusCode::CONFLICT,
            "SETUP_COMPLETE",
            "Setup is already complete",
        ),
        Err(error) => internal(error),
    }
}

/// `GET /api/auth/vault/providers`: enabled social providers, without credentials.
pub async fn providers(State(state): State<AppState>) -> Response {
    let mut response = Json(json!({"data": {"providers": state.oauth.public()}})).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocialBody {
    #[serde(default)]
    provider: String,
    #[serde(default, rename = "callbackURL")]
    callback_url: Option<String>,
    #[serde(default, rename = "errorCallbackURL")]
    error_callback_url: Option<String>,
}

/// Accepts a same-origin return address and reduces it to a local path, so the
/// sign-in flow can never redirect to another site.
pub fn local_return_path(public_base_url: &str, value: Option<&str>, fallback: &str) -> String {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return fallback.to_owned();
    };
    if value.starts_with('/') && !value.starts_with("//") && !value.contains('\\') {
        return value.to_owned();
    }
    let (Ok(base), Ok(candidate)) = (url::Url::parse(public_base_url), url::Url::parse(value))
    else {
        return fallback.to_owned();
    };
    if candidate.origin() != base.origin() {
        return fallback.to_owned();
    }
    let mut path = candidate.path().to_owned();
    if let Some(query) = candidate.query() {
        path.push('?');
        path.push_str(query);
    }
    path
}

/// `POST /api/auth/sign-in/social`: answers `{url, redirect: true}` for the browser.
pub async fn sign_in_social(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = untrusted_origin(&state, &headers) {
        return response;
    }
    let ip = client(&state, &headers, &peer);
    if !state.limiter.allow("sign-in", &ip, rate_limit::SIGN_IN) {
        return too_many_requests();
    }
    let input: SocialBody = match parse(&body) {
        Ok(input) => input,
        Err(response) => return *response,
    };
    let Some(provider) = state.oauth.get(&input.provider) else {
        return auth_error(
            StatusCode::NOT_FOUND,
            "PROVIDER_NOT_FOUND",
            "Provider not found",
        );
    };
    let callback = local_return_path(&state.public_base_url, input.callback_url.as_deref(), "/");
    let error_callback = local_return_path(
        &state.public_base_url,
        input.error_callback_url.as_deref(),
        "/login",
    );
    let pending = oauth::PendingSignIn::new(provider.slug, callback, error_callback);
    let url = oauth::authorization_url(&state.public_base_url, provider, &pending);
    let mut response = Json(json!({"url": url, "redirect": true})).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        oauth::state_cookie(
            &oauth::seal(&state.vault_secret, &pending),
            state.secure_cookies,
        ),
    );
    response
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn redirect(location: &str, cookies: &[HeaderValue]) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    for cookie in cookies {
        response
            .headers_mut()
            .append(header::SET_COOKIE, cookie.clone());
    }
    response
}

fn with_error(path: &str, code: &str) -> String {
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}error={code}")
}

enum Linked {
    Session(String),
    Refused(&'static str),
}

/// `GET /api/auth/callback/{provider}`
pub async fn oauth_callback(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    Path(provider_slug): Path<String>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let clear = oauth::clear_state_cookie(state.secure_cookies);
    let pending = cookie_value(&headers, oauth::STATE_COOKIE)
        .and_then(|sealed| oauth::open(&state.vault_secret, sealed))
        .filter(|pending| pending.provider == provider_slug)
        .filter(|pending| {
            query
                .state
                .as_deref()
                .is_some_and(|state| constant_time_eq(state, &pending.state))
        });
    let Some(pending) = pending else {
        return redirect("/login?error=state_mismatch", &[clear]);
    };
    let fail = |code: &str| {
        redirect(
            &with_error(&pending.error_callback, code),
            std::slice::from_ref(&clear),
        )
    };
    if query.error.is_some() {
        return fail("access_denied");
    }
    let (Some(provider), Some(code)) = (state.oauth.get(&provider_slug), query.code.as_deref())
    else {
        return fail("unable_to_get_user_info");
    };
    let identity = match oauth::fetch_identity(
        &state.http,
        &state.public_base_url,
        provider,
        code,
        &pending.verifier,
    )
    .await
    {
        Ok(identity) => identity,
        Err(failure) => return fail(failure.code()),
    };
    let ip = rate_limit::client_ip(&headers, peer.0, state.trust_proxy_headers);
    let registration_open = server_settings(&state.db).await.registration_enabled;
    let linked = async {
        let mut tx = state.db.begin().await?;
        let email = identity.email.trim().to_lowercase();
        let linked = sqlx::query_as::<_, (String, bool)>(
            r#"SELECT u.id, u.banned FROM oauth_accounts a JOIN users u ON u.id = a.user_id
               WHERE a.provider = ? AND a.provider_account_id = ?"#,
        )
        .bind(provider.slug)
        .bind(&identity.account_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (user_id, banned) = match linked {
            Some(found) => found,
            None => {
                let by_email = sqlx::query_as::<_, (String, bool)>("SELECT id, banned FROM users WHERE email = ?")
                    .bind(&email)
                    .fetch_optional(&mut *tx)
                    .await?;
                let now = db::now();
                let found = match by_email {
                    Some(found) => {
                        sqlx::query("UPDATE users SET email_verified = 1, updated_at = ? WHERE id = ?")
                            .bind(&now)
                            .bind(&found.0)
                            .execute(&mut *tx)
                            .await?;
                        found
                    }
                    None if !registration_open => return Ok(Linked::Refused("unable_to_create_user")),
                    None => {
                        let id = new_id();
                        let name: String = identity.name.trim().chars().take(80).collect();
                        sqlx::query(
                            r#"INSERT INTO users (id, name, email, email_verified, image, created_at, updated_at)
                               VALUES (?,?,?,1,?,?,?)"#,
                        )
                        .bind(&id)
                        .bind(if name.is_empty() { email.clone() } else { name })
                        .bind(&email)
                        .bind(&identity.image)
                        .bind(&now)
                        .bind(&now)
                        .execute(&mut *tx)
                        .await?;
                        (id, false)
                    }
                };
                sqlx::query(
                    "INSERT INTO oauth_accounts (id, user_id, provider, provider_account_id, created_at) VALUES (?,?,?,?,?)",
                )
                .bind(new_id())
                .bind(&found.0)
                .bind(provider.slug)
                .bind(&identity.account_id)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
                found
            }
        };
        if banned {
            return Ok(Linked::Refused("banned"));
        }
        let token = create_session(&mut tx, &user_id, Some(&ip), user_agent(&headers)).await?;
        tx.commit().await?;
        Ok::<_, AppError>(Linked::Session(token))
    }
    .await;
    match linked {
        Ok(Linked::Session(token)) => redirect(
            &pending.callback,
            &[clear, session_cookie(&token, state.secure_cookies)],
        ),
        Ok(Linked::Refused(code)) => fail(code),
        Err(error) => {
            tracing::error!(%error, "social sign-in failed");
            fail("internal_server_error")
        }
    }
}

/// Any other `/api/auth/*` path.
pub async fn not_found() -> Response {
    auth_error(StatusCode::NOT_FOUND, "NOT_FOUND", "Not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_must_match_the_public_address() {
        let base = "https://vault.example.com";
        let mut headers = HeaderMap::new();
        assert!(
            !origin_is_trusted(base, &headers),
            "missing origin is refused"
        );
        headers.insert(header::ORIGIN, "https://vault.example.com".parse().unwrap());
        assert!(origin_is_trusted(base, &headers));
        headers.insert(header::ORIGIN, "https://evil.example.com".parse().unwrap());
        assert!(!origin_is_trusted(base, &headers));
        headers.insert(header::ORIGIN, "http://vault.example.com".parse().unwrap());
        assert!(
            !origin_is_trusted(base, &headers),
            "scheme is part of the origin"
        );
        let mut referer_only = HeaderMap::new();
        referer_only.insert(
            header::REFERER,
            "https://vault.example.com/login".parse().unwrap(),
        );
        assert!(origin_is_trusted(base, &referer_only));
    }

    #[test]
    fn return_paths_stay_on_this_site() {
        let base = "https://vault.example.com";
        assert_eq!(local_return_path(base, Some("/keys?x=1"), "/"), "/keys?x=1");
        assert_eq!(
            local_return_path(base, Some("//evil.example.com"), "/"),
            "/"
        );
        assert_eq!(
            local_return_path(base, Some("https://evil.example.com/"), "/"),
            "/"
        );
        assert_eq!(
            local_return_path(base, Some("/\\evil.example.com"), "/"),
            "/"
        );
        assert_eq!(
            local_return_path(base, Some("https://vault.example.com/storage"), "/"),
            "/storage"
        );
        assert_eq!(local_return_path(base, None, "/login"), "/login");
    }

    #[test]
    fn error_codes_are_appended_to_existing_queries() {
        assert_eq!(
            with_error("/login?returnTo=%2F", "access_denied"),
            "/login?returnTo=%2F&error=access_denied"
        );
        assert_eq!(with_error("/login", "banned"), "/login?error=banned");
    }
}
