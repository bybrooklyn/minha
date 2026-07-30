//! Durable local state, using an explicitly versioned SQLite schema.

use crate::cache::{CacheClass, ObservedInputManifest};
use crate::facts::{BoardEntry, BoardKind, BoardScope, BoardStatus};
use crate::memory::{MemoryHit, MemoryRecord, MemoryScope, MemorySettings};
use crate::protocol::{
    AgentState, EventAgentId, EventEnvelope, ExitState, IncidentSeverity, IncidentView,
    IssueClarificationView, MIN_TYPED_PROTOCOL_VERSION, Mode, PlanTaskState, RunId, RuntimeEvent, TodoItem,
    TodoState,
};
use crate::provider::ModelDescriptor;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fmt::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use thiserror::Error;
use tokio::sync::broadcast;

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("event payload is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    FutureSchema { found: i64, supported: i64 },
    #[error("invalid stored run id: {0}")]
    RunId(#[from] uuid::Error),
    #[error("invalid stored timestamp: {0}")]
    Timestamp(#[from] chrono::ParseError),
    #[error("invalid stored mode: {0}")]
    Mode(String),
    #[error("invalid stored state: {0}")]
    State(String),
    #[error("could not create database directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("coordination lease is already held for resource: {0}")]
    LeaseConflict(String),
    #[error("invalid stored coordination value: {0}")]
    Coordination(String),
    #[error("invalid memory: {0}")]
    Memory(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: RunId,
    pub title: String,
    pub goal: String,
    pub mode: Mode,
    pub state: ExitState,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub summary: Option<String>,
    pub pending_question: Option<String>,
    pub archived: bool,
    pub parent_run_id: Option<RunId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub run_id: RunId,
    pub sequence: u64,
    pub role: String,
    pub content: serde_json::Value,
    pub compacted: bool,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: String,
    pub root: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub run_id: RunId,
    pub agent_id: EventAgentId,
    pub parent_agent_id: Option<EventAgentId>,
    pub role: String,
    pub model: String,
    pub state: AgentState,
    pub task_id: Option<String>,
    pub attempt: u32,
    pub generation: u64,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub run_id: RunId,
    pub task_id: String,
    pub objective: String,
    pub state: PlanTaskState,
    pub paths: Vec<String>,
    pub dependencies: Vec<String>,
    pub assigned_agent_id: Option<EventAgentId>,
    pub attempt: u32,
    pub max_attempts: u32,
    pub generation: u64,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub session_input: u64,
    pub session_output: u64,
    pub session_cached_input: u64,
    pub session_cache_write: u64,
    pub session_reasoning_output: u64,
    pub lifetime_input: u64,
    pub lifetime_output: u64,
    pub lifetime_cached_input: u64,
    pub lifetime_cache_write: u64,
    pub lifetime_reasoning_output: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderCostTotals {
    pub estimated_usd: f64,
    pub cache_savings_usd: f64,
    pub priced_turns: u64,
    pub unpriced_turns: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TodoRollupDetails {
    pub active_goals: Vec<String>,
    pub blocked_work: Vec<String>,
    pub recently_completed: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CacheTotals {
    pub entries: u64,
    pub bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    pub bypasses: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub saved_input_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CachedResult {
    pub key: String,
    pub class: String,
    pub value: Vec<u8>,
    pub manifest: serde_json::Value,
    pub stored_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub hits: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CachedModelCatalog {
    pub models: Vec<ModelDescriptor>,
    pub etag: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    events_tx: broadcast::Sender<EventEnvelope>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        archive_prototype_database(path)?;
        let connection = Connection::open(path)?;
        let (events_tx, _) = broadcast::channel(4_096);
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
            events_tx,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        let (events_tx, _) = broadcast::channel(4_096);
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
            events_tx,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn migrate(&self) -> Result<(), StoreError> {
        let connection = self.connection.lock();
        connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
        let mode: String = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == SCHEMA_VERSION {
            debug_assert!(mode == "wal" || mode == "memory");
            return Ok(());
        }
        if version > SCHEMA_VERSION {
            return Err(StoreError::FutureSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version < 3 {
            // v2 was an unqualified prototype. The v3 contract intentionally
            // starts clean so no stringly event can masquerade as replayable
            // session state.
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 DROP TABLE IF EXISTS messages;
                 DROP TABLE IF EXISTS events;
                 DROP TABLE IF EXISTS runs;
                 CREATE TABLE runs (
                   run_id TEXT PRIMARY KEY,
                   title TEXT NOT NULL,
                   goal TEXT NOT NULL,
                   mode TEXT NOT NULL,
                   state TEXT NOT NULL,
                   model TEXT,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   input_tokens INTEGER NOT NULL DEFAULT 0,
                   output_tokens INTEGER NOT NULL DEFAULT 0,
                   summary TEXT,
                   pending_question TEXT,
                   archived INTEGER NOT NULL DEFAULT 0,
                   parent_run_id TEXT
                 );
                 CREATE TABLE messages (
                   run_id TEXT NOT NULL,
                   sequence INTEGER NOT NULL,
                   role TEXT NOT NULL,
                   content TEXT NOT NULL,
                   compacted INTEGER NOT NULL DEFAULT 0,
                   occurred_at TEXT NOT NULL,
                   PRIMARY KEY (run_id, sequence),
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE TABLE events (
                   run_id TEXT NOT NULL,
                   sequence INTEGER NOT NULL,
                   protocol_version INTEGER NOT NULL,
                   turn_id TEXT,
                   kind TEXT NOT NULL,
                   payload TEXT NOT NULL,
                   occurred_at TEXT NOT NULL,
                   PRIMARY KEY (run_id, sequence),
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS events_by_time ON events(occurred_at);
                 CREATE INDEX IF NOT EXISTS runs_by_updated ON runs(updated_at DESC);
                 CREATE INDEX IF NOT EXISTS runs_active_by_updated ON runs(archived, updated_at DESC);
                 PRAGMA user_version = 3;
                 COMMIT;",
            )?;
        }
        if version < 4 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS workspaces (
                   workspace_id TEXT PRIMARY KEY,
                   root TEXT NOT NULL UNIQUE,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS run_workspaces (
                   run_id TEXT PRIMARY KEY,
                   workspace_id TEXT NOT NULL,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE,
                   FOREIGN KEY (workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS agents (
                   run_id TEXT NOT NULL,
                   agent_id TEXT NOT NULL,
                   parent_agent_id TEXT,
                   role TEXT NOT NULL,
                   model TEXT NOT NULL,
                   state TEXT NOT NULL,
                   task_id TEXT,
                   attempt INTEGER NOT NULL DEFAULT 0,
                   generation INTEGER NOT NULL DEFAULT 0,
                   started_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   finished_at TEXT,
                   PRIMARY KEY (run_id, agent_id),
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS tasks (
                   run_id TEXT NOT NULL,
                   task_id TEXT NOT NULL,
                   objective TEXT NOT NULL,
                   state TEXT NOT NULL,
                   paths_json TEXT NOT NULL,
                   assigned_agent_id TEXT,
                   attempt INTEGER NOT NULL DEFAULT 0,
                   max_attempts INTEGER NOT NULL DEFAULT 2,
                   generation INTEGER NOT NULL DEFAULT 0,
                   last_error TEXT,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   PRIMARY KEY (run_id, task_id),
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS task_dependencies (
                   run_id TEXT NOT NULL,
                   prerequisite_id TEXT NOT NULL,
                   dependent_id TEXT NOT NULL,
                   PRIMARY KEY (run_id, prerequisite_id, dependent_id),
                   FOREIGN KEY (run_id, prerequisite_id) REFERENCES tasks(run_id, task_id) ON DELETE CASCADE,
                   FOREIGN KEY (run_id, dependent_id) REFERENCES tasks(run_id, task_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS leases (
                   run_id TEXT NOT NULL,
                   resource TEXT NOT NULL,
                   task_id TEXT NOT NULL,
                   agent_id TEXT NOT NULL,
                   generation INTEGER NOT NULL,
                   expires_at TEXT NOT NULL,
                   PRIMARY KEY (run_id, resource),
                   FOREIGN KEY (run_id, task_id) REFERENCES tasks(run_id, task_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS board_entries (
                   entry_id TEXT PRIMARY KEY,
                   workspace_id TEXT NOT NULL,
                   run_id TEXT,
                   scope TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   subject TEXT NOT NULL,
                   body TEXT NOT NULL,
                   task_id TEXT,
                   author_agent_id TEXT,
                   confidence INTEGER NOT NULL,
                   status TEXT NOT NULL,
                   supersedes_id TEXT,
                   evidence_json TEXT NOT NULL,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   FOREIGN KEY (workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE,
                   FOREIGN KEY (supersedes_id) REFERENCES board_entries(entry_id)
                 );
                 CREATE TABLE IF NOT EXISTS board_revisions (
                   entry_id TEXT NOT NULL,
                   revision INTEGER NOT NULL,
                   body TEXT NOT NULL,
                   status TEXT NOT NULL,
                   author_agent_id TEXT,
                   created_at TEXT NOT NULL,
                   PRIMARY KEY (entry_id, revision),
                   FOREIGN KEY (entry_id) REFERENCES board_entries(entry_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS usage_turns (
                   usage_id INTEGER PRIMARY KEY AUTOINCREMENT,
                   run_id TEXT NOT NULL,
                   agent_id TEXT,
                   model TEXT NOT NULL,
                   input_tokens INTEGER NOT NULL,
                   output_tokens INTEGER NOT NULL,
                   context_tokens INTEGER,
                   occurred_at TEXT NOT NULL,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS model_catalogs (
                   workspace_id TEXT PRIMARY KEY,
                   etag TEXT,
                   models_json TEXT NOT NULL,
                   fetched_at TEXT NOT NULL,
                   FOREIGN KEY (workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS agents_by_state ON agents(run_id, state);
                 CREATE INDEX IF NOT EXISTS tasks_by_state ON tasks(run_id, state);
                 CREATE INDEX IF NOT EXISTS leases_by_expiry ON leases(expires_at);
                 CREATE INDEX IF NOT EXISTS board_by_run ON board_entries(run_id, status, updated_at DESC);
                 CREATE INDEX IF NOT EXISTS board_by_workspace ON board_entries(workspace_id, scope, status, updated_at DESC);
                 CREATE INDEX IF NOT EXISTS usage_by_run ON usage_turns(run_id, occurred_at);
                 PRAGMA user_version = 4;
                 COMMIT;",
            )?;
        }
        if version < 5 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 ALTER TABLE usage_turns ADD COLUMN cached_input_tokens INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE usage_turns ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE usage_turns ADD COLUMN reasoning_output_tokens INTEGER NOT NULL DEFAULT 0;
                 CREATE TABLE IF NOT EXISTS cache_entries (
                   cache_key TEXT PRIMARY KEY,
                   workspace_id TEXT NOT NULL,
                   class TEXT NOT NULL,
                   value BLOB NOT NULL,
                   manifest_json TEXT NOT NULL,
                   stored_at TEXT NOT NULL,
                   last_used_at TEXT NOT NULL,
                   expires_at TEXT,
                   hits INTEGER NOT NULL DEFAULT 0,
                   size_bytes INTEGER NOT NULL,
                   FOREIGN KEY (workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS hive_messages (
                   message_id TEXT PRIMARY KEY,
                   run_id TEXT NOT NULL,
                   room_id TEXT NOT NULL,
                   sender_id TEXT NOT NULL,
                   recipient TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   payload_json TEXT NOT NULL,
                   occurred_at TEXT NOT NULL,
                   expires_at TEXT,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS office_artifacts (
                   artifact_id TEXT PRIMARY KEY,
                   run_id TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   digest TEXT NOT NULL,
                   body BLOB NOT NULL,
                   provenance_json TEXT NOT NULL,
                   created_at TEXT NOT NULL,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS book_index (
                   book_id TEXT NOT NULL,
                   version TEXT NOT NULL,
                   source TEXT NOT NULL,
                   trust TEXT NOT NULL,
                   metadata_json TEXT NOT NULL,
                   fingerprint TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   PRIMARY KEY (book_id, version)
                 );
                 CREATE TABLE IF NOT EXISTS incidents (
                   incident_id TEXT PRIMARY KEY,
                   run_id TEXT,
                   code TEXT NOT NULL,
                   severity TEXT NOT NULL,
                   category TEXT NOT NULL,
                   retryable INTEGER NOT NULL,
                   summary TEXT NOT NULL,
                   detail TEXT,
                   correlation_id TEXT NOT NULL,
                   resolved INTEGER NOT NULL DEFAULT 0,
                   occurred_at TEXT NOT NULL,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS compaction_checkpoints (
                   checkpoint_id TEXT PRIMARY KEY,
                   run_id TEXT NOT NULL,
                   parent_id TEXT,
                   summary TEXT NOT NULL,
                   manifest_json TEXT NOT NULL,
                   estimated_tokens_before INTEGER NOT NULL,
                   created_at TEXT NOT NULL,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS cache_by_workspace_lru ON cache_entries(workspace_id, last_used_at);
                 CREATE INDEX IF NOT EXISTS hive_by_run_time ON hive_messages(run_id, occurred_at);
                 CREATE INDEX IF NOT EXISTS incidents_by_run_time ON incidents(run_id, occurred_at);
                 CREATE INDEX IF NOT EXISTS books_by_trust ON book_index(trust, updated_at);
                 PRAGMA user_version = 5;
                 COMMIT;",
            )?;
        }
        if version < 6 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS cache_stats (
                   workspace_id TEXT PRIMARY KEY,
                   hits INTEGER NOT NULL DEFAULT 0,
                   misses INTEGER NOT NULL DEFAULT 0,
                   writes INTEGER NOT NULL DEFAULT 0,
                   bypasses INTEGER NOT NULL DEFAULT 0,
                   bytes_read INTEGER NOT NULL DEFAULT 0,
                   bytes_written INTEGER NOT NULL DEFAULT 0,
                   saved_input_tokens INTEGER NOT NULL DEFAULT 0,
                   FOREIGN KEY (workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE
                 );
                 PRAGMA user_version = 6;
                 COMMIT;",
            )?;
        }
        if version < 7 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS issue_intakes (
                   run_id TEXT PRIMARY KEY,
                   schema_version INTEGER NOT NULL,
                   status TEXT NOT NULL,
                   snapshot_json TEXT NOT NULL,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS issue_intakes_by_status
                   ON issue_intakes(status, updated_at DESC);
                 PRAGMA user_version = 7;
                 COMMIT;",
            )?;
        }
        if version < 8 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS agent_todos (
                   run_id TEXT NOT NULL,
                   agent_id TEXT NOT NULL,
                   todo_id TEXT NOT NULL,
                   objective TEXT NOT NULL,
                   state TEXT NOT NULL,
                   sort_order INTEGER NOT NULL,
                   blocker TEXT,
                   evidence_json TEXT NOT NULL DEFAULT '[]',
                   revision INTEGER NOT NULL DEFAULT 1,
                   updated_at TEXT NOT NULL,
                   PRIMARY KEY (run_id, agent_id, todo_id),
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS agent_todos_by_state
                   ON agent_todos(run_id, agent_id, state, sort_order);
                 CREATE TABLE IF NOT EXISTS memories (
                   memory_id TEXT PRIMARY KEY,
                   workspace_id TEXT,
                   run_id TEXT,
                   scope TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   subject TEXT NOT NULL,
                   body TEXT NOT NULL,
                   confidence INTEGER NOT NULL,
                   salience INTEGER NOT NULL,
                   provenance_json TEXT NOT NULL,
                   entities_json TEXT NOT NULL DEFAULT '[]',
                   valid_from TEXT NOT NULL,
                   valid_until TEXT,
                   access_count INTEGER NOT NULL DEFAULT 0,
                   pinned INTEGER NOT NULL DEFAULT 0,
                   supersedes_id TEXT,
                   tombstone INTEGER NOT NULL DEFAULT 0,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE SET NULL,
                   FOREIGN KEY (supersedes_id) REFERENCES memories(memory_id)
                 );
                 CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                   memory_id UNINDEXED, subject, body, entities
                 );
                 CREATE TABLE IF NOT EXISTS memory_relations (
                   source_id TEXT NOT NULL,
                   relation TEXT NOT NULL,
                   target_id TEXT NOT NULL,
                   confidence INTEGER NOT NULL,
                   PRIMARY KEY (source_id, relation, target_id),
                   FOREIGN KEY (source_id) REFERENCES memories(memory_id) ON DELETE CASCADE,
                   FOREIGN KEY (target_id) REFERENCES memories(memory_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS memories_by_scope
                   ON memories(scope, workspace_id, run_id, tombstone, pinned, updated_at DESC);
                 PRAGMA user_version = 8;
                 COMMIT;",
            )?;
        }
        if version < 9 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS runs (
                   run_id TEXT PRIMARY KEY,
                   title TEXT NOT NULL,
                   goal TEXT NOT NULL,
                   mode TEXT NOT NULL,
                   state TEXT NOT NULL,
                   model TEXT,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   input_tokens INTEGER NOT NULL DEFAULT 0,
                   output_tokens INTEGER NOT NULL DEFAULT 0,
                   summary TEXT,
                   pending_question TEXT,
                   archived INTEGER NOT NULL DEFAULT 0,
                   parent_run_id TEXT
                 );
                 CREATE TABLE IF NOT EXISTS hive_messages (
                   message_id TEXT PRIMARY KEY,
                   run_id TEXT NOT NULL,
                   room_id TEXT NOT NULL,
                   sender_id TEXT NOT NULL,
                   recipient TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   payload_json TEXT NOT NULL,
                   occurred_at TEXT NOT NULL,
                   expires_at TEXT,
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS office_rooms (
                   run_id TEXT NOT NULL,
                   room_id TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   purpose TEXT NOT NULL,
                   owner_id TEXT,
                   state TEXT NOT NULL DEFAULT 'open',
                   created_at TEXT NOT NULL,
                   closed_at TEXT,
                   closure_summary TEXT,
                   PRIMARY KEY (run_id, room_id),
                   FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS office_room_members (
                   run_id TEXT NOT NULL,
                   room_id TEXT NOT NULL,
                   member_id TEXT NOT NULL,
                   joined_at TEXT NOT NULL,
                   PRIMARY KEY (run_id, room_id, member_id),
                   FOREIGN KEY (run_id, room_id) REFERENCES office_rooms(run_id, room_id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS office_read_cursors (
                   run_id TEXT NOT NULL,
                   room_id TEXT NOT NULL,
                   consumer_id TEXT NOT NULL,
                   last_sequence INTEGER NOT NULL DEFAULT 0,
                   updated_at TEXT NOT NULL,
                   PRIMARY KEY (run_id, room_id, consumer_id),
                   FOREIGN KEY (run_id, room_id) REFERENCES office_rooms(run_id, room_id) ON DELETE CASCADE
                 );
                 INSERT OR IGNORE INTO office_rooms
                   (run_id, room_id, kind, purpose, owner_id, state, created_at)
                 SELECT run_id, room_id,
                   CASE WHEN room_id = 'run' THEN 'run' ELSE 'temporary' END,
                   CASE WHEN room_id = 'run' THEN 'Run coordination' ELSE room_id END,
                   MIN(sender_id), 'open', MIN(occurred_at)
                 FROM hive_messages GROUP BY run_id, room_id;
                 INSERT OR IGNORE INTO office_room_members
                   (run_id, room_id, member_id, joined_at)
                 SELECT run_id, room_id, sender_id, MIN(occurred_at)
                 FROM hive_messages GROUP BY run_id, room_id, sender_id;
                 INSERT OR IGNORE INTO office_room_members
                   (run_id, room_id, member_id, joined_at)
                 SELECT run_id, room_id, recipient, MIN(occurred_at)
                 FROM hive_messages GROUP BY run_id, room_id, recipient;
                 ALTER TABLE hive_messages ADD COLUMN room_sequence INTEGER NOT NULL DEFAULT 0;
                 UPDATE hive_messages AS message
                 SET room_sequence = (
                   SELECT COUNT(*) FROM hive_messages AS prior
                   WHERE prior.run_id = message.run_id
                     AND prior.room_id = message.room_id
                     AND (prior.occurred_at < message.occurred_at
                       OR (prior.occurred_at = message.occurred_at AND prior.message_id <= message.message_id))
                 );
                 CREATE UNIQUE INDEX IF NOT EXISTS hive_by_room_sequence
                   ON hive_messages(run_id, room_id, room_sequence);
                 CREATE INDEX IF NOT EXISTS office_rooms_by_state
                   ON office_rooms(run_id, state, created_at);
                 PRAGMA user_version = 9;
                 COMMIT;",
            )?;
        }
        // Keep the additive v8 contract self-healing for databases opened by
        // early v8 development builds that predated the normalized entity
        // table. This is intentionally idempotent and does not rewrite data.
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_entities (
               memory_id TEXT NOT NULL,
               entity TEXT NOT NULL,
               PRIMARY KEY (memory_id, entity),
               FOREIGN KEY (memory_id) REFERENCES memories(memory_id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS memory_entities_by_entity
               ON memory_entities(entity, memory_id);
             CREATE TABLE IF NOT EXISTS provider_balance_state (
               workspace_id TEXT NOT NULL,
               provider TEXT NOT NULL,
               currency TEXT NOT NULL,
               current_balance REAL NOT NULL,
               high_water_balance REAL NOT NULL,
               updated_at TEXT NOT NULL,
               PRIMARY KEY (workspace_id, provider, currency)
             );
             CREATE TABLE IF NOT EXISTS memory_settings (
               workspace_id TEXT PRIMARY KEY,
               enabled INTEGER NOT NULL DEFAULT 1,
               use_memory INTEGER NOT NULL DEFAULT 1,
               generate INTEGER NOT NULL DEFAULT 1,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS memory_extraction_queue (
               run_id TEXT PRIMARY KEY,
               status TEXT NOT NULL DEFAULT 'pending',
               queued_at TEXT NOT NULL,
               processed_at TEXT,
               FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
             );
             CREATE TABLE IF NOT EXISTS hive_consumed (
               message_id TEXT NOT NULL,
               recipient TEXT NOT NULL,
               consumed_at TEXT NOT NULL,
               PRIMARY KEY (message_id, recipient),
               FOREIGN KEY (message_id) REFERENCES hive_messages(message_id) ON DELETE CASCADE
             );
             CREATE TABLE IF NOT EXISTS hive_dedup (
               run_id TEXT NOT NULL,
               dedupe_key TEXT NOT NULL,
               message_id TEXT NOT NULL,
               created_at TEXT NOT NULL,
               PRIMARY KEY (run_id, dedupe_key),
               FOREIGN KEY (message_id) REFERENCES hive_messages(message_id) ON DELETE CASCADE
             );
             PRAGMA user_version = 1;",
        )?;
        debug_assert!(mode == "wal" || mode == "memory");
        Ok(())
    }

    pub fn journal_mode(&self) -> Result<String, StoreError> {
        Ok(self
            .connection
            .lock()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
    }

    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self
            .connection
            .lock()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    pub fn todos(&self, run_id: RunId, agent_id: EventAgentId) -> Result<Vec<TodoItem>, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT todo_id, objective, state, sort_order, blocker, evidence_json, revision
             FROM agent_todos WHERE run_id = ?1 AND agent_id = ?2
             ORDER BY sort_order, todo_id",
        )?;
        let rows = statement.query_map(params![run_id.to_string(), agent_id.to_string()], |row| {
            let state: String = row.get(2)?;
            let evidence: String = row.get(5)?;
            Ok(TodoItem {
                id: row.get(0)?,
                objective: row.get(1)?,
                state: todo_state_from_name(&state),
                order: row.get::<_, u32>(3)?,
                blocker: row.get(4)?,
                evidence: serde_json::from_str(&evidence).unwrap_or_default(),
                revision: row.get::<_, i64>(6)?.max(0) as u64,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sqlite)
    }

    pub fn upsert_todo(
        &self,
        run_id: RunId,
        agent_id: EventAgentId,
        mut item: TodoItem,
    ) -> Result<TodoItem, StoreError> {
        let current = self
            .connection
            .lock()
            .query_row(
                "SELECT revision FROM agent_todos WHERE run_id = ?1 AND agent_id = ?2 AND todo_id = ?3",
                params![run_id.to_string(), agent_id.to_string(), item.id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        item.revision = current.unwrap_or(0).max(0) as u64 + 1;
        self.connection.lock().execute(
            "INSERT INTO agent_todos
             (run_id, agent_id, todo_id, objective, state, sort_order, blocker, evidence_json, revision, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(run_id, agent_id, todo_id) DO UPDATE SET
               objective = excluded.objective, state = excluded.state,
               sort_order = excluded.sort_order, blocker = excluded.blocker,
               evidence_json = excluded.evidence_json, revision = excluded.revision,
               updated_at = excluded.updated_at",
            params![
                run_id.to_string(), agent_id.to_string(), item.id, item.objective,
                todo_state_name(item.state), item.order, item.blocker,
                serde_json::to_string(&item.evidence)?, item.revision as i64, Utc::now()
            ],
        )?;
        self.record_runtime_event(
            run_id,
            RuntimeEvent::TodoChanged {
                agent_id,
                item: item.clone(),
            },
        )?;
        let (active, blocked, completed, stale_agents) = self.todo_rollup(run_id)?;
        let details = self.todo_rollup_details(run_id, 3)?;
        self.record_runtime_event(
            run_id,
            RuntimeEvent::TodoRollupChanged {
                active,
                blocked,
                completed,
                stale_agents,
                active_goals: details.active_goals,
                blocked_work: details.blocked_work,
                recently_completed: details.recently_completed,
            },
        )?;
        Ok(item)
    }

    pub fn clear_todos(&self, run_id: RunId, agent_id: EventAgentId) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "DELETE FROM agent_todos WHERE run_id = ?1 AND agent_id = ?2",
            params![run_id.to_string(), agent_id.to_string()],
        )?;
        let (active, blocked, completed, stale_agents) = self.todo_rollup(run_id)?;
        let details = self.todo_rollup_details(run_id, 3)?;
        self.record_runtime_event(
            run_id,
            RuntimeEvent::TodoRollupChanged {
                active,
                blocked,
                completed,
                stale_agents,
                active_goals: details.active_goals,
                blocked_work: details.blocked_work,
                recently_completed: details.recently_completed,
            },
        )?;
        Ok(())
    }

    pub fn todo_rollup(&self, run_id: RunId) -> Result<(u64, u64, u64, u64), StoreError> {
        let run_id = run_id.to_string();
        let connection = self.connection.lock();
        let (active, blocked, completed) = connection.query_row(
            "SELECT
               COALESCE(SUM(CASE WHEN state IN ('pending', 'in_progress') THEN 1 ELSE 0 END), 0),
               COALESCE(SUM(CASE WHEN state = 'blocked' THEN 1 ELSE 0 END), 0),
               COALESCE(SUM(CASE WHEN state = 'completed' THEN 1 ELSE 0 END), 0)
             FROM agent_todos WHERE run_id = ?1",
            [&run_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        let stale_agents = connection.query_row(
            "SELECT COUNT(*) FROM agents a
             WHERE a.run_id = ?1 AND a.state NOT IN ('completed', 'failed', 'cancelled')
               AND NOT EXISTS (
                 SELECT 1 FROM agent_todos t
                 WHERE t.run_id = a.run_id AND t.agent_id = a.agent_id
               )",
            [&run_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok((
            active.max(0) as u64,
            blocked.max(0) as u64,
            completed.max(0) as u64,
            stale_agents.max(0) as u64,
        ))
    }

    pub fn todo_rollup_details(&self, run_id: RunId, limit: usize) -> Result<TodoRollupDetails, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT objective, state, blocker FROM agent_todos
             WHERE run_id = ?1 ORDER BY updated_at DESC, sort_order, todo_id",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let limit = limit.clamp(1, 10);
        let mut active = Vec::new();
        let mut blocked = Vec::new();
        let mut completed = Vec::new();
        for row in rows {
            let (objective, state, blocker) = row?;
            match state.as_str() {
                "pending" | "in_progress" if active.len() < limit => active.push(objective),
                "blocked" if blocked.len() < limit => blocked.push(match blocker {
                    Some(blocker) if !blocker.trim().is_empty() => format!("{objective}: {blocker}"),
                    _ => objective,
                }),
                "completed" if completed.len() < limit => completed.push(objective),
                _ => {}
            }
        }
        Ok(TodoRollupDetails {
            active_goals: active,
            blocked_work: blocked,
            recently_completed: completed,
        })
    }

    pub fn put_memory(&self, mut memory: MemoryRecord) -> Result<MemoryRecord, StoreError> {
        if !memory.is_safe() {
            return Err(StoreError::Memory(
                "empty or secret-bearing memory rejected".into(),
            ));
        }
        let now = Utc::now();
        memory.updated_at = now;
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate = transaction
            .query_row(
                "SELECT memory_id FROM memories
                 WHERE scope = ?1 AND workspace_id IS ?2 AND run_id IS ?3
                   AND kind = ?4 AND subject = ?5 AND body = ?6 AND tombstone = 0
                 LIMIT 1",
                params![
                    memory.scope.as_str(),
                    memory.workspace_id,
                    memory.run_id.map(|id| id.to_string()),
                    memory.kind,
                    memory.subject,
                    memory.body
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(id) = duplicate {
            memory.id = id;
            transaction.commit()?;
            return Ok(memory);
        }
        if let Some(supersedes) = memory.supersedes_id.as_deref() {
            transaction.execute(
                "UPDATE memories SET valid_until = ?2, updated_at = ?2 WHERE memory_id = ?1",
                params![supersedes, now],
            )?;
        }
        transaction.execute(
            "INSERT INTO memories
             (memory_id, workspace_id, run_id, scope, kind, subject, body, confidence, salience,
              provenance_json, entities_json, valid_from, valid_until, access_count, pinned,
              supersedes_id, tombstone, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                memory.id,
                memory.workspace_id,
                memory.run_id.map(|id| id.to_string()),
                memory.scope.as_str(),
                memory.kind,
                memory.subject,
                memory.body,
                i64::from(memory.confidence),
                i64::from(memory.salience),
                serde_json::to_string(&memory.provenance)?,
                serde_json::to_string(&memory.entities)?,
                memory.valid_from,
                memory.valid_until,
                memory.access_count as i64,
                i64::from(memory.pinned),
                memory.supersedes_id,
                i64::from(memory.tombstone),
                memory.created_at,
                memory.updated_at
            ],
        )?;
        transaction.execute(
            "INSERT INTO memories_fts (memory_id, subject, body, entities) VALUES (?1, ?2, ?3, ?4)",
            params![memory.id, memory.subject, memory.body, memory.entities.join(" ")],
        )?;
        for entity in &memory.entities {
            transaction.execute(
                "INSERT OR IGNORE INTO memory_entities (memory_id, entity) VALUES (?1, ?2)",
                params![memory.id, entity],
            )?;
        }
        transaction.commit()?;
        Ok(memory)
    }

    pub fn search_memories(
        &self,
        workspace_id: &str,
        run_id: Option<RunId>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryHit>, StoreError> {
        let terms = query
            .split(|character: char| !character.is_alphanumeric() && character != '_' && character != '-')
            .filter(|term| !term.is_empty())
            .take(12)
            .collect::<Vec<_>>();
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let fts_query = terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let run_id_text = run_id.map(|id| id.to_string());
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT m.memory_id, m.workspace_id, m.run_id, m.scope, m.kind, m.subject, m.body,
                    m.confidence, m.salience, m.provenance_json, m.entities_json, m.valid_from,
                    m.valid_until, m.access_count, m.pinned, m.supersedes_id, m.tombstone,
                    m.created_at, m.updated_at, bm25(memories_fts)
             FROM memories_fts JOIN memories m ON m.memory_id = memories_fts.memory_id
             WHERE memories_fts MATCH ?1 AND m.tombstone = 0
               AND (m.valid_until IS NULL OR m.valid_until > ?4)
               AND (m.scope = 'user' OR (m.scope = 'project' AND m.workspace_id = ?2)
                    OR (m.scope = 'run' AND m.run_id = ?3))
             ORDER BY m.pinned DESC, bm25(memories_fts), m.updated_at DESC LIMIT 200",
        )?;
        let rows = statement.query_map(params![fts_query, workspace_id, run_id_text, Utc::now()], |row| {
            let stored_run: Option<String> = row.get(2)?;
            let scope: String = row.get(3)?;
            let entities_json: String = row.get(10)?;
            let provenance_json: String = row.get(9)?;
            let memory = MemoryRecord {
                id: row.get(0)?,
                workspace_id: row.get(1)?,
                run_id: stored_run.and_then(|value| RunId::from_str(&value).ok()),
                scope: memory_scope_from_name(&scope),
                kind: row.get(4)?,
                subject: row.get(5)?,
                body: row.get(6)?,
                confidence: row.get::<_, i64>(7)?.clamp(0, 100) as u8,
                salience: row.get::<_, i64>(8)?.clamp(0, 100) as u8,
                provenance: serde_json::from_str(&provenance_json).unwrap_or_default(),
                entities: serde_json::from_str(&entities_json).unwrap_or_default(),
                valid_from: row.get(11)?,
                valid_until: row.get(12)?,
                access_count: row.get::<_, i64>(13)?.max(0) as u64,
                pinned: row.get::<_, i64>(14)? != 0,
                supersedes_id: row.get(15)?,
                tombstone: row.get::<_, i64>(16)? != 0,
                created_at: row.get(17)?,
                updated_at: row.get(18)?,
            };
            Ok((memory, row.get::<_, f64>(19)?))
        })?;
        let lower_terms = terms
            .iter()
            .map(|term| term.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let mut hits = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(memory, bm25)| {
                let mut reasons = vec!["fts".into()];
                let exact_entities = memory
                    .entities
                    .iter()
                    .filter(|entity| {
                        lower_terms
                            .iter()
                            .any(|term| entity.to_ascii_lowercase() == *term)
                    })
                    .count();
                let scope_bonus = match memory.scope {
                    MemoryScope::Run => 0.30,
                    MemoryScope::Project => 0.20,
                    MemoryScope::User => 0.10,
                };
                if exact_entities > 0 {
                    reasons.push("entity".into());
                }
                if memory.pinned {
                    reasons.push("pinned".into());
                }
                let score = 1.0 / (1.0 + bm25.abs())
                    + exact_entities as f64 * 0.25
                    + scope_bonus
                    + f64::from(memory.confidence) / 1_000.0
                    + f64::from(memory.salience) / 1_000.0
                    + if memory.pinned { 0.5 } else { 0.0 };
                MemoryHit {
                    memory,
                    score,
                    reasons,
                }
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.memory.id.cmp(&right.memory.id))
        });
        hits.truncate(limit.min(20));
        for hit in &mut hits {
            connection.execute(
                "UPDATE memories SET access_count = access_count + 1 WHERE memory_id = ?1",
                [&hit.memory.id],
            )?;
            hit.memory.access_count = hit.memory.access_count.saturating_add(1);
        }
        Ok(hits)
    }

    pub fn set_memory_state(
        &self,
        memory_id: &str,
        pinned: Option<bool>,
        tombstone: Option<bool>,
    ) -> Result<bool, StoreError> {
        let changed = self.connection.lock().execute(
            "UPDATE memories SET pinned = COALESCE(?2, pinned), tombstone = COALESCE(?3, tombstone), updated_at = ?4
             WHERE memory_id = ?1",
            params![memory_id, pinned.map(i64::from), tombstone.map(i64::from), Utc::now()],
        )? > 0;
        if tombstone == Some(true) {
            self.connection
                .lock()
                .execute("DELETE FROM memories_fts WHERE memory_id = ?1", [memory_id])?;
        }
        Ok(changed)
    }

    pub fn memory(&self, memory_id: &str) -> Result<Option<MemoryRecord>, StoreError> {
        self.connection
            .lock()
            .query_row(
                "SELECT memory_id, workspace_id, run_id, scope, kind, subject, body,
                        confidence, salience, provenance_json, entities_json, valid_from,
                        valid_until, access_count, pinned, supersedes_id, tombstone,
                        created_at, updated_at
                 FROM memories WHERE memory_id = ?1",
                [memory_id],
                memory_record_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn memory_settings(&self, workspace_id: &str) -> Result<MemorySettings, StoreError> {
        Ok(self
            .connection
            .lock()
            .query_row(
                "SELECT enabled, use_memory, generate FROM memory_settings WHERE workspace_id = ?1",
                [workspace_id],
                |row| {
                    Ok(MemorySettings {
                        enabled: row.get::<_, i64>(0)? != 0,
                        use_memory: row.get::<_, i64>(1)? != 0,
                        generate: row.get::<_, i64>(2)? != 0,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn set_memory_settings(
        &self,
        workspace_id: &str,
        settings: MemorySettings,
    ) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "INSERT INTO memory_settings (workspace_id, enabled, use_memory, generate, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(workspace_id) DO UPDATE SET enabled = excluded.enabled,
               use_memory = excluded.use_memory, generate = excluded.generate,
               updated_at = excluded.updated_at",
            params![
                workspace_id,
                i64::from(settings.enabled),
                i64::from(settings.use_memory),
                i64::from(settings.generate),
                Utc::now()
            ],
        )?;
        Ok(())
    }

    pub fn queue_memory_extraction(&self, run_id: RunId) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "INSERT OR IGNORE INTO memory_extraction_queue (run_id, status, queued_at)
             VALUES (?1, 'pending', ?2)",
            params![run_id.to_string(), Utc::now()],
        )?;
        Ok(())
    }

    pub fn pending_memory_extractions(&self, limit: usize) -> Result<Vec<RunId>, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT run_id FROM memory_extraction_queue WHERE status = 'pending'
             ORDER BY queued_at LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.min(20) as i64], |row| row.get::<_, String>(0))?;
        rows.map(|row| RunId::from_str(&row?).map_err(StoreError::from))
            .collect()
    }

    pub fn finish_memory_extraction(&self, run_id: RunId, status: &str) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "UPDATE memory_extraction_queue SET status = ?2, processed_at = ?3 WHERE run_id = ?1",
            params![run_id.to_string(), status, Utc::now()],
        )?;
        Ok(())
    }

    pub fn update_provider_balance_high_water(
        &self,
        workspace_id: &str,
        provider: &str,
        currency: &str,
        current: f64,
    ) -> Result<f64, StoreError> {
        let connection = self.connection.lock();
        connection.execute(
            "INSERT INTO provider_balance_state
               (workspace_id, provider, currency, current_balance, high_water_balance, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)
             ON CONFLICT(workspace_id, provider, currency) DO UPDATE SET
               current_balance = excluded.current_balance,
               high_water_balance = MAX(provider_balance_state.high_water_balance, excluded.current_balance),
               updated_at = excluded.updated_at",
            params![workspace_id, provider, currency, current, Utc::now()],
        )?;
        Ok(connection.query_row(
            "SELECT high_water_balance FROM provider_balance_state
             WHERE workspace_id = ?1 AND provider = ?2 AND currency = ?3",
            params![workspace_id, provider, currency],
            |row| row.get(0),
        )?)
    }

    pub fn create_run(&self, goal: &str, mode: Mode) -> Result<RunRecord, StoreError> {
        let now = Utc::now();
        let title = session_title(goal);
        let run = RunRecord {
            id: RunId::new(),
            title,
            goal: goal.to_owned(),
            mode,
            state: ExitState::Pending,
            model: None,
            created_at: now,
            updated_at: now,
            input_tokens: 0,
            output_tokens: 0,
            summary: None,
            pending_question: None,
            archived: false,
            parent_run_id: None,
        };
        self.connection.lock().execute(
            "INSERT INTO runs (run_id, title, goal, mode, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.id.to_string(),
                run.title,
                run.goal,
                mode_name(run.mode),
                state_name(run.state),
                run.created_at,
                run.updated_at
            ],
        )?;
        Ok(run)
    }

    pub fn run(&self, id: RunId) -> Result<Option<RunRecord>, StoreError> {
        let row = self
            .connection
            .lock()
            .query_row(
                "SELECT run_id, title, goal, mode, state, model, created_at, updated_at, input_tokens, output_tokens, summary, pending_question, archived, parent_run_id FROM runs WHERE run_id = ?1",
                [id.to_string()],
                row_to_run,
            )
            .optional()?;
        row.map(decode_run).transpose()
    }

    pub fn latest_run(&self) -> Result<Option<RunRecord>, StoreError> {
        let row = self
            .connection
            .lock()
            .query_row(
                "SELECT run_id, title, goal, mode, state, model, created_at, updated_at, input_tokens, output_tokens, summary, pending_question, archived, parent_run_id FROM runs WHERE archived = 0 ORDER BY updated_at DESC LIMIT 1",
                [],
                row_to_run,
            )
            .optional()?;
        row.map(decode_run).transpose()
    }

    pub fn list_runs(&self, limit: usize) -> Result<Vec<RunRecord>, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT run_id, title, goal, mode, state, model, created_at, updated_at, input_tokens, output_tokens, summary, pending_question, archived, parent_run_id FROM runs WHERE archived = 0 ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.min(1_000) as i64], row_to_run)?;
        rows.map(|row| decode_run(row?)).collect()
    }

    pub fn ensure_workspace(&self, root: &Path) -> Result<WorkspaceRecord, StoreError> {
        let root = root.to_string_lossy().into_owned();
        let id = workspace_id(&root);
        let now = Utc::now();
        self.connection.lock().execute(
            "INSERT INTO workspaces (workspace_id, root, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(workspace_id) DO UPDATE SET root = excluded.root, updated_at = excluded.updated_at",
            params![id, root, now],
        )?;
        Ok(WorkspaceRecord {
            id,
            root,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn attach_run_workspace(&self, run_id: RunId, workspace_id: &str) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "INSERT INTO run_workspaces (run_id, workspace_id) VALUES (?1, ?2)
             ON CONFLICT(run_id) DO UPDATE SET workspace_id = excluded.workspace_id",
            params![run_id.to_string(), workspace_id],
        )?;
        Ok(())
    }

    pub fn workspace_for_run(&self, run_id: RunId) -> Result<Option<String>, StoreError> {
        Ok(self
            .connection
            .lock()
            .query_row(
                "SELECT workspace_id FROM run_workspaces WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_model_catalog(
        &self,
        workspace_id: &str,
        models: &[ModelDescriptor],
        etag: Option<&str>,
    ) -> Result<CachedModelCatalog, StoreError> {
        let fetched_at = Utc::now();
        self.connection.lock().execute(
            "INSERT INTO model_catalogs (workspace_id, etag, models_json, fetched_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(workspace_id) DO UPDATE SET etag = excluded.etag,
               models_json = excluded.models_json, fetched_at = excluded.fetched_at",
            params![workspace_id, etag, serde_json::to_string(models)?, fetched_at],
        )?;
        Ok(CachedModelCatalog {
            models: models.to_vec(),
            etag: etag.map(str::to_owned),
            fetched_at,
        })
    }

    pub fn touch_model_catalog(&self, workspace_id: &str) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "UPDATE model_catalogs SET fetched_at = ?2 WHERE workspace_id = ?1",
            params![workspace_id, Utc::now()],
        )?;
        Ok(())
    }

    pub fn model_catalog(&self, workspace_id: &str) -> Result<Option<CachedModelCatalog>, StoreError> {
        let row = self
            .connection
            .lock()
            .query_row(
                "SELECT models_json, etag, fetched_at FROM model_catalogs WHERE workspace_id = ?1",
                [workspace_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, DateTime<Utc>>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(models, etag, fetched_at)| {
            Ok(CachedModelCatalog {
                models: serde_json::from_str(&models)?,
                etag,
                fetched_at,
            })
        })
        .transpose()
    }

    pub fn put_cached_result(
        &self,
        workspace_id: &str,
        key: &str,
        class: CacheClass,
        value: &[u8],
        manifest: &ObservedInputManifest,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<bool, StoreError> {
        if class == CacheClass::Never {
            self.record_cache_bypass(workspace_id)?;
            return Ok(false);
        }
        let now = Utc::now();
        let connection = self.connection.lock();
        connection.execute(
            "INSERT INTO cache_entries
             (cache_key, workspace_id, class, value, manifest_json, stored_at, last_used_at,
              expires_at, hits, size_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, 0, ?8)
             ON CONFLICT(cache_key) DO UPDATE SET workspace_id = excluded.workspace_id,
               class = excluded.class, value = excluded.value, manifest_json = excluded.manifest_json,
               stored_at = excluded.stored_at, last_used_at = excluded.last_used_at,
               expires_at = excluded.expires_at, hits = 0, size_bytes = excluded.size_bytes",
            params![
                key,
                workspace_id,
                cache_class_name(class),
                value,
                serde_json::to_string(manifest)?,
                now,
                expires_at,
                value.len() as i64
            ],
        )?;
        connection.execute(
            "INSERT INTO cache_stats (workspace_id, writes, bytes_written)
             VALUES (?1, 1, ?2)
             ON CONFLICT(workspace_id) DO UPDATE SET
               writes = writes + 1, bytes_written = bytes_written + excluded.bytes_written",
            params![workspace_id, value.len() as i64],
        )?;
        Ok(true)
    }

    pub fn cached_result(&self, workspace_id: &str, key: &str) -> Result<Option<CachedResult>, StoreError> {
        let now = Utc::now();
        let connection = self.connection.lock();
        let result = connection
            .query_row(
                "SELECT class, value, manifest_json, stored_at, expires_at, hits
                 FROM cache_entries WHERE workspace_id = ?1 AND cache_key = ?2
                   AND (expires_at IS NULL OR expires_at > ?3)",
                params![workspace_id, key, now],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, DateTime<Utc>>(3)?,
                        row.get::<_, Option<DateTime<Utc>>>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((class, value, manifest, stored_at, expires_at, hits)) = result else {
            connection.execute(
                "INSERT INTO cache_stats (workspace_id, misses) VALUES (?1, 1)
                 ON CONFLICT(workspace_id) DO UPDATE SET misses = misses + 1",
                [workspace_id],
            )?;
            return Ok(None);
        };
        connection.execute(
            "UPDATE cache_entries SET hits = hits + 1, last_used_at = ?3
             WHERE workspace_id = ?1 AND cache_key = ?2",
            params![workspace_id, key, now],
        )?;
        connection.execute(
            "INSERT INTO cache_stats (workspace_id, hits, bytes_read) VALUES (?1, 1, ?2)
             ON CONFLICT(workspace_id) DO UPDATE SET
               hits = hits + 1, bytes_read = bytes_read + excluded.bytes_read",
            params![workspace_id, value.len() as i64],
        )?;
        Ok(Some(CachedResult {
            key: key.to_owned(),
            class,
            value,
            manifest: serde_json::from_str(&manifest)?,
            stored_at,
            expires_at,
            hits: hits.max(0) as u64 + 1,
        }))
    }

    /// Record a cache hit that was served by the in-memory tier.
    ///
    /// SQLite remains the durable source of truth: a hot entry is accepted only
    /// while the corresponding durable entry still exists and has not expired.
    /// Returning `false` tells the caller to discard the hot entry and perform a
    /// normal durable lookup, which also records the eventual miss.
    pub fn touch_cached_result(
        &self,
        workspace_id: &str,
        key: &str,
        bytes_read: u64,
    ) -> Result<bool, StoreError> {
        let now = Utc::now();
        let connection = self.connection.lock();
        let updated = connection.execute(
            "UPDATE cache_entries SET hits = hits + 1, last_used_at = ?3
             WHERE workspace_id = ?1 AND cache_key = ?2
               AND (expires_at IS NULL OR expires_at > ?3)",
            params![workspace_id, key, now],
        )?;
        if updated == 0 {
            return Ok(false);
        }
        connection.execute(
            "INSERT INTO cache_stats (workspace_id, hits, bytes_read) VALUES (?1, 1, ?2)
             ON CONFLICT(workspace_id) DO UPDATE SET
               hits = hits + 1, bytes_read = bytes_read + excluded.bytes_read",
            params![workspace_id, bytes_read as i64],
        )?;
        Ok(true)
    }

    pub fn cache_totals(&self, workspace_id: &str) -> Result<CacheTotals, StoreError> {
        let connection = self.connection.lock();
        let (entries, bytes): (i64, i64) = connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0)
             FROM cache_entries WHERE workspace_id = ?1",
            [workspace_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let stats: Option<(i64, i64, i64, i64, i64, i64, i64)> = connection
            .query_row(
                "SELECT hits, misses, writes, bypasses, bytes_read, bytes_written,
                        saved_input_tokens FROM cache_stats WHERE workspace_id = ?1",
                [workspace_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        let (hits, misses, writes, bypasses, bytes_read, bytes_written, saved_input_tokens) =
            stats.unwrap_or_default();
        Ok(CacheTotals {
            entries: entries.max(0) as u64,
            bytes: bytes.max(0) as u64,
            hits: hits.max(0) as u64,
            misses: misses.max(0) as u64,
            writes: writes.max(0) as u64,
            bypasses: bypasses.max(0) as u64,
            bytes_read: bytes_read.max(0) as u64,
            bytes_written: bytes_written.max(0) as u64,
            saved_input_tokens: saved_input_tokens.max(0) as u64,
        })
    }

    pub fn save_issue_clarification(
        &self,
        run_id: RunId,
        clarification: &IssueClarificationView,
    ) -> Result<(), StoreError> {
        let now = Utc::now();
        self.connection.lock().execute(
            "INSERT INTO issue_intakes
             (run_id, schema_version, status, snapshot_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(run_id) DO UPDATE SET
               schema_version = excluded.schema_version,
               status = excluded.status,
               snapshot_json = excluded.snapshot_json,
               updated_at = excluded.updated_at",
            params![
                run_id.to_string(),
                i64::from(clarification.schema_version),
                clarification_status_name(clarification.status),
                serde_json::to_string(clarification)?,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn issue_clarification(&self, run_id: RunId) -> Result<Option<IssueClarificationView>, StoreError> {
        let snapshot = self
            .connection
            .lock()
            .query_row(
                "SELECT snapshot_json FROM issue_intakes WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        snapshot
            .map(|snapshot| serde_json::from_str(&snapshot).map_err(StoreError::from))
            .transpose()
    }

    pub fn record_cache_savings(
        &self,
        workspace_id: &str,
        saved_input_tokens: u64,
    ) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "INSERT INTO cache_stats (workspace_id, saved_input_tokens) VALUES (?1, ?2)
             ON CONFLICT(workspace_id) DO UPDATE SET saved_input_tokens =
               saved_input_tokens + excluded.saved_input_tokens",
            params![workspace_id, saved_input_tokens as i64],
        )?;
        Ok(())
    }

    pub fn record_cache_bypass(&self, workspace_id: &str) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "INSERT INTO cache_stats (workspace_id, bypasses) VALUES (?1, 1)
             ON CONFLICT(workspace_id) DO UPDATE SET bypasses = bypasses + 1",
            [workspace_id],
        )?;
        Ok(())
    }

    pub fn record_compaction_checkpoint(
        &self,
        run_id: RunId,
        summary: &str,
        manifest: &ObservedInputManifest,
        estimated_tokens_before: u64,
    ) -> Result<String, StoreError> {
        let checkpoint_id = uuid::Uuid::now_v7().to_string();
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let parent_id: Option<String> = transaction
            .query_row(
                "SELECT checkpoint_id FROM compaction_checkpoints
                 WHERE run_id = ?1 ORDER BY created_at DESC, checkpoint_id DESC LIMIT 1",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        transaction.execute(
            "INSERT INTO compaction_checkpoints
             (checkpoint_id, run_id, parent_id, summary, manifest_json,
              estimated_tokens_before, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                checkpoint_id,
                run_id.to_string(),
                parent_id,
                summary,
                serde_json::to_string(manifest)?,
                estimated_tokens_before as i64,
                Utc::now(),
            ],
        )?;
        transaction.execute(
            "UPDATE messages SET compacted = 1 WHERE run_id = ?1 AND compacted = 0",
            [run_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(checkpoint_id)
    }

    pub fn record_incident(
        &self,
        run_id: Option<RunId>,
        incident: &IncidentView,
        detail: Option<&str>,
    ) -> Result<String, StoreError> {
        let incident_id = uuid::Uuid::now_v7().to_string();
        self.connection.lock().execute(
            "INSERT INTO incidents
             (incident_id, run_id, code, severity, category, retryable, summary, detail,
              correlation_id, resolved, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)",
            params![
                incident_id,
                run_id.map(|id| id.to_string()),
                incident.code,
                incident_severity_name(incident.severity),
                incident.category,
                incident.retryable,
                incident.summary,
                detail,
                incident.correlation_id,
                Utc::now(),
            ],
        )?;
        Ok(incident_id)
    }

    pub fn prune_cache(&self, workspace_id: &str, max_bytes: u64) -> Result<u64, StoreError> {
        let now = Utc::now();
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut removed = transaction.execute(
            "DELETE FROM cache_entries WHERE workspace_id = ?1 AND expires_at IS NOT NULL AND expires_at <= ?2",
            params![workspace_id, now],
        )? as u64;
        let mut bytes: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM cache_entries WHERE workspace_id = ?1",
            [workspace_id],
            |row| row.get(0),
        )?;
        if bytes.max(0) as u64 > max_bytes {
            let mut statement = transaction.prepare(
                "SELECT cache_key, size_bytes FROM cache_entries
                 WHERE workspace_id = ?1 ORDER BY last_used_at ASC, cache_key ASC",
            )?;
            let candidates = statement
                .query_map([workspace_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            for (key, size) in candidates {
                if bytes.max(0) as u64 <= max_bytes {
                    break;
                }
                removed += transaction.execute(
                    "DELETE FROM cache_entries WHERE workspace_id = ?1 AND cache_key = ?2",
                    params![workspace_id, key],
                )? as u64;
                bytes = bytes.saturating_sub(size.max(0));
            }
        }
        transaction.commit()?;
        Ok(removed)
    }

    pub fn sync_bundled_books(&self) -> Result<usize, StoreError> {
        let manifest = crate::books::SignedRegistryManifest::bundled()?;
        manifest
            .validate()
            .map_err(|error| StoreError::Coordination(format!("invalid bundled books: {error:?}")))?;
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now();
        let mut count = 0;
        for pack in manifest.packs {
            for entry in pack.entries {
                let metadata = serde_json::to_string(&entry)?;
                let fingerprint = format!("{:x}", Sha256::digest(metadata.as_bytes()));
                transaction.execute(
                    "INSERT INTO book_index
                     (book_id, version, source, trust, metadata_json, fingerprint, updated_at)
                     VALUES (?1, ?2, ?3, 'verified', ?4, ?5, ?6)
                     ON CONFLICT(book_id, version) DO UPDATE SET metadata_json = excluded.metadata_json,
                       fingerprint = excluded.fingerprint, updated_at = excluded.updated_at",
                    params![
                        entry.id,
                        entry.version,
                        manifest.registry_id,
                        metadata,
                        fingerprint,
                        now
                    ],
                )?;
                count += 1;
            }
        }
        transaction.commit()?;
        Ok(count)
    }

    pub fn indexed_book_count(&self) -> Result<u64, StoreError> {
        let count: i64 = self.connection.lock().query_row(
            "SELECT COUNT(*) FROM book_index WHERE trust != 'disabled'",
            [],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_hive_message(
        &self,
        run_id: RunId,
        message_id: &str,
        room_id: &str,
        sender_id: &str,
        recipient: &str,
        kind: &str,
        payload: &serde_json::Value,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<String, StoreError> {
        let normalized = payload
            .get("body")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let task_scope = payload
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let refs = payload.get("refs").cloned().unwrap_or_default();
        let dedupe_key = format!(
            "{:x}",
            Sha256::digest(format!("{recipient}\0{kind}\0{task_scope}\0{normalized}\0{refs}").as_bytes())
        );
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO office_rooms
             (run_id, room_id, kind, purpose, owner_id, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6)",
            params![
                run_id.to_string(),
                room_id,
                if room_id == "run" { "run" } else { "temporary" },
                if room_id == "run" {
                    "Run coordination"
                } else {
                    room_id
                },
                sender_id,
                Utc::now()
            ],
        )?;
        for member in [sender_id, recipient] {
            transaction.execute(
                "INSERT OR IGNORE INTO office_room_members
                 (run_id, room_id, member_id, joined_at) VALUES (?1, ?2, ?3, ?4)",
                params![run_id.to_string(), room_id, member, Utc::now()],
            )?;
        }
        if let Some(existing) = transaction
            .query_row(
                "SELECT message_id FROM hive_dedup WHERE run_id = ?1 AND dedupe_key = ?2",
                params![run_id.to_string(), dedupe_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            transaction.commit()?;
            return Ok(existing);
        }
        let room_sequence: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(room_sequence), 0) + 1 FROM hive_messages
             WHERE run_id = ?1 AND room_id = ?2",
            params![run_id.to_string(), room_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO hive_messages
             (message_id, run_id, room_id, sender_id, recipient, kind, payload_json,
              occurred_at, expires_at, room_sequence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                message_id,
                run_id.to_string(),
                room_id,
                sender_id,
                recipient,
                kind,
                serde_json::to_string(payload)?,
                Utc::now(),
                expires_at,
                room_sequence
            ],
        )?;
        transaction.execute(
            "INSERT INTO hive_dedup (run_id, dedupe_key, message_id, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![run_id.to_string(), dedupe_key, message_id, Utc::now()],
        )?;
        transaction.commit()?;
        Ok(message_id.to_owned())
    }

    pub fn office_room_messages(
        &self,
        run_id: RunId,
        room_id: &str,
        consumer_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let last_sequence = transaction
            .query_row(
                "SELECT last_sequence FROM office_read_cursors
                 WHERE run_id = ?1 AND room_id = ?2 AND consumer_id = ?3",
                params![run_id.to_string(), room_id, consumer_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
        let mut statement = transaction.prepare(
            "SELECT message_id, room_sequence, sender_id, recipient, kind, payload_json, occurred_at
             FROM hive_messages
             WHERE run_id = ?1 AND room_id = ?2 AND room_sequence > ?3
               AND (expires_at IS NULL OR expires_at > ?4)
             ORDER BY room_sequence LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                run_id.to_string(),
                room_id,
                last_sequence,
                Utc::now(),
                limit.min(100) as i64
            ],
            |row| {
                let payload: String = row.get(5)?;
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "sequence": row.get::<_, i64>(1)?,
                    "room": room_id,
                    "sender": row.get::<_, String>(2)?,
                    "recipient": row.get::<_, String>(3)?,
                    "kind": row.get::<_, String>(4)?,
                    "payload": serde_json::from_str::<serde_json::Value>(&payload).unwrap_or_default(),
                    "occurred_at": row.get::<_, DateTime<Utc>>(6)?,
                }))
            },
        )?;
        let messages = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if let Some(sequence) = messages
            .last()
            .and_then(|message| message.get("sequence"))
            .and_then(serde_json::Value::as_i64)
        {
            transaction.execute(
                "INSERT INTO office_read_cursors
                 (run_id, room_id, consumer_id, last_sequence, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(run_id, room_id, consumer_id) DO UPDATE SET
                   last_sequence = MAX(last_sequence, excluded.last_sequence),
                   updated_at = excluded.updated_at",
                params![run_id.to_string(), room_id, consumer_id, sequence, Utc::now()],
            )?;
        }
        transaction.commit()?;
        Ok(messages)
    }

    pub fn close_office_room(&self, run_id: RunId, room_id: &str, summary: &str) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "UPDATE office_rooms SET state = 'closed', closed_at = ?3, closure_summary = ?4
             WHERE run_id = ?1 AND room_id = ?2 AND state = 'open'",
            params![run_id.to_string(), room_id, Utc::now(), summary],
        )?;
        Ok(())
    }

    pub fn close_office_rooms(&self, run_id: RunId, summary: &str) -> Result<u64, StoreError> {
        let changed = self.connection.lock().execute(
            "UPDATE office_rooms SET state = 'closed', closed_at = ?2, closure_summary = ?3
             WHERE run_id = ?1 AND state = 'open'",
            params![run_id.to_string(), Utc::now(), summary],
        )?;
        Ok(changed as u64)
    }

    pub fn hive_inbox(
        &self,
        run_id: RunId,
        recipient: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT message_id, room_id, sender_id, recipient, kind, payload_json, occurred_at
             FROM hive_messages
             WHERE run_id = ?1 AND (recipient = ?2 OR recipient = 'group:all')
               AND (expires_at IS NULL OR expires_at > ?3)
               AND NOT EXISTS (
                 SELECT 1 FROM hive_consumed c
                 WHERE c.message_id = hive_messages.message_id AND c.recipient = ?2
               )
             ORDER BY occurred_at, message_id LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![run_id.to_string(), recipient, Utc::now(), limit.min(100) as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, DateTime<Utc>>(6)?,
                ))
            },
        )?;
        let messages = rows
            .map(|row| {
                let (id, room, sender, recipient, kind, payload, occurred_at) = row?;
                Ok(json!({
                    "id": id,
                    "room": room,
                    "sender": sender,
                    "recipient": recipient,
                    "kind": kind,
                    "payload": serde_json::from_str::<serde_json::Value>(&payload)?,
                    "occurred_at": occurred_at,
                }))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        drop(statement);
        for message in &messages {
            if let Some(id) = message.get("id").and_then(serde_json::Value::as_str) {
                transaction.execute(
                    "INSERT OR IGNORE INTO hive_consumed (message_id, recipient, consumed_at)
                     VALUES (?1, ?2, ?3)",
                    params![id, recipient, Utc::now()],
                )?;
            }
        }
        transaction.commit()?;
        Ok(messages)
    }

    pub fn put_office_artifact(
        &self,
        run_id: RunId,
        artifact_id: &str,
        kind: &str,
        body: &[u8],
        provenance: &serde_json::Value,
    ) -> Result<String, StoreError> {
        let digest = format!("{:x}", Sha256::digest(body));
        self.connection.lock().execute(
            "INSERT INTO office_artifacts
             (artifact_id, run_id, kind, digest, body, provenance_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(artifact_id) DO NOTHING",
            params![
                artifact_id,
                run_id.to_string(),
                kind,
                digest,
                body,
                serde_json::to_string(provenance)?,
                Utc::now()
            ],
        )?;
        Ok(digest)
    }

    pub fn office_health(&self, run_id: RunId) -> Result<(u64, u64, u64), StoreError> {
        let connection = self.connection.lock();
        let active: i64 = connection.query_row(
            "SELECT COUNT(*) FROM agents WHERE run_id = ?1 AND state NOT IN ('completed','failed','cancelled')",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        let blocked: i64 = connection.query_row(
            "SELECT COUNT(*) FROM tasks WHERE run_id = ?1 AND state IN ('blocked','failed')",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        let open: i64 = connection.query_row(
            "SELECT COUNT(*) FROM tasks WHERE run_id = ?1 AND state NOT IN ('completed','failed')",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        Ok((active.max(0) as u64, open.max(0) as u64, blocked.max(0) as u64))
    }

    pub fn usage_totals(&self, run_id: Option<RunId>) -> Result<UsageTotals, StoreError> {
        let connection = self.connection.lock();
        let (lifetime_input, lifetime_output, lifetime_cached, lifetime_write, lifetime_reasoning): (
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = connection.query_row(
            "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cached_input_tokens), 0), COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(reasoning_output_tokens), 0) FROM usage_turns",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        let (session_input, session_output, session_cached, session_write, session_reasoning) =
            if let Some(run_id) = run_id {
                connection
                    .query_row(
                        "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                            COALESCE(SUM(cached_input_tokens), 0), COALESCE(SUM(cache_write_tokens), 0),
                            COALESCE(SUM(reasoning_output_tokens), 0)
                     FROM usage_turns WHERE run_id = ?1",
                        [run_id.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, i64>(4)?,
                            ))
                        },
                    )
                    .optional()?
                    .unwrap_or_default()
            } else {
                (0, 0, 0, 0, 0)
            };
        Ok(UsageTotals {
            session_input: session_input.max(0) as u64,
            session_output: session_output.max(0) as u64,
            session_cached_input: session_cached.max(0) as u64,
            session_cache_write: session_write.max(0) as u64,
            session_reasoning_output: session_reasoning.max(0) as u64,
            lifetime_input: lifetime_input.max(0) as u64,
            lifetime_output: lifetime_output.max(0) as u64,
            lifetime_cached_input: lifetime_cached.max(0) as u64,
            lifetime_cache_write: lifetime_write.max(0) as u64,
            lifetime_reasoning_output: lifetime_reasoning.max(0) as u64,
        })
    }

    pub fn deepseek_cost_totals(&self, run_id: Option<RunId>) -> Result<ProviderCostTotals, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT model, input_tokens, output_tokens, cached_input_tokens
             FROM usage_turns WHERE (?1 IS NULL OR run_id = ?1)",
        )?;
        let run_id = run_id.map(|id| id.to_string());
        let rows = statement.query_map([run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?.max(0) as u64,
                row.get::<_, i64>(2)?.max(0) as u64,
                row.get::<_, i64>(3)?.max(0) as u64,
            ))
        })?;
        let mut totals = ProviderCostTotals::default();
        for row in rows {
            let (model, input, output, cached_input) = row?;
            if !model.starts_with("deepseek/") && !model.starts_with("deepseek-") {
                continue;
            }
            let Some(pricing) = crate::deepseek::pricing_for_model(&model) else {
                totals.unpriced_turns = totals.unpriced_turns.saturating_add(1);
                continue;
            };
            totals.estimated_usd +=
                crate::deepseek::estimate_cost_usd(&model, input, cached_input, output).unwrap_or(0.0);
            totals.cache_savings_usd += cached_input.min(input) as f64
                * (pricing.cache_miss_input_per_million - pricing.cache_hit_input_per_million)
                / 1_000_000.0;
            totals.priced_turns = totals.priced_turns.saturating_add(1);
        }
        Ok(totals)
    }

    pub fn record_usage_turn(
        &self,
        run_id: RunId,
        agent_id: Option<EventAgentId>,
        model: &str,
        usage: crate::usage::TokenUsage,
        context_tokens: Option<u64>,
    ) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "INSERT INTO usage_turns
             (run_id, agent_id, model, input_tokens, output_tokens, cached_input_tokens,
              cache_write_tokens, reasoning_output_tokens, context_tokens, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                run_id.to_string(),
                agent_id.map(|id| id.to_string()),
                model,
                usage.input as i64,
                usage.output as i64,
                usage.cached_input as i64,
                usage.cache_write as i64,
                usage.reasoning_output as i64,
                context_tokens.map(|value| value as i64),
                Utc::now()
            ],
        )?;
        Ok(())
    }

    pub fn replace_tasks(&self, run_id: RunId, tasks: &[TaskRecord]) -> Result<(), StoreError> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM task_dependencies WHERE run_id = ?1",
            [run_id.to_string()],
        )?;
        transaction.execute("DELETE FROM tasks WHERE run_id = ?1", [run_id.to_string()])?;
        for task in tasks {
            transaction.execute(
                "INSERT INTO tasks
                 (run_id, task_id, objective, state, paths_json, assigned_agent_id, attempt,
                  max_attempts, generation, last_error, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    run_id.to_string(),
                    task.task_id,
                    task.objective,
                    task_state_name(task.state),
                    serde_json::to_string(&task.paths)?,
                    task.assigned_agent_id.map(|id| id.to_string()),
                    task.attempt,
                    task.max_attempts,
                    task.generation as i64,
                    task.last_error,
                    task.created_at,
                    task.updated_at
                ],
            )?;
        }
        for task in tasks {
            for prerequisite in &task.dependencies {
                transaction.execute(
                    "INSERT INTO task_dependencies (run_id, prerequisite_id, dependent_id)
                     VALUES (?1, ?2, ?3)",
                    params![run_id.to_string(), prerequisite, task.task_id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn tasks(&self, run_id: RunId) -> Result<Vec<TaskRecord>, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT task_id, objective, state, paths_json, assigned_agent_id, attempt,
                    max_attempts, generation, last_error, created_at, updated_at
             FROM tasks WHERE run_id = ?1 ORDER BY created_at, task_id",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, u32>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, DateTime<Utc>>(9)?,
                row.get::<_, DateTime<Utc>>(10)?,
            ))
        })?;
        let mut tasks = rows
            .map(|row| {
                let (
                    task_id,
                    objective,
                    state,
                    paths,
                    assigned_agent_id,
                    attempt,
                    max_attempts,
                    generation,
                    last_error,
                    created_at,
                    updated_at,
                ) = row?;
                Ok(TaskRecord {
                    run_id,
                    task_id,
                    objective,
                    state: parse_task_state(&state)?,
                    paths: serde_json::from_str(&paths)?,
                    dependencies: Vec::new(),
                    assigned_agent_id: assigned_agent_id.map(|id| id.parse()).transpose()?,
                    attempt,
                    max_attempts,
                    generation: generation.max(0) as u64,
                    last_error,
                    created_at,
                    updated_at,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let mut dependencies = connection
            .prepare("SELECT prerequisite_id, dependent_id FROM task_dependencies WHERE run_id = ?1")?;
        let dependencies = dependencies
            .query_map([run_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (prerequisite, dependent) in dependencies {
            if let Some(task) = tasks.iter_mut().find(|task| task.task_id == dependent) {
                task.dependencies.push(prerequisite);
            }
        }
        Ok(tasks)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_task(
        &self,
        run_id: RunId,
        task_id: &str,
        state: PlanTaskState,
        agent_id: Option<EventAgentId>,
        attempt: u32,
        generation: u64,
        last_error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "UPDATE tasks SET state = ?3, assigned_agent_id = ?4, attempt = ?5,
              generation = ?6, last_error = ?7, updated_at = ?8
             WHERE run_id = ?1 AND task_id = ?2",
            params![
                run_id.to_string(),
                task_id,
                task_state_name(state),
                agent_id.map(|id| id.to_string()),
                attempt,
                generation as i64,
                last_error,
                Utc::now()
            ],
        )?;
        Ok(())
    }

    pub fn upsert_agent(&self, agent: &AgentRecord) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "INSERT INTO agents
             (run_id, agent_id, parent_agent_id, role, model, state, task_id, attempt,
              generation, started_at, updated_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(run_id, agent_id) DO UPDATE SET state = excluded.state,
              task_id = excluded.task_id, attempt = excluded.attempt,
              generation = excluded.generation, updated_at = excluded.updated_at,
              finished_at = excluded.finished_at",
            params![
                agent.run_id.to_string(),
                agent.agent_id.to_string(),
                agent.parent_agent_id.map(|id| id.to_string()),
                agent.role,
                agent.model,
                agent_state_name(agent.state),
                agent.task_id,
                agent.attempt,
                agent.generation as i64,
                agent.started_at,
                agent.updated_at,
                agent.finished_at
            ],
        )?;
        Ok(())
    }

    pub fn agents(&self, run_id: RunId) -> Result<Vec<AgentRecord>, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT agent_id, parent_agent_id, role, model, state, task_id, attempt,
                    generation, started_at, updated_at, finished_at
             FROM agents WHERE run_id = ?1 ORDER BY started_at, agent_id",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, DateTime<Utc>>(8)?,
                row.get::<_, DateTime<Utc>>(9)?,
                row.get::<_, Option<DateTime<Utc>>>(10)?,
            ))
        })?;
        rows.map(|row| {
            let (
                agent_id,
                parent,
                role,
                model,
                state,
                task_id,
                attempt,
                generation,
                started_at,
                updated_at,
                finished_at,
            ) = row?;
            Ok(AgentRecord {
                run_id,
                agent_id: agent_id.parse()?,
                parent_agent_id: parent.map(|id| id.parse()).transpose()?,
                role,
                model,
                state: parse_agent_state(&state)?,
                task_id,
                attempt,
                generation: generation.max(0) as u64,
                started_at,
                updated_at,
                finished_at,
            })
        })
        .collect()
    }

    pub fn acquire_task_leases(
        &self,
        run_id: RunId,
        task_id: &str,
        agent_id: EventAgentId,
        generation: u64,
        resources: &[String],
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM leases WHERE expires_at <= ?1", [Utc::now()])?;
        for resource in resources {
            let held: Option<String> = transaction
                .query_row(
                    "SELECT task_id FROM leases WHERE run_id = ?1 AND resource = ?2",
                    params![run_id.to_string(), resource],
                    |row| row.get(0),
                )
                .optional()?;
            if held.is_some_and(|held| held != task_id) {
                return Err(StoreError::LeaseConflict(resource.clone()));
            }
        }
        for resource in resources {
            transaction.execute(
                "INSERT INTO leases (run_id, resource, task_id, agent_id, generation, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(run_id, resource) DO UPDATE SET task_id = excluded.task_id,
                  agent_id = excluded.agent_id, generation = excluded.generation,
                  expires_at = excluded.expires_at",
                params![
                    run_id.to_string(),
                    resource,
                    task_id,
                    agent_id.to_string(),
                    generation as i64,
                    expires_at
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn release_task_leases(
        &self,
        run_id: RunId,
        task_id: &str,
        agent_id: EventAgentId,
        generation: u64,
    ) -> Result<usize, StoreError> {
        Ok(self.connection.lock().execute(
            "DELETE FROM leases WHERE run_id = ?1 AND task_id = ?2 AND agent_id = ?3 AND generation = ?4",
            params![
                run_id.to_string(),
                task_id,
                agent_id.to_string(),
                generation as i64
            ],
        )?)
    }

    pub fn reclaim_expired_leases(&self) -> Result<usize, StoreError> {
        Ok(self
            .connection
            .lock()
            .execute("DELETE FROM leases WHERE expires_at <= ?1", [Utc::now()])?)
    }

    pub fn insert_board_entry(&self, entry: &BoardEntry) -> Result<(), StoreError> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO board_entries
             (entry_id, workspace_id, run_id, scope, kind, subject, body, task_id,
              author_agent_id, confidence, status, supersedes_id, evidence_json,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                entry.id,
                entry.workspace_id,
                entry.run_id.map(|id| id.to_string()),
                entry.scope.as_str(),
                entry.kind.as_str(),
                entry.subject,
                entry.body,
                entry.task_id,
                entry.author_agent_id.map(|id| id.to_string()),
                entry.confidence,
                entry.status.as_str(),
                entry.supersedes_id,
                serde_json::to_string(&entry.evidence)?,
                entry.created_at,
                entry.updated_at
            ],
        )?;
        transaction.execute(
            "INSERT INTO board_revisions
             (entry_id, revision, body, status, author_agent_id, created_at)
             VALUES (?1, 1, ?2, ?3, ?4, ?5)",
            params![
                entry.id,
                entry.body,
                entry.status.as_str(),
                entry.author_agent_id.map(|id| id.to_string()),
                entry.created_at
            ],
        )?;
        if let Some(supersedes) = &entry.supersedes_id {
            transaction.execute(
                "UPDATE board_entries SET status = 'superseded', updated_at = ?2 WHERE entry_id = ?1",
                params![supersedes, entry.created_at],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn board_entry(&self, id: &str) -> Result<Option<BoardEntry>, StoreError> {
        let row = self
            .connection
            .lock()
            .query_row(
                "SELECT entry_id, workspace_id, run_id, scope, kind, subject, body, task_id,
                        author_agent_id, confidence, status, supersedes_id, evidence_json,
                        created_at, updated_at
                 FROM board_entries WHERE entry_id = ?1",
                [id],
                raw_board_row,
            )
            .optional()?;
        row.map(decode_board_row).transpose()
    }

    pub fn board_entries(
        &self,
        workspace_id: &str,
        run_id: Option<RunId>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BoardEntry>, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT entry_id, workspace_id, run_id, scope, kind, subject, body, task_id,
                    author_agent_id, confidence, status, supersedes_id, evidence_json,
                    created_at, updated_at
             FROM board_entries
             WHERE workspace_id = ?1 AND status != 'superseded'
               AND (scope = 'project' OR run_id = ?2)
             ORDER BY CASE kind WHEN 'blocker' THEN 0 WHEN 'decision' THEN 1 ELSE 2 END,
                      updated_at DESC
             LIMIT 1000",
        )?;
        let rows = statement.query_map(
            params![workspace_id, run_id.map(|id| id.to_string())],
            raw_board_row,
        )?;
        let query = query
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let mut entries = Vec::new();
        for row in rows {
            let entry = decode_board_row(row?)?;
            if query.as_ref().is_none_or(|query| {
                format!("{} {}", entry.subject, entry.body)
                    .to_lowercase()
                    .contains(query)
            }) {
                entries.push(entry);
                if entries.len() >= limit.clamp(1, 200) {
                    break;
                }
            }
        }
        Ok(entries)
    }

    pub fn revise_board_entry(
        &self,
        id: &str,
        body: Option<&str>,
        status: Option<BoardStatus>,
        author_agent_id: Option<EventAgentId>,
    ) -> Result<Option<BoardEntry>, StoreError> {
        let Some(mut entry) = self.board_entry(id)? else {
            return Ok(None);
        };
        if let Some(body) = body {
            entry.body = body.to_owned();
        }
        if let Some(status) = status {
            entry.status = status;
        }
        entry.updated_at = Utc::now();
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(revision), 0) + 1 FROM board_revisions WHERE entry_id = ?1",
            [id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE board_entries SET body = ?2, status = ?3, updated_at = ?4 WHERE entry_id = ?1",
            params![id, entry.body, entry.status.as_str(), entry.updated_at],
        )?;
        transaction.execute(
            "INSERT INTO board_revisions
             (entry_id, revision, body, status, author_agent_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                revision,
                entry.body,
                entry.status.as_str(),
                author_agent_id.map(|agent| agent.to_string()),
                entry.updated_at
            ],
        )?;
        transaction.commit()?;
        Ok(Some(entry))
    }

    pub fn pin_board_entry(&self, id: &str) -> Result<Option<BoardEntry>, StoreError> {
        let Some(mut entry) = self.board_entry(id)? else {
            return Ok(None);
        };
        if !matches!(entry.kind, BoardKind::Decision | BoardKind::Constraint) {
            return Err(StoreError::Coordination(
                "only decisions and constraints can be pinned to project scope".into(),
            ));
        }
        entry.scope = BoardScope::Project;
        entry.updated_at = Utc::now();
        self.connection.lock().execute(
            "UPDATE board_entries SET scope = 'project', updated_at = ?2 WHERE entry_id = ?1",
            params![id, entry.updated_at],
        )?;
        Ok(Some(entry))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.events_tx.subscribe()
    }

    /// Broadcast a transient delta without forcing one SQLite transaction per
    /// provider token. The completed transcript item is persisted separately
    /// and is therefore authoritative during replay.
    pub fn publish_runtime_event(&self, run_id: RunId, event: RuntimeEvent) {
        let mut envelope = EventEnvelope::new(run_id, u64::MAX, event);
        envelope.sequence = u64::MAX;
        let _ = self.events_tx.send(envelope);
    }

    pub fn rename_run(&self, id: RunId, title: &str) -> Result<(), StoreError> {
        let title = title.trim();
        if title.is_empty() {
            return Ok(());
        }
        self.connection.lock().execute(
            "UPDATE runs SET title = ?2, updated_at = ?3 WHERE run_id = ?1",
            params![id.to_string(), title, Utc::now()],
        )?;
        self.record_runtime_event(id, RuntimeEvent::SessionRenamed { title: title.into() })?;
        Ok(())
    }

    pub fn archive_run(&self, id: RunId) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "UPDATE runs SET archived = 1, updated_at = ?2 WHERE run_id = ?1",
            params![id.to_string(), Utc::now()],
        )?;
        self.record_runtime_event(id, RuntimeEvent::SessionArchived)?;
        Ok(())
    }

    pub fn fork_run(&self, source: RunId) -> Result<RunRecord, StoreError> {
        let source_run = self.run(source)?.ok_or_else(|| {
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session not found",
            ))
        })?;
        let mut fork = self.create_run(&source_run.goal, source_run.mode)?;
        fork.title = format!("{} (fork)", source_run.title);
        fork.parent_run_id = Some(source);
        self.connection.lock().execute(
            "UPDATE runs SET title = ?2, parent_run_id = ?3 WHERE run_id = ?1",
            params![fork.id.to_string(), fork.title, source.to_string()],
        )?;
        if let Some(workspace_id) = self.workspace_for_run(source)? {
            self.attach_run_workspace(fork.id, &workspace_id)?;
            for mut entry in self
                .board_entries(&workspace_id, Some(source), None, 200)?
                .into_iter()
                .filter(|entry| entry.scope == BoardScope::Session && entry.run_id == Some(source))
            {
                entry.id = uuid::Uuid::now_v7().to_string();
                entry.run_id = Some(fork.id);
                entry.author_agent_id = None;
                entry.created_at = Utc::now();
                entry.updated_at = entry.created_at;
                self.insert_board_entry(&entry)?;
            }
        }
        let forked_tasks = self
            .tasks(source)?
            .into_iter()
            .map(|mut task| {
                task.run_id = fork.id;
                task.state = PlanTaskState::Pending;
                task.assigned_agent_id = None;
                task.attempt = 0;
                task.generation = 0;
                task.last_error = None;
                task.created_at = Utc::now();
                task.updated_at = task.created_at;
                task
            })
            .collect::<Vec<_>>();
        if !forked_tasks.is_empty() {
            self.replace_tasks(fork.id, &forked_tasks)?;
        }
        for message in self.messages(source)? {
            self.append_message(fork.id, &message.role, &message.content, message.compacted)?;
        }
        if let Some(clarification) = self.issue_clarification(source)? {
            self.save_issue_clarification(fork.id, &clarification)?;
        }
        self.record_runtime_event(
            fork.id,
            RuntimeEvent::SessionStarted {
                kind: "fork".into(),
                goal: source_run.goal.clone(),
            },
        )?;
        for event in self.events(source)? {
            if matches!(
                &event.event,
                RuntimeEvent::SessionStarted { .. }
                    | RuntimeEvent::SessionForked { .. }
                    | RuntimeEvent::SessionArchived
                    | RuntimeEvent::SessionRenamed { .. }
            ) {
                continue;
            }
            self.record_runtime_event(fork.id, event.event)?;
        }
        self.record_runtime_event(fork.id, RuntimeEvent::SessionForked { source })?;
        self.record_runtime_event(
            fork.id,
            RuntimeEvent::SessionState {
                state: ExitState::Pending,
            },
        )?;
        Ok(fork)
    }

    pub fn update_run_state(
        &self,
        id: RunId,
        state: ExitState,
        model: Option<&str>,
        summary: Option<&str>,
        question: Option<&str>,
    ) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "UPDATE runs SET state = ?2, model = COALESCE(?3, model), summary = COALESCE(?4, summary), pending_question = ?5, updated_at = ?6 WHERE run_id = ?1",
            params![id.to_string(), state_name(state), model, summary, question, Utc::now()],
        )?;
        Ok(())
    }

    pub fn add_usage(&self, id: RunId, input: u64, output: u64) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "UPDATE runs SET input_tokens = input_tokens + ?2, output_tokens = output_tokens + ?3, updated_at = ?4 WHERE run_id = ?1",
            params![id.to_string(), input as i64, output as i64, Utc::now()],
        )?;
        Ok(())
    }

    pub fn append_message(
        &self,
        run_id: RunId,
        role: &str,
        content: &serde_json::Value,
        compacted: bool,
    ) -> Result<u64, StoreError> {
        let connection = self.connection.lock();
        let next: i64 = connection.query_row(
            "SELECT COALESCE(MAX(sequence), -1) + 1 FROM messages WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        connection.execute(
            "INSERT INTO messages (run_id, sequence, role, content, compacted, occurred_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id.to_string(),
                next,
                role,
                serde_json::to_string(content)?,
                compacted,
                Utc::now()
            ],
        )?;
        Ok(next as u64)
    }

    pub fn messages(&self, run_id: RunId) -> Result<Vec<StoredMessage>, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT sequence, role, content, compacted, occurred_at FROM messages WHERE run_id = ?1 ORDER BY sequence",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, DateTime<Utc>>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (sequence, role, content, compacted, occurred_at) = row?;
            Ok(StoredMessage {
                run_id,
                sequence: sequence as u64,
                role,
                content: serde_json::from_str(&content)?,
                compacted,
                occurred_at,
            })
        })
        .collect()
    }

    pub fn append_event(&self, event: &EventEnvelope) -> Result<(), StoreError> {
        self.connection.lock().execute(
            "INSERT INTO events (run_id, sequence, protocol_version, turn_id, kind, payload, occurred_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                event.run_id.to_string(),
                event.sequence as i64,
                event.protocol_version,
                event.turn_id.map(|id| id.to_string()),
                event.kind(),
                serde_json::to_string(&event.event)?,
                event.occurred_at
            ],
        )?;
        let _ = self.events_tx.send(event.clone());
        Ok(())
    }

    pub fn record_event(
        &self,
        run_id: RunId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<EventEnvelope, StoreError> {
        self.record_runtime_event(
            run_id,
            RuntimeEvent::Legacy {
                kind: kind.to_owned(),
                payload,
            },
        )
    }

    pub fn record_runtime_event(
        &self,
        run_id: RunId,
        event: RuntimeEvent,
    ) -> Result<EventEnvelope, StoreError> {
        let connection = self.connection.lock();
        let next: i64 = connection.query_row(
            "SELECT COALESCE(MAX(sequence), -1) + 1 FROM events WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        let event = EventEnvelope::new(run_id, next as u64, event);
        connection.execute(
            "INSERT INTO events (run_id, sequence, protocol_version, turn_id, kind, payload, occurred_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                event.run_id.to_string(),
                event.sequence as i64,
                event.protocol_version,
                event.turn_id.map(|id| id.to_string()),
                event.kind(),
                serde_json::to_string(&event.event)?,
                event.occurred_at
            ],
        )?;
        drop(connection);
        let _ = self.events_tx.send(event.clone());
        Ok(event)
    }

    pub fn event(&self, run_id: RunId, sequence: u64) -> Result<Option<EventEnvelope>, StoreError> {
        let result = self
            .connection
            .lock()
            .query_row(
                "SELECT protocol_version, turn_id, kind, payload, occurred_at FROM events WHERE run_id = ?1 AND sequence = ?2",
                params![run_id.to_string(), sequence as i64],
                |row| {
                    Ok((
                        row.get::<_, u16>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        result
            .map(|(protocol_version, turn_id, kind, payload, occurred_at)| {
                let event = decode_event(protocol_version, &kind, &payload)?;
                Ok(EventEnvelope {
                    protocol_version,
                    run_id,
                    turn_id: turn_id.map(|id| id.parse()).transpose()?,
                    sequence,
                    event,
                    occurred_at,
                })
            })
            .transpose()
    }

    pub fn events(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT sequence, protocol_version, turn_id, kind, payload, occurred_at FROM events WHERE run_id = ?1 ORDER BY sequence",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, u16>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, DateTime<Utc>>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (sequence, protocol_version, turn_id, kind, payload, occurred_at) = row?;
            Ok(EventEnvelope {
                protocol_version,
                run_id,
                turn_id: turn_id.map(|id| id.parse()).transpose()?,
                sequence: sequence as u64,
                event: decode_event(protocol_version, &kind, &payload)?,
                occurred_at,
            })
        })
        .collect()
    }
}

fn archive_prototype_database(path: &Path) -> Result<Option<PathBuf>, StoreError> {
    if !path.is_file() {
        return Ok(None);
    }
    let connection = Connection::open(path)?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    drop(connection);
    if version == 0 || version == SCHEMA_VERSION {
        return Ok(None);
    }
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("minha.sqlite3");
    let archive = path.with_file_name(format!("{file_name}.prototype-v{version}-{timestamp}.bak"));
    std::fs::rename(path, &archive)?;
    for suffix in ["-wal", "-shm"] {
        let companion = PathBuf::from(format!("{}{suffix}", path.display()));
        if companion.is_file() {
            let archived = PathBuf::from(format!("{}{suffix}", archive.display()));
            std::fs::rename(companion, archived)?;
        }
    }
    Ok(Some(archive))
}

type RawRunRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    DateTime<Utc>,
    DateTime<Utc>,
    i64,
    i64,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
);

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRunRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    ))
}

fn decode_run(row: RawRunRow) -> Result<RunRecord, StoreError> {
    let (
        id,
        title,
        goal,
        mode,
        state,
        model,
        created_at,
        updated_at,
        input,
        output,
        summary,
        question,
        archived,
        parent_run_id,
    ) = row;
    Ok(RunRecord {
        id: RunId::from_str(&id)?,
        title,
        goal,
        mode: Mode::from_str(&mode).map_err(StoreError::Mode)?,
        state: parse_state(&state)?,
        model,
        created_at,
        updated_at,
        input_tokens: input.max(0) as u64,
        output_tokens: output.max(0) as u64,
        summary,
        pending_question: question,
        archived,
        parent_run_id: parent_run_id.map(|id| id.parse()).transpose()?,
    })
}

fn decode_event(protocol_version: u16, kind: &str, payload: &str) -> Result<RuntimeEvent, StoreError> {
    if protocol_version >= MIN_TYPED_PROTOCOL_VERSION {
        Ok(serde_json::from_str(payload)?)
    } else {
        Ok(RuntimeEvent::Legacy {
            kind: kind.to_owned(),
            payload: serde_json::from_str(payload)?,
        })
    }
}

type RawBoardRow = (
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    String,
    Option<String>,
    String,
    DateTime<Utc>,
    DateTime<Utc>,
);

fn raw_board_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawBoardRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
    ))
}

fn decode_board_row(row: RawBoardRow) -> Result<BoardEntry, StoreError> {
    let (
        id,
        workspace_id,
        run_id,
        scope,
        kind,
        subject,
        body,
        task_id,
        author_agent_id,
        confidence,
        status,
        supersedes_id,
        evidence,
        created_at,
        updated_at,
    ) = row;
    Ok(BoardEntry {
        id,
        workspace_id,
        run_id: run_id.map(|id| id.parse()).transpose()?,
        scope: parse_board_scope(&scope)?,
        kind: parse_board_kind(&kind)?,
        subject,
        body,
        task_id,
        author_agent_id: author_agent_id.map(|id| id.parse()).transpose()?,
        confidence: confidence.clamp(0, 100) as u8,
        status: parse_board_status(&status)?,
        supersedes_id,
        evidence: serde_json::from_str(&evidence)?,
        created_at,
        updated_at,
    })
}

fn workspace_id(root: &str) -> String {
    let digest = Sha256::digest(root.as_bytes());
    let mut id = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        let _ = write!(&mut id, "{byte:02x}");
    }
    id
}

fn cache_class_name(class: CacheClass) -> &'static str {
    match class {
        CacheClass::Exact => "exact",
        CacheClass::Ttl => "ttl",
        CacheClass::Never => "never",
    }
}

fn clarification_status_name(status: crate::protocol::ClarificationStatus) -> &'static str {
    match status {
        crate::protocol::ClarificationStatus::Collecting => "collecting",
        crate::protocol::ClarificationStatus::Reviewing => "reviewing",
        crate::protocol::ClarificationStatus::Confirmed => "confirmed",
        crate::protocol::ClarificationStatus::Cancelled => "cancelled",
    }
}

fn incident_severity_name(severity: IncidentSeverity) -> &'static str {
    match severity {
        IncidentSeverity::Info => "info",
        IncidentSeverity::Warning => "warning",
        IncidentSeverity::Error => "error",
        IncidentSeverity::Critical => "critical",
    }
}

fn task_state_name(state: PlanTaskState) -> &'static str {
    match state {
        PlanTaskState::Pending => "pending",
        PlanTaskState::Running => "running",
        PlanTaskState::Completed => "completed",
        PlanTaskState::Blocked => "blocked",
        PlanTaskState::Failed => "failed",
    }
}

const fn todo_state_name(state: TodoState) -> &'static str {
    match state {
        TodoState::Pending => "pending",
        TodoState::InProgress => "in_progress",
        TodoState::Completed => "completed",
        TodoState::Blocked => "blocked",
        TodoState::Dropped => "dropped",
    }
}

fn todo_state_from_name(value: &str) -> TodoState {
    match value {
        "in_progress" => TodoState::InProgress,
        "completed" => TodoState::Completed,
        "blocked" => TodoState::Blocked,
        "dropped" => TodoState::Dropped,
        _ => TodoState::Pending,
    }
}

fn memory_scope_from_name(value: &str) -> MemoryScope {
    match value {
        "project" => MemoryScope::Project,
        "run" => MemoryScope::Run,
        _ => MemoryScope::User,
    }
}

fn memory_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
    let stored_run: Option<String> = row.get(2)?;
    let scope: String = row.get(3)?;
    let provenance_json: String = row.get(9)?;
    let entities_json: String = row.get(10)?;
    Ok(MemoryRecord {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        run_id: stored_run.and_then(|value| RunId::from_str(&value).ok()),
        scope: memory_scope_from_name(&scope),
        kind: row.get(4)?,
        subject: row.get(5)?,
        body: row.get(6)?,
        confidence: row.get::<_, i64>(7)?.clamp(0, 100) as u8,
        salience: row.get::<_, i64>(8)?.clamp(0, 100) as u8,
        provenance: serde_json::from_str(&provenance_json).unwrap_or_default(),
        entities: serde_json::from_str(&entities_json).unwrap_or_default(),
        valid_from: row.get(11)?,
        valid_until: row.get(12)?,
        access_count: row.get::<_, i64>(13)?.max(0) as u64,
        pinned: row.get::<_, i64>(14)? != 0,
        supersedes_id: row.get(15)?,
        tombstone: row.get::<_, i64>(16)? != 0,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
    })
}

fn parse_task_state(value: &str) -> Result<PlanTaskState, StoreError> {
    match value {
        "pending" => Ok(PlanTaskState::Pending),
        "running" => Ok(PlanTaskState::Running),
        "completed" => Ok(PlanTaskState::Completed),
        "blocked" => Ok(PlanTaskState::Blocked),
        "failed" => Ok(PlanTaskState::Failed),
        other => Err(StoreError::Coordination(format!("unknown task state {other}"))),
    }
}

fn agent_state_name(state: AgentState) -> &'static str {
    match state {
        AgentState::Starting => "starting",
        AgentState::Inspecting => "inspecting",
        AgentState::Planning => "planning",
        AgentState::Working => "working",
        AgentState::Waiting => "waiting",
        AgentState::Integrating => "integrating",
        AgentState::Verifying => "verifying",
        AgentState::Completed => "completed",
        AgentState::Failed => "failed",
        AgentState::Cancelled => "cancelled",
    }
}

fn parse_agent_state(value: &str) -> Result<AgentState, StoreError> {
    match value {
        "starting" => Ok(AgentState::Starting),
        "inspecting" => Ok(AgentState::Inspecting),
        "planning" => Ok(AgentState::Planning),
        "working" => Ok(AgentState::Working),
        "waiting" => Ok(AgentState::Waiting),
        "integrating" => Ok(AgentState::Integrating),
        "verifying" => Ok(AgentState::Verifying),
        "completed" => Ok(AgentState::Completed),
        "failed" => Ok(AgentState::Failed),
        "cancelled" => Ok(AgentState::Cancelled),
        other => Err(StoreError::Coordination(format!("unknown agent state {other}"))),
    }
}

fn parse_board_scope(value: &str) -> Result<BoardScope, StoreError> {
    match value {
        "session" => Ok(BoardScope::Session),
        "project" => Ok(BoardScope::Project),
        other => Err(StoreError::Coordination(format!("unknown board scope {other}"))),
    }
}

fn parse_board_kind(value: &str) -> Result<BoardKind, StoreError> {
    match value {
        "decision" => Ok(BoardKind::Decision),
        "constraint" => Ok(BoardKind::Constraint),
        "finding" => Ok(BoardKind::Finding),
        "blocker" => Ok(BoardKind::Blocker),
        "artifact" => Ok(BoardKind::Artifact),
        "progress" => Ok(BoardKind::Progress),
        other => Err(StoreError::Coordination(format!("unknown board kind {other}"))),
    }
}

fn parse_board_status(value: &str) -> Result<BoardStatus, StoreError> {
    match value {
        "open" => Ok(BoardStatus::Open),
        "resolved" => Ok(BoardStatus::Resolved),
        "superseded" => Ok(BoardStatus::Superseded),
        other => Err(StoreError::Coordination(format!("unknown board status {other}"))),
    }
}

fn session_title(goal: &str) -> String {
    let title = goal.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        "New session".into()
    } else if title.chars().count() > 72 {
        title.chars().take(69).collect::<String>() + "..."
    } else {
        title
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Interactive => "interactive",
        Mode::Batch => "batch",
        Mode::Review => "review",
    }
}

pub fn state_name(state: ExitState) -> &'static str {
    match state {
        ExitState::Pending => "pending",
        ExitState::Running => "running",
        ExitState::Succeeded => "succeeded",
        ExitState::Failed => "failed",
        ExitState::Cancelled => "cancelled",
        ExitState::Blocked => "blocked",
        ExitState::Inconclusive => "inconclusive",
        ExitState::NeedsInput => "needs_input",
        ExitState::UsagePaused => "usage_paused",
        ExitState::ApprovalRequired => "approval_required",
        ExitState::AuthUnavailable => "auth_unavailable",
        ExitState::ModelUnavailable => "model_unavailable",
    }
}

fn parse_state(value: &str) -> Result<ExitState, StoreError> {
    Ok(match value {
        "pending" => ExitState::Pending,
        "running" => ExitState::Running,
        "succeeded" => ExitState::Succeeded,
        "failed" => ExitState::Failed,
        "cancelled" => ExitState::Cancelled,
        "blocked" => ExitState::Blocked,
        "inconclusive" => ExitState::Inconclusive,
        "needs_input" => ExitState::NeedsInput,
        "usage_paused" => ExitState::UsagePaused,
        "approval_required" => ExitState::ApprovalRequired,
        "auth_unavailable" => ExitState::AuthUnavailable,
        "model_unavailable" => ExitState::ModelUnavailable,
        other => return Err(StoreError::State(other.to_owned())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn migration_enables_wal_and_is_idempotent() {
        let dir = tempdir().expect("test operation should succeed");
        let path = dir.path().join("state.sqlite3");
        let store = Store::open(&path).expect("test operation should succeed");
        assert_eq!(
            store.journal_mode().expect("test operation should succeed"),
            "wal"
        );
        assert_eq!(
            store.schema_version().expect("test operation should succeed"),
            SCHEMA_VERSION
        );
        store.migrate().expect("test operation should succeed");
    }

    #[test]
    fn provider_balance_high_water_is_durable_and_monotonic() {
        let dir = tempdir().expect("temporary directory");
        let path = dir.path().join("balance.sqlite3");
        let store = Store::open(&path).expect("store");
        assert_eq!(
            store
                .update_provider_balance_high_water("workspace", "deepseek", "USD", 10.0)
                .expect("first balance"),
            10.0
        );
        assert_eq!(
            store
                .update_provider_balance_high_water("workspace", "deepseek", "USD", 4.0)
                .expect("lower balance"),
            10.0
        );
        drop(store);
        let reopened = Store::open(&path).expect("reopened store");
        assert_eq!(
            reopened
                .update_provider_balance_high_water("workspace", "deepseek", "USD", 12.0)
                .expect("higher balance"),
            12.0
        );
    }

    #[test]
    fn v2_prototype_data_is_discarded_before_current_migration() {
        let dir = tempdir().expect("test operation should succeed");
        let path = dir.path().join("legacy.sqlite3");
        let connection = Connection::open(&path).expect("test operation should succeed");
        connection
            .execute_batch(
                "CREATE TABLE runs (run_id TEXT PRIMARY KEY, goal TEXT);
                 INSERT INTO runs VALUES ('old', 'prototype');
                 PRAGMA user_version = 2;",
            )
            .expect("test operation should succeed");
        drop(connection);
        let store = Store::open(&path).expect("test operation should succeed");
        assert_eq!(
            store.schema_version().expect("test operation should succeed"),
            SCHEMA_VERSION
        );
        assert!(
            store
                .list_runs(10)
                .expect("test operation should succeed")
                .is_empty()
        );
    }

    #[test]
    fn pre_v1_prototype_session_data_is_archived_before_clean_reset() {
        let dir = tempdir().expect("test operation should succeed");
        let path = dir.path().join("v3.sqlite3");
        let connection = Connection::open(&path).expect("test operation should succeed");
        let run_id = RunId::new();
        let now = Utc::now();
        connection
            .execute_batch(
                "CREATE TABLE runs (
                   run_id TEXT PRIMARY KEY,
                   title TEXT NOT NULL,
                   goal TEXT NOT NULL,
                   mode TEXT NOT NULL,
                   state TEXT NOT NULL,
                   model TEXT,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   input_tokens INTEGER NOT NULL DEFAULT 0,
                   output_tokens INTEGER NOT NULL DEFAULT 0,
                   summary TEXT,
                   pending_question TEXT,
                   archived INTEGER NOT NULL DEFAULT 0,
                   parent_run_id TEXT
                 );
                 PRAGMA user_version = 3;",
            )
            .expect("test operation should succeed");
        connection
            .execute(
                "INSERT INTO runs
                 (run_id, title, goal, mode, state, created_at, updated_at)
                 VALUES (?1, 'kept', 'preserve me', 'interactive', 'pending', ?2, ?2)",
                params![run_id.to_string(), now],
            )
            .expect("test operation should succeed");
        drop(connection);

        let store = Store::open(&path).expect("test operation should succeed");
        assert_eq!(
            store.schema_version().expect("test operation should succeed"),
            SCHEMA_VERSION
        );
        assert!(store.run(run_id).expect("run lookup").is_none());
        let archived = std::fs::read_dir(dir.path())
            .expect("archive directory")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("prototype-v3-"));
        assert!(archived);
    }

    #[test]
    fn v5_database_adds_durable_cache_metrics() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("v5.sqlite3");
        let connection = Connection::open(&path).expect("v5 database");
        connection
            .execute_batch("PRAGMA user_version = 5;")
            .expect("v5 marker");
        drop(connection);
        let store = Store::open(&path).expect("migrated store");
        assert_eq!(store.schema_version().expect("schema version"), SCHEMA_VERSION);
        let table: String = store
            .connection
            .lock()
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'cache_stats'",
                [],
                |row| row.get(0),
            )
            .expect("cache_stats table");
        assert_eq!(table, "cache_stats");
    }

    #[test]
    fn v6_database_adds_durable_issue_intakes() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("v6.sqlite3");
        let connection = Connection::open(&path).expect("v6 database");
        connection
            .execute_batch("PRAGMA user_version = 6;")
            .expect("v6 marker");
        drop(connection);

        let store = Store::open(&path).expect("migrated store");
        assert_eq!(store.schema_version().expect("schema version"), SCHEMA_VERSION);
        let table: String = store
            .connection
            .lock()
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'issue_intakes'",
                [],
                |row| row.get(0),
            )
            .expect("issue_intakes table");
        assert_eq!(table, "issue_intakes");
    }

    #[test]
    fn v8_adds_durable_agent_todos_and_memory_indexes() {
        let store = Store::in_memory().expect("in-memory store");
        let run = store.create_run("todo test", Mode::Batch).expect("run");
        let agent = EventAgentId::new();
        let first = store
            .upsert_todo(
                run.id,
                agent,
                TodoItem {
                    id: "t1".into(),
                    objective: "inspect parser".into(),
                    state: TodoState::Pending,
                    order: 0,
                    blocker: None,
                    evidence: vec![],
                    revision: 0,
                },
            )
            .expect("first revision");
        assert_eq!(first.revision, 1);
        let second = store
            .upsert_todo(
                run.id,
                agent,
                TodoItem {
                    state: TodoState::Completed,
                    evidence: vec!["test:ok".into()],
                    ..first
                },
            )
            .expect("second revision");
        assert_eq!(second.revision, 2);
        assert_eq!(store.todos(run.id, agent).expect("todos"), vec![second]);
        let rollup = store.todo_rollup_details(run.id, 3).expect("todo details");
        assert!(rollup.active_goals.is_empty());
        assert!(rollup.blocked_work.is_empty());
        assert_eq!(rollup.recently_completed, vec!["inspect parser"]);
        let fts: String = store
            .connection
            .lock()
            .query_row(
                "SELECT name FROM sqlite_master WHERE name = 'memories_fts'",
                [],
                |row| row.get(0),
            )
            .expect("memory fts table");
        assert_eq!(fts, "memories_fts");
    }

    #[test]
    fn v8_prototype_office_messages_are_archived_before_clean_reset() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("v8-office.sqlite3");
        let connection = Connection::open(&path).expect("v8 database");
        let run_id = RunId::new();
        connection
            .execute_batch(
                "CREATE TABLE runs (run_id TEXT PRIMARY KEY);
                 CREATE TABLE hive_messages (
                   message_id TEXT PRIMARY KEY,
                   run_id TEXT NOT NULL,
                   room_id TEXT NOT NULL,
                   sender_id TEXT NOT NULL,
                   recipient TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   payload_json TEXT NOT NULL,
                   occurred_at TEXT NOT NULL,
                   expires_at TEXT
                 );
                 PRAGMA user_version = 8;",
            )
            .expect("v8 schema");
        connection
            .execute("INSERT INTO runs (run_id) VALUES (?1)", [run_id.to_string()])
            .expect("legacy run");
        connection
            .execute(
                "INSERT INTO hive_messages
                 (message_id, run_id, room_id, sender_id, recipient, kind, payload_json, occurred_at)
                 VALUES ('legacy-message', ?1, 'run', 'agent:a', 'manager', 'finding', ?2, ?3)",
                params![
                    run_id.to_string(),
                    json!({"body":"legacy finding"}).to_string(),
                    Utc::now()
                ],
            )
            .expect("legacy message");
        drop(connection);

        let store = Store::open(&path).expect("migrated store");
        assert_eq!(store.schema_version().expect("schema version"), SCHEMA_VERSION);
        let messages = store
            .office_room_messages(run_id, "run", "tui", 20)
            .expect("migrated room messages");
        assert!(messages.is_empty());
        let archived = std::fs::read_dir(directory.path())
            .expect("archive directory")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("prototype-v8-"));
        assert!(archived);
    }

    #[test]
    fn memories_are_secret_safe_scoped_ranked_and_supersedable() {
        let store = Store::in_memory().expect("in-memory store");
        let project = "project-a";
        let run = store.create_run("memory test", Mode::Batch).expect("run");

        let mut user = MemoryRecord::candidate(
            MemoryScope::User,
            "preference",
            "Rust validation",
            "Run clippy with warnings denied before completion.",
        );
        user.pinned = true;
        user.entities = vec!["clippy".into(), "Rust".into()];
        let user = store.put_memory(user).expect("user memory");

        let mut project_memory = MemoryRecord::candidate(
            MemoryScope::Project,
            "constraint",
            "Rust validation",
            "This project requires workspace clippy checks.",
        );
        project_memory.workspace_id = Some(project.into());
        project_memory.entities = vec!["clippy".into()];
        let project_memory = store.put_memory(project_memory).expect("project memory");

        let mut other_project = MemoryRecord::candidate(
            MemoryScope::Project,
            "constraint",
            "Rust validation elsewhere",
            "A different workspace also mentions clippy.",
        );
        other_project.workspace_id = Some("project-b".into());
        store.put_memory(other_project).expect("other project memory");

        let mut episodic = MemoryRecord::candidate(
            MemoryScope::Run,
            "outcome",
            "Clippy incident",
            "The current run repaired a clippy diagnostic.",
        );
        episodic.run_id = Some(run.id);
        episodic.entities = vec!["clippy".into()];
        store.put_memory(episodic).expect("run memory");

        let hits = store
            .search_memories(project, Some(run.id), "clippy", 10)
            .expect("search memories");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].memory.id, user.id, "pinned memories rank first");
        assert!(hits.iter().all(|hit| hit.memory.access_count == 1));
        assert!(hits.iter().any(|hit| hit.memory.id == project_memory.id));
        assert!(
            hits.iter()
                .all(|hit| hit.memory.workspace_id.as_deref() != Some("project-b"))
        );

        let mut replacement = MemoryRecord::candidate(
            MemoryScope::Project,
            "constraint",
            "Rust validation",
            "This project requires format, test, and clippy checks.",
        );
        replacement.workspace_id = Some(project.into());
        replacement.supersedes_id = Some(project_memory.id.clone());
        let replacement = store.put_memory(replacement).expect("replacement");
        let hits = store
            .search_memories(project, Some(run.id), "validation", 10)
            .expect("search replacement");
        assert!(hits.iter().any(|hit| hit.memory.id == replacement.id));
        assert!(hits.iter().all(|hit| hit.memory.id != project_memory.id));

        assert!(
            store
                .set_memory_state(&replacement.id, None, Some(true))
                .expect("delete")
        );
        assert!(
            store
                .search_memories(project, Some(run.id), "validation", 10)
                .expect("search deleted")
                .iter()
                .all(|hit| hit.memory.id != replacement.id)
        );

        let secret = MemoryRecord::candidate(
            MemoryScope::User,
            "credential",
            "API token",
            "api_key = test-secret-placeholder",
        );
        assert!(matches!(store.put_memory(secret), Err(StoreError::Memory(_))));
    }

    #[test]
    fn memory_settings_and_extraction_queue_are_durable() {
        let store = Store::in_memory().expect("in-memory store");
        let workspace = "memory-settings";
        assert_eq!(
            store.memory_settings(workspace).expect("defaults"),
            MemorySettings::default()
        );

        let settings = MemorySettings {
            enabled: true,
            use_memory: false,
            generate: false,
        };
        store
            .set_memory_settings(workspace, settings)
            .expect("save settings");
        assert_eq!(
            store.memory_settings(workspace).expect("saved settings"),
            settings
        );

        let run = store.create_run("extract memory", Mode::Batch).expect("run");
        store.queue_memory_extraction(run.id).expect("queue extraction");
        store
            .queue_memory_extraction(run.id)
            .expect("deduplicate extraction");
        assert_eq!(
            store.pending_memory_extractions(20).expect("pending"),
            vec![run.id]
        );
        store
            .finish_memory_extraction(run.id, "completed")
            .expect("finish extraction");
        assert!(
            store
                .pending_memory_extractions(20)
                .expect("finished queue")
                .is_empty()
        );
    }

    #[test]
    fn hive_messages_are_typed_by_callers_deduplicated_and_consumed_once() {
        let store = Store::in_memory().expect("in-memory store");
        let run = store.create_run("hive test", Mode::Batch).expect("run");
        let payload = json!({
            "body":"Parser uses the wrong bound",
            "task_id":"parser",
            "refs":["artifact:abc"]
        });
        let first = store
            .insert_hive_message(
                run.id,
                "message-1",
                "run",
                "agent:a",
                "agent:b",
                "finding",
                &payload,
                None,
            )
            .expect("first message");
        let duplicate = store
            .insert_hive_message(
                run.id,
                "message-2",
                "run",
                "agent:a",
                "agent:b",
                "finding",
                &payload,
                None,
            )
            .expect("duplicate message");
        assert_eq!(first, duplicate);
        assert_eq!(
            store
                .hive_inbox(run.id, "agent:b", 20)
                .expect("first inbox")
                .len(),
            1
        );
        assert!(
            store
                .hive_inbox(run.id, "agent:b", 20)
                .expect("consumed inbox")
                .is_empty()
        );
        assert_eq!(
            store
                .office_room_messages(run.id, "run", "tui", 20)
                .expect("independent TUI cursor")
                .len(),
            1
        );
        assert!(
            store
                .office_room_messages(run.id, "run", "tui", 20)
                .expect("advanced TUI cursor")
                .is_empty()
        );
        assert_eq!(
            store
                .office_room_messages(run.id, "run", "auditor", 20)
                .expect("independent auditor cursor")
                .len(),
            1
        );
        store
            .close_office_room(run.id, "run", "coordination complete")
            .expect("close room");
    }

    #[test]
    fn issue_clarification_round_trips_and_survives_forking() {
        let store = Store::in_memory().expect("in-memory store");
        let run = store
            .create_run("it doesn't work", Mode::Interactive)
            .expect("source run");
        let mut clarification = crate::clarify::analyze(&run.goal, "auto");
        clarification.pending_batch = Some(crate::clarify::make_fallback_batch(&clarification));
        store
            .save_issue_clarification(run.id, &clarification)
            .expect("saved issue clarification");

        assert_eq!(
            store
                .issue_clarification(run.id)
                .expect("loaded issue clarification"),
            Some(clarification.clone())
        );

        let fork = store.fork_run(run.id).expect("forked run");
        assert_eq!(
            store
                .issue_clarification(fork.id)
                .expect("forked issue clarification"),
            Some(clarification)
        );
    }

    #[test]
    fn cache_and_incident_state_round_trip() {
        let directory = tempdir().expect("temporary directory");
        let store = Store::in_memory().expect("in-memory store");
        let workspace = store
            .ensure_workspace(directory.path())
            .expect("workspace record");
        let run = store
            .create_run("exercise durable runtime state", Mode::Batch)
            .expect("run record");
        store
            .attach_run_workspace(run.id, &workspace.id)
            .expect("workspace attachment");
        store
            .append_message(run.id, "user", &json!({"text": "large context"}), false)
            .expect("message before compaction");
        let manifest =
            ObservedInputManifest::new([crate::cache::ObservedInput::new("request", b"stable input")]);
        let key = crate::cache::cache_key("test/v1", b"request", &manifest);
        assert!(
            store
                .put_cached_result(
                    &workspace.id,
                    &key,
                    CacheClass::Exact,
                    b"cached output",
                    &manifest,
                    None,
                )
                .expect("cache write")
        );
        let cached = store
            .cached_result(&workspace.id, &key)
            .expect("cache read")
            .expect("cache hit");
        assert_eq!(cached.value, b"cached output");
        assert_eq!(store.cache_totals(&workspace.id).expect("cache totals").hits, 1);
        assert!(
            store
                .touch_cached_result(&workspace.id, &key, cached.value.len() as u64)
                .expect("hot-tier touch")
        );
        let totals = store
            .cache_totals(&workspace.id)
            .expect("cache totals after hot hit");
        assert_eq!(totals.hits, 2);
        assert_eq!(totals.bytes_read, (cached.value.len() * 2) as u64);
        let checkpoint = store
            .record_compaction_checkpoint(run.id, "durable summary", &manifest, 42_000)
            .expect("compaction checkpoint");
        assert!(!checkpoint.is_empty());
        assert!(
            store
                .messages(run.id)
                .expect("compacted messages")
                .iter()
                .all(|message| message.compacted)
        );

        let incident = IncidentView {
            code: "provider.transport".into(),
            severity: IncidentSeverity::Warning,
            category: "network".into(),
            summary: "temporary connection failure".into(),
            retryable: true,
            correlation_id: "test-correlation".into(),
            actions: vec!["retry".into()],
        };
        let incident_id = store
            .record_incident(Some(run.id), &incident, Some("diagnostic detail"))
            .expect("incident write");
        assert!(!incident_id.is_empty());
    }

    #[test]
    fn coordination_state_round_trips() {
        let dir = tempdir().expect("test operation should succeed");
        let store = Store::in_memory().expect("test operation should succeed");
        let workspace = store
            .ensure_workspace(dir.path())
            .expect("test operation should succeed");
        let run = store
            .create_run("coordinate work", Mode::Batch)
            .expect("test operation should succeed");
        store
            .attach_run_workspace(run.id, &workspace.id)
            .expect("test operation should succeed");

        let now = Utc::now();
        let task_a = TaskRecord {
            run_id: run.id,
            task_id: "inspect".into(),
            objective: "inspect files".into(),
            state: PlanTaskState::Pending,
            paths: vec!["src".into()],
            dependencies: Vec::new(),
            assigned_agent_id: None,
            attempt: 0,
            max_attempts: 2,
            generation: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        let mut task_b = task_a.clone();
        task_b.task_id = "change".into();
        task_b.objective = "change files".into();
        task_b.dependencies = vec![task_a.task_id.clone()];
        store
            .replace_tasks(run.id, &[task_a, task_b])
            .expect("test operation should succeed");
        let tasks = store.tasks(run.id).expect("test operation should succeed");
        assert_eq!(
            tasks
                .iter()
                .find(|task| task.task_id == "change")
                .expect("test operation should succeed")
                .dependencies,
            vec!["inspect"]
        );

        let agent_id = EventAgentId::new();
        store
            .upsert_agent(&AgentRecord {
                run_id: run.id,
                agent_id,
                parent_agent_id: None,
                role: "worker".into(),
                model: "gpt-5.3-codex-spark".into(),
                state: AgentState::Working,
                task_id: Some("change".into()),
                attempt: 1,
                generation: 0,
                started_at: now,
                updated_at: now,
                finished_at: None,
            })
            .expect("test operation should succeed");
        assert_eq!(
            store.agents(run.id).expect("test operation should succeed")[0].agent_id,
            agent_id
        );

        let lease_until = now + chrono::Duration::minutes(5);
        store
            .acquire_task_leases(run.id, "change", agent_id, 0, &["src".into()], lease_until)
            .expect("test operation should succeed");
        let other_agent = EventAgentId::new();
        assert!(matches!(
            store.acquire_task_leases(run.id, "inspect", other_agent, 0, &["src".into()], lease_until),
            Err(StoreError::LeaseConflict(_))
        ));
        assert_eq!(
            store
                .release_task_leases(run.id, "change", agent_id, 0)
                .expect("test operation should succeed"),
            1
        );

        let mut entry = BoardEntry::session(
            workspace.id.clone(),
            run.id,
            BoardKind::Decision,
            "Use a DAG",
            "Schedule only ready tasks.",
        );
        entry.task_id = Some("inspect".into());
        store
            .insert_board_entry(&entry)
            .expect("test operation should succeed");
        store
            .pin_board_entry(&entry.id)
            .expect("test operation should succeed");
        let pinned = store
            .board_entry(&entry.id)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(pinned.scope, BoardScope::Project);
        store
            .revise_board_entry(&entry.id, Some("Schedule persisted ready tasks."), None, None)
            .expect("test operation should succeed");
        assert_eq!(
            store
                .board_entries(&workspace.id, Some(run.id), Some("persisted"), 10)
                .expect("test operation should succeed")
                .len(),
            1
        );

        let model: ModelDescriptor = serde_json::from_value(serde_json::json!({
            "slug": "gpt-5.6-luna",
            "context_window": 272000
        }))
        .expect("test operation should succeed");
        store
            .save_model_catalog(&workspace.id, std::slice::from_ref(&model), Some("etag"))
            .expect("test operation should succeed");
        assert_eq!(
            store
                .model_catalog(&workspace.id)
                .expect("test operation should succeed")
                .expect("test operation should succeed")
                .models,
            vec![model]
        );

        store
            .add_usage(run.id, 13, 5)
            .expect("test operation should succeed");
        store
            .record_usage_turn(
                run.id,
                Some(agent_id),
                "gpt-5.3-codex-spark",
                crate::usage::TokenUsage {
                    input: 13,
                    output: 5,
                    cached_input: 3,
                    cache_write: 1,
                    reasoning_output: 2,
                },
                Some(18),
            )
            .expect("test operation should succeed");
        assert_eq!(
            store
                .usage_totals(Some(run.id))
                .expect("test operation should succeed"),
            UsageTotals {
                session_input: 13,
                session_output: 5,
                session_cached_input: 3,
                session_cache_write: 1,
                session_reasoning_output: 2,
                lifetime_input: 13,
                lifetime_output: 5,
                lifetime_cached_input: 3,
                lifetime_cache_write: 1,
                lifetime_reasoning_output: 2,
            }
        );

        store
            .record_usage_turn(
                run.id,
                Some(agent_id),
                "deepseek/deepseek-v4-flash",
                crate::usage::TokenUsage {
                    input: 1_000_000,
                    output: 1_000_000,
                    cached_input: 500_000,
                    cache_write: 0,
                    reasoning_output: 0,
                },
                Some(1_000_000),
            )
            .expect("record DeepSeek usage");
        let cost = store.deepseek_cost_totals(Some(run.id)).expect("DeepSeek cost");
        assert!((cost.estimated_usd - 0.3514).abs() < f64::EPSILON);
        assert!((cost.cache_savings_usd - 0.0686).abs() < f64::EPSILON);
        assert_eq!(cost.priced_turns, 1);
        assert_eq!(cost.unpriced_turns, 0);
    }

    #[test]
    fn runs_messages_and_events_round_trip() {
        let store = Store::in_memory().expect("test operation should succeed");
        let run = store
            .create_run("fix it", Mode::Batch)
            .expect("test operation should succeed");
        store
            .append_message(run.id, "user", &serde_json::json!({"text": "fix it"}), false)
            .expect("test operation should succeed");
        store
            .record_event(run.id, "started", serde_json::json!({"ok": true}))
            .expect("test operation should succeed");
        store
            .add_usage(run.id, 10, 3)
            .expect("test operation should succeed");
        store
            .update_run_state(
                run.id,
                ExitState::Succeeded,
                Some("gpt-5.6-luna"),
                Some("done"),
                None,
            )
            .expect("test operation should succeed");
        let loaded = store
            .run(run.id)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(loaded.state, ExitState::Succeeded);
        assert_eq!(loaded.input_tokens, 10);
        assert_eq!(
            store
                .messages(run.id)
                .expect("test operation should succeed")
                .len(),
            1
        );
        assert_eq!(
            store.events(run.id).expect("test operation should succeed").len(),
            1
        );
    }

    #[test]
    fn legacy_event_api_round_trips() {
        let store = Store::in_memory().expect("test operation should succeed");
        let run = store
            .create_run("test", Mode::Batch)
            .expect("test operation should succeed");
        let event = EventEnvelope::new(
            run.id,
            0,
            RuntimeEvent::Legacy {
                kind: "finished".into(),
                payload: serde_json::json!({"ok": true}),
            },
        );
        store.append_event(&event).expect("test operation should succeed");
        assert_eq!(
            store
                .event(event.run_id, 0)
                .expect("test operation should succeed"),
            Some(event)
        );
    }

    #[test]
    fn sessions_can_be_renamed_forked_and_archived() {
        let store = Store::in_memory().expect("test operation should succeed");
        let run = store
            .create_run("a useful goal", Mode::Interactive)
            .expect("test operation should succeed");
        store
            .rename_run(run.id, "renamed")
            .expect("test operation should succeed");
        store
            .append_message(run.id, "user", &serde_json::json!({"text": "hello"}), false)
            .expect("test operation should succeed");
        let fork = store.fork_run(run.id).expect("test operation should succeed");
        assert_eq!(fork.parent_run_id, Some(run.id));
        assert_eq!(
            store
                .messages(fork.id)
                .expect("test operation should succeed")
                .len(),
            1
        );
        store.archive_run(run.id).expect("test operation should succeed");
        assert!(
            store
                .run(run.id)
                .expect("test operation should succeed")
                .expect("test operation should succeed")
                .archived
        );
    }
}
