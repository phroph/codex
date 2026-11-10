//! Per-workflow durable journal. Committed events are authoritative; snapshots and outputs
//! are derived views. Loading never dispatches agents or retries interrupted work.

use super::model::WorkflowState;
use super::model::validate_id;
use super::model::validate_state;
use serde::Deserialize;
use serde::Serialize;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(crate) struct WorkflowStore {
    directory: PathBuf,
    owner_thread_id: String,
    plan_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowEvent {
    revision: u64,
    kind: String,
    timestamp: String,
    state: WorkflowState,
}

struct Replay {
    state: Option<WorkflowState>,
    committed_bytes: u64,
}

impl WorkflowStore {
    pub(crate) fn new(
        codex_home: &Path,
        owner_thread_id: &str,
        plan_id: &str,
    ) -> Result<Self, String> {
        validate_id(owner_thread_id, "owner thread id")?;
        validate_id(plan_id, "workflow id")?;
        Ok(Self {
            directory: codex_home
                .join("workflows")
                .join(owner_thread_id)
                .join(plan_id),
            owner_thread_id: owner_thread_id.to_string(),
            plan_id: plan_id.to_string(),
        })
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) async fn load(&self) -> Result<Option<WorkflowState>, String> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            if !store
                .directory
                .try_exists()
                .map_err(|err| format!("inspect workflow directory: {err}"))?
            {
                return Ok(None);
            }
            let _lock = store.lock()?;
            Ok(store.replay()?.state)
        })
        .await
        .map_err(|err| format!("workflow load task failed: {err}"))?
    }

    /// The caller serializes in-memory mutations. The file lock and revision check additionally
    /// prevent another process or stale handle from replacing a committed workflow revision.
    pub(crate) async fn save(&self, state: &mut WorkflowState, event: &str) -> Result<(), String> {
        validate_state(state)?;
        self.check_identity(state)?;
        if event.trim().is_empty() || event.len() > 1024 {
            return Err("workflow event kind must contain 1–1024 bytes".to_string());
        }
        let store = self.clone();
        let candidate = state.clone();
        let kind = event.to_string();
        let committed = tokio::task::spawn_blocking(move || store.save_sync(candidate, kind))
            .await
            .map_err(|err| format!("workflow save task failed: {err}"))??;
        *state = committed;
        Ok(())
    }

    fn check_identity(&self, state: &WorkflowState) -> Result<(), String> {
        if state.owner_thread_id != self.owner_thread_id || state.plan.id != self.plan_id {
            return Err("workflow state identity does not match its store".to_string());
        }
        Ok(())
    }

    fn lock(&self) -> Result<File, String> {
        std::fs::create_dir_all(&self.directory)
            .map_err(|err| format!("create workflow directory: {err}"))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.directory.join(".lock"))
            .map_err(|err| format!("open workflow lock: {err}"))?;
        lock.lock().map_err(|err| format!("lock workflow: {err}"))?;
        Ok(lock)
    }

    fn replay(&self) -> Result<Replay, String> {
        let path = self.directory.join("events.jsonl");
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if self
                    .directory
                    .join("state.json")
                    .try_exists()
                    .map_err(|err| format!("inspect workflow snapshot: {err}"))?
                {
                    return Err("workflow journal is missing but a snapshot exists; inspect the stored workflow before continuing".to_string());
                }
                return Ok(Replay {
                    state: None,
                    committed_bytes: 0,
                });
            }
            Err(err) => return Err(format!("open workflow journal: {err}")),
        };
        let mut reader = BufReader::new(file);
        let mut state: Option<WorkflowState> = None;
        let mut committed_bytes = 0_u64;
        let mut line_number = 0_u64;
        loop {
            let mut line = Vec::new();
            let bytes = reader
                .read_until(b'\n', &mut line)
                .map_err(|err| format!("read workflow journal: {err}"))?;
            if bytes == 0 {
                break;
            }
            if line.last() != Some(&b'\n') {
                // A record without its terminating newline was never fully committed.
                break;
            }
            line_number += 1;
            let event: WorkflowEvent = serde_json::from_slice(&line).map_err(|err| {
                format!("invalid committed workflow event at line {line_number}: {err}")
            })?;
            validate_state(&event.state)?;
            self.check_identity(&event.state)?;
            let expected = state
                .as_ref()
                .map_or(0, |state| state.revision)
                .checked_add(1)
                .ok_or_else(|| "workflow revision overflow".to_string())?;
            if event.revision != expected || event.state.revision != event.revision {
                return Err(format!(
                    "workflow event at line {line_number} has an invalid revision"
                ));
            }
            state = Some(event.state);
            committed_bytes += bytes as u64;
        }
        Ok(Replay {
            state,
            committed_bytes,
        })
    }

    fn save_sync(&self, mut state: WorkflowState, kind: String) -> Result<WorkflowState, String> {
        let _lock = self.lock()?;
        let replay = self.replay()?;
        let stored_revision = replay.state.as_ref().map_or(0, |state| state.revision);
        if state.revision != stored_revision {
            return Err(format!(
                "workflow revision changed: expected {}, stored {stored_revision}; reload before updating",
                state.revision
            ));
        }
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| "workflow revision overflow".to_string())?;
        let event = WorkflowEvent {
            revision: state.revision,
            kind,
            timestamp: chrono::Utc::now().to_rfc3339(),
            state: state.clone(),
        };
        let mut bytes =
            serde_json::to_vec(&event).map_err(|err| format!("encode workflow event: {err}"))?;
        bytes.push(b'\n');
        let mut journal = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.directory.join("events.jsonl"))
            .map_err(|err| format!("open workflow journal for append: {err}"))?;
        // Preserve every committed event; discard only an interrupted, uncommitted tail.
        // Windows append-only handles cannot truncate. The workflow lock serializes writes,
        // so seek explicitly after trimming the uncommitted tail on a writable handle.
        journal
            .set_len(replay.committed_bytes)
            .map_err(|err| format!("remove uncommitted workflow journal tail: {err}"))?;
        journal
            .seek(SeekFrom::Start(replay.committed_bytes))
            .map_err(|err| format!("seek to committed workflow journal end: {err}"))?;
        journal
            .write_all(&bytes)
            .and_then(|()| journal.sync_all())
            .map_err(|err| format!("commit workflow event: {err}"))?;
        // Once the journal is durable, its revision is committed even if a derived view fails.
        // Reporting a failed commit here could cause the caller to replay an already run action.
        if let Err(error) = self.materialize(&state) {
            tracing::warn!(workflow_id = %self.plan_id, revision = state.revision, %error,
                "workflow committed; derived files could not be refreshed");
        }
        Ok(state)
    }

    fn materialize(&self, state: &WorkflowState) -> Result<(), String> {
        let snapshot = serde_json::to_vec_pretty(state)
            .map_err(|err| format!("encode workflow snapshot: {err}"))?;
        atomic_write(&self.directory.join("state.json"), &snapshot)?;
        let outputs = self.directory.join("outputs");
        std::fs::create_dir_all(&outputs)
            .map_err(|err| format!("create workflow outputs: {err}"))?;
        for (id, task) in &state.tasks {
            let path = outputs.join(format!("{id}.txt"));
            match task.output.as_ref() {
                Some(output) => atomic_write(&path, output.as_bytes())?,
                None => match std::fs::remove_file(&path) {
                    Ok(()) => (),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
                    Err(err) => return Err(format!("clear invalidated task output: {err}")),
                },
            }
        }
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "workflow output has no parent directory".to_string())?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|err| format!("create temporary workflow output: {err}"))?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|err| format!("write workflow output: {err}"))?;
    temporary
        .persist(path)
        .map_err(|err| format!("replace workflow output: {}", err.error))?;
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
