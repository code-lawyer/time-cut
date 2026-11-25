use super::{Report, Target, archive::Result};
use rusqlite::{Connection, OptionalExtension, params};

pub(super) fn device_key(identity: &str) -> String {
    blake3::hash(identity.as_bytes()).to_hex().to_string()
}

// Used by the unattended acceptance run. Only arm an existing archive with one
// known phone. The normal application then owns both scans, even across restarts.
#[cfg(windows)]
fn single_phone(archive: &Connection) -> Result<String> {
    use std::collections::BTreeSet;
    let has = |table: &str| archive.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?)",
        [table], |row|row.get::<_,bool>(0));
    let mut sources = BTreeSet::new();
    for table in ["sources", "phone_runs", "phone_inbox"] {
        if !has(table)? { continue; }
        let mut query = archive.prepare(&format!("SELECT DISTINCT source FROM {table} WHERE source LIKE 'wpd:%' OR source LIKE 'afc:%'"))?;
        for id in query.query_map([],|r|r.get::<_,String>(0))? { sources.insert(id?); }
    }
    let ambiguous = "此命令需要只归档过一台手机的图库";
    if sources.len() == 1 { return Ok(sources.into_iter().next().unwrap()); }
    let windows: Vec<_> = sources.iter().filter(|s|s.starts_with("wpd:")).collect();
    let [windows] = windows.as_slice() else { return Err(ambiguous.into()) };
    for native in sources.iter().filter(|s|s.starts_with("afc:")) {
        let supplement: bool = has("phone_native_supplements")? && has("phone_runs")? && archive.query_row(
            "SELECT EXISTS(SELECT 1 FROM phone_native_supplements n JOIN phone_runs r ON n.run_id=r.id
             WHERE n.source=? AND r.source=?)", params![native,windows],|r|r.get(0))?;
        let same_camera: bool = has("phone_camera_roots")? && archive.query_row(
            "SELECT EXISTS(SELECT 1 FROM phone_camera_roots WHERE source=?1 AND (serial=?2 OR udid=?2))",
            params![native,&windows[4..]], |r|r.get(0))?;
        if !supplement && !same_camera { return Err(ambiguous.into()); }
    }
    Ok((*windows).clone())
}

#[cfg(all(windows, test))]
#[test]
fn an_acceptance_rescan_can_arm_an_apple_only_gallery_and_rejects_unrelated_devices() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE sources(source TEXT PRIMARY KEY);
        INSERT INTO sources VALUES('afc:one')").unwrap();
    assert_eq!(single_phone(&db).unwrap(),"afc:one");
    db.execute("INSERT INTO sources VALUES('wpd:serial')",[]).unwrap();
    assert!(single_phone(&db).is_err());
    crate::ingest::inbox::initialize(&db).unwrap();
    db.execute("INSERT INTO phone_camera_roots VALUES('afc:one','SERIAL','one')",[]).unwrap();
    assert_eq!(single_phone(&db).unwrap(),"wpd:serial");
    db.execute("INSERT INTO sources VALUES('afc:another')",[]).unwrap();
    assert!(single_phone(&db).is_err());
}

#[cfg(windows)]
pub(super) fn arm_existing(control: &Connection, minimum: u64, repeat: bool) -> Result<()> {
    let tx = control.unchecked_transaction()?;
    let dest = super::target(&tx)?.ok_or("请先选择此手机的原件图库")?;
    let path = std::path::Path::new(&dest.root)
        .join(".time-cut")
        .join("imports.sqlite3");
    let archive = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let identity = single_phone(&archive)?;
    let key = device_key(&identity);
    tx.execute("INSERT INTO phone_verification(library,album,device,minimum) VALUES(?,?,?,?)
        ON CONFLICT(library,album,device) DO UPDATE SET minimum=excluded.minimum,baseline=NULL,verified_at=NULL,
        attempts=0,last_checked=NULL,last_phase=NULL
        WHERE phone_verification.minimum<>excluded.minimum OR phone_verification.verified_at IS NOT NULL",
        params![dest.library, dest.album, key, minimum])?;
    tx.execute(
        "UPDATE config SET data=json_set(data,'$.auto',json('true')) WHERE id=1",
        [],
    )?;
    if repeat {
        tx.execute(
            "INSERT OR IGNORE INTO phone_repeat VALUES(?,?,?)",
            params![dest.library, dest.album, key],
        )?;
    } else {
        tx.execute(
            "DELETE FROM phone_repeat WHERE library=? AND album=? AND device=?",
            params![dest.library, dest.album, key],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn capture_minimum(db: &Connection, dest: &Target, key: &str, scope: Option<&str>) -> Result<u64> {
    let requested: Option<u64> = db.query_row(
        "SELECT minimum FROM phone_verification WHERE library=? AND album=? AND device=? AND verified_at IS NULL",
        params![dest.library, dest.album, key], |r| r.get(0),
    ).optional()?;
    let Some(requested) = requested else { return Ok(0); };
    // Scoped runs maintain their floor in the archive. An older report can count
    // a directory that Android has since hidden, while the explicit request still applies.
    if scope.is_some() {
        return Ok(requested);
    }
    // Earlier builds kept the highest device count only in the 40-entry history.
    // An incomplete or empty session can report the old floor without rediscovering it.
    let observed: Option<u64> = db.query_row(
        "SELECT MAX(MAX(COALESCE(json_extract(data,'$.discovered'),0),
                        COALESCE(json_extract(data,'$.expected_minimum'),0)))
         FROM history WHERE json_extract(data,'$.device_key')=? AND json_extract(data,'$.album_scope') IS ?",
        params![key,scope], |r| r.get(0),
    )?;
    let floor = requested.max(observed.unwrap_or(0));
    if floor > requested {
        db.execute("UPDATE phone_verification SET minimum=MAX(minimum,?)
            WHERE library=? AND album=? AND device=? AND verified_at IS NULL",
            params![floor, dest.library, dest.album, key])?;
    }
    Ok(floor)
}

// The optional cable-attached acceptance pass must never delay the normal unplug notice.
pub(super) fn observe_requested_repeat(
    db: &Connection,
    dest: &Target,
    report: &mut Report,
) -> Result<()> {
    let Some(key) = report.device_key.clone() else {
        return Ok(());
    };
    let requested: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM phone_repeat WHERE library=? AND album=? AND device=?)",
        params![dest.library, dest.album, key],
        |r| r.get(0),
    )?;
    if !requested {
        return Ok(());
    }
    let phase = report.phase.clone();
    observe(db, dest, report)?;
    if report.safe_at.is_some() {
        report.repeat_due = !report.verified;
        report.phase = phase;
    }
    if report.verified {
        db.execute(
            "DELETE FROM phone_repeat WHERE library=? AND album=? AND device=?",
            params![dest.library, dest.album, key],
        )?;
    }
    Ok(())
}

pub(super) fn observe(db: &Connection, dest: &Target, report: &mut Report) -> Result<()> {
    let Some(key) = &report.device_key else {
        return Ok(());
    };
    let request: Option<(u64, Option<String>)> = db.query_row(
        "SELECT minimum,baseline FROM phone_verification WHERE library=? AND album=? AND device=? AND verified_at IS NULL",
        params![dest.library, dest.album, key], |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;
    let Some((minimum, baseline)) = request else {
        return Ok(());
    };
    report.expected_minimum = Some(minimum);
    let now = chrono::Utc::now().timestamp();
    let mut next_baseline = baseline;
    if let Some(inventory) = &report.inventory {
        let comparison = match &report.album_scope {
            Some(scope) => format!("{scope}/{inventory}"),
            None => inventory.clone(),
        };
        if report.discovered < minimum {
            report.phase = "等待完整媒体清单".into();
        } else if report.added == 0 && next_baseline.as_ref() == Some(&comparison) {
            report.verified = true;
        } else {
            next_baseline = Some(comparison);
            report.phase = "等待全量复扫".into();
        }
    }
    db.execute("UPDATE phone_verification SET baseline=?,verified_at=?,attempts=attempts+1,last_checked=?,last_phase=?
        WHERE library=? AND album=? AND device=?",
        params![next_baseline, report.verified.then_some(now), now, report.phase.as_str(), dest.library, dest.album, key])?;
    Ok(())
}
