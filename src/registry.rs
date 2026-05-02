use crate::session::Session;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Default, Clone)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<Uuid, Arc<Session>>>>,
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
}
