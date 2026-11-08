//! Social sign-in with Google, GitHub and Discord (authorization code + PKCE).
//!
//! The rules match the 2.x Better Auth configuration:
//! * A provider must report a *verified* email address, or sign-in stops.
//! * An existing provider identity signs in its linked user.
//! * Otherwise a verified email links to the account with that email address
//!   (`allowDifferentEmails: false`: no identity is ever linked across emails).
//! * Otherwise a new pending account is created, unless registration is closed.
//!
//! Between the redirect and the callback, the state, PKCE verifier and return
//! paths travel in a short-lived cookie signed with `VAULT_SECRET`, scoped to the
//! callback path. The callback refuses a state that does not match that cookie,
//! which stops login CSRF.

use std::time::Duration;

use axum::http::HeaderValue;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::identity::{constant_time_eq, random_token, set_cookie};

pub const STATE_COOKIE: &str = "vault_oauth";
pub const STATE_COOKIE_PATH: &str = "/api/auth/callback";
const STATE_LIFETIME_SECONDS: i64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderId {
    Google,
    GitHub,
    Discord,
}

struct Definition {
    id: ProviderId,
    slug: &'static str,
    label: &'static str,
    client_id_variable: &'static str,
    client_secret_variable: &'static str,
    scopes: &'static [&'static str],
    authorize_url: &'static str,
    token_url: &'static str,
}

const DEFINITIONS: [Definition; 3] = [
    Definition {
        id: ProviderId::Google,
        slug: "google",
        label: "Google",
        client_id_variable: "GOOGLE_CLIENT_ID",
        client_secret_variable: "GOOGLE_CLIENT_SECRET",
        scopes: &["openid", "email", "profile"],
        authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
        token_url: "https://oauth2.googleapis.com/token",
    },
    Definition {
        id: ProviderId::GitHub,
        slug: "github",
        label: "GitHub",
        client_id_variable: "GITHUB_CLIENT_ID",
        client_secret_variable: "GITHUB_CLIENT_SECRET",
        scopes: &["read:user", "user:email"],
        authorize_url: "https://github.com/login/oauth/authorize",
        token_url: "https://github.com/login/oauth/access_token",
    },
    Definition {
        id: ProviderId::Discord,
        slug: "discord",
        label: "Discord",
        client_id_variable: "DISCORD_CLIENT_ID",
        client_secret_variable: "DISCORD_CLIENT_SECRET",
        scopes: &["identify", "email"],
        authorize_url: "https://discord.com/oauth2/authorize",
        token_url: "https://discord.com/api/oauth2/token",
    },
];

#[derive(Debug, Clone)]
pub struct Provider {
    pub id: ProviderId,
    pub slug: &'static str,
    pub label: &'static str,
    client_id: String,
    client_secret: String,
    scopes: &'static [&'static str],
    authorize_url: &'static str,
    token_url: &'static str,
}

/// Public provider metadata for the login page. Never includes credentials.
#[derive(Debug, Clone, Serialize)]
pub struct PublicProvider {
    pub id: &'static str,
    pub label: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    providers: Vec<Provider>,
}

impl Registry {
    /// Builds the registry from `<PROVIDER>_CLIENT_ID` / `_CLIENT_SECRET` pairs.
    /// A provider is enabled only when both are set; setting one alone is an error.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut providers = Vec::new();
        for definition in DEFINITIONS {
            let client_id = lookup(definition.client_id_variable)
                .map(|value| value.trim().to_owned())
                .unwrap_or_default();
            let client_secret = lookup(definition.client_secret_variable)
                .map(|value| value.trim().to_owned())
                .unwrap_or_default();
            if client_id.is_empty() != client_secret.is_empty() {
                let (missing, present) = if client_id.is_empty() {
                    (
                        definition.client_id_variable,
                        definition.client_secret_variable,
                    )
                } else {
                    (
                        definition.client_secret_variable,
                        definition.client_id_variable,
                    )
                };
                anyhow::bail!(
                    "{} OAuth configuration is incomplete: {missing} must be set when {present} is set",
                    definition.label
                );
            }
            if client_id.is_empty() {
                continue;
            }
            providers.push(Provider {
                id: definition.id,
                slug: definition.slug,
                label: definition.label,
                client_id,
                client_secret,
                scopes: definition.scopes,
                authorize_url: definition.authorize_url,
                token_url: definition.token_url,
            });
        }
        Ok(Self { providers })
    }

    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn get(&self, slug: &str) -> Option<&Provider> {
        self.providers.iter().find(|provider| provider.slug == slug)
    }

    pub fn public(&self) -> Vec<PublicProvider> {
        self.providers
            .iter()
            .map(|provider| PublicProvider {
                id: provider.slug,
                label: provider.label,
            })
            .collect()
    }
}

/// What the signed state cookie carries between redirect and callback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingSignIn {
    #[serde(rename = "s")]
    pub state: String,
    #[serde(rename = "v")]
    pub verifier: String,
    #[serde(rename = "p")]
    pub provider: String,
    #[serde(rename = "c")]
    pub callback: String,
    #[serde(rename = "e")]
    pub error_callback: String,
    #[serde(rename = "x")]
    pub expires: i64,
}

impl PendingSignIn {
    pub fn new(provider: &str, callback: String, error_callback: String) -> Self {
        Self {
            state: random_token(32),
            verifier: random_token(48),
            provider: provider.to_owned(),
            callback,
            error_callback,
            expires: chrono::Utc::now().timestamp() + STATE_LIFETIME_SECONDS,
        }
    }

    pub fn challenge(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()))
    }
}

fn signature(secret: &str, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(b"vault-oauth-state\0");
    mac.update(payload.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub fn seal(secret: &str, pending: &PendingSignIn) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(pending).expect("state serializes"));
    let signature = signature(secret, &payload);
    format!("{payload}.{signature}")
}

/// Verifies the cookie's signature and expiry. The caller still compares `state`.
pub fn open(secret: &str, sealed: &str) -> Option<PendingSignIn> {
    let (payload, provided) = sealed.split_once('.')?;
    if !constant_time_eq(&signature(secret, payload), provided) {
        return None;
    }
    let pending: PendingSignIn =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    (pending.expires > chrono::Utc::now().timestamp()).then_some(pending)
}

pub fn state_cookie(value: &str, secure: bool) -> HeaderValue {
    set_cookie(
        STATE_COOKIE,
        value,
        STATE_LIFETIME_SECONDS,
        secure,
        STATE_COOKIE_PATH,
    )
}

pub fn clear_state_cookie(secure: bool) -> HeaderValue {
    set_cookie(STATE_COOKIE, "", 0, secure, STATE_COOKIE_PATH)
}

pub fn redirect_uri(public_base_url: &str, provider: &Provider) -> String {
    format!("{public_base_url}/api/auth/callback/{}", provider.slug)
}

pub fn authorization_url(
    public_base_url: &str,
    provider: &Provider,
    pending: &PendingSignIn,
) -> String {
    let mut url = url::Url::parse(provider.authorize_url).expect("provider URLs are valid");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &provider.client_id)
            .append_pair("redirect_uri", &redirect_uri(public_base_url, provider))
            .append_pair("scope", &provider.scopes.join(" "))
            .append_pair("state", &pending.state)
            .append_pair("code_challenge", &pending.challenge())
            .append_pair("code_challenge_method", "S256");
        if provider.id == ProviderId::Google {
            query.append_pair("prompt", "select_account");
        }
    }
    url.into()
}

/// The identity a provider vouched for. `email` is always a verified address.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderIdentity {
    pub account_id: String,
    pub email: String,
    pub name: String,
    pub image: Option<String>,
}

/// Why a provider exchange failed, as the `error` query value app.js understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthFailure {
    /// The provider would not share a verified email.
    EmailNotFound,
    /// The code exchange or profile request failed.
    UnableToGetUserInfo,
}

impl OAuthFailure {
    pub fn code(self) -> &'static str {
        match self {
            Self::EmailNotFound => "email_not_found",
            Self::UnableToGetUserInfo => "unable_to_get_user_info",
        }
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Clipboard-Vault")
        .build()
        .expect("build OAuth HTTP client")
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
}

/// Exchanges the authorization code and loads the provider profile.
pub async fn fetch_identity(
    client: &reqwest::Client,
    public_base_url: &str,
    provider: &Provider,
    code: &str,
    verifier: &str,
) -> Result<ProviderIdentity, OAuthFailure> {
    let redirect = redirect_uri(public_base_url, provider);
    let token = client
        .post(provider.token_url)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect.as_str()),
            ("client_id", provider.client_id.as_str()),
            ("client_secret", provider.client_secret.as_str()),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|_| OAuthFailure::UnableToGetUserInfo)?;
    if !token.status().is_success() {
        return Err(OAuthFailure::UnableToGetUserInfo);
    }
    let access_token = token
        .json::<TokenResponse>()
        .await
        .ok()
        .and_then(|token| token.access_token)
        .filter(|token| !token.is_empty())
        .ok_or(OAuthFailure::UnableToGetUserInfo)?;
    match provider.id {
        ProviderId::Google => {
            let profile = get_json(
                client,
                "https://openidconnect.googleapis.com/v1/userinfo",
                &access_token,
                &[],
            )
            .await?;
            google_identity(&profile)
        }
        ProviderId::GitHub => {
            let headers = [
                ("accept", "application/vnd.github+json"),
                ("x-github-api-version", "2022-11-28"),
            ];
            let profile = get_json(
                client,
                "https://api.github.com/user",
                &access_token,
                &headers,
            )
            .await?;
            let emails = get_json(
                client,
                "https://api.github.com/user/emails",
                &access_token,
                &headers,
            )
            .await?;
            github_identity(&profile, &emails)
        }
        ProviderId::Discord => {
            let profile = get_json(
                client,
                "https://discord.com/api/users/@me",
                &access_token,
                &[],
            )
            .await?;
            discord_identity(&profile)
        }
    }
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    headers: &[(&str, &str)],
) -> Result<Value, OAuthFailure> {
    let mut request = client.get(url).bearer_auth(access_token);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request
        .send()
        .await
        .map_err(|_| OAuthFailure::UnableToGetUserInfo)?;
    if !response.status().is_success() {
        return Err(OAuthFailure::UnableToGetUserInfo);
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| OAuthFailure::UnableToGetUserInfo)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

pub fn google_identity(profile: &Value) -> Result<ProviderIdentity, OAuthFailure> {
    let account_id = text(profile, "sub").ok_or(OAuthFailure::UnableToGetUserInfo)?;
    let verified = profile.get("email_verified").and_then(Value::as_bool) == Some(true);
    let email = text(profile, "email")
        .filter(|_| verified)
        .ok_or(OAuthFailure::EmailNotFound)?;
    Ok(ProviderIdentity {
        name: text(profile, "name").unwrap_or_else(|| email.clone()),
        image: text(profile, "picture"),
        account_id,
        email,
    })
}

/// The primary verified address, else any verified one, as providers.ts chose.
pub fn select_verified_github_email(emails: &Value) -> Option<String> {
    let entries = emails.as_array()?;
    let verified = |entry: &&Value| entry.get("verified").and_then(Value::as_bool) == Some(true);
    let primary = |entry: &&Value| entry.get("primary").and_then(Value::as_bool) == Some(true);
    entries
        .iter()
        .filter(verified)
        .find(primary)
        .or_else(|| entries.iter().find(verified))
        .and_then(|entry| text(entry, "email"))
}

pub fn github_identity(profile: &Value, emails: &Value) -> Result<ProviderIdentity, OAuthFailure> {
    let account_id = match profile.get("id") {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(id)) if !id.is_empty() => id.clone(),
        _ => return Err(OAuthFailure::UnableToGetUserInfo),
    };
    let email = select_verified_github_email(emails).ok_or(OAuthFailure::EmailNotFound)?;
    let login = text(profile, "login").unwrap_or_else(|| "GitHub user".into());
    Ok(ProviderIdentity {
        account_id,
        email,
        name: text(profile, "name").unwrap_or(login),
        image: text(profile, "avatar_url"),
    })
}

pub fn select_verified_discord_email(profile: &Value) -> Option<String> {
    (profile.get("verified").and_then(Value::as_bool) == Some(true))
        .then(|| text(profile, "email"))
        .flatten()
}

pub fn discord_identity(profile: &Value) -> Result<ProviderIdentity, OAuthFailure> {
    let account_id = text(profile, "id").ok_or(OAuthFailure::UnableToGetUserInfo)?;
    let email = select_verified_discord_email(profile).ok_or(OAuthFailure::EmailNotFound)?;
    let username = text(profile, "username").unwrap_or_else(|| "Discord user".into());
    let image = text(profile, "avatar")
        .map(|avatar| format!("https://cdn.discordapp.com/avatars/{account_id}/{avatar}.png"));
    Ok(ProviderIdentity {
        name: text(profile, "global_name").unwrap_or(username),
        account_id,
        email,
        image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn registry(values: &[(&str, &str)]) -> anyhow::Result<Registry> {
        let map: HashMap<String, String> = values
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        Registry::from_lookup(|name| map.get(name).cloned())
    }

    #[test]
    fn only_complete_credential_pairs_enable_a_provider() {
        assert!(registry(&[]).unwrap().public().is_empty());
        let enabled = registry(&[
            ("GITHUB_CLIENT_ID", "id"),
            ("GITHUB_CLIENT_SECRET", "secret"),
        ])
        .unwrap();
        let public = enabled.public();
        assert_eq!(public.len(), 1);
        assert_eq!(public[0].id, "github");
        let error = registry(&[("DISCORD_CLIENT_ID", "id")])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("DISCORD_CLIENT_SECRET must be set"),
            "{error}"
        );
        // Blank values count as unset.
        assert!(
            registry(&[("GOOGLE_CLIENT_ID", "  "), ("GOOGLE_CLIENT_SECRET", "")])
                .unwrap()
                .public()
                .is_empty()
        );
    }

    #[test]
    fn public_metadata_never_contains_secrets() {
        let enabled = registry(&[
            ("GOOGLE_CLIENT_ID", "client"),
            ("GOOGLE_CLIENT_SECRET", "hunter2"),
        ])
        .unwrap();
        let serialized = serde_json::to_string(&enabled.public()).unwrap();
        assert!(!serialized.contains("hunter2"));
        assert!(!serialized.contains("client\""));
    }

    #[test]
    fn github_prefers_the_primary_verified_email() {
        let emails = json!([
            {"email": "old@example.com", "primary": false, "verified": true},
            {"email": "main@example.com", "primary": true, "verified": true},
        ]);
        assert_eq!(
            select_verified_github_email(&emails).as_deref(),
            Some("main@example.com")
        );
        let unverified_primary = json!([
            {"email": "main@example.com", "primary": true, "verified": false},
            {"email": "other@example.com", "primary": false, "verified": true},
        ]);
        assert_eq!(
            select_verified_github_email(&unverified_primary).as_deref(),
            Some("other@example.com")
        );
        assert_eq!(
            select_verified_github_email(&json!([{"email": "x@example.com", "verified": false}])),
            None
        );
        assert_eq!(select_verified_github_email(&json!({"email": "x"})), None);
    }

    #[test]
    fn discord_requires_a_verified_email() {
        assert_eq!(
            select_verified_discord_email(&json!({"email": "a@example.com", "verified": true}))
                .as_deref(),
            Some("a@example.com")
        );
        assert_eq!(
            select_verified_discord_email(&json!({"email": "a@example.com", "verified": false})),
            None
        );
        assert_eq!(
            discord_identity(&json!({"id": "1", "email": "a@example.com"})),
            Err(OAuthFailure::EmailNotFound)
        );
    }

    #[test]
    fn google_requires_a_verified_email() {
        let identity = google_identity(
            &json!({"sub": "42", "email": "g@example.com", "email_verified": true, "name": "G"}),
        )
        .unwrap();
        assert_eq!(identity.account_id, "42");
        assert_eq!(
            google_identity(
                &json!({"sub": "42", "email": "g@example.com", "email_verified": false})
            ),
            Err(OAuthFailure::EmailNotFound)
        );
    }

    #[test]
    fn state_cookies_are_tamper_evident() {
        let pending = PendingSignIn::new("github", "/".into(), "/login".into());
        let sealed = seal("a-secret-of-sufficient-length-000000", &pending);
        assert_eq!(
            open("a-secret-of-sufficient-length-000000", &sealed),
            Some(pending.clone())
        );
        assert_eq!(open("another-secret-of-sufficient-length-0", &sealed), None);
        let (payload, signature) = sealed.split_once('.').unwrap();
        let forged = format!("{payload}x.{signature}");
        assert_eq!(open("a-secret-of-sufficient-length-000000", &forged), None);
        let mut expired = pending;
        expired.expires = 0;
        assert_eq!(
            open(
                "a-secret-of-sufficient-length-000000",
                &seal("a-secret-of-sufficient-length-000000", &expired)
            ),
            None
        );
    }

    #[test]
    fn authorization_urls_carry_state_and_pkce() {
        let enabled = registry(&[
            ("GITHUB_CLIENT_ID", "id"),
            ("GITHUB_CLIENT_SECRET", "secret"),
        ])
        .unwrap();
        let provider = enabled.get("github").unwrap();
        let pending = PendingSignIn::new("github", "/".into(), "/login".into());
        let url = authorization_url("https://vault.example.com", provider, &pending);
        assert!(url.starts_with("https://github.com/login/oauth/authorize?"));
        assert!(url.contains(&format!("state={}", pending.state)));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fvault.example.com%2Fapi%2Fauth%2Fcallback%2Fgithub"
        ));
        assert!(!url.contains("secret"));
    }
}
