use super::{Archive, Report, Result, Target, View, catalog, inbox, recent, request_index};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter};

// Skip unchanged inventories in the idle worker. Newly received content, a new
// complete list, and an explicit retry each trigger another local check.
pub(super) type CheckedLists = HashMap<(String, i64), (i64, i64)>;

fn reconcile_saved_lists(
    probe: &Connection,
    control: &Connection,
    dest: &Target,
    label: &str,
    view: &Mutex<View>,
    checked: &mut CheckedLists,
) -> Result<()> {
    let schema: u8 = probe.query_row("SELECT count(*) FROM sqlite_schema WHERE
        (type='table' AND name IN ('phone_lists','phone_returned_data','phone_inbox','source_versions'))
        OR (type='index' AND name='phone_list_signature')", [], |r| r.get(0))?;
    if schema != 5 { return Ok(()); }
    let mut query = probe.prepare("SELECT r.id,r.source,r.started,r.sessions,r.copies,r.bytes,l.checked_at,
        (SELECT COALESCE(MAX(id),0) FROM phone_inbox) FROM phone_runs r JOIN phone_lists l ON l.run_id=r.id WHERE r.safe_at IS NULL")?;
    let runs = query.query_map([], |row| {
        let source: String = row.get(1)?;
        let report = Report {
            source: label.to_owned(),
            phase: "已核对本机接收记录".into(),
            phone: true,
            capture: true,
            device_key: Some(super::verification::device_key(&source)),
            capture_started_at: Some(row.get(2)?),
            receive_attempt: row.get(3)?,
            total_received: row.get(4)?,
            total_received_bytes: row.get(5)?,
            ..Default::default()
        };
        Ok((row.get::<_,i64>(0)?, (row.get::<_,i64>(6)?, row.get::<_,i64>(7)?), report))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (run, stamp, mut report) in runs {
        let index = (dest.root.clone(), run);
        if checked.get(&index) == Some(&stamp) { continue; }
        if inbox::saved_list::has_candidates(probe, run)? {
            let mut guard = catalog::guard_destination(dest, catalog::album_path);
            let _destination = guard()?;
            let archive = Archive::open(Path::new(&dest.root))?;
            inbox::initialize(&archive.db)?;
            let restored = inbox::saved_list::reconcile(&archive, run, stamp.0)?;
            if restored > 0 {
                report.newly_verified = restored;
                super::inventory::restore(&archive.db, run, &mut report, false)?;
                super::record_history(control, &report)?;
                let mut live = view.lock().unwrap();
                if live.target.as_ref().is_some_and(|active| active.library == dest.library && active.album == dest.album) {
                    live.report = report;
                }
                live.recent = recent(control)?;
            }
        }
        checked.insert(index, stamp);
    }
    Ok(())
}

// These jobs are keyed by catalog/album so they survive a restart, a renamed album,
// switching the active library, or unplugging the phone after the release transaction.
pub(super) fn process(
    app: &AppHandle,
    view: &Arc<Mutex<View>>,
    control: &Connection,
    unavailable: &catalog::Unavailable,
    checked: &mut CheckedLists,
) -> Result<()> {
    let jobs = control
        .prepare("SELECT library,album,label FROM phone_work")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (library, album, label) in jobs {
        if unavailable.contains_key(&(library.clone(), album)) {
            continue;
        }
        let Some(root) = catalog::album_path(&library, album)? else {
            continue;
        };
        let dest = Target {
            root,
            library,
            album,
            auto: false,
        };
        let state = Path::new(&dest.root)
            .join(".time-cut")
            .join("imports.sqlite3");
        if !state.is_file() {
            continue;
        }
        let probe = Connection::open_with_flags(state, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        probe.busy_timeout(Duration::from_secs(3))?;
        if !probe.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='phone_runs')",
            [],
            |r| r.get::<_, bool>(0),
        )? {
            continue;
        }
        if let Err(error) = reconcile_saved_lists(&probe, control, &dest, &label, view, checked) {
            view.lock().unwrap().device_error = format!("暂时无法核对本机接收记录。{error}");
        }
        if let Some((safe_at,source,started,count,inventory))=probe.query_row(
            "SELECT safe_at,source,started,discovered,inventory FROM phone_runs WHERE safe_at IS NOT NULL ORDER BY safe_at DESC LIMIT 1",
            [],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,u64>(3)?,r.get::<_,String>(4)?))).optional()? {
            let remaining=inbox::pending_local(&probe)?;
            let open=probe.query_row("SELECT EXISTS(SELECT 1 FROM phone_runs WHERE safe_at IS NULL)",[],|r|r.get::<_,bool>(0))?;
            if remaining==0 && open {continue;}
            drop(probe);
            let raw=control.query_row("SELECT data FROM history WHERE json_extract(data,'$.safe_at')=? ORDER BY id DESC LIMIT 1",
                [safe_at],|r|r.get::<_,String>(0)).optional()?;
            let mut report=if let Some(raw)=raw {serde_json::from_str(&raw)?} else {
                Report{source:label,phone:true,capture:true,safe_at:Some(safe_at),capture_started_at:Some(started),
                    discovered:count,device_key:Some(super::verification::device_key(&source)),inventory:Some(inventory),..Default::default()}
            };
            report.local_pending=remaining;
            if remaining>0 {
                control.execute("INSERT OR IGNORE INTO importing VALUES(?,?)",params![dest.library,dest.album])?;
                let mut guard=catalog::guard_destination(&dest,catalog::album_path);
                let mut archive={let _guard=guard()?;Archive::open(Path::new(&dest.root))?};
                {let mut live=view.lock().unwrap();live.busy=true;report.phase="正在本地整理".into();live.report=report.clone();}
                {let _guard=guard()?;inbox::recover(&archive)?;archive.reconcile()?;}
                let mut last=Instant::now();
                loop {
                    let _guard=guard()?;
                    let Some(added)=inbox::file_next(&mut archive)? else {break;};
                    report.local_filed+=u64::from(added);
                    report.local_duplicates+=u64::from(!added);
                    report.local_pending=report.local_pending.saturating_sub(1);
                    if last.elapsed()>=Duration::from_millis(200) {
                        view.lock().unwrap().report=report.clone();
                        last=Instant::now();
                    }
                }
                (report.local_filed,report.local_duplicates)=archive.db.query_row(
                    "SELECT filed,duplicates FROM phone_runs WHERE source=? AND safe_at=?",params![source,safe_at],|r|Ok((r.get(0)?,r.get(1)?)))?;
            }
            let tx=control.unchecked_transaction()?;
            // The marker also covers a crash just after filing the last local item.
            let interrupted=tx.execute("DELETE FROM importing WHERE library=? AND album=?",params![dest.library,dest.album])? > 0;
            if interrupted {request_index(&tx,&dest.library,dest.album)?;}
            if !open {tx.execute("DELETE FROM phone_work WHERE library=? AND album=?",params![dest.library,dest.album])?;}
            report.phase="本地原件整理完成".into();
            let raw=serde_json::to_string(&report)?;
            if tx.execute("UPDATE history SET data=? WHERE json_extract(data,'$.safe_at')=?",params![raw,safe_at])?==0 {
                tx.execute("INSERT INTO history(data) VALUES(?)",[raw])?;
            }
            tx.commit()?;
            {let mut live=view.lock().unwrap();live.busy=false;live.report=report;live.recent=recent(control)?;}
            if interrupted {let _=app.emit("timecut-index",());}
        }
    }
    Ok(())
}
