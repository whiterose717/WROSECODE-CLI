use crate::session::Session;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open_default() -> Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
        Self::open(&home.join(".wrosecode/state.db"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS sessions (
               id TEXT PRIMARY KEY, created INTEGER NOT NULL, updated INTEGER NOT NULL,
               summary TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
               payload_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS tool_calls (
               id TEXT PRIMARY KEY, session_id TEXT NOT NULL, kind TEXT NOT NULL,
               title TEXT NOT NULL, status TEXT NOT NULL, elapsed_ms INTEGER NOT NULL DEFAULT 0,
               output_preview TEXT NOT NULL DEFAULT '', created INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS flags (
               id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
               flag TEXT NOT NULL, source TEXT NOT NULL, verdict TEXT NOT NULL,
               created INTEGER NOT NULL, UNIQUE(session_id, flag)
             );
             CREATE TABLE IF NOT EXISTS errors (
               id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
               class TEXT NOT NULL, message TEXT NOT NULL, context TEXT NOT NULL,
               created INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated DESC);
             CREATE INDEX IF NOT EXISTS idx_flags_session ON flags(session_id, created DESC);",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn save_session(&self, session: &Session) -> Result<()> {
        let payload = serde_json::to_string(&session.redacted_for_storage())?;
        let updated = now();
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("session database lock poisoned"))?
            .execute(
                "INSERT INTO sessions(id,created,updated,summary,provider,model,payload_json)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(id) DO UPDATE SET updated=excluded.updated,summary=excluded.summary,
                 provider=excluded.provider,model=excluded.model,payload_json=excluded.payload_json",
                params![session.name, session.created as i64, updated as i64, session.summary,
                    session.provider_name, session.model, payload],
            )?;
        Ok(())
    }

    pub fn load_session(&self, id: &str) -> Result<Option<Session>> {
        let payload: Option<String> = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("session database lock poisoned"))?
            .query_row(
                "SELECT payload_json FROM sessions WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        payload
            .map(|value| Ok(serde_json::from_str(&value)?))
            .transpose()
    }

    pub fn latest(&self) -> Result<Option<Session>> {
        let payload: Option<String> = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("session database lock poisoned"))?
            .query_row(
                "SELECT payload_json FROM sessions ORDER BY updated DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        payload
            .map(|value| Ok(serde_json::from_str(&value)?))
            .transpose()
    }

    pub fn record_flag(
        &self,
        session: &str,
        flag: &str,
        source: &str,
        verdict: &str,
    ) -> Result<()> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("session database lock poisoned"))?
            .execute(
                "INSERT OR IGNORE INTO flags(session_id,flag,source,verdict,created) VALUES(?1,?2,?3,?4,?5)",
                params![session, flag, source, verdict, now() as i64],
            )?;
        Ok(())
    }

    pub fn record_error(
        &self,
        session: &str,
        class: &str,
        message: &str,
        context: &str,
    ) -> Result<()> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("session database lock poisoned"))?
            .execute(
                "INSERT INTO errors(session_id,class,message,context,created) VALUES(?1,?2,?3,?4,?5)",
                params![session, class, message, context, now() as i64],
            )?;
        Ok(())
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_session() {
        let path = std::env::temp_dir().join(format!("wrose-store-{}.db", std::process::id()));
        let store = Store::open(&path).unwrap();
        let session = Session::fresh();
        store.save_session(&session).unwrap();
        assert_eq!(
            store.load_session(&session.name).unwrap().unwrap().name,
            session.name
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }
}
