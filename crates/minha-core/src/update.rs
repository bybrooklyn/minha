//! Self-update support through the authenticated GitHub CLI.

use crate::cache::redact_secrets;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const DEFAULT_REPOSITORY: &str = "bybrooklyn/minha";
const MAX_DIAGNOSTIC_BYTES: usize = 512;
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RELEASE_JSON_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct UpdateOptions {
    pub repository: Option<String>,
    pub check: bool,
}

#[derive(Debug, Serialize)]
pub struct UpdateResult {
    pub repository: String,
    pub current_version: String,
    pub latest_version: String,
    pub target: String,
    pub binary_asset: String,
    pub checksum_asset: String,
    pub updated: bool,
    pub restart_required: bool,
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(rename = "tagName")]
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
}

struct InstallOutcome {
    updated: bool,
    restart_required: bool,
    message: String,
}

#[derive(Debug)]
struct Version<'a> {
    core: Vec<u64>,
    pre: Vec<Identifier<'a>>,
}

#[derive(Debug)]
enum Identifier<'a> {
    Numeric(u64),
    Text(&'a str),
}

pub fn check_or_update(options: &UpdateOptions) -> Result<UpdateResult, String> {
    let repository = match options
        .repository
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        Some(repository) => repository.to_owned(),
        None => DEFAULT_REPOSITORY.to_owned(),
    };
    validate_repository(&repository)?;
    let current_version = env!("CARGO_PKG_VERSION").to_owned();
    let target = target_triple();
    let release: Release = run_gh_json(&[
        "release",
        "view",
        "--repo",
        &repository,
        "--json",
        "tagName,name,assets",
    ])?;
    let latest_version = release.tag_name.trim_start_matches('v').to_owned();
    if compare_versions(&current_version, &latest_version)? != Ordering::Less {
        return Ok(UpdateResult {
            repository,
            current_version,
            latest_version,
            target,
            binary_asset: String::new(),
            checksum_asset: String::new(),
            updated: false,
            restart_required: false,
            message: Some("already up to date".into()),
        });
    }
    let (binary, checksum) = select_assets(&release.assets, &target)?;
    if options.check {
        return Ok(UpdateResult {
            repository,
            current_version,
            latest_version,
            target,
            binary_asset: binary.name.clone(),
            checksum_asset: checksum.name.clone(),
            updated: false,
            restart_required: false,
            message: Some("update available; no files changed".into()),
        });
    }

    let executable = std::env::current_exe().map_err(|error| {
        format!(
            "could not locate current executable: {}",
            short_error(&error.to_string())
        )
    })?;
    let download_dir = unique_temp_dir("minha-update")?;
    let result = install_release(
        &repository,
        &release.tag_name,
        binary,
        checksum,
        &download_dir,
        &executable,
    );
    let _ = fs::remove_dir_all(&download_dir);
    result.map(|outcome| UpdateResult {
        repository,
        current_version,
        latest_version,
        target,
        binary_asset: binary.name.clone(),
        checksum_asset: checksum.name.clone(),
        updated: outcome.updated,
        restart_required: outcome.restart_required,
        message: Some(outcome.message),
    })
}

fn install_release(
    repository: &str,
    tag: &str,
    binary: &Asset,
    checksum: &Asset,
    download_dir: &Path,
    executable: &Path,
) -> Result<InstallOutcome, String> {
    run_gh_download(repository, tag, &binary.name, download_dir)?;
    run_gh_download(repository, tag, &checksum.name, download_dir)?;
    let binary_path = download_dir.join(&binary.name);
    let checksum_path = download_dir.join(&checksum.name);
    let metadata = fs::metadata(&binary_path).map_err(io_message)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_ASSET_BYTES {
        return Err("downloaded release asset has an invalid size".into());
    }
    let checksum_metadata = fs::metadata(&checksum_path).map_err(io_message)?;
    if !checksum_metadata.is_file() || checksum_metadata.len() == 0 || checksum_metadata.len() > 4096 {
        return Err("downloaded checksum asset has an invalid size".into());
    }
    let expected = parse_checksum(&fs::read_to_string(&checksum_path).map_err(io_message)?)?;
    let actual = sha256_file(&binary_path)?;
    if expected != actual {
        return Err("release checksum does not match downloaded binary".into());
    }
    let staged = sibling_temp_path(executable);
    fs::copy(&binary_path, &staged).map_err(io_message)?;
    #[cfg(unix)]
    copy_mode(executable, &staged)?;
    #[cfg(unix)]
    {
        fs::rename(&staged, executable).map_err(io_message)?;
        Ok(InstallOutcome {
            updated: true,
            restart_required: true,
            message: "updated atomically; restart Minha to use the new binary".into(),
        })
    }
    #[cfg(windows)]
    {
        return Ok(InstallOutcome {
            updated: false,
            restart_required: false,
            message: format!(
                "download verified and staged at {}; replace the running executable after Minha exits",
                staged.display()
            ),
        });
    }
}

fn run_gh_json<T: for<'de> Deserialize<'de>>(args: &[&str]) -> Result<T, String> {
    let output = run_gh(args, Duration::from_secs(30), MAX_RELEASE_JSON_BYTES)?;
    if !output.status.success() {
        return Err(format!(
            "gh release query failed: {}",
            bounded_redacted(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "gh returned invalid release JSON: {}",
            short_error(&error.to_string())
        )
    })
}

fn run_gh_download(repository: &str, tag: &str, asset: &str, directory: &Path) -> Result<(), String> {
    let directory_string = directory.to_string_lossy().into_owned();
    let output = run_gh(
        &[
            "release",
            "download",
            tag,
            "--repo",
            repository,
            "--pattern",
            asset,
            "--dir",
            &directory_string,
            "--clobber",
        ],
        Duration::from_secs(120),
        64 * 1024,
    )?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh release download failed: {}",
            bounded_redacted(&output.stderr)
        ))
    }
}

struct GhOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_gh(args: &[&str], timeout: Duration, cap: usize) -> Result<GhOutput, String> {
    let mut child = Command::new("gh")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run gh: {}", short_error(&error.to_string())))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "gh did not provide stdout".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "gh did not provide stderr".to_owned())?;
    let stdout_reader = thread::spawn(move || read_capped(stdout, cap));
    let stderr_reader = thread::spawn(move || read_capped(stderr, cap));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        if child.try_wait().map_err(io_message)?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().map_err(io_message)?;
            timed_out = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait().map_err(io_message)?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| "gh stdout reader panicked".to_owned())?
        .map_err(io_message)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "gh stderr reader panicked".to_owned())?
        .map_err(io_message)?;
    if timed_out {
        return Err(format!("gh timed out after {} seconds", timeout.as_secs()));
    }
    Ok(GhOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_capped(mut reader: impl Read, cap: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(cap.min(64 * 1024));
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = cap.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(output)
}

fn select_assets<'a>(assets: &'a [Asset], target: &str) -> Result<(&'a Asset, &'a Asset), String> {
    let target = target.to_ascii_lowercase();
    let expected_binary = if target.contains("windows") {
        format!("minha-{target}.exe")
    } else {
        format!("minha-{target}")
    };
    let binary = assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(&expected_binary))
        .ok_or_else(|| format!("release has no Minha binary for target {target}"))?;
    let checksum = assets
        .iter()
        .find(|asset| {
            asset
                .name
                .eq_ignore_ascii_case(&format!("{}.sha256", binary.name))
        })
        .ok_or_else(|| format!("release has no SHA-256 asset for {}", binary.name))?;
    Ok((binary, checksum))
}

fn target_triple() -> String {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "macos") => "x86_64-apple-darwin".into(),
        ("aarch64", "macos") => "aarch64-apple-darwin".into(),
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu".into(),
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu".into(),
        ("x86_64", "windows") => "x86_64-pc-windows-msvc".into(),
        ("aarch64", "windows") => "aarch64-pc-windows-msvc".into(),
        (arch, os) => format!("{arch}-{os}"),
    }
}

fn parse_checksum(text: &str) -> Result<String, String> {
    text.split_whitespace()
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|value| value.to_ascii_lowercase())
        .ok_or_else(|| "checksum asset is not a SHA-256 digest".into())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(io_message)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(io_message)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn compare_versions(left: &str, right: &str) -> Result<Ordering, String> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    Ok(left
        .core
        .cmp(&right.core)
        .then_with(|| match (left.pre.is_empty(), right.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => compare_pre(&left.pre, &right.pre),
        }))
}

fn parse_version(value: &str) -> Result<Version<'_>, String> {
    let value = value.trim().trim_start_matches('v');
    let (without_build, _) = match value.split_once('+') {
        Some(parts) => parts,
        None => (value, ""),
    };
    let (core, pre) = match without_build.split_once('-') {
        Some(parts) => parts,
        None => (without_build, ""),
    };
    let core = core
        .split('.')
        .map(|part| {
            if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
                Err(format!("invalid release version: {value}"))
            } else {
                part.parse::<u64>()
                    .map_err(|_| format!("invalid release version: {value}"))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if core.len() != 3 {
        return Err(format!("invalid release version: {value}"));
    }
    let pre = if pre.is_empty() {
        Vec::new()
    } else {
        pre.split('.')
            .map(|part| {
                if part.is_empty() {
                    Err(format!("invalid release version: {value}"))
                } else if part.bytes().all(|byte| byte.is_ascii_digit()) {
                    if part.len() > 1 && part.starts_with('0') {
                        Err(format!("invalid release version: {value}"))
                    } else {
                        part.parse::<u64>()
                            .map(Identifier::Numeric)
                            .map_err(|_| format!("invalid release version: {value}"))
                    }
                } else if !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                {
                    Ok(Identifier::Text(part))
                } else {
                    Err(format!("invalid release version: {value}"))
                }
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(Version { core, pre })
}

fn compare_pre(left: &[Identifier<'_>], right: &[Identifier<'_>]) -> Ordering {
    for (left, right) in left.iter().zip(right) {
        let ordering = match (left, right) {
            (Identifier::Numeric(a), Identifier::Numeric(b)) => a.cmp(b),
            (Identifier::Numeric(_), Identifier::Text(_)) => Ordering::Less,
            (Identifier::Text(_), Identifier::Numeric(_)) => Ordering::Greater,
            (Identifier::Text(a), Identifier::Text(b)) => a.cmp(b),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

fn validate_repository(repository: &str) -> Result<(), String> {
    let mut parts = repository.split('/');
    let valid_component = |value: &str| {
        !value.is_empty()
            && value.len() <= 100
            && !value.starts_with('-')
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    if !matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(name), None)
        if valid_component(owner) && valid_component(name))
    {
        Err("repository must be an owner/name value".into())
    } else {
        Ok(())
    }
}

fn unique_temp_dir(prefix: &str) -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&path).map_err(io_message)?;
    Ok(path)
}

fn sibling_temp_path(executable: &Path) -> PathBuf {
    let name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .map_or(String::from("minha"), str::to_owned);
    executable.with_file_name(format!(".{name}.update-{}", std::process::id()))
}

#[cfg(unix)]
fn copy_mode(source: &Path, destination: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(source).map_err(io_message)?.permissions().mode();
    fs::set_permissions(destination, fs::Permissions::from_mode(mode)).map_err(io_message)
}

fn io_message(error: io::Error) -> String {
    short_error(&error.to_string())
}
fn short_error(error: &str) -> String {
    bounded_redacted(error.as_bytes())
}
fn bounded_redacted(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes)
        .replace("ghp_", "[redacted] ")
        .replace("github_pat_", "[redacted] ");
    let text = redact_secrets(&text);
    if text.len() <= MAX_DIAGNOSTIC_BYTES {
        return text.trim().to_owned();
    }
    let mut end = MAX_DIAGNOSTIC_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", text[..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_follow_semver_prerelease_rules() {
        assert_eq!(
            compare_versions("v1.2.3-alpha.2", "1.2.3-alpha.10").ok(),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_versions("1.2.3", "1.2.3-rc.1").ok(),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_versions("1.2.3+build.1", "v1.2.3+build.2").ok(),
            Some(Ordering::Equal)
        );
        assert!(compare_versions("1.2.3-alpha.01", "1.2.3").is_err());
    }

    #[test]
    fn checksum_parser_accepts_common_sha_format() {
        let digest = "A".repeat(64);
        assert_eq!(
            parse_checksum(&format!("{digest}  minha-linux\n")).ok(),
            Some(digest.to_ascii_lowercase())
        );
    }

    #[test]
    fn asset_selection_requires_target_and_checksum() {
        let assets = vec![
            Asset {
                name: "minha-x86_64-unknown-linux-gnu".into(),
            },
            Asset {
                name: "minha-x86_64-unknown-linux-gnu.sha256".into(),
            },
        ];
        assert!(select_assets(&assets, "x86_64-unknown-linux-gnu").is_ok());
        assert!(select_assets(&assets, "x86_64-pc-windows-msvc").is_err());
    }

    #[test]
    fn repository_and_diagnostics_are_strict_and_unicode_safe() {
        assert!(validate_repository("bybrooklyn/minha").is_ok());
        assert!(validate_repository("--help/minha").is_err());
        assert!(validate_repository("owner/repo/extra").is_err());
        let detail = format!("{}é", "x".repeat(MAX_DIAGNOSTIC_BYTES - 1));
        let bounded = bounded_redacted(detail.as_bytes());
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.ends_with('…'));
    }
}
