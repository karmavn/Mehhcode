//! Shared child registry, global concurrency budget, and per-child control.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use mehh_ai::{now_ms, Message, UserContent, UserMessage};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Default cap on concurrently running subagents.
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 4;

/// Handle for one registered subagent child.
pub struct SubagentHandle {
    pub agent_id: String,
    pub mode: String,
    pub started_ms: i64,
    pub cancel: CancellationToken,
    pub steering_queue: Arc<Mutex<VecDeque<Message>>>,
}

/// Summary of a registered subagent child.
pub struct SubagentSummary {
    pub agent_id: String,
    pub mode: String,
    pub elapsed_ms: i64,
}

/// Shared registry of live subagent children plus a global concurrency budget.
pub struct SubagentControl {
    permits: Arc<Semaphore>,
    children: Arc<Mutex<HashMap<String, SubagentHandle>>>,
}

impl SubagentControl {
    /// Creates a control allowing at most `max_concurrent` simultaneous children.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent)),
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Acquires one global concurrency permit.
    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit {
        Arc::clone(&self.permits).acquire_owned().await.expect("semaphore closed")
    }

    /// Registers a child and returns its handle (handle cancel kills the child).
    pub fn register(&self, agent_id: String, mode: String, cancel: CancellationToken) -> SubagentHandle {
        let started_ms = now_ms();
        let steering_queue = Arc::new(Mutex::new(VecDeque::new()));
        self.children
            .lock()
            .unwrap()
            .insert(
                agent_id.clone(),
                SubagentHandle {
                    agent_id: agent_id.clone(),
                    mode: mode.clone(),
                    started_ms,
                    cancel: cancel.clone(),
                    steering_queue: steering_queue.clone(),
                },
            );
        SubagentHandle {
            agent_id,
            mode,
            started_ms,
            cancel,
            steering_queue,
        }
    }

    /// Removes a child from the registry (called on every exit path).
    pub fn unregister(&self, agent_id: &str) {
        self.children.lock().unwrap().remove(agent_id);
    }

    /// Returns summaries for all registered children.
    pub fn list(&self) -> Vec<SubagentSummary> {
        let children = self.children.lock().unwrap();
        let now = now_ms();
        children
            .values()
            .map(|h| SubagentSummary {
                agent_id: h.agent_id.clone(),
                mode: h.mode.clone(),
                elapsed_ms: now - h.started_ms,
            })
            .collect()
    }

    /// Queues a steering user message for a child; false for unknown ids.
    pub fn steer(&self, agent_id: &str, text: &str) -> bool {
        let mut children = self.children.lock().unwrap();
        if let Some(handle) = children.get_mut(agent_id) {
            handle.steering_queue.lock().unwrap().push_back(Message::User(UserMessage {
                content: UserContent::Text(text.to_string()),
                timestamp: now_ms(),
            }));
            true
        } else {
            false
        }
    }

    /// Cancels only the given child's token and removes the handle; false for unknown ids.
    pub fn kill(&self, agent_id: &str) -> bool {
        let mut children = self.children.lock().unwrap();
        if let Some(handle) = children.remove(agent_id) {
            handle.cancel.cancel();
            true
        } else {
            false
        }
    }
}
