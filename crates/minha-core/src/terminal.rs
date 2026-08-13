//! Fixed, bounded PTY support for interactive agent terminal work.
//!
//! This module intentionally exposes no shell-string execution surface. A
//! terminal is started with an argv vector, every submitted command line is
//! checked again before it reaches the PTY, and observation returns a parsed
//! VT100 screen rather than raw escape bytes. Metadata is versioned and
//! durable under `.minha/`; the bounded raw-output ring is process-local.

use chrono::Utc;
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TrySendError},
    },
    thread,
    time::Duration,
};
use thiserror::Error;

pub const TERMINAL_SCHEMA_VERSION: u16 = 1;
pub const TERMINAL_RAW_RING_CAP: usize = 64 * 1024;
pub const TERMINAL_BATCH_STEP_CAP: usize = 8;
pub const TERMINAL_SCREEN_CAP: usize = 64 * 1024;
pub const TERMINAL_BATCH_WAIT_CAP_MS: u64 = 60_000;
const METADATA_FILE: &str = "terminal-sessions-v1.json";

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("invalid terminal arguments: {0}")]
    Invalid(String),
    #[error("terminal command denied by safety policy: {0}")]
    Denied(String),
    #[error("terminal session {0} was not found")]
    Missing(String),
    #[error("terminal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("terminal PTY failed: {0}")]
    Pty(String),
    #[error("terminal metadata failed: {0}")]
    Metadata(String),
}

/// The executor maps this to either a normal tool result or a durable human
/// pause. Secret and privilege prompts are intentionally never auto-answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalResult {
    Output(Value),
    NeedsHuman(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalSessionMetadataV1 {
    pub schema_version: u16,
    pub id: String,
    pub workspace: String,
    /// Redacted argv only. Raw interactive output and secret values are never
    /// persisted in this metadata record.
    pub argv: Vec<String>,
    pub cwd: String,
    pub pid: Option<u32>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct TerminalMetadataFileV1 {
    schema_version: u16,
    sessions: Vec<TerminalSessionMetadataV1>,
}

struct TerminalSession {
    root: PathBuf,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    receiver: Receiver<Vec<u8>>,
    receiver_dropped: Arc<AtomicBool>,
    parser: vt100::Parser,
    raw_ring: VecDeque<u8>,
    metadata: TerminalSessionMetadataV1,
}

impl TerminalSession {
    fn pump(&mut self) {
        while let Ok(chunk) = self.receiver.try_recv() {
            self.parser.process(&chunk);
            append_ring(&mut self.raw_ring, &chunk, TERMINAL_RAW_RING_CAP);
        }
        if let Ok(Some(exit)) = self.child.try_wait() {
            self.metadata.status = "exited".into();
            self.metadata.exit_code = Some(exit.exit_code());
            self.metadata.updated_at = now_string();
        }
    }

    fn observation(&mut self, max_bytes: usize) -> Value {
        self.pump();
        let screen = redact_terminal_text(&self.parser.screen().contents());
        let (screen, truncated) = cap_utf8(screen, max_bytes.min(TERMINAL_SCREEN_CAP));
        let (rows, cols) = self.parser.screen().size();
        json!({
            "schema_version": TERMINAL_SCHEMA_VERSION,
            "session": self.metadata.id,
            "status": self.metadata.status,
            "exit_code": self.metadata.exit_code,
            "rows": rows,
            "cols": cols,
            "screen": screen,
            "truncated": truncated || self.receiver_dropped.load(Ordering::Relaxed),
            "raw_ring_bytes": self.raw_ring.len(),
            "prompt_requires_human": sensitive_prompt(&self.parser.screen().contents()),
        })
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<(), TerminalError> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.master
            .resize(size)
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        self.parser.screen_mut().set_size(rows, cols);
        self.metadata.updated_at = now_string();
        Ok(())
    }

    fn close(&mut self) -> Result<(), TerminalError> {
        if self.metadata.status == "running" {
            self.child.kill()?;
            self.metadata.status = "closed".into();
            self.metadata.updated_at = now_string();
        }
        self.pump();
        Ok(())
    }
}

#[derive(Default)]
struct TerminalRegistry {
    sessions: BTreeMap<String, TerminalSession>,
}

fn registry() -> &'static Mutex<TerminalRegistry> {
    static REGISTRY: OnceLock<Mutex<TerminalRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(TerminalRegistry::default()))
}

/// Execute the one static `terminal` tool. `line_validator` is intentionally
/// supplied by the executor so terminal input uses the same destructive and
/// read-only policy as ordinary `exec` calls.
pub fn execute(
    root: &Path,
    read_only: bool,
    allow_destructive: bool,
    args: &Value,
    line_validator: impl Fn(&str, bool, bool) -> Result<(), TerminalError>,
) -> Result<TerminalResult, TerminalError> {
    let action = required_str(args, "action")?;
    match action {
        "start" => start(root, read_only, allow_destructive, args, line_validator),
        "observe" => observe(root, args),
        "batch" => batch(root, read_only, allow_destructive, args, line_validator),
        "resize" => resize(root, args),
        "close" => close(root, args),
        other => Err(TerminalError::Invalid(format!(
            "action must be start, observe, batch, resize, or close; got {other}"
        ))),
    }
}

fn start(
    root: &Path,
    read_only: bool,
    allow_destructive: bool,
    args: &Value,
    line_validator: impl Fn(&str, bool, bool) -> Result<(), TerminalError>,
) -> Result<TerminalResult, TerminalError> {
    let mut argv = string_array(args.get("argv"), "argv")?;
    if argv.is_empty() {
        return Err(TerminalError::Invalid("argv must not be empty".into()));
    }
    if argv.len() > 64 || argv.iter().map(String::len).sum::<usize>() > 4 * 1024 {
        return Err(TerminalError::Invalid(
            "terminal argv must contain at most 64 words and 4096 bytes".into(),
        ));
    }
    let cwd = resolve_cwd(root, args.get("cwd").and_then(Value::as_str))?;
    // A PTY is the explicit full-interaction surface. A plain shell is useful
    // here only because every later line is safety-gated; avoid profile files
    // so starting it cannot execute arbitrary local startup code first.
    if is_shell_program(&argv[0]) {
        if read_only {
            return Err(TerminalError::Denied(
                "read-only roles cannot start an interactive shell".into(),
            ));
        }
        argv = safe_shell_argv(&argv)?;
    } else {
        line_validator(&argv.join(" "), read_only, allow_destructive)?;
    }
    if argv.iter().any(|argument| looks_like_secret(argument)) {
        return Err(TerminalError::Denied(
            "terminal argv appears to include a credential; use a human-controlled prompt instead".into(),
        ));
    }
    let rows = bounded_dimension(args.get("rows"), 24, "rows")?;
    let cols = bounded_dimension(args.get("cols"), 80, "cols")?;
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| TerminalError::Pty(error.to_string()))?;
    let mut command = CommandBuilder::new(&argv[0]);
    command.args(argv.iter().skip(1));
    command.cwd(&cwd);
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| TerminalError::Pty(error.to_string()))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| TerminalError::Pty(error.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| TerminalError::Pty(error.to_string()))?;
    let receiver_dropped = Arc::new(AtomicBool::new(false));
    let receiver = start_reader(reader, Arc::clone(&receiver_dropped));
    let id = uuid::Uuid::now_v7().to_string();
    let now = now_string();
    let metadata = TerminalSessionMetadataV1 {
        schema_version: TERMINAL_SCHEMA_VERSION,
        id: id.clone(),
        workspace: root.display().to_string(),
        argv: argv.iter().map(|value| redact_argv_value(value)).collect(),
        cwd: cwd.display().to_string(),
        pid: child.process_id(),
        status: "running".into(),
        created_at: now.clone(),
        updated_at: now,
        exit_code: None,
    };
    let mut registry = registry().lock();
    registry.sessions.insert(
        id.clone(),
        TerminalSession {
            root: root.to_path_buf(),
            master: pair.master,
            child,
            writer,
            receiver,
            receiver_dropped,
            parser: vt100::Parser::new(rows, cols, 256),
            raw_ring: VecDeque::with_capacity(TERMINAL_RAW_RING_CAP),
            metadata,
        },
    );
    persist_metadata(root, &registry)?;
    let session = registry
        .sessions
        .get_mut(&id)
        .ok_or_else(|| TerminalError::Missing(id.clone()))?;
    Ok(TerminalResult::Output(session.observation(TERMINAL_SCREEN_CAP)))
}

fn observe(root: &Path, args: &Value) -> Result<TerminalResult, TerminalError> {
    let id = required_str(args, "session")?;
    let cap = args
        .get("max_output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(TERMINAL_SCREEN_CAP as u64)
        .clamp(1, TERMINAL_SCREEN_CAP as u64) as usize;
    let mut registry = registry().lock();
    let session = session_for_root(&mut registry, root, id)?;
    let output = session.observation(cap);
    persist_metadata(root, &registry)?;
    Ok(TerminalResult::Output(output))
}

fn batch(
    root: &Path,
    read_only: bool,
    allow_destructive: bool,
    args: &Value,
    line_validator: impl Fn(&str, bool, bool) -> Result<(), TerminalError>,
) -> Result<TerminalResult, TerminalError> {
    let id = required_str(args, "session")?;
    let steps = args
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| TerminalError::Invalid("steps must be an array".into()))?;
    if steps.is_empty() || steps.len() > TERMINAL_BATCH_STEP_CAP {
        return Err(TerminalError::Invalid(format!(
            "steps must contain 1..={TERMINAL_BATCH_STEP_CAP} entries"
        )));
    }
    let mut registry = registry().lock();
    let session = session_for_root(&mut registry, root, id)?;
    session.pump();
    if sensitive_prompt(&session.parser.screen().contents()) {
        return Ok(TerminalResult::NeedsHuman(
            "terminal is asking for a credential or privilege response; a human must take over".into(),
        ));
    }
    let mut total_wait_ms = 0_u64;
    for step in steps {
        let line = step.get("line").and_then(Value::as_str).unwrap_or("");
        let wait_ms = step
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(30_000);
        total_wait_ms = total_wait_ms.saturating_add(wait_ms);
        if total_wait_ms > TERMINAL_BATCH_WAIT_CAP_MS {
            return Err(TerminalError::Invalid(format!(
                "terminal batch waits may total at most {TERMINAL_BATCH_WAIT_CAP_MS}ms"
            )));
        }
        if !line.is_empty() {
            if line.contains('\n') || line.contains('\r') {
                return Err(TerminalError::Invalid(
                    "each terminal batch line must contain exactly one command line".into(),
                ));
            }
            line_validator(line, read_only, allow_destructive)?;
            session.writer.write_all(line.as_bytes())?;
            session.writer.write_all(b"\n")?;
        }
        session.writer.flush()?;
        if wait_ms > 0 {
            thread::sleep(Duration::from_millis(wait_ms));
        }
        session.pump();
        if sensitive_prompt(&session.parser.screen().contents()) {
            return Ok(TerminalResult::NeedsHuman(
                "terminal is asking for a credential or privilege response; a human must take over".into(),
            ));
        }
        if let Some(expected) = step.get("expect").and_then(Value::as_str)
            && !session.parser.screen().contents().contains(expected)
        {
            return Err(TerminalError::Invalid(format!(
                "terminal batch expectation was not observed: {expected:?}"
            )));
        }
    }
    let output = session.observation(TERMINAL_SCREEN_CAP);
    persist_metadata(root, &registry)?;
    Ok(TerminalResult::Output(output))
}

fn resize(root: &Path, args: &Value) -> Result<TerminalResult, TerminalError> {
    let id = required_str(args, "session")?;
    let rows = bounded_dimension(args.get("rows"), 24, "rows")?;
    let cols = bounded_dimension(args.get("cols"), 80, "cols")?;
    let mut registry = registry().lock();
    let session = session_for_root(&mut registry, root, id)?;
    session.resize(rows, cols)?;
    let output = session.observation(TERMINAL_SCREEN_CAP);
    persist_metadata(root, &registry)?;
    Ok(TerminalResult::Output(output))
}

fn close(root: &Path, args: &Value) -> Result<TerminalResult, TerminalError> {
    let id = required_str(args, "session")?;
    let mut registry = registry().lock();
    let session = session_for_root(&mut registry, root, id)?;
    session.close()?;
    let output = session.observation(TERMINAL_SCREEN_CAP);
    persist_metadata(root, &registry)?;
    Ok(TerminalResult::Output(output))
}

fn session_for_root<'a>(
    registry: &'a mut TerminalRegistry,
    root: &Path,
    id: &str,
) -> Result<&'a mut TerminalSession, TerminalError> {
    let session = registry
        .sessions
        .get_mut(id)
        .ok_or_else(|| TerminalError::Missing(id.to_owned()))?;
    if session.root != root {
        return Err(TerminalError::Denied(
            "terminal session belongs to a different workspace".into(),
        ));
    }
    Ok(session)
}

fn start_reader(mut reader: Box<dyn Read + Send>, dropped: Arc<AtomicBool>) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::sync_channel(16);
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            match sender.try_send(buffer[..read].to_vec()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => dropped.store(true, Ordering::Relaxed),
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    });
    receiver
}

fn append_ring(ring: &mut VecDeque<u8>, bytes: &[u8], cap: usize) {
    if bytes.len() >= cap {
        ring.clear();
        ring.extend(bytes[bytes.len() - cap..].iter().copied());
        return;
    }
    while ring.len().saturating_add(bytes.len()) > cap {
        let _ = ring.pop_front();
    }
    ring.extend(bytes.iter().copied());
}

fn resolve_cwd(root: &Path, requested: Option<&str>) -> Result<PathBuf, TerminalError> {
    let candidate = requested.map_or_else(|| root.to_path_buf(), |value| root.join(value));
    let canonical = fs::canonicalize(&candidate)?;
    if !canonical.starts_with(root) || !canonical.is_dir() {
        return Err(TerminalError::Denied(
            "terminal cwd is outside the workspace".into(),
        ));
    }
    Ok(canonical)
}

fn metadata_path(root: &Path) -> PathBuf {
    root.join(".minha").join(METADATA_FILE)
}

fn persist_metadata(root: &Path, registry: &TerminalRegistry) -> Result<(), TerminalError> {
    let sessions = registry
        .sessions
        .values()
        .filter(|session| session.root == root)
        .map(|session| session.metadata.clone())
        .collect::<Vec<_>>();
    let document = TerminalMetadataFileV1 {
        schema_version: TERMINAL_SCHEMA_VERSION,
        sessions,
    };
    let path = metadata_path(root);
    let parent = path
        .parent()
        .ok_or_else(|| TerminalError::Metadata("metadata path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let contents =
        serde_json::to_vec_pretty(&document).map_err(|error| TerminalError::Metadata(error.to_string()))?;
    fs::write(&temporary, contents)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, TerminalError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| TerminalError::Invalid(format!("{key} is required")))
}

fn string_array(value: Option<&Value>, name: &str) -> Result<Vec<String>, TerminalError> {
    value
        .and_then(Value::as_array)
        .ok_or_else(|| TerminalError::Invalid(format!("{name} must be an array")))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .filter(|item| !item.is_empty())
                .ok_or_else(|| TerminalError::Invalid(format!("{name} must contain non-empty strings")))
        })
        .collect()
}

fn bounded_dimension(value: Option<&Value>, default: u16, name: &str) -> Result<u16, TerminalError> {
    let raw = value.and_then(Value::as_u64).unwrap_or(u64::from(default));
    if !(4..=400).contains(&raw) {
        return Err(TerminalError::Invalid(format!(
            "{name} must be between 4 and 400"
        )));
    }
    Ok(raw as u16)
}

fn is_shell_program(program: &str) -> bool {
    matches!(
        Path::new(program).file_name().and_then(|name| name.to_str()),
        Some("sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "pwsh" | "powershell" | "cmd")
    )
}

fn safe_shell_argv(argv: &[String]) -> Result<Vec<String>, TerminalError> {
    let program = Path::new(&argv[0])
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| TerminalError::Invalid("terminal shell program is invalid".into()))?;
    match program {
        "bash" => Ok(vec![argv[0].clone(), "--noprofile".into(), "--norc".into()]),
        "zsh" => Ok(vec![argv[0].clone(), "-f".into()]),
        "sh" | "dash" | "ksh" => Ok(vec![argv[0].clone()]),
        _ => Err(TerminalError::Denied(
            "interactive terminal supports sh, bash, zsh, dash, or ksh only; use argv for a non-shell program"
                .into(),
        )),
    }
}

fn sensitive_prompt(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "password",
        "passphrase",
        "api key",
        "access token",
        "credential",
        "one-time code",
        "verification code",
        "sudo",
        "administrator password",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn looks_like_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("api_key=")
        || lower.contains("token=")
        || lower.starts_with("sk-")
        || lower.starts_with("tp-")
        || lower.contains("password=")
}

fn redact_argv_value(value: &str) -> String {
    if looks_like_secret(value) {
        "[redacted]".to_owned()
    } else {
        value.to_owned()
    }
}

fn redact_terminal_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if lower.contains("password")
                || lower.contains("passphrase")
                || lower.contains("api key")
                || lower.contains("access token")
            {
                "[sensitive terminal prompt redacted]".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cap_utf8(text: String, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}\n[terminal screen truncated]", &text[..end]), true)
}

fn now_string() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_only_the_latest_bounded_bytes() {
        let mut ring = VecDeque::new();
        append_ring(&mut ring, b"abcdef", 4);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), b"cdef");
        append_ring(&mut ring, b"gh", 4);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), b"efgh");
    }

    #[test]
    fn shell_start_is_profile_free_and_other_shells_are_rejected() {
        assert_eq!(
            safe_shell_argv(&["bash".into()]).expect("bash"),
            vec!["bash", "--noprofile", "--norc"]
        );
        assert!(safe_shell_argv(&["powershell".into()]).is_err());
    }

    #[test]
    fn sensitive_prompts_are_redacted_and_pause_writes() {
        assert!(sensitive_prompt("Password: "));
        assert_eq!(
            redact_terminal_text("Password: super-secret"),
            "[sensitive terminal prompt redacted]"
        );
    }

    #[test]
    fn metadata_does_not_keep_secret_argv_values() {
        assert_eq!(redact_argv_value("sk-secret"), "[redacted]");
        assert_eq!(redact_argv_value("cargo"), "cargo");
    }
}
