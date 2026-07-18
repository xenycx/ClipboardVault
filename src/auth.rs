use axum::http::{HeaderMap, header};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    error::{AppError, AppResult},
    models::{AuthContext, BridgeSession, VerifiedApiKey},
};

#[derive(Clone)]
pub struct AuthBridge {
    client: reqwest::Client,
    base_url: String,
    secret: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyVerifyRequest<'a> {
    key: &'a str,
    permission: &'a str,
}

#[derive(Debug, Deserialize)]
struct BridgeEnvelope<T> {
    data: T,
}

impl AuthBridge {
    pub fn new(base_url: &str, secret: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("build auth HTTP client"),
            base_url: base_url.to_owned(),
            secret: secret.to_owned(),
        }
    }

    pub async fn session(
        &self,
        headers: &HeaderMap,
        requested_organization: Option<&str>,
    ) -> AppResult<BridgeSession> {
        let cookie = headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .ok_or(AppError::Unauthorized)?;
        let mut request = self
            .client
            .get(format!("{}/internal/session", self.base_url))
            .header("x-auth-bridge-secret", &self.secret)
            .header(header::COOKIE, cookie);
        if let Some(organization) = requested_organization {
            request = request.header("x-workspace-id", organization);
        }
        self.decode(request.send().await).await
    }

    pub async fn verify_key(&self, key: &str, permission: &str) -> AppResult<VerifiedApiKey> {
        let response = self
            .client
            .post(format!("{}/internal/verify-key", self.base_url))
            .header("x-auth-bridge-secret", &self.secret)
            .json(&KeyVerifyRequest { key, permission })
            .send()
            .await;
        self.decode(response).await
    }

    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        headers: &[(&str, &str)],
    ) -> AppResult<T> {
        let mut request = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .header("x-auth-bridge-secret", &self.secret);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.decode(request.send().await).await
    }

    pub async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        headers: &[(&str, &str)],
    ) -> AppResult<T> {
        let mut request = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .header("x-auth-bridge-secret", &self.secret)
            .json(body);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.decode(request.send().await).await
    }

    pub async fn delete_json<T: DeserializeOwned>(
        &self,
        path: &str,
        headers: &[(&str, &str)],
    ) -> AppResult<T> {
        let mut request = self
            .client
            .delete(format!("{}{}", self.base_url, path))
            .header("x-auth-bridge-secret", &self.secret);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.decode(request.send().await).await
    }

    async fn decode<T: DeserializeOwned>(
        &self,
        response: Result<reqwest::Response, reqwest::Error>,
    ) -> AppResult<T> {
        let response = response.map_err(|_| AppError::AuthUnavailable)?;
        match response.status() {
            StatusCode::UNAUTHORIZED => return Err(AppError::Unauthorized),
            StatusCode::FORBIDDEN => return Err(AppError::Forbidden),
            StatusCode::NOT_FOUND => return Err(AppError::NotFound),
            status if !status.is_success() => return Err(AppError::AuthUnavailable),
            _ => {}
        }
        let envelope = response
            .json::<BridgeEnvelope<T>>()
            .await
            .map_err(|_| AppError::AuthUnavailable)?;
        Ok(envelope.data)
    }
}

pub fn api_key_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        && !value.trim().is_empty()
    {
        return Some(value.trim().to_owned());
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub fn workspace_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == "vault_workspace").then_some(value)
            })
        })
}

pub async fn authorize(
    bridge: &AuthBridge,
    headers: &HeaderMap,
    permission: &str,
) -> AppResult<AuthContext> {
    if let Some(key) = api_key_from_headers(headers) {
        let verified = bridge.verify_key(&key, permission).await?;
        return Ok(AuthContext {
            user_id: verified.created_by_user_id,
            api_key_id: Some(verified.id),
            organization_id: verified.organization_id,
            role: "api_key".into(),
            is_global_admin: false,
        });
    }

    let session = bridge.session(headers, workspace_cookie(headers)).await?;
    if session.user.approval_status != "approved" {
        return Err(AppError::PendingApproval);
    }
    let organization_id = session.active_organization_id.ok_or(AppError::Forbidden)?;
    Ok(AuthContext {
        user_id: Some(session.user.id),
        api_key_id: None,
        organization_id,
        role: session.active_role.unwrap_or_else(|| "member".into()),
        is_global_admin: session.user.role.split(',').any(|role| role == "admin"),
    })
}
