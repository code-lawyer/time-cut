use std::{fs, path::Path, time::Duration};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use super::archive::{Result, archive_relative, open_stable_file};

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Totals {
    pub files_verified: u64,
    bytes_verified: u64,
    pub failed: u64,
}

/// Read only the archive manifest and its originals. Never start the ingest worker or repair records.
pub(super) fn verify(root: &Path) -> Result<Totals> {
    let root = root.canonicalize()?;
    let manifest = root
        .join(".time-cut")
        .join("imports.sqlite3")
        .canonicalize()?;
    if !manifest.starts_with(&root) {
        return Err("Manifest is outside the archive".into());
    }
    let db = Connection::open_with_flags(manifest, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let revision = || db.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0));
    let initial = revision()?;
    let mut totals = Totals::default();
    {
        let mut query = db.prepare("SELECT path,size,digest FROM files")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let relative: String = row.get(0)?;
            let size: u64 = row.get(1)?;
            let digest: String = row.get(2)?;
            if check_file(&root, Path::new(&relative), size, &digest).unwrap_or(false) {
                totals.files_verified += 1;
                totals.bytes_verified += size;
            } else {
                totals.failed += 1;
            }
        }
    }
    if revision()? != initial {
        return Err("Archive changed during verification, retry once idle".into());
    }
    Ok(totals)
}

fn check_file(root: &Path, relative: &Path, size: u64, digest: &str) -> Result<bool> {
    if !archive_relative(relative) {
        return Ok(false);
    }
    let path = root.join(relative);
    if !fs::symlink_metadata(&path)?.is_file() {
        return Ok(false);
    }
    let resolved = path.canonicalize()?;
    if !resolved.starts_with(root) {
        return Ok(false);
    }
    let file = open_stable_file(&resolved)?;
    if file.metadata()?.len() != size {
        return Ok(false);
    }
    let mut hash = blake3::Hasher::new();
    hash.update_reader(file)?;
    Ok(hash.finalize().to_hex().as_str() == digest)
}
