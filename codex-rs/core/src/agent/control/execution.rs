//! Tracks local running capacity and releases reservations through the shared guard.
//! The local permit owns the running count; root and MAv1 turns remain unrestricted.

use super::LocalAgentControl;
use crate::agent::types::AgentExecutionGuard;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[derive(Default)]
pub(super) struct AgentExecutionLimiter {
    active: AtomicUsize,
    max_threads: OnceLock<usize>,
    active_paths: Mutex<HashMap<AgentPath, usize>>,
}

struct LocalExecutionPermit {
    limiter: Arc<AgentExecutionLimiter>,
    agent_path: Option<AgentPath>,
}

impl Drop for LocalExecutionPermit {
    fn drop(&mut self) {
        if let Some(path) = &self.agent_path {
            let mut paths = self
                .limiter
                .active_paths
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(count) = paths.get_mut(path) {
                *count -= 1;
                if *count == 0 {
                    paths.remove(path);
                }
            }
        }
        self.limiter.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl LocalAgentControl {
    pub(crate) fn ensure_execution_capacity(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> CodexResult<()> {
        if !is_execution_limited(multi_agent_version, session_source) {
            return Ok(());
        }
        let max_threads = self.runtime.agent_execution_limiter.max_threads();
        if self.runtime.agent_execution_limiter.has_capacity() {
            Ok(())
        } else {
            Err(
                CodexErr::new(CodexErrorDetails::AgentLimitReached { max_threads })
                    .with_agent_context(AgentErrorContext::ExecutionCapacity),
            )
        }
    }

    pub(crate) fn execution_guard(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> Option<AgentExecutionGuard> {
        is_execution_limited(multi_agent_version, session_source).then(|| {
            Arc::clone(&self.runtime.agent_execution_limiter).guard(session_source.get_agent_path())
        })
    }

    pub(crate) async fn workflow_attempt_has_live_work(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<bool> {
        let metadata = self.runtime.ensure_agent_known(thread_id)?;
        let has_execution_guard = || {
            metadata
                .agent_path
                .as_ref()
                .is_some_and(|path| self.runtime.agent_execution_limiter.has_active_path(path))
        };
        if has_execution_guard() {
            return Ok(true);
        }
        let manager = self.runtime.upgrade()?;
        let thread = match manager.get_thread(thread_id).await {
            Ok(thread) => thread,
            Err(error) if matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                return Ok(has_execution_guard());
            }
            Err(error) => return Err(error),
        };
        // Keep reservation inspection atomic with task registration. Abort removes
        // ActiveTurn before joining the task; its execution guard covers that gap.
        let status = thread.agent_status().await;
        let active = thread.session.active_turn.lock().await;
        Ok(active.is_some()
            || has_execution_guard()
            || matches!(status, AgentStatus::PendingInit | AgentStatus::Running))
    }
}

impl AgentExecutionLimiter {
    pub(super) fn initialize(&self, max_threads: usize) {
        self.max_threads.get_or_init(|| max_threads);
    }

    fn max_threads(&self) -> usize {
        self.max_threads.get().copied().unwrap_or(usize::MAX)
    }

    fn has_capacity(&self) -> bool {
        self.active.load(Ordering::Acquire) < self.max_threads()
    }

    fn has_active_path(&self, path: &AgentPath) -> bool {
        self.active_paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(path)
    }

    fn guard(self: Arc<Self>, agent_path: Option<AgentPath>) -> AgentExecutionGuard {
        if let Some(path) = &agent_path {
            *self
                .active_paths
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(path.clone())
                .or_default() += 1;
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        AgentExecutionGuard::new(LocalExecutionPermit {
            limiter: self,
            agent_path,
        })
    }
}

fn is_execution_limited(
    multi_agent_version: MultiAgentVersion,
    session_source: &SessionSource,
) -> bool {
    multi_agent_version == MultiAgentVersion::V2
        && matches!(session_source, SessionSource::SubAgent(_))
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
