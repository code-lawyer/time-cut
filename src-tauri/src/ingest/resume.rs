use super::archive::{Item, Result, family};
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};
use std::time::UNIX_EPOCH;

// A scheduling key, never proof that two files have the same contents. Some phones
// change every WPD object ID on reconnect, and can expose several AAE files at one path.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Entry {
    path: String,
    signature: String,
    occurrence: u32,
}

pub(super) struct PhoneQueue {
    source: String,
    known_paths: HashSet<String>,
    #[cfg(test)]
    known_families: HashSet<String>,
    received_versions: HashMap<String, HashSet<String>>,
    pending: HashSet<Entry>,
    interrupted: HashMap<String, i64>,
    occurrences: HashMap<(String, String), u32>,
    handles: HashMap<String, Entry>,
    #[cfg(test)]
    missing_members: HashSet<String>,
    #[cfg(test)]
    postponed: Vec<Item>,
    inventory: Vec<[u8; 32]>,
}

impl PhoneQueue {
    pub fn for_capture(db: &Connection, source: &str) -> Result<Self> {
        let mut queue = Self::open(db, source)?;
        let mut query =
            db.prepare("SELECT entry,version FROM phone_inbox WHERE source=? AND version<>''")?;
        let mut rows = query.query([source])?;
        while let Some(row) = rows.next()? {
            queue
                .received_versions
                .entry(row.get(0)?)
                .or_default()
                .insert(row.get(1)?);
            queue.known_paths.insert(row.get(0)?);
        }
        Ok(queue)
    }

    pub fn open(db: &Connection, source: &str) -> Result<Self> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS phone_pending (
            source TEXT NOT NULL, entry TEXT NOT NULL, signature TEXT NOT NULL,
            occurrence INTEGER NOT NULL, family TEXT NOT NULL,
            attempted_at INTEGER NOT NULL DEFAULT 0, error TEXT, device_version TEXT,
            last_lookup INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(source,entry,signature,occurrence)) WITHOUT ROWID;",
        )?;
        let has_version: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('phone_pending') WHERE name='device_version')",
            [], |r| r.get(0),
        )?;
        if !has_version {
            let tx = db.unchecked_transaction()?;
            tx.execute("ALTER TABLE phone_pending ADD COLUMN device_version TEXT", [])?;
            tx.execute("ALTER TABLE phone_pending ADD COLUMN last_lookup INTEGER NOT NULL DEFAULT 0", [])?;
            let has_manifest: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='phone_list_items')",
                [], |r| r.get(0),
            )?;
            if has_manifest {
                // Only the last complete list for this source can supply an old locator.
                // The size and date alone cannot supply a receipt or resolve two siblings.
                tx.execute("UPDATE phone_pending AS p SET device_version=(
                    SELECT NULLIF(MIN(m.version),'') FROM phone_runs r JOIN phone_lists l ON l.run_id=r.id
                    JOIN phone_list_items m ON m.run_id=r.id AND m.entry=p.entry
                    WHERE r.source=p.source AND r.safe_at IS NULL AND m.size>0 AND m.modified IS NOT NULL
                      AND p.signature=CAST(m.size AS TEXT)||'/'||m.modified HAVING COUNT(*)=1)
                    WHERE p.occurrence=1 AND NOT EXISTS(SELECT 1 FROM phone_pending s
                      WHERE s.source=p.source AND s.entry=p.entry AND s.signature=p.signature AND s.occurrence>1)", [])?;
            }
            tx.commit()?;
        }
        let mut known_paths = HashSet::new();
        #[cfg(test)]
        let mut known_families = HashSet::new();
        let mut archived = db.prepare("SELECT entry,family FROM sources WHERE source=?")?;
        let mut rows = archived.query([source])?;
        while let Some(row) = rows.next()? {
            known_paths.insert(row.get(0)?);
            #[cfg(test)]
            known_families.insert(row.get(1)?);
        }
        let mut pending = HashSet::new();
        let mut interrupted = HashMap::<String, i64>::new();
        let mut saved = db.prepare(
            "SELECT entry,signature,occurrence,family,attempted_at FROM phone_pending WHERE source=?",
        )?;
        let mut rows = saved.query([source])?;
        while let Some(row) = rows.next()? {
            pending.insert(Entry {
                path: row.get(0)?,
                signature: row.get(1)?,
                occurrence: row.get(2)?,
            });
            let attempted: i64 = row.get(4)?;
            if attempted > 0 {
                let last = interrupted.entry(row.get(3)?).or_default();
                *last = (*last).max(attempted);
            }
        }
        Ok(Self {
            source: source.to_owned(),
            known_paths,
            #[cfg(test)]
            known_families,
            received_versions: HashMap::new(),
            pending,
            interrupted,
            occurrences: HashMap::new(),
            handles: HashMap::new(),
            #[cfg(test)]
            missing_members: HashSet::new(),
            #[cfg(test)]
            postponed: Vec::new(),
            inventory: Vec::new(),
        })
    }

    pub fn count(&self) -> u64 {
        self.pending.len() as u64
    }

    pub fn withhold_folders(
        &mut self,
        db: &Connection,
        folders: &HashSet<std::path::PathBuf>,
    ) -> Result<()> {
        let mut delete = db.prepare_cached(
            "DELETE FROM phone_pending WHERE source=? AND entry=?
            AND signature=? AND occurrence=?",
        )?;
        for entry in &self.pending {
            if folders
                .iter()
                .any(|dir| std::path::Path::new(&entry.path).starts_with(dir))
            {
                delete.execute(params![
                    self.source,
                    entry.path,
                    entry.signature,
                    entry.occurrence
                ])?;
            }
        }
        self.pending.retain(|entry| {
            !folders
                .iter()
                .any(|dir| std::path::Path::new(&entry.path).starts_with(dir))
        });
        Ok(())
    }

    pub fn unlisted_pending(&self) -> u64 {
        self.pending.iter().filter(|e| self.occurrences.get(&(e.path.clone(),e.signature.clone()))
            .copied().unwrap_or(0) < e.occurrence).count() as u64
    }

    pub fn discover(&mut self, db: &Connection, item: &Item) -> Result<()> {
        if self.handles.contains_key(&item.id) {
            return Ok(());
        }
        let path = item.relative.to_string_lossy().into_owned();
        let modified = item
            .modified
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
        let size = item.size.map(|n| n.to_string()).unwrap_or_default();
        let time = modified
            .map(|d| d.as_nanos().to_string())
            .unwrap_or_default();
        let signature = format!("{size}/{time}");
        let count = self
            .occurrences
            .entry((path.clone(), signature.clone()))
            .or_default();
        *count += 1;
        let entry = Entry {
            path,
            signature,
            occurrence: *count,
        };
        let group = family(&item.relative);
        // Do not reinsert then delete a durable pending row for every receipt on every retry.
        // The receiver still checks the actual backing file before acknowledging it.
        let mut known_version = item.version.as_ref().is_some_and(|version| {
            self.received_versions
                .get(&entry.path)
                .is_some_and(|stored| stored.contains(version))
        });
        let lookup_version = !known_version && self.known_paths.contains(&entry.path);
        #[cfg(test)]
        if !lookup_version && self.known_families.contains(&group) {
            self.missing_members.insert(group.clone());
        }
        if lookup_version {
            if let Some(version) = &item.version {
                known_version = db.prepare_cached("SELECT EXISTS(
                    SELECT 1 FROM sources WHERE source=?1 AND entry=?2 AND version=?3
                    UNION ALL SELECT 1 FROM source_versions WHERE source=?1 AND entry=?2 AND version=?3)")?
                    .query_row(params![self.source, entry.path, version], |r| r.get(0))?;
            }
        }
        if !known_version {
            self.remember(db, &entry, &group, item.version.as_deref())?;
        }
        self.handles.insert(item.id.clone(), entry);
        Ok(())
    }

    fn remember(&mut self, db: &Connection, entry: &Entry, group: &str, version: Option<&str>) -> Result<()> {
        if self.pending.contains(entry) {
            return Ok(());
        }
        // The original observation can carry a locator. A later lookalike cannot
        // retrospectively supply one for a legacy row whose locator was lost.
        db.prepare_cached(
            "INSERT OR IGNORE INTO phone_pending(source,entry,signature,occurrence,family,device_version)
             VALUES(?,?,?,?,?,?)",
        )?
        .execute(params![self.source, entry.path, entry.signature, entry.occurrence, group, version.filter(|v| !v.is_empty())])?;
        self.pending.insert(entry.clone());
        Ok(())
    }

    #[cfg(test)]
    pub fn defer(&mut self, item: &Item) -> bool {
        let group = family(&item.relative);
        if self.known_families.contains(&group) || self.interrupted.contains_key(&group) {
            self.postponed.push(item.clone());
            return true;
        }
        false
    }

    #[cfg(test)]
    pub fn take_postponed(&mut self) -> Vec<Item> {
        let mut items = std::mem::take(&mut self.postponed);
        // Stable sorting preserves the provider's primary-before-companion ordering.
        items.sort_by_cached_key(|item| {
            let group = family(&item.relative);
            if let Some(last) = self.interrupted.get(&group) {
                (2, *last)
            } else if self.missing_members.contains(&group) {
                (0, 0)
            } else {
                (1, 0)
            }
        });
        items
    }

    // Commit before the blocking driver call. A killed process cannot run an error handler.
    pub fn attempting(&mut self, db: &Connection, item: &Item, error: Option<&str>) -> Result<()> {
        let entry = self
            .handles
            .get(&item.id)
            .ok_or("缺少导入队列记录")?
            .clone();
        self.remember(db, &entry, &family(&item.relative), item.version.as_deref())?;
        db.prepare_cached(
            "UPDATE phone_pending SET attempted_at=?,error=?
            WHERE source=? AND entry=? AND signature=? AND occurrence=?",
        )?
        .execute(params![
            chrono::Utc::now().timestamp_micros(),
            error,
            self.source,
            entry.path,
            entry.signature,
            entry.occurrence
        ])?;
        Ok(())
    }

    pub fn verified(&mut self, db: &Connection, item: &Item, digest: &str) -> Result<()> {
        let entry = self.handles.remove(&item.id).ok_or("缺少导入队列记录")?;
        if self.pending.contains(&entry) {
            db.prepare_cached(
                "DELETE FROM phone_pending WHERE source=? AND entry=?
                AND signature=? AND occurrence=?",
            )?
            .execute(params![
                self.source,
                entry.path,
                entry.signature,
                entry.occurrence
            ])?;
            self.pending.remove(&entry);
        }
        let mut hash = blake3::Hasher::new();
        hash.update(entry.path.as_bytes());
        hash.update(&[0]);
        hash.update(digest.as_bytes());
        self.inventory.push(*hash.finalize().as_bytes());
        Ok(())
    }

    pub fn inventory_digest(&mut self) -> String {
        self.inventory.sort_unstable();
        let mut hash = blake3::Hasher::new();
        for entry in &self.inventory {
            hash.update(entry);
        }
        hash.finalize().to_hex().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_pending_versions_are_recovered_only_from_a_unique_matching_source_list() {
        let db = Connection::open_in_memory().unwrap();
        crate::ingest::inbox::initialize(&db).unwrap();
        db.execute_batch("CREATE TABLE sources(source TEXT,entry TEXT,family TEXT);
            CREATE TABLE phone_pending(source TEXT, entry TEXT, signature TEXT, occurrence INTEGER,
                family TEXT, attempted_at INTEGER DEFAULT 0, error TEXT,
                PRIMARY KEY(source,entry,signature,occurrence)) WITHOUT ROWID;
            INSERT INTO phone_runs(id,source,started) VALUES(1,'phone',1),(2,'other',1);
            INSERT INTO phone_lists VALUES(1,1,3),(2,1,1);
            INSERT INTO phone_list_items VALUES
                (1,0,'a.jpg','unique',8,'1000',NULL),(1,1,'edit.aae','first',8,'1000',NULL),
                (1,2,'edit.aae','second',8,'1000',NULL),(2,0,'gone.mov','other-phone',8,'1000',NULL);
            INSERT INTO phone_pending(source,entry,signature,occurrence,family) VALUES
                ('phone','a.jpg','8/1000',1,'a'),('phone','edit.aae','8/1000',1,'edit'),
                ('phone','gone.mov','8/1000',1,'gone'),('phone','a.jpg','9/1000',1,'a');").unwrap();
        let queue = PhoneQueue::for_capture(&db, "phone").unwrap();
        assert_eq!(queue.count(), 4);
        let versions: Vec<(String, String)> = db.prepare(
            "SELECT entry,device_version FROM phone_pending WHERE device_version IS NOT NULL"
        ).unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(versions, [("a.jpg".into(), "unique".into())]);
        let mut report = crate::ingest::Report::default();
        let first = crate::ingest::inventory::restore(&db, 1, &mut report, true).unwrap().unwrap();
        let second = crate::ingest::inventory::restore(&db, 1, &mut report, true).unwrap().unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(report.list_unseen, 2);
        assert_eq!(db.query_row("SELECT COUNT(digest) FROM phone_list_items", [], |r| r.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(PhoneQueue::for_capture(&db, "phone").unwrap().count(), 4);
    }
}
