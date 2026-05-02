use crate::paths;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct PersistedSession {
    pub id: Uuid,
    pub cwd: PathBuf,
    pub name: String,
    pub model: Option<String>,
    pub started_at_ms: u128,
}

impl Store {
    pub fn open() -> Result<Self> {
        let dir = paths::state_dir()?;
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("state.sqlite");
        tracing::info!(path = %path.display(), "opening state store");
        let conn = Connection::open(&path).context("open sqlite")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                name TEXT NOT NULL,
                model TEXT,
                started_at_ms INTEGER NOT NULL,
                last_seen_ms INTEGER NOT NULL,
                closed_by_user INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn insert(
        &self,
        id: Uuid,
        cwd: &str,
        name: &str,
        model: Option<&str>,
        started_at_ms: u128,
    ) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT OR REPLACE INTO sessions
                 (id, cwd, name, model, started_at_ms, last_seen_ms, closed_by_user)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, 0)",
            params![id.to_string(), cwd, name, model, started_at_ms as i64],
        )?;
        Ok(())
    }

    pub fn mark_closed_by_user(&self, id: Uuid) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE sessions SET closed_by_user = 1 WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    pub fn list_resumable(&self) -> Result<Vec<PersistedSession>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, cwd, name, model, started_at_ms FROM sessions
             WHERE closed_by_user = 0
             ORDER BY started_at_ms ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id_str: String = row.get(0)?;
            let id = Uuid::parse_str(&id_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(PersistedSession {
                id,
                cwd: PathBuf::from(row.get::<_, String>(1)?),
                name: row.get(2)?,
                model: row.get(3)?,
                started_at_ms: row.get::<_, i64>(4)? as u128,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}
