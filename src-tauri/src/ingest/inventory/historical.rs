use super::{Item, PathBuf, Report, Result};
use crate::ingest::inbox::read_time;
use rusqlite::{Connection, params};
use std::time::SystemTime;

pub(in crate::ingest) struct HistoricalItem {
    pub path: PathBuf,
    size: u64,
    modified: SystemTime,
    version: Option<String>,
}

impl HistoricalItem {
    pub fn unique_member<'a>(&self, media: &'a [Item]) -> Option<&'a Item> {
        let mut matches = media.iter().filter(|item| {
            item.relative == self.path
                && item.size == Some(self.size)
                && item.modified == Some(self.modified)
        });
        let item = matches.next()?;
        let version = item.version.as_deref().filter(|v| !v.is_empty())?;
        (matches.next().is_none() && self.version.as_deref().is_none_or(|v| v == version))
            .then_some(item)
    }
}

/// Old queues only recorded metadata. Use these fields to look for a file, never
/// to certify its contents. Every lookup needs a completely enumerated directory.
pub(super) fn restore(
    db: &Connection,
    run: i64,
    report: &mut Report,
    choose: bool,
) -> Result<Option<HistoricalItem>> {
    let mut query = db.prepare_cached(
        "SELECT p.entry,p.signature,p.device_version,p.occurrence,
           EXISTS(SELECT 1 FROM phone_pending s WHERE s.source=p.source AND s.entry=p.entry
                  AND s.signature=p.signature AND s.occurrence>1)
         FROM phone_pending p JOIN phone_runs r ON r.source=p.source WHERE r.id=?1
           AND p.occurrence>(SELECT COUNT(*) FROM phone_list_items m INDEXED BY phone_list_signature
                WHERE m.run_id=?1 AND m.entry=p.entry AND p.signature=COALESCE(CAST(m.size AS TEXT),'')||'/'||
                CASE WHEN m.modified LIKE '-%' THEN '' ELSE COALESCE(m.modified,'') END)
         ORDER BY p.last_lookup,p.entry,p.signature,p.occurrence",
    )?;
    let mut rows = query.query([run])?;
    let mut selected = None;
    report.list_unseen = 0;
    while let Some(row) = rows.next()? {
        report.list_unseen += 1;
        if !choose || selected.is_some() || row.get::<_, u32>(3)? != 1 || row.get::<_, bool>(4)? {
            continue;
        }
        let signature: String = row.get(1)?;
        let Some((size, time)) = signature.split_once('/') else {
            continue;
        };
        let Some(size) = size.parse::<u64>().ok().filter(|n| *n > 0) else {
            continue;
        };
        let Ok(modified) = read_time(time) else {
            continue;
        };
        let item = HistoricalItem {
            path: PathBuf::from(row.get::<_, String>(0)?),
            size,
            modified,
            version: row.get(2)?,
        };
        selected = Some((item, signature));
    }
    drop(rows);
    let Some((item, signature)) = selected else {
        return Ok(None);
    };
    // Rotate even when the device goes away before the folder can be opened.
    db.prepare_cached("UPDATE phone_pending SET last_lookup=? WHERE source=(SELECT source FROM phone_runs WHERE id=?)
        AND entry=? AND signature=? AND occurrence=1")?
        .execute(params![chrono::Utc::now().timestamp_millis(),run,item.path.to_string_lossy(),signature])?;
    Ok(Some(item))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn identical_metadata_is_only_a_locator_and_ambiguous_or_changed_versions_are_skipped() {
        let mut target = HistoricalItem {
            path: "DCIM/edit.aae".into(),
            size: 9,
            modified: UNIX_EPOCH,
            version: None,
        };
        let mut first = Item {
            id: "handle-1".into(),
            relative: target.path.clone(),
            size: Some(9),
            modified: Some(UNIX_EPOCH),
            version: Some("version-a".into()),
        };
        let mut other = first.clone();
        other.id = "handle-2".into();
        other.version = Some("version-b".into());
        assert!(
            target
                .unique_member(&[first.clone(), other.clone()])
                .is_none()
        );
        target.version = first.version.clone();
        assert!(
            target
                .unique_member(&[first.clone(), other.clone()])
                .is_none()
        );
        assert!(target.unique_member(std::slice::from_ref(&other)).is_none());
        assert_eq!(
            target
                .unique_member(std::slice::from_ref(&first))
                .unwrap()
                .id,
            first.id
        );
        first.version = None;
        target.version = None;
        assert!(target.unique_member(&[first]).is_none());
    }
}
