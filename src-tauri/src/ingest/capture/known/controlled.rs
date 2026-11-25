use super::*;
use rusqlite::OptionalExtension;

/// Only a complete but short stream is a candidate for this test. An ordinary
/// USB error has no complete content fingerprint. Never follow the target
/// with another file on the same session because the driver may replay it.
pub(super) fn prepare(db: &Connection, run: i64) -> Result<Option<Vec<Target>>> {
    let selected = db.prepare_cached(
        "SELECT m.ordinal,m.entry,m.version,m.size,m.modified,m.digest,
                MAX(o.first_controlled IS NOT NULL)
         FROM phone_returned_data o
         JOIN phone_list_items m INDEXED BY phone_list_signature
           ON m.run_id=o.run_id AND m.entry=o.entry AND m.version=o.version AND m.size=o.reported_size
         LEFT JOIN phone_item_retries t ON t.run_id=m.run_id AND t.entry=m.entry AND t.version=m.version
         WHERE o.run_id=?1 AND o.returned_size<o.reported_size AND m.digest IS NULL AND m.modified IS NOT NULL
           AND (lower(m.entry) LIKE '%.jpg' OR lower(m.entry) LIKE '%.jpeg')
           AND NOT EXISTS(SELECT 1 FROM phone_list_items s INDEXED BY phone_list_signature
               WHERE s.run_id=m.run_id AND s.entry=m.entry AND s.ordinal<>m.ordinal)
         GROUP BY m.ordinal
         HAVING MAX(COALESCE(o.last_controlled>o.first_controlled,0))=0
         ORDER BY COALESCE(t.attempted,0),m.ordinal LIMIT 1",
    )?.query_row([run], |row| Ok((target_from_row(row)?, row.get::<_,bool>(6)?))).optional()?;
    let Some((target, reverse)) = selected else {
        return Ok(None);
    };
    let Some(parent) = target.item.relative.parent() else {
        return Ok(None);
    };
    let prefix = if parent.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}{}", parent.display(), std::path::MAIN_SEPARATOR)
    };
    let upper = format!("{prefix}{}", char::MAX);
    let mut q = db.prepare_cached(
        "SELECT m.ordinal,m.entry,m.version,m.size,m.modified,m.digest FROM phone_list_items m INDEXED BY phone_list_signature
         WHERE m.run_id=?1 AND m.entry>=?2 AND m.entry<?3 AND m.digest IS NOT NULL
           AND m.size BETWEEN 4096 AND 262144 AND m.modified IS NOT NULL AND m.version IS NOT NULL
           AND NOT EXISTS(SELECT 1 FROM phone_list_items s INDEXED BY phone_list_signature
               WHERE s.run_id=m.run_id AND s.entry=m.entry AND s.ordinal<>m.ordinal)
           AND NOT EXISTS(SELECT 1 FROM phone_returned_data o WHERE o.run_id=m.run_id
               AND o.entry=?4 AND o.version=?5 AND o.digest=m.digest)
         ORDER BY m.size,m.ordinal LIMIT 16",
    )?;
    let mut controls = Vec::new();
    let mut digests = HashSet::new();
    for row in q.query_map(
        params![
            run,
            prefix,
            upper,
            target.item.relative.to_string_lossy(),
            target.item.version
        ],
        target_from_row,
    )? {
        let candidate = row?;
        if candidate.item.relative.parent() == Some(parent)
            && digests.insert(candidate.reference.clone())
        {
            controls.push(candidate);
            if controls.len() == 2 {
                break;
            }
        }
    }
    if controls.len() < 2 {
        return Ok(None);
    }
    if reverse {
        controls.reverse();
    }
    // Rotate even when a bad response to the first control prevents opening the target.
    db.prepare_cached("INSERT OR REPLACE INTO phone_item_retries VALUES(?,?,?,?)")?
        .execute(params![
            run,
            target.item.relative.to_string_lossy(),
            target.item.version,
            chrono::Utc::now().timestamp_micros()
        ])?;
    controls.push(target);
    Ok(Some(controls))
}
