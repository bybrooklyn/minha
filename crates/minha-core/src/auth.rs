//! Secret-safe credential storage and OAuth/device-flow implementation.

use crate::models::Model;
use base64::Engine;
use rand::RngCore;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    fs::{self},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

#[derive(Clone)]
pub struct Secret(SecretString);
impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(SecretString::new(value.into().into()))
    }
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(REDACTED)")
    }
}

#[derive(Clone)]
pub enum Credential {
    ApiKey {
        model: Model,
        key: Secret,
    },
    OAuth {
        model: Model,
        access_token: Secret,
        refresh_token: Option<Secret>,
        expires_at_unix: Option<i64>,
    },
}
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { model, .. } => f
                .debug_struct("ApiKey")
                .field("model", model)
                .field("key", &"REDACTED")
                .finish(),
            Self::OAuth {
                model,
                expires_at_unix,
                ..
            } => f
                .debug_struct("OAuth")
                .field("model", model)
                .field("expires_at_unix", expires_at_unix)
                .field("tokens", &"REDACTED")
                .finish(),
        }
    }
}

pub trait CredentialStore: Send + Sync {
    fn get(&self, model: Model) -> Option<Credential>;
    fn put(&self, credential: Credential);
    fn remove(&self, model: Model) -> Option<Credential>;
}

#[derive(Clone, Default)]
pub struct MemoryCredentialStore {
    entries: Arc<RwLock<HashMap<Model, Credential>>>,
}
impl CredentialStore for MemoryCredentialStore {
    fn get(&self, model: Model) -> Option<Credential> {
        self.entries.read().ok()?.get(&model).cloned()
    }
    fn put(&self, credential: Credential) {
        let model = match &credential {
            Credential::ApiKey { model, .. } | Credential::OAuth { model, .. } => *model,
        };
        if let Ok(mut e) = self.entries.write() {
            e.insert(model, credential);
        }
    }
    fn remove(&self, model: Model) -> Option<Credential> {
        self.entries.write().ok()?.remove(&model)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthConfig {
    pub client_id: String,
    pub authorization_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
}
#[derive(Clone)]
pub struct DeviceAuthorization {
    pub verification_uri: String,
    pub user_code: String,
    pub device_code: Secret,
    pub expires_in_seconds: u64,
    pub interval_seconds: u64,
}
impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceAuthorization")
            .field("verification_uri", &self.verification_uri)
            .field("user_code", &self.user_code)
            .field("device_code", &"REDACTED")
            .field("expires_in_seconds", &self.expires_in_seconds)
            .field("interval_seconds", &self.interval_seconds)
            .finish()
    }
}
#[derive(Clone)]
pub struct OAuthToken {
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub expires_in_seconds: Option<u64>,
}
impl fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthToken")
            .field("access_token", &"REDACTED")
            .field("refresh_token", &"REDACTED")
            .field("expires_in_seconds", &self.expires_in_seconds)
            .finish()
    }
}

pub const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_ISSUER: &str = "https://auth.openai.com";
pub const OPENAI_DEVICE_USERCODE_PATH: &str = "/api/accounts/deviceauth/usercode";
pub const OPENAI_DEVICE_TOKEN_PATH: &str = "/api/accounts/deviceauth/token";
pub const OPENAI_DEVICE_VERIFICATION_PATH: &str = "/codex/device";
pub const OPENAI_CALLBACK_PORT: u16 = 1455;
pub const OPENAI_CALLBACK_FALLBACK_PORT: u16 = 1457;
pub const OPENAI_SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub const OPENAI_DEVICE_CALLBACK_URI: &str = "https://auth.openai.com/deviceauth/callback";

#[derive(Clone, Debug)]
pub struct Pkce {
    pub verifier: Secret,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let mut bytes = [0_u8; 64];
        rand::rng().fill_bytes(&mut bytes);
        let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let challenge =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier: Secret::new(verifier),
            challenge,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceCodeRequest<'a> {
    pub client_id: &'a str,
}

#[derive(Clone, Deserialize, Eq, PartialEq)]
pub struct DeviceCodeResponse {
    pub device_auth_id: String,
    #[serde(alias = "user_code", alias = "usercode")]
    pub user_code: String,
    #[serde(default, deserialize_with = "deserialize_interval")]
    pub interval: u64,
}
impl fmt::Debug for DeviceCodeResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCodeResponse")
            .field("device_auth_id", &"REDACTED")
            .field("user_code", &"REDACTED")
            .field("interval", &self.interval)
            .finish()
    }
}

#[derive(Clone, Serialize)]
pub struct DeviceTokenRequest<'a> {
    pub device_auth_id: &'a str,
    pub user_code: &'a str,
}
impl fmt::Debug for DeviceTokenRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceTokenRequest")
            .field("device_auth_id", &"REDACTED")
            .field("user_code", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Serialize)]
pub struct AuthorizationCodeExchange<'a> {
    pub grant_type: &'static str,
    pub code: &'a str,
    pub redirect_uri: &'a str,
    pub client_id: &'a str,
    pub code_verifier: &'a str,
}
impl fmt::Debug for AuthorizationCodeExchange<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationCodeExchange")
            .field("grant_type", &self.grant_type)
            .field("code", &"REDACTED")
            .field("redirect_uri", &self.redirect_uri)
            .field("client_id", &self.client_id)
            .field("code_verifier", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Deserialize)]
pub struct OAuthTokenResponse {
    #[serde(default)]
    pub id_token: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default, alias = "expires_in")]
    pub expires_in_seconds: Option<u64>,
}
impl fmt::Debug for OAuthTokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthTokenResponse")
            .field("tokens", &"REDACTED")
            .finish()
    }
}

pub fn openai_oauth_config() -> OAuthConfig {
    OAuthConfig {
        client_id: OPENAI_CLIENT_ID.to_owned(),
        authorization_url: format!("{OPENAI_ISSUER}/oauth/authorize"),
        token_url: format!("{OPENAI_ISSUER}/oauth/token"),
        scopes: OPENAI_SCOPES.split_whitespace().map(str::to_owned).collect(),
    }
}

pub fn openai_redirect_uri(port: u16) -> Option<String> {
    match port {
        OPENAI_CALLBACK_PORT | OPENAI_CALLBACK_FALLBACK_PORT => {
            Some(format!("http://localhost:{port}/auth/callback"))
        }
        _ => None,
    }
}

pub fn device_usercode_request(client_id: &str) -> DeviceCodeRequest<'_> {
    DeviceCodeRequest { client_id }
}
impl DeviceCodeRequest<'_> {
    pub fn json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}
pub fn device_token_request<'a>(device_auth_id: &'a str, user_code: &'a str) -> DeviceTokenRequest<'a> {
    DeviceTokenRequest {
        device_auth_id,
        user_code,
    }
}
impl DeviceTokenRequest<'_> {
    pub fn json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

pub fn authorization_url(config: &OAuthConfig, redirect_uri: &str, pkce: &Pkce, state: &str) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", &config.scopes.join(" "))
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    format!(
        "{}?{}",
        config.authorization_url.trim_end_matches('/'),
        query.finish()
    )
}

pub fn exchange_request<'a>(
    client_id: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    pkce: &'a Pkce,
) -> AuthorizationCodeExchange<'a> {
    AuthorizationCodeExchange {
        grant_type: "authorization_code",
        code,
        redirect_uri,
        client_id,
        code_verifier: pkce.verifier.expose(),
    }
}
impl AuthorizationCodeExchange<'_> {
    pub fn form_body(&self) -> String {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", self.grant_type)
            .append_pair("code", self.code)
            .append_pair("redirect_uri", self.redirect_uri)
            .append_pair("client_id", self.client_id)
            .append_pair("code_verifier", self.code_verifier);
        form.finish()
    }
}

impl DeviceCodeResponse {
    pub fn into_authorization(self, verification_uri: impl Into<String>) -> DeviceAuthorization {
        DeviceAuthorization {
            verification_uri: verification_uri.into(),
            user_code: self.user_code,
            device_code: Secret::new(self.device_auth_id),
            expires_in_seconds: 15 * 60,
            interval_seconds: self.interval,
        }
    }
}

impl OAuthTokenResponse {
    pub fn into_token(self, expires_in_seconds: Option<u64>) -> OAuthToken {
        OAuthToken {
            access_token: Secret::new(self.access_token),
            refresh_token: Some(Secret::new(self.refresh_token)),
            expires_in_seconds,
        }
    }
}

/// A locally stored ChatGPT login. JWT claims in this type are informational only: this
/// module decodes them to display account details, but does not verify their signatures.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthRecord {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub expires_at_unix: Option<i64>,
}

pub type AuthSession = AuthRecord;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountProfile {
    pub name: String,
    pub label: String,
    pub enabled: bool,
    pub account_id: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct AccountProfileIndex {
    active: Option<String>,
    #[serde(default)]
    profiles: Vec<AccountProfile>,
}

impl fmt::Debug for AuthRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthRecord")
            .field("access_token", &"REDACTED")
            .field("refresh_token", &"REDACTED")
            .field("id_token", &"REDACTED")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("expires_at_unix", &self.expires_at_unix)
            .finish()
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct UnverifiedJwtClaims {
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub expires_at_unix: Option<i64>,
}

/// Decode the payload of a JWT without authenticating it.
pub fn parse_unverified_jwt_claims(jwt: &str) -> Result<UnverifiedJwtClaims, AuthError> {
    let payload = jwt.split('.').nth(1).ok_or(AuthError::InvalidResponse)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AuthError::InvalidResponse)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| AuthError::InvalidResponse)?;
    let object = value.as_object().ok_or(AuthError::InvalidResponse)?;
    let nested = object
        .get("https://api.openai.com/auth")
        .and_then(serde_json::Value::as_object);
    let email = object
        .get("email")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            object
                .get("https://api.openai.com/profile")
                .and_then(|v| v.get("email"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned);
    let account_id = nested
        .and_then(|claims| {
            claims
                .get("chatgpt_account_id")
                .or_else(|| claims.get("account_id"))
        })
        .and_then(serde_json::Value::as_str)
        .or_else(|| object.get("account_id").and_then(serde_json::Value::as_str))
        .map(str::to_owned);
    let expires_at_unix = object.get("exp").and_then(serde_json::Value::as_i64);
    Ok(UnverifiedJwtClaims {
        email,
        account_id,
        expires_at_unix,
    })
}

fn oauth_base_url(config: &OAuthConfig) -> Result<Url, AuthError> {
    let mut url = Url::parse(&config.token_url).map_err(|_| AuthError::InvalidResponse)?;
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn device_endpoint(config: &OAuthConfig, suffix: &str) -> Result<Url, AuthError> {
    let mut url = oauth_base_url(config)?;
    url.set_path(&format!("/api/accounts/deviceauth/{suffix}"));
    Ok(url)
}

fn device_verification_url(config: &OAuthConfig) -> Result<String, AuthError> {
    let mut url = oauth_base_url(config)?;
    url.set_path(OPENAI_DEVICE_VERIFICATION_PATH);
    Ok(url.to_string())
}

#[derive(Clone)]
pub struct CodexOAuthClient {
    http: reqwest::Client,
    pub config: OAuthConfig,
    pub poll_timeout: Duration,
}

impl CodexOAuthClient {
    pub fn new(config: OAuthConfig) -> Result<Self, AuthError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .build()
                .map_err(|e| AuthError::Transport(e.to_string()))?,
            config,
            poll_timeout: Duration::from_secs(15 * 60),
        })
    }

    pub async fn begin_device_authorization(&self) -> Result<DeviceAuthorization, AuthError> {
        let response = self
            .http
            .post(device_endpoint(&self.config, "usercode")?)
            .json(&device_usercode_request(&self.config.client_id))
            .send()
            .await
            .map_err(|e| AuthError::Transport(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(if status == reqwest::StatusCode::NOT_FOUND {
                AuthError::Unsupported
            } else {
                AuthError::Http(status.as_u16())
            });
        }
        let data: DeviceCodeResponse = response.json().await.map_err(|_| AuthError::InvalidResponse)?;
        Ok(data.into_authorization(device_verification_url(&self.config)?))
    }

    pub async fn complete_device_authorization(
        &self,
        device: &DeviceAuthorization,
    ) -> Result<AuthRecord, AuthError> {
        let started = tokio::time::Instant::now();
        let mut interval = Duration::from_secs(device.interval_seconds.max(1));
        let response: CodeSuccessResponse = loop {
            let response = self
                .http
                .post(device_endpoint(&self.config, "token")?)
                .json(&DeviceTokenRequest {
                    device_auth_id: device.device_code.expose(),
                    user_code: &device.user_code,
                })
                .send()
                .await
                .map_err(|e| AuthError::Transport(e.to_string()))?;
            if response.status().is_success() {
                break response.json().await.map_err(|_| AuthError::InvalidResponse)?;
            }
            // RFC 8628 polling semantics: pending and slow-down arrive as
            // 400 with an `error` body field; a real denial also arrives as
            // 400 and must fail fast instead of polling for the full timeout.
            let status = response.status();
            let error = response
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|body| {
                    body.get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            match error.as_str() {
                "authorization_pending" | "" if status == reqwest::StatusCode::FORBIDDEN => {}
                "authorization_pending" | "" if status == reqwest::StatusCode::NOT_FOUND => {}
                "authorization_pending" => {}
                "slow_down" => {
                    interval = interval.saturating_add(Duration::from_secs(5));
                }
                "access_denied" => return Err(AuthError::Denied),
                "expired_token" => return Err(AuthError::Expired),
                _ => return Err(AuthError::Http(status.as_u16())),
            }
            if started.elapsed() >= self.poll_timeout {
                return Err(AuthError::Expired);
            }
            tokio::time::sleep(interval.min(self.poll_timeout.saturating_sub(started.elapsed()))).await;
        };
        self.exchange_authorization_code(&response).await
    }

    async fn exchange_authorization_code(
        &self,
        response: &CodeSuccessResponse,
    ) -> Result<AuthRecord, AuthError> {
        let body = [
            ("grant_type", "authorization_code"),
            ("code", response.authorization_code.as_str()),
            ("redirect_uri", OPENAI_DEVICE_CALLBACK_URI),
            ("client_id", self.config.client_id.as_str()),
            ("code_verifier", response.code_verifier.as_str()),
        ];
        let token: OAuthTokenResponse = self
            .http
            .post(&self.config.token_url)
            .form(&body)
            .send()
            .await
            .map_err(|e| AuthError::Transport(e.to_string()))?
            .error_for_status()
            .map_err(|e| AuthError::Transport(e.to_string()))?
            .json()
            .await
            .map_err(|_| AuthError::InvalidResponse)?;
        record_from_tokens(token)
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<AuthRecord, AuthError> {
        let body = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.config.client_id.as_str()),
        ];
        let token: OAuthTokenResponse = self
            .http
            .post(&self.config.token_url)
            .form(&body)
            .send()
            .await
            .map_err(|e| AuthError::Transport(e.to_string()))?
            .error_for_status()
            .map_err(|e| AuthError::Transport(e.to_string()))?
            .json()
            .await
            .map_err(|_| AuthError::InvalidResponse)?;
        let mut record = record_from_tokens(token)?;
        if record.refresh_token.as_deref() == Some("") {
            record.refresh_token = Some(refresh_token.to_owned());
        }
        Ok(record)
    }
}

#[derive(Deserialize)]
struct CodeSuccessResponse {
    authorization_code: String,
    code_verifier: String,
    #[allow(dead_code)]
    code_challenge: String,
}

fn record_from_tokens(token: OAuthTokenResponse) -> Result<AuthRecord, AuthError> {
    let claims_source = if token.id_token.is_empty() {
        &token.access_token
    } else {
        &token.id_token
    };
    let claims = parse_unverified_jwt_claims(claims_source).ok();
    let expires_at_unix = claims.as_ref().and_then(|c| c.expires_at_unix).or_else(|| {
        token.expires_in_seconds.and_then(|s| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|now| now.as_secs() as i64 + s as i64)
        })
    });
    Ok(AuthRecord {
        access_token: token.access_token,
        refresh_token: Some(token.refresh_token),
        id_token: Some(token.id_token),
        account_id: claims.as_ref().and_then(|c| c.account_id.clone()),
        email: claims.and_then(|c| c.email),
        expires_at_unix,
    })
}

pub fn default_auth_path() -> Result<PathBuf, AuthError> {
    dirs::home_dir()
        .map(|p| p.join(".minha/auth.json"))
        .ok_or(AuthError::HomeDirectoryUnavailable)
}

pub fn account_profiles_dir() -> Result<PathBuf, AuthError> {
    dirs::home_dir()
        .map(|path| path.join(".minha/accounts"))
        .ok_or(AuthError::HomeDirectoryUnavailable)
}

fn profile_index_path() -> Result<PathBuf, AuthError> {
    Ok(account_profiles_dir()?.join("profiles.json"))
}

fn valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn profile_auth_path(name: &str) -> Result<PathBuf, AuthError> {
    if !valid_profile_name(name) {
        return Err(AuthError::InvalidProfileName);
    }
    Ok(account_profiles_dir()?.join(format!("{name}.json")))
}

async fn load_profile_index() -> Result<AccountProfileIndex, AuthError> {
    match tokio::fs::read(profile_index_path()?).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| AuthError::InvalidResponse),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AccountProfileIndex::default()),
        Err(error) => Err(AuthError::Io(error.to_string())),
    }
}

async fn save_profile_index(index: &AccountProfileIndex) -> Result<(), AuthError> {
    let bytes = serde_json::to_vec_pretty(index).map_err(|_| AuthError::InvalidResponse)?;
    save_private_bytes(profile_index_path()?, &bytes).await
}

async fn save_private_bytes(path: PathBuf, bytes: &[u8]) -> Result<(), AuthError> {
    let parent = path
        .parent()
        .ok_or_else(|| AuthError::Io("private path has no parent".into()))?
        .to_path_buf();
    tokio::fs::create_dir_all(&parent)
        .await
        .map_err(|error| AuthError::Io(error.to_string()))?;
    let owned = bytes.to_vec();
    tokio::task::spawn_blocking(move || atomic_private_write(&parent, &path, &owned))
        .await
        .map_err(|error| AuthError::Io(error.to_string()))?
        .map_err(|error| AuthError::Io(error.to_string()))
}

/// Write private bytes through a randomly named exclusive temporary file,
/// then atomically rename over the destination. The random name avoids the
/// pid-based predictable path (no symlink/truncate race), and permission
/// failures are propagated instead of swallowed.
fn atomic_private_write(parent: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub async fn list_account_profiles() -> Result<Vec<AccountProfile>, AuthError> {
    Ok(load_profile_index().await?.profiles)
}

pub async fn active_account_profile() -> Result<Option<AccountProfile>, AuthError> {
    let index = load_profile_index().await?;
    Ok(index
        .active
        .as_deref()
        .and_then(|active| index.profiles.iter().find(|profile| profile.name == active))
        .cloned())
}

pub async fn load_account_profile(name: &str) -> Result<Option<AuthRecord>, AuthError> {
    load_auth(profile_auth_path(name)?).await
}

pub async fn save_account_profile(
    name: &str,
    label: &str,
    record: &AuthRecord,
    make_active: bool,
) -> Result<(), AuthError> {
    save_auth(profile_auth_path(name)?, record).await?;
    let mut index = load_profile_index().await?;
    let profile = AccountProfile {
        name: name.to_owned(),
        label: if label.trim().is_empty() {
            name.to_owned()
        } else {
            label.trim().to_owned()
        },
        enabled: true,
        account_id: record.account_id.clone(),
        email: record.email.clone(),
    };
    if let Some(existing) = index.profiles.iter_mut().find(|candidate| candidate.name == name) {
        *existing = profile;
    } else {
        index.profiles.push(profile);
        index.profiles.sort_by(|left, right| left.name.cmp(&right.name));
    }
    if make_active || index.active.is_none() {
        index.active = Some(name.to_owned());
    }
    save_profile_index(&index).await
}

pub async fn set_active_account_profile(name: &str) -> Result<(), AuthError> {
    let mut index = load_profile_index().await?;
    let profile = index
        .profiles
        .iter()
        .find(|profile| profile.name == name)
        .ok_or(AuthError::ProfileNotFound)?;
    if !profile.enabled {
        return Err(AuthError::ProfileDisabled);
    }
    index.active = Some(name.to_owned());
    save_profile_index(&index).await
}

pub async fn set_account_profile_enabled(name: &str, enabled: bool) -> Result<(), AuthError> {
    let mut index = load_profile_index().await?;
    let profile = index
        .profiles
        .iter_mut()
        .find(|profile| profile.name == name)
        .ok_or(AuthError::ProfileNotFound)?;
    profile.enabled = enabled;
    if !enabled && index.active.as_deref() == Some(name) {
        index.active = index
            .profiles
            .iter()
            .find(|profile| profile.enabled)
            .map(|profile| profile.name.clone());
    }
    save_profile_index(&index).await
}

pub async fn remove_account_profile(name: &str) -> Result<bool, AuthError> {
    let path = profile_auth_path(name)?;
    let removed = logout(path).await?;
    let mut index = load_profile_index().await?;
    let previous_len = index.profiles.len();
    index.profiles.retain(|profile| profile.name != name);
    if index.active.as_deref() == Some(name) {
        index.active = index
            .profiles
            .iter()
            .find(|profile| profile.enabled)
            .map(|profile| profile.name.clone());
    }
    if removed || index.profiles.len() != previous_len {
        save_profile_index(&index).await?;
    }
    Ok(removed || index.profiles.len() != previous_len)
}

pub async fn enabled_account_records() -> Result<Vec<(AccountProfile, AuthRecord)>, AuthError> {
    let index = load_profile_index().await?;
    let mut records = Vec::new();
    for profile in index.profiles.into_iter().filter(|profile| profile.enabled) {
        if let Some(record) = load_account_profile(&profile.name).await? {
            records.push((profile, record));
        }
    }
    Ok(records)
}

pub async fn load_auth(path: impl AsRef<Path>) -> Result<Option<AuthRecord>, AuthError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| AuthError::InvalidResponse),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AuthError::Io(e.to_string())),
    }
}

pub async fn save_auth(path: impl AsRef<Path>, record: &AuthRecord) -> Result<(), AuthError> {
    let path = path.as_ref();
    let bytes = serde_json::to_vec_pretty(record).map_err(|_| AuthError::InvalidResponse)?;
    save_private_bytes(path.to_path_buf(), &bytes).await
}

pub async fn logout(path: impl AsRef<Path>) -> Result<bool, AuthError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(AuthError::Io(e.to_string())),
    }
}

pub async fn auth_status(path: impl AsRef<Path>) -> Result<bool, AuthError> {
    Ok(load_auth(path).await?.is_some())
}

pub async fn load_default_auth() -> Result<Option<AuthRecord>, AuthError> {
    if let Some(profile) = active_account_profile().await?
        && profile.enabled
        && let Some(record) = load_account_profile(&profile.name).await?
    {
        return Ok(Some(record));
    }
    load_auth(default_auth_path()?).await
}

pub async fn save_default_auth(record: &AuthRecord) -> Result<(), AuthError> {
    save_auth(default_auth_path()?, record).await?;
    save_account_profile("default", "Default", record, true).await
}

pub async fn logout_default() -> Result<bool, AuthError> {
    let legacy_removed = logout(default_auth_path()?).await?;
    if let Some(active) = active_account_profile().await? {
        return Ok(remove_account_profile(&active.name).await? || legacy_removed);
    }
    Ok(legacy_removed)
}

pub async fn default_auth_status() -> Result<bool, AuthError> {
    Ok(load_default_auth().await?.is_some())
}

fn deserialize_interval<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumberOrString {
        Number(u64),
        String(String),
    }
    match NumberOrString::deserialize(deserializer)? {
        NumberOrString::Number(value) => Ok(value),
        NumberOrString::String(value) => value.trim().parse().map_err(serde::de::Error::custom),
    }
}

pub trait OAuthProvider: Send + Sync {
    fn begin_device_authorization(&self, config: &OAuthConfig) -> Result<DeviceAuthorization, AuthError>;
    fn poll_device_token(
        &self,
        config: &OAuthConfig,
        device: &DeviceAuthorization,
    ) -> Result<OAuthToken, AuthError>;
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthError {
    Unsupported,
    InvalidResponse,
    Expired,
    Denied,
    Transport(String),
    Http(u16),
    Io(String),
    HomeDirectoryUnavailable,
    InvalidProfileName,
    ProfileNotFound,
    ProfileDisabled,
}
impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for AuthError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    struct ScriptedResponse {
        status: u16,
        body: &'static str,
    }

    /// A scripted OAuth device server: serves the scripted token-poll
    /// responses in order, then one successful exchange response. The
    /// exchange accept times out after 5s so denial tests that never
    /// exchange can finish promptly.
    struct DeviceFlowServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        join: thread::JoinHandle<io::Result<()>>,
    }

    impl DeviceFlowServer {
        fn start(token_responses: Vec<ScriptedResponse>, exchange_body: &'static str) -> io::Result<Self> {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            let address = listener.local_addr()?;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded_requests = Arc::clone(&requests);
            let join = thread::spawn(move || {
                for response in token_responses {
                    let (mut stream, _) = listener.accept()?;
                    let request = read_http_request(&mut stream)?;
                    if let Ok(mut requests) = recorded_requests.lock() {
                        requests.push(request);
                    }
                    let reason = match response.status {
                        200 => "OK",
                        400 => "Bad Request",
                        _ => "Fixture Response",
                    };
                    let header = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        response.status,
                        reason,
                        response.body.len()
                    );
                    stream.write_all(header.as_bytes())?;
                    stream.write_all(response.body.as_bytes())?;
                    stream.flush()?;
                }
                listener.set_nonblocking(true)?;
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let request = read_http_request(&mut stream)?;
                            if let Ok(mut requests) = recorded_requests.lock() {
                                requests.push(request);
                            }
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                                exchange_body.len()
                            );
                            stream.write_all(header.as_bytes())?;
                            stream.write_all(exchange_body.as_bytes())?;
                            stream.flush()?;
                            return Ok(());
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            });
            Ok(Self {
                base_url: format!("http://{address}"),
                requests,
                join,
            })
        }

        fn finish(self) -> Vec<String> {
            self.join
                .join()
                .expect("fixture thread panicked")
                .expect("fixture failed");
            Arc::try_unwrap(self.requests)
                .ok()
                .and_then(|mutex| mutex.into_inner().ok())
                .unwrap_or_default()
        }
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> io::Result<String> {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut body_length = None;
        loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if body_length.is_none()
                && let Some(header_end) = bytes
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
            {
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                });
                body_length = content_length.or(Some(0));
            }
            if let (Some(header_end), Some(body_length)) = (find_header_end(&bytes), body_length)
                && bytes.len() >= header_end + body_length
            {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn find_header_end(bytes: &[u8]) -> Option<usize> {
        bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
    }

    fn device_client(base_url: &str) -> CodexOAuthClient {
        CodexOAuthClient::new(OAuthConfig {
            client_id: "client-1".into(),
            authorization_url: format!("{base_url}/authorize"),
            token_url: format!("{base_url}/token_exchange"),
            scopes: vec![],
        })
        .expect("test operation should succeed")
    }

    fn device_authorization() -> DeviceAuthorization {
        DeviceAuthorization {
            verification_uri: "http://fixture/verify".into(),
            user_code: "user-code".into(),
            device_code: Secret::new("device-code"),
            expires_in_seconds: 900,
            interval_seconds: 1,
        }
    }

    #[test]
    fn store_round_trip_and_debug_redacts() {
        let s = MemoryCredentialStore::default();
        s.put(Credential::ApiKey {
            model: Model::Spark,
            key: Secret::new("do-not-print"),
        });
        let c = s.get(Model::Spark).expect("test operation should succeed");
        assert!(format!("{c:?}").contains("REDACTED"));
        assert!(!format!("{c:?}").contains("do-not-print"));
        assert!(s.remove(Model::Spark).is_some());
    }
    #[test]
    fn oauth_contract_has_no_secret_in_debug() {
        let t = OAuthToken {
            access_token: Secret::new("token"),
            refresh_token: None,
            expires_in_seconds: Some(60),
        };
        assert!(!format!("{t:?}").contains("\"token\""));
    }

    #[test]
    fn typed_requests_match_device_and_pkce_contracts() {
        let user = device_usercode_request(OPENAI_CLIENT_ID);
        assert_eq!(
            user.json().expect("test operation should succeed"),
            r#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#
        );
        let poll = device_token_request("device-secret", "user-secret");
        assert_eq!(
            poll.json().expect("test operation should succeed"),
            r#"{"device_auth_id":"device-secret","user_code":"user-secret"}"#
        );

        let pkce = Pkce::generate();
        assert_eq!(pkce.challenge.len(), 43);
        let exchange = exchange_request(
            OPENAI_CLIENT_ID,
            "auth-code",
            "http://localhost:1455/auth/callback",
            &pkce,
        );
        assert!(exchange.form_body().contains("grant_type=authorization_code"));
        assert!(format!("{exchange:?}").contains("REDACTED"));
    }

    #[test]
    fn openai_authorization_url_uses_s256_and_allowed_callbacks() {
        let config = openai_oauth_config();
        let pkce = Pkce::generate();
        let url = authorization_url(
            &config,
            &openai_redirect_uri(OPENAI_CALLBACK_PORT).expect("test operation should succeed"),
            &pkce,
            "state",
        );
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("code_challenge_method=S256"));
        assert_eq!(openai_redirect_uri(1456), None);
        assert_eq!(
            device_verification_url(&config).expect("test operation should succeed"),
            "https://auth.openai.com/codex/device"
        );
    }

    #[test]
    fn jwt_claims_are_decoded_but_not_verified() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            br#"{"email":"user@example.test","exp":2000000000,"https://api.openai.com/auth":{"chatgpt_account_id":"acct_test"}}"#,
        );
        let claims = parse_unverified_jwt_claims(&format!("header.{payload}.not-a-real-signature"))
            .expect("test operation should succeed");
        assert_eq!(claims.email.as_deref(), Some("user@example.test"));
        assert_eq!(claims.account_id.as_deref(), Some("acct_test"));
        assert_eq!(claims.expires_at_unix, Some(2_000_000_000));
    }

    #[tokio::test]
    async fn auth_file_round_trip_and_debug_redaction() {
        let directory = tempfile::tempdir().expect("test operation should succeed");
        let path = directory.path().join("auth.json");
        let record = AuthRecord {
            access_token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            id_token: Some("id-secret".into()),
            account_id: Some("acct_test".into()),
            email: Some("user@example.test".into()),
            expires_at_unix: Some(123),
        };
        save_auth(&path, &record)
            .await
            .expect("test operation should succeed");
        assert_eq!(
            load_auth(&path).await.expect("test operation should succeed"),
            Some(record.clone())
        );
        assert!(!format!("{record:?}").contains("secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path)
                    .expect("test operation should succeed")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(logout(&path).await.expect("test operation should succeed"));
        assert!(!logout(&path).await.expect("test operation should succeed"));
    }

    #[tokio::test]
    async fn auth_write_is_atomic_and_leaves_no_predictable_temp_files() {
        let directory = tempfile::tempdir().expect("test operation should succeed");
        let path = directory.path().join("auth.json");
        let record = AuthRecord {
            access_token: "first".into(),
            refresh_token: None,
            id_token: None,
            account_id: None,
            email: None,
            expires_at_unix: None,
        };
        save_auth(&path, &record)
            .await
            .expect("test operation should succeed");
        let replaced = AuthRecord {
            access_token: "second".into(),
            ..record.clone()
        };
        save_auth(&path, &replaced)
            .await
            .expect("test operation should succeed");
        assert_eq!(
            load_auth(&path)
                .await
                .expect("test operation should succeed")
                .expect("auth exists")
                .access_token,
            "second"
        );
        let leftovers = fs::read_dir(directory.path())
            .expect("test operation should succeed")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("tmp-"))
            .count();
        assert_eq!(leftovers, 0, "predictable temp files were left behind");
    }

    #[tokio::test]
    async fn device_poll_retries_pending_then_exchanges() {
        let fixture = match DeviceFlowServer::start(
            vec![
                ScriptedResponse {
                    status: 400,
                    body: r#"{"error":"authorization_pending"}"#,
                },
                ScriptedResponse {
                    status: 400,
                    body: r#"{"error":"authorization_pending"}"#,
                },
                ScriptedResponse {
                    status: 200,
                    body: r#"{"authorization_code":"auth-code","code_verifier":"verifier","code_challenge":"challenge"}"#,
                },
            ],
            r#"{"access_token":"access-secret","refresh_token":"refresh-secret","id_token":"","expires_in_seconds":3600}"#,
        ) {
            Ok(fixture) => fixture,
            Err(error) => panic!("could not start fixture: {error}"),
        };
        let client = device_client(&fixture.base_url);
        let record = client
            .complete_device_authorization(&device_authorization())
            .await
            .expect("pending polls must eventually complete the exchange");
        assert_eq!(record.access_token, "access-secret");
        assert_eq!(record.refresh_token.as_deref(), Some("refresh-secret"));
        let requests = fixture.finish();
        assert_eq!(requests.len(), 4, "two pending polls, one code, one exchange");
        let first_poll = &requests[0];
        assert!(
            first_poll
                .to_ascii_lowercase()
                .contains("/api/accounts/deviceauth/token"),
            "unexpected poll target: {requests:?}"
        );
        let (_, body) = first_poll
            .split_once("\r\n\r\n")
            .expect("poll request had no body");
        assert_eq!(
            body,
            r#"{"device_auth_id":"device-code","user_code":"user-code"}"#
        );
    }

    #[tokio::test]
    async fn device_denial_fails_fast_without_polling() {
        let fixture = DeviceFlowServer::start(
            vec![ScriptedResponse {
                status: 400,
                body: r#"{"error":"access_denied"}"#,
            }],
            r#"{}"#,
        )
        .expect("test operation should succeed");
        let client = device_client(&fixture.base_url);
        let started = std::time::Instant::now();
        let error = client
            .complete_device_authorization(&device_authorization())
            .await
            .expect_err("access_denied must not keep polling");
        assert!(matches!(error, AuthError::Denied));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "denial must fail fast, not sleep through poll intervals"
        );
        let requests = fixture.finish();
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn device_expired_token_fails_fast() {
        let fixture = DeviceFlowServer::start(
            vec![ScriptedResponse {
                status: 400,
                body: r#"{"error":"expired_token"}"#,
            }],
            r#"{}"#,
        )
        .expect("test operation should succeed");
        let client = device_client(&fixture.base_url);
        let error = client
            .complete_device_authorization(&device_authorization())
            .await
            .expect_err("expired_token must not keep polling");
        assert!(matches!(error, AuthError::Expired));
        let requests = fixture.finish();
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn device_unknown_error_surfaces_http_status() {
        let fixture = DeviceFlowServer::start(
            vec![ScriptedResponse {
                status: 400,
                body: r#"{"error":"server_error"}"#,
            }],
            r#"{}"#,
        )
        .expect("test operation should succeed");
        let client = device_client(&fixture.base_url);
        let error = client
            .complete_device_authorization(&device_authorization())
            .await
            .expect_err("unexpected errors must surface instead of polling");
        assert!(matches!(error, AuthError::Http(400)));
        let requests = fixture.finish();
        assert_eq!(requests.len(), 1);
    }
}
