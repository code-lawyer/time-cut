//! Attach already received bytes to the last complete phone inventory after a
//! partial scan. Only the live device can confirm that inventory for unplugging.
use super::{Archive, Connection, Location, Result, params, verified_local};
use rusqlite::OptionalExtension;
use std::{fs, io};

const CANDIDATES: &str = "SELECT m.ordinal,m.size,i.digest,i.blob,i.archive_path
    FROM phone_list_items m JOIN phone_runs r ON r.id=m.run_id
    JOIN phone_lists l ON l.run_id=r.id
    JOIN phone_inbox i ON i.source=r.source AND i.entry=m.entry AND i.version=m.version
      AND i.run_id=r.id AND i.size=m.size AND i.modified=m.modified
    WHERE r.id=? AND r.safe_at IS NULL AND m.digest IS NULL AND m.version<>'' AND m.size>0
      AND NOT EXISTS(SELECT 1 FROM phone_list_items s INDEXED BY phone_list_signature
        WHERE s.run_id=r.id AND s.entry=m.entry AND s.ordinal<>m.ordinal
          AND (s.version=m.version OR (s.size=m.size AND s.modified=m.modified)))
      AND NOT EXISTS(SELECT 1 FROM phone_inbox h WHERE h.source=r.source AND h.entry=m.entry
        AND h.version=m.version AND h.digest<>i.digest)
      AND NOT EXISTS(SELECT 1 FROM sources h WHERE h.source=r.source AND h.entry=m.entry
        AND h.version=m.version AND h.digest<>i.digest)
      AND NOT EXISTS(SELECT 1 FROM source_versions h WHERE h.source=r.source AND h.entry=m.entry
        AND h.version=m.version AND h.digest<>i.digest)
      AND NOT EXISTS(SELECT 1 FROM phone_returned_data h WHERE h.run_id=r.id AND h.entry=m.entry
        AND h.version=m.version AND h.reported_size=m.size AND h.digest<>i.digest)";

pub(in crate::ingest) fn has_candidates(db: &Connection, run: i64) -> Result<bool> {
    Ok(db.prepare_cached(CANDIDATES)?.exists([run])?)
}

pub(in crate::ingest) fn reconcile(archive: &Archive, run: i64, listed_at: i64) -> Result<u64> {
    let tx = archive.db.unchecked_transaction()?;
    let snapshot = tx
        .query_row(
            "SELECT checked_at FROM phone_lists WHERE run_id=?",
            [run],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    if snapshot != Some(listed_at) {
        return Ok(0);
    }
    let candidates = tx
        .prepare_cached(CANDIDATES)?
        .query_map([run], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, String>(2)?,
                Location {
                    blob: row.get(3)?,
                    archived: row.get(4)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut restored = 0;
    {
        let mut put = tx.prepare_cached(
            "UPDATE phone_list_items SET digest=? WHERE run_id=? AND ordinal=? AND digest IS NULL",
        )?;
        for (ordinal, size, digest, location) in candidates {
            let Ok(file) = location.path(&archive.root) else {
                continue;
            };
            let matches = fs::symlink_metadata(&file).is_ok_and(|m| m.is_file() && m.len() == size);
            if !matches {
                continue;
            }
            match verified_local(archive, &file, &digest) {
                Ok(true) => restored += put.execute(params![digest, run, ordinal])? as u64,
                Ok(false) => (),
                Err(e)
                    if e.downcast_ref::<io::Error>().is_some_and(|e| {
                        matches!(
                            e.kind(),
                            io::ErrorKind::NotFound
                                | io::ErrorKind::PermissionDenied
                                | io::ErrorKind::WouldBlock
                        )
                    }) =>
                {
                    ()
                }
                Err(e) => return Err(e),
            }
        }
    }
    tx.commit()?;
    Ok(restored)
}
