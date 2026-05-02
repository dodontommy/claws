use crate::session::Session;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// A persisted session that the daemon tried — and failed — to resume on
/// startup. We keep enough metadata in memory to surface a "✗ resume failed"
/// row in the TUI so the user notices instead of the session silently
/// disappearing. Cleared from this list when the user presses `x` on it.
#[derive(Debug, Clone)]
pub struct FailedResume {
    pub id: Uuid,
    pub cwd: PathBuf,
    pub name: String,
    pub display_override: Option<String>,
    pub failed_at_ms: u128,
    pub reason: String,
}

#[derive(Default, Clone)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<Uuid, Arc<Session>>>>,
    failed: Arc<Mutex<Vec<FailedResume>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, session: Session) -> Arc<Session> {
        let arc = Arc::new(session);
        self.inner.lock().unwrap().insert(arc.id, arc.clone());
        arc
    }

    pub fn get(&self, id: Uuid) -> Option<Arc<Session>> {
        self.inner.lock().unwrap().get(&id).cloned()
    }

    pub fn remove(&self, id: Uuid) -> Option<Arc<Session>> {
        self.inner.lock().unwrap().remove(&id)
    }

    pub fn all(&self) -> Vec<Arc<Session>> {
        self.inner.lock().unwrap().values().cloned().collect()
    }

    pub fn close_all(&self) {
        for s in self.all() {
            s.close();
        }
    }

    pub fn record_resume_failure(&self, fr: FailedResume) {
        self.failed.lock().unwrap().push(fr);
    }

    pub fn failed_resumes(&self) -> Vec<FailedResume> {
        self.failed.lock().unwrap().clone()
    }

    /// Drop a recorded failure (called when the user presses `x` on a
    /// resume-failed row). Returns true if the entry existed.
    pub fn forget_failed_resume(&self, id: Uuid) -> bool {
        let mut g = self.failed.lock().unwrap();
        let before = g.len();
        g.retain(|f| f.id != id);
        before != g.len()
    }
}
