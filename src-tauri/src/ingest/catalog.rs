use super::{Target, archive::Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    collections::{HashMap, HashSet},
    sync::{RwLock, RwLockReadGuard},
    time::Duration,
};

// Catalog mutations wait for the current original to finish publishing. Between files the
// worker checks the generation, so removing or renaming an album stops writes to its old root.
static GENERATION: RwLock<u64> = RwLock::new(0);

pub(crate) fn changing<T>(
    operation: impl FnOnce() -> std::result::Result<T, String>,
) -> std::result::Result<T, String> {
    let mut generation = GENERATION.write().map_err(|e| e.to_string())?;
    let result = operation();
    *generation = generation.wrapping_add(1);
    result
}

pub(super) fn generation() -> Result<u64> {
    Ok(*GENERATION.read().map_err(|e| e.to_string())?)
}

pub(super) fn guard_destination<'a>(
    dest: &'a Target,
    mut lookup: impl FnMut(&str, i64) -> Result<Option<String>> + 'a,
) -> impl FnMut() -> Result<RwLockReadGuard<'static, u64>> + 'a {
    let mut validated = None;
    move || {
        let guard = GENERATION.read().map_err(|e| e.to_string())?;
        if validated != Some(*guard) {
            let path = lookup(&dest.library, dest.album)?;
            if path.as_deref() != Some(dest.root.as_str()) {
                return Err("接收原件的相册已移动或移除".into());
            }
            validated = Some(*guard);
        }
        Ok(guard)
    }
}

/// The owning library may be inactive. An inaccessible catalog is a retryable error. Only
/// a registered database successfully queried for the album can establish its removal.
pub(super) fn album_path(library: &str, album: i64) -> Result<Option<String>> {
    let _lease = crate::t_storage::DbConnectionLease::acquire()?;
    let config = crate::t_config::load_app_config()?;
    if !config.libraries.iter().any(|entry| entry.id == library) {
        return Ok(None);
    }
    let path = crate::t_storage::get_library_db_path(library)?;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(2))?;
    Ok(db
        .query_row("SELECT path FROM albums WHERE id=?", [album], |r| r.get(0))
        .optional()?)
}

pub(super) type Unavailable = HashMap<(String, i64), String>;

pub(super) fn synchronize(
    db: &Connection,
    mut lookup: impl FnMut(&str, i64) -> Result<Option<String>>,
) -> Result<Unavailable> {
    let mut known = HashSet::<(String, i64)>::new();
    {
        let mut rows = db.prepare("SELECT data FROM config UNION SELECT target FROM queue")?;
        for raw in rows.query_map([], |r| r.get::<_, String>(0))? {
            let t: Target = serde_json::from_str(&raw?)?;
            known.insert((t.library, t.album));
        }
        let mut rows = db.prepare(
            "SELECT library,album FROM indexing UNION SELECT library,album FROM phone_work",
        )?;
        for entry in rows.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))? {
            known.insert(entry?);
        }
    }
    let mut unavailable = HashMap::new();
    for (library, album) in known {
        let new_path = match lookup(&library, album) {
            Ok(path) => path,
            Err(e) => {
                unavailable.insert((library, album), e.to_string());
                continue;
            }
        };
        let tx = db.unchecked_transaction()?;
        if let Some(path) = new_path {
            tx.execute("UPDATE config SET data=json_set(data,'$.root',?3)
                WHERE json_extract(data,'$.library')=?1 AND json_extract(data,'$.album')=?2 AND json_extract(data,'$.root')<>?3", params![library, album, path])?;
            // If two old entries converge on the same destination, a single copy job is enough.
            tx.execute("UPDATE OR REPLACE queue SET target=json_set(target,'$.root',?3)
                WHERE json_extract(target,'$.library')=?1 AND json_extract(target,'$.album')=?2 AND json_extract(target,'$.root')<>?3", params![library, album, path])?;
        } else {
            tx.execute(
                "DELETE FROM phone_work WHERE library=? AND album=?",
                params![library, album],
            )?;
            tx.execute("DELETE FROM config WHERE json_extract(data,'$.library')=?1 AND json_extract(data,'$.album')=?2", params![library, album])?;
            tx.execute("DELETE FROM queue WHERE json_extract(target,'$.library')=?1 AND json_extract(target,'$.album')=?2", params![library, album])?;
            tx.execute(
                "DELETE FROM indexing WHERE library=? AND album=?",
                params![library, album],
            )?;
            tx.execute(
                "DELETE FROM index_file_retries WHERE library=? AND album=?",
                params![library, album],
            )?;
        }
        tx.commit()?;
    }
    Ok(unavailable)
}
