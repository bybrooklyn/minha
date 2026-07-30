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

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential file is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("provider API key must not be empty")]
    Empty,
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
}
