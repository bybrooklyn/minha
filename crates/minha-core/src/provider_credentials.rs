//! Private, atomic provider credential storage. Credentials never enter SQLite.

use secrecy::SecretString;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use thiserror::Error;
use url::Url;

use crate::mimo::XIAOMI_MIMO_BASE_URL;

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential file is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("provider API key must not be empty")]
    Empty,
    #[error("provider base URL must be an absolute HTTPS URL without credentials, query, or fragment")]
    BaseUrl,
}

/// A secret-safe, user-scoped MiMo credential.  It intentionally lives
/// outside project configuration and SQLite so a workspace can never commit
/// it accidentally.
#[derive(Clone)]
pub struct XiaomiMiMoCredential {
    pub api_key: SecretString,
    pub base_url: String,
}

impl std::fmt::Debug for XiaomiMiMoCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("XiaomiMiMoCredential")
            .field("api_key", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .finish()
    }
}

pub fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join("minha/providers.json"))
}

pub fn save_deepseek_key(path: &Path, key: &str) -> Result<(), CredentialError> {
    if key.trim().is_empty() {
        return Err(CredentialError::Empty);
    }
    let mut document = read_document(path)?;
    document["schema_version"] = json!(1);
    document["providers"]["deepseek"]["api_key"] = json!(key);
    atomic_private_write(path, &serde_json::to_vec_pretty(&document)?)
}

pub fn load_deepseek_key(path: &Path) -> Result<Option<SecretString>, CredentialError> {
    Ok(read_document(path)?
        .pointer("/providers/deepseek/api_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(|key| SecretString::from(key.to_owned())))
}

pub fn remove_deepseek(path: &Path) -> Result<bool, CredentialError> {
    let mut document = read_document(path)?;
    let removed = document
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .and_then(|providers| providers.remove("deepseek"))
        .is_some();
    if removed {
        atomic_private_write(path, &serde_json::to_vec_pretty(&document)?)?;
    }
    Ok(removed)
}

pub fn save_xiaomi_mimo(path: &Path, api_key: &str, base_url: Option<&str>) -> Result<(), CredentialError> {
    if api_key.trim().is_empty() {
        return Err(CredentialError::Empty);
    }
    let base_url = normalize_mimo_base_url(base_url.unwrap_or(XIAOMI_MIMO_BASE_URL))?;
    let mut document = read_document(path)?;
    document["schema_version"] = json!(1);
    document["providers"]["xiaomi_mimo"] = json!({
        "api_key": api_key,
        "base_url": base_url,
    });
    atomic_private_write(path, &serde_json::to_vec_pretty(&document)?)
}

pub fn load_xiaomi_mimo(path: &Path) -> Result<Option<XiaomiMiMoCredential>, CredentialError> {
    let document = read_document(path)?;
    let Some(api_key) = document
        .pointer("/providers/xiaomi_mimo/api_key")
        .and_then(Value::as_str)
        .filter(|key| !key.trim().is_empty())
    else {
        return Ok(None);
    };
    let base_url = document
        .pointer("/providers/xiaomi_mimo/base_url")
        .and_then(Value::as_str)
        .unwrap_or(XIAOMI_MIMO_BASE_URL);
    Ok(Some(XiaomiMiMoCredential {
        api_key: SecretString::from(api_key.to_owned()),
        base_url: normalize_mimo_base_url(base_url)?,
    }))
}

pub fn remove_xiaomi_mimo(path: &Path) -> Result<bool, CredentialError> {
    let mut document = read_document(path)?;
    let removed = document
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .and_then(|providers| providers.remove("xiaomi_mimo"))
        .is_some();
    if removed {
        atomic_private_write(path, &serde_json::to_vec_pretty(&document)?)?;
    }
    Ok(removed)
}

fn normalize_mimo_base_url(base_url: &str) -> Result<String, CredentialError> {
    let parsed = Url::parse(base_url.trim()).map_err(|_| CredentialError::BaseUrl)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(CredentialError::BaseUrl);
    }
    Ok(base_url.trim().trim_end_matches('/').to_owned())
}

fn read_document(path: &Path) -> Result<Value, CredentialError> {
    if !path.is_file() {
        return Ok(json!({"schema_version":1,"providers":{}}));
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), CredentialError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn credentials_round_trip_without_debug_exposure() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("providers.json");
        save_deepseek_key(&path, "ds-secret").expect("save key");
        let key = load_deepseek_key(&path).expect("load key").expect("stored key");
        assert_eq!(key.expose_secret(), "ds-secret");
        assert!(!format!("{key:?}").contains("ds-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(remove_deepseek(&path).expect("remove"));
        assert!(load_deepseek_key(&path).expect("load removed").is_none());
    }

    #[test]
    fn xiaomi_mimo_round_trip_keeps_key_secret_and_url_explicit() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("providers.json");
        save_xiaomi_mimo(
            &path,
            "tp-secret",
            Some("https://token-plan-sgp.xiaomimimo.com/v1/"),
        )
        .expect("save MiMo");
        let credential = load_xiaomi_mimo(&path)
            .expect("load MiMo")
            .expect("stored MiMo credential");
        assert_eq!(credential.api_key.expose_secret(), "tp-secret");
        assert_eq!(credential.base_url, "https://token-plan-sgp.xiaomimimo.com/v1");
        assert!(!format!("{credential:?}").contains("tp-secret"));
        assert!(remove_xiaomi_mimo(&path).expect("remove MiMo"));
        assert!(load_xiaomi_mimo(&path).expect("load removed").is_none());
    }

    #[test]
    fn xiaomi_mimo_rejects_non_https_or_credential_urls() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("providers.json");
        assert!(matches!(
            save_xiaomi_mimo(&path, "sk-secret", Some("http://example.test/v1")),
            Err(CredentialError::BaseUrl)
        ));
        assert!(matches!(
            save_xiaomi_mimo(&path, "sk-secret", Some("https://name:pass@example.test/v1")),
            Err(CredentialError::BaseUrl)
        ));
    }
}
