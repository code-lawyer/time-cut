mod archive;
mod audit_archive;
#[cfg(windows)]
mod afc;
mod capture;
mod inbox;
mod inventory;
#[cfg(test)]
mod legacy_fixture;
mod local_work;
#[cfg(windows)]
mod notice;
pub(crate) use archive::is_undated;
pub(crate) mod catalog;
#[cfg(test)]
mod lifecycle_tests;
mod resume;
#[cfg(test)]
mod tests;
mod verification;
#[cfg(windows)]
mod wpd;

use archive::{Archive, Folder, Result, ScanEvent, Source};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    root: String,
    album: i64,
    library: String,
    auto: bool,
}

// The history and frontend use the original string representation. Scheduling
// uses the states below so that new display messages cannot change retry rules.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
enum Phase {
    #[default]
    Idle,
    Completed,
    Pending,
    AwaitAppleDirectory,
    MediaUnavailable,
    SessionInterrupted,
    ContentMismatch,
    AppleNeedsSupplement,
    Message(String),
}

impl Phase {
    fn as_str(&self) -> &str {
        match self {
            Self::Idle => "",
            Self::Completed => "已完成",
            Self::Pending => "仍有素材待补",
            Self::AwaitAppleDirectory => "等待 Apple 原件目录",
            Self::MediaUnavailable => "手机媒体暂不可读",
            Self::SessionInterrupted => "手机会话中断",
            Self::ContentMismatch => "手机返回内容与清单不符",
            Self::AppleNeedsSupplement => "Apple 相机原件待补",
            Self::Message(text) => text,
        }
    }
}

impl From<String> for Phase {
    fn from(text: String) -> Self {
        [
            Self::Idle,
            Self::Completed,
            Self::Pending,
            Self::AwaitAppleDirectory,
            Self::MediaUnavailable,
            Self::SessionInterrupted,
            Self::ContentMismatch,
            Self::AppleNeedsSupplement,
        ]
        .into_iter()
        .find(|phase| phase.as_str() == text.as_str())
        .unwrap_or(Self::Message(text))
    }
}

impl From<&str> for Phase {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl Serialize for Phase {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    source: String,
    phase: Phase,
    current: String,
    scanned: u64,
    discovered: u64,
    transferred_bytes: u64,
    total_bytes: Option<u64>,
    added: u64,
    skipped: u64,
    failed: u64,
    phone: bool,
    capture: bool,
    capture_started_at: Option<i64>,
    safe_at: Option<i64>,
    received_bytes: u64,
    compared_bytes: u64,
    receive_attempt: u64,
    total_received: u64,
    total_received_bytes: u64,
    local_pending: u64,
    local_errors: u64,
    local_error: Option<String>,
    local_filed: u64,
    local_duplicates: u64,
    checked_folders: u64,
    total_folders: u64,
    list_at: Option<i64>,
    list_total: Option<u64>,
    list_covered: u64,
    newly_verified: u64,
    list_unseen: u64,
    source_mismatches: u64,
    #[cfg(windows)]
    native_originals: Option<afc::Audit>,
    rechecked: u64,
    listed_folders: u64,
    known_folders: u64,
    listed_items: u64,
    partial_coverage: Option<inventory::Coverage>,
    export_shortfall: Option<inventory::Shortfall>,
    pending: u64,
    device_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_scope: Option<String>,
    inventory: Option<String>,
    verified: bool,
    repeat_due: bool,
    expected_minimum: Option<u64>,
    issues: Vec<String>,
}

#[derive(Clone, Default, Serialize)]
pub struct View {
    target: Option<Target>,
    devices: Vec<String>,
    device_error: String,
    busy: bool,
    next_retry_at: Option<i64>,
    apple_retry_at: Option<i64>,
    report: Report,
    recent: Vec<Report>,
}

#[derive(Clone, Serialize)]
pub struct IndexJob {
    library: String,
    album: i64,
    revision: i64,
}

enum Wake {
    Check,
    Retry,
}
pub struct Importer {
    view: Arc<Mutex<View>>,
    signal: mpsc::Sender<Wake>,
}

fn control() -> Result<Connection> {
    let dir = crate::t_config::get_app_data_dir()?;
    std::fs::create_dir_all(&dir)?;
    let db = Connection::open(dir.join("import-control.sqlite3"))?;
    db.busy_timeout(Duration::from_secs(5))?;
    initialize_control(&db)?;
    Ok(db)
}

fn initialize_control(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS config(id INTEGER PRIMARY KEY, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS queue(id INTEGER PRIMARY KEY, path TEXT NOT NULL, target TEXT NOT NULL, UNIQUE(path,target));
        CREATE TABLE IF NOT EXISTS history(id INTEGER PRIMARY KEY, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS indexing(library TEXT, album INTEGER, revision INTEGER, PRIMARY KEY(library,album));
        CREATE TABLE IF NOT EXISTS index_file_retries(library TEXT NOT NULL,album INTEGER NOT NULL,path TEXT NOT NULL,
            PRIMARY KEY(library,album,path)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS importing(library TEXT, album INTEGER, PRIMARY KEY(library,album));
        CREATE TABLE IF NOT EXISTS phone_work(library TEXT NOT NULL,album INTEGER NOT NULL,label TEXT NOT NULL,PRIMARY KEY(library,album));
        CREATE TABLE IF NOT EXISTS phone_repeat(library TEXT NOT NULL,album INTEGER NOT NULL,device TEXT NOT NULL,
            PRIMARY KEY(library,album,device));
        CREATE TABLE IF NOT EXISTS phone_verification(library TEXT NOT NULL, album INTEGER NOT NULL,
            device TEXT NOT NULL, minimum INTEGER NOT NULL, baseline TEXT, verified_at INTEGER,
            attempts INTEGER NOT NULL DEFAULT 0, last_checked INTEGER, last_phase TEXT,
            PRIMARY KEY(library,album,device));")?;
    Ok(())
}

fn target(db: &Connection) -> Result<Option<Target>> {
    let raw: Option<String> = db
        .query_row("SELECT data FROM config WHERE id=1", [], |r| r.get(0))
        .optional()?;
    Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?)
}
fn recent(db: &Connection) -> Result<Vec<Report>> {
    let mut query = db.prepare("SELECT data FROM history ORDER BY id DESC LIMIT 6")?;
    let mut reports = vec![];
    for row in query.query_map([], |r| r.get::<_, String>(0))? {
        reports.push(serde_json::from_str(&row?)?);
    }
    Ok(reports)
}
fn request_index(db: &Connection, library: &str, album: i64) -> Result<()> {
    // Monotonic across acknowledgments. A late scan can never acknowledge a newer transfer.
    let rev = chrono::Utc::now().timestamp_micros();
    db.execute("INSERT INTO indexing VALUES(?,?,?) ON CONFLICT(library,album) DO UPDATE SET revision=MAX(excluded.revision,indexing.revision+1)",
        params![library, album, rev])?;
    Ok(())
}

fn retry_interrupted_index_file(
    db: &Connection,
    library: &str,
    album: i64,
    path: &str,
) -> Result<bool> {
    Ok(db.execute(
        "INSERT OR IGNORE INTO index_file_retries(library,album,path)
        SELECT library,album,?3 FROM indexing WHERE library=?1 AND album=?2",
        params![library, album, path],
    )? == 1)
}

fn acknowledge_index(db: &Connection, library: &str, album: i64, revision: i64) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    if tx.execute(
        "DELETE FROM indexing WHERE library=? AND album=? AND revision=?",
        params![library, album, revision],
    )? > 0
    {
        tx.execute(
            "DELETE FROM index_file_retries WHERE library=? AND album=?",
            params![library, album],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn recover_interrupted_imports(db: &Connection) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    {
        let mut rows = tx.prepare("SELECT library,album FROM importing")?;
        for old in rows.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (library, album) = old?;
            request_index(&tx, &library, album)?;
        }
    }
    tx.execute("DELETE FROM importing", [])?;
    tx.commit()?;
    Ok(())
}

fn register(app: &AppHandle, root: &Path) -> Result<(String, i64)> {
    if !root.is_absolute() || !root.is_dir() {
        return Err("请选择一个存在的绝对文件夹路径".into());
    }
    let canon = archive::media_directory(root)?;
    let library = crate::t_config::current_library_id()?;
    let id = crate::t_cmds::with_current_library(&library, || {
        for album in crate::t_sqlite::Album::get_all_albums()? {
            if Path::new(&album.path).canonicalize().ok().as_ref() == Some(&canon) {
                return album.id.ok_or_else(|| "相册缺少编号".into());
            }
        }
        let name = root
            .file_name()
            .unwrap_or(root.as_os_str())
            .to_string_lossy()
            .into_owned();
        crate::t_cmds::add_album(
            app.clone(),
            &root.to_string_lossy(),
            name,
            String::new(),
            7,
            0,
            vec![".time-cut".into()],
        )?
        .id
        .ok_or_else(|| "无法登记相册编号".into())
    })?;
    Ok((library, id))
}

#[tauri::command]
pub fn timecut_status(state: State<Importer>) -> View {
    state.view.lock().unwrap().clone()
}

#[tauri::command]
pub fn timecut_tray_active(app: AppHandle) -> bool {
    #[cfg(windows)]
    {
        app.tray_by_id("timecut").is_some()
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        false
    }
}

#[tauri::command]
pub async fn timecut_configure(
    app: AppHandle,
    path: String,
    auto: bool,
) -> std::result::Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        let state = app.state::<Importer>();
        let root = PathBuf::from(&path);
        if !root.is_absolute() || !root.is_dir() {
            return Err("请选择存在的图库目录".into());
        }
        let busy = {
            let view = state.view.lock().unwrap();
            if view.busy && view.target.as_ref().is_none_or(|t| t.root != path) {
                return Err("请等本次导入完成后再更换图库".into());
            }
            view.busy
        };
        if !busy {
            drop(Archive::open(&root)?);
        }
        let (library, album) = register(&app, &root)?;
        let registered_path =
            catalog::album_path(&library, album)?.ok_or("接收原件的相册已移除")?;
        let chosen = Target {
            root: registered_path,
            album,
            library,
            auto,
        };
        let db = control()?;
        let mut view = state.view.lock().unwrap();
        if view.busy && view.target.as_ref().is_none_or(|t| t.root != chosen.root) {
            return Err("请等本次导入完成后再更换图库".into());
        }
        let tx = db.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO config VALUES(1,?)",
            [serde_json::to_string(&chosen)?],
        )?;
        request_index(&tx, &chosen.library, chosen.album)?;
        tx.commit()?;
        view.target = Some(chosen);
        drop(view);
        let _ = state.signal.send(Wake::Retry);
        let _ = app.emit("timecut-index", ());
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn timecut_set_auto(state: State<Importer>, auto: bool) -> std::result::Result<(), String> {
    (|| -> Result<()> {
        let db = control()?;
        let mut view = state.view.lock().unwrap();
        if db.execute(
            "UPDATE config SET data=json_set(data,'$.auto',json(?)) WHERE id=1",
            [if auto { "true" } else { "false" }],
        )? == 0
        {
            return Err("请先选定接收原件的图库目录".into());
        }
        view.target = target(&db)?;
        view.next_retry_at = None;
        view.apple_retry_at = None;
        drop(view);
        state.signal.send(Wake::Retry)?;
        Ok(())
    })()
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn timecut_folder(
    app: AppHandle,
    path: String,
    index_only: bool,
) -> std::result::Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        let db = control()?;
        if index_only {
            let (library, album) = register(&app, Path::new(&path))?;
            request_index(&db, &library, album)?;
            let _ = app.emit("timecut-index", ());
        } else {
            let mut target = target(&db)?.ok_or("请先选定接收原件的图库目录")?;
            target.root = catalog::album_path(&target.library, target.album)?
                .ok_or("接收原件的相册已移除，请重新选择图库")?;
            Folder::new(Path::new(&path), Path::new(&target.root))?;
            db.execute(
                "INSERT OR IGNORE INTO queue(path,target) VALUES(?,?)",
                params![path, serde_json::to_string(&target)?],
            )?;
            let _ = app.state::<Importer>().signal.send(Wake::Check);
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn timecut_retry(state: State<Importer>) -> std::result::Result<(), String> {
    state.signal.send(Wake::Retry).map_err(|e| e.to_string())
}

#[cfg(windows)]
pub(crate) fn wake_phone_import(app: &AppHandle) {
    if let Some(state) = app.try_state::<Importer>() {
        let _ = state.signal.send(Wake::Retry);
    }
}

#[tauri::command]
pub fn timecut_indexes() -> std::result::Result<Vec<IndexJob>, String> {
    (|| -> Result<_> {
        let db = control()?;
        let mut q = db.prepare("SELECT library,album,revision FROM indexing")?;
        Ok(q.query_map([], |r| {
            Ok(IndexJob {
                library: r.get(0)?,
                album: r.get(1)?,
                revision: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    })()
    .map_err(|e| e.to_string())
}
#[tauri::command]
pub fn timecut_retry_interrupted_index(
    library: String,
    album: i64,
) -> std::result::Result<bool, String> {
    crate::t_cmds::with_current_library(&library, || {
        let Some(trace) = crate::t_utils::read_index_trace()
            .filter(|t| t.album_id == album && !t.file_path.is_empty())
        else {
            return Ok(false);
        };
        control()
            .and_then(|db| retry_interrupted_index_file(&db, &library, album, &trace.file_path))
            .map_err(|e| e.to_string())
    })
}

#[tauri::command]
pub fn timecut_indexed(
    library: String,
    album: i64,
    revision: i64,
) -> std::result::Result<(), String> {
    control()
        .and_then(|db| acknowledge_index(&db, &library, album, revision))
        .map_err(|e| e.to_string())
}

fn issue(report: &mut Report, message: String) {
    report.failed += 1;
    if report.issues.len() < 12 {
        report.issues.push(message);
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
enum TransferStop {
    AutoDisabled,
    DestinationChanged,
    Disconnected,
    SessionInterrupted,
    ContentMismatch,
}

#[cfg(windows)]
impl TransferStop {
    fn phase(self) -> Phase {
        match self {
            Self::AutoDisabled => "已停止自动导入".into(),
            Self::DestinationChanged => "图库已更改，等待重新检测".into(),
            Self::Disconnected => "设备已断开".into(),
            Self::SessionInterrupted => Phase::SessionInterrupted,
            Self::ContentMismatch => Phase::ContentMismatch,
        }
    }
}

#[cfg(windows)]
impl std::fmt::Display for TransferStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.phase().as_str())
    }
}

#[cfg(windows)]
impl std::error::Error for TransferStop {}

fn copy_folder<Guard>(
    root: &Path,
    path: &Path,
    mut update: impl FnMut(&Report),
    mut check_destination: impl FnMut() -> Result<Guard>,
    mut check_active: impl FnMut() -> Result<()>,
) -> Report {
    let mut report = Report {
        source: path.to_string_lossy().into_owned(),
        phase: "检查图库".into(),
        ..Default::default()
    };
    update(&report);
    let run = (|| -> Result<()> {
        check_active()?;
        let source = Folder::new(path, root)?;
        let mut archive = {
            let _guard = check_destination()?;
            Archive::open(root)?
        };
        let mut reconciled = false;
        report.phase = "正在读取".into();
        update(&report);
        let mut last = Instant::now();
        let mut publish = |status: &Report, immediately: bool| {
            if immediately || last.elapsed() >= Duration::from_millis(150) {
                update(status);
                last = Instant::now();
            }
        };
        source.visit(&mut |event| {
            check_active()?;
            let item = match event {
                ScanEvent::Inspected { media } => {
                    report.scanned += 1;
                    report.discovered += u64::from(media);
                    publish(&report, report.scanned == 1);
                    return Ok(());
                }
                ScanEvent::Unreadable(message) => {
                    issue(&mut report, message);
                    return Ok(());
                }
                ScanEvent::Ready(item) => item,
                _ => return Ok(()),
            };
            let _guard = check_destination()?;
            if !reconciled {
                archive.reconcile()?;
                reconciled = true;
            }
            report.phase = "正在复制".into();
            report.current = item.relative.to_string_lossy().into_owned();
            report.total_bytes = item.size;
            publish(&report, report.added + report.skipped == 0);
            let result = archive.import(&source, &item, |bytes| {
                report.transferred_bytes = bytes;
                publish(&report, false);
                check_active()
            });
            let first_original = match result {
                Ok(true) => {
                    report.added += 1;
                    report.added == 1
                }
                Ok(false) => {
                    report.skipped += 1;
                    false
                }
                Err(error) => {
                    #[cfg(windows)]
                    if error.is::<TransferStop>() {
                        return Err(error);
                    }
                    check_active()?;
                    let message = format!("{}：{error}", report.current);
                    issue(&mut report, message);
                    false
                }
            };
            report.phase = "正在读取".into();
            report.current.clear();
            report.total_bytes = None;
            report.transferred_bytes = 0;
            publish(&report, first_original);
            Ok(())
        })
    })();
    report.current.clear();
    report.total_bytes = None;
    report.transferred_bytes = 0;
    if let Err(error) = run {
        #[cfg(windows)]
        if let Some(stop) = error.downcast_ref::<TransferStop>() {
            report.phase = stop.phase();
            return report;
        }
        issue(&mut report, error.to_string());
    }
    report.phase = if report.failed > 0 {
        "部分未完成".into()
    } else if report.added + report.skipped == 0 {
        "未发现可读照片或视频".into()
    } else {
        Phase::Completed
    };
    report
}

#[derive(Clone, Copy)]
enum ImportSource<'a> {
    Folder(&'a Path),
    #[cfg(windows)]
    Phone(&'a wpd::Device),
}

fn job(
    app: &AppHandle,
    view: &Arc<Mutex<View>>,
    dest: &Target,
    source: ImportSource<'_>,
    check_active: impl FnMut() -> Result<()>,
) -> Result<Report> {
    let (label, phone) = match source {
        ImportSource::Folder(path) => (path.to_string_lossy(), false),
        #[cfg(windows)]
        ImportSource::Phone(device) => (std::borrow::Cow::Borrowed(device.name.as_str()), true),
    };
    // A crash may occur after publishing an original and before requesting an index. The
    // durable marker covers this entire window and disappears in the completion transaction.
    let db = control()?;
    if phone {
        #[cfg(windows)]
        if let Some(tray) = app.tray_by_id("timecut") {
            let _ = tray.set_tooltip(Some("Time-cut · 正在核对手机原件"));
        }
        db.execute(
            "INSERT OR REPLACE INTO phone_work VALUES(?,?,?)",
            params![dest.library, dest.album, label.as_ref()],
        )?;
    } else {
        db.execute(
            "INSERT OR IGNORE INTO importing VALUES(?,?)",
            params![dest.library, dest.album],
        )?;
    }
    let mut index_at = 1;
    let progress = |p: &Report| {
        {
            let mut view = view.lock().unwrap();
            view.busy = true;
            view.next_retry_at = None;
            view.report = p.clone();
        }
        if !phone && p.added >= index_at {
            // Show the first original promptly. Double later thresholds so a
            // growing library is not rescanned after every new file.
            index_at = p
                .added
                .saturating_add(1)
                .checked_next_power_of_two()
                .unwrap_or(u64::MAX)
                .max(32);
            match request_index(&db, &dest.library, dest.album) {
                Ok(()) => {
                    let _ = app.emit("timecut-index", ());
                }
                Err(e) => eprintln!("Time-cut early index request: {e}"),
            }
        }
    };
    let guard = catalog::guard_destination(dest, catalog::album_path);
    let root = Path::new(&dest.root);
    let mut report = match source {
        ImportSource::Folder(path) => copy_folder(root, path, progress, guard, check_active),
        #[cfg(windows)]
        ImportSource::Phone(device) => capture::receive(
            root,
            &label,
            |(key, scope)| verification::capture_minimum(&db, dest, key, scope),
            || Ok(Box::new(wpd::Phone::open(device)?)),
            progress,
            guard,
            check_active,
        ),
    };
    let tx = db.unchecked_transaction()?;
    if phone {
        verification::observe_requested_repeat(&tx, dest, &mut report)?;
    }
    if !phone && report.added > 0 {
        request_index(&tx, &dest.library, dest.album)?;
    }
    tx.execute(
        "DELETE FROM importing WHERE library=? AND album=?",
        params![dest.library, dest.album],
    )?;
    record_history(&tx, &report)?;
    tx.commit()?;
    {
        let mut live = view.lock().unwrap();
        live.busy = false;
        live.report = report.clone();
    }
    if !phone && report.added > 0 {
        let _ = app.emit("timecut-index", ());
    }
    if report.safe_at.is_some() {
        let _ = app.emit("timecut-originals-safe", &report);
        #[cfg(windows)]
        notice::originals_safe(app);
    }
    view.lock().unwrap().recent = recent(&db)?;
    Ok(report)
}

fn record_history(db: &Connection, report: &Report) -> Result<()> {
    let encoded = serde_json::to_string(report)?;
    if report.added == 0 && report.skipped == 0 {
        let last: Option<(i64, String)> = db.query_row(
            "SELECT id,data FROM history ORDER BY id DESC LIMIT 1", [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        if let Some((id, raw)) = last {
            if raw == encoded { return Ok(()); }
            if report.capture && report.safe_at.is_none() {
                if let Ok(mut previous) = serde_json::from_str::<Report>(&raw) {
                    previous.receive_attempt = report.receive_attempt;
                    if previous == *report {
                        db.execute("UPDATE history SET data=? WHERE id=?", params![encoded,id])?;
                        return Ok(());
                    }
                }
            }
        }
    }
    db.execute("INSERT INTO history(data) VALUES(?)", [encoded])?;
    db.execute("DELETE FROM history WHERE id NOT IN (SELECT id FROM history ORDER BY id DESC LIMIT 40)", [])?;
    Ok(())
}

fn next_folder(
    db: &Connection,
    unavailable: &catalog::Unavailable,
) -> Result<Option<(i64, String, Target)>> {
    let mut jobs = db.prepare("SELECT id,path,target FROM queue ORDER BY id")?;
    let mut rows = jobs.query([])?;
    while let Some(row) = rows.next()? {
        let dest: Target = serde_json::from_str(&row.get::<_, String>(2)?)?;
        if !unavailable.contains_key(&(dest.library.clone(), dest.album)) {
            return Ok(Some((row.get(0)?, row.get(1)?, dest)));
        }
    }
    Ok(None)
}

#[cfg(windows)]
fn next_phone_retry(report: &Report, previous: Option<Duration>) -> Option<Duration> {
    if report.safe_at.is_some() {
        return report.repeat_due.then_some(Duration::from_secs(3));
    }
    if report.failed == 0 && report.pending == 0 && report.phase == Phase::Completed {
        return None;
    }
    let first = Duration::from_secs(20);
    if report.newly_verified > 0 && report.failed == 0 && report.phase == Phase::Pending {
        return Some(Duration::from_secs(3));
    }
    Some(if report.added > 0 || report.newly_verified > 0 {
        first
    } else {
        previous.map_or(first, |delay| (delay * 2).min(Duration::from_secs(300)))
    })
}

fn worker(app: &AppHandle, view: &Arc<Mutex<View>>, rx: &mpsc::Receiver<Wake>) -> Result<()> {
    #[cfg(windows)]
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(windows)]
    let removed = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    let _notifications =
        wpd::Notifications::register(app.state::<Importer>().signal.clone(), removed.clone());
    #[cfg(windows)]
    let apartment = wpd::Apartment::new();
    let db = control()?;
    recover_interrupted_imports(&db)?;
    {
        let mut live = view.lock().unwrap();
        live.target = target(&db)?;
        live.recent = recent(&db)?;
    }
    let mut completed = HashMap::<String, Option<(Instant, Duration)>>::new();
    #[cfg(windows)]
    let mut native_checks = HashMap::<String,Instant>::new();
    #[cfg(windows)]
    let mut next_unlisted_probe = Instant::now();
    #[cfg(windows)]
    let mut camera_delay = Duration::from_secs(30);
    #[cfg(windows)]
    let mut camera_error = String::new();
    #[cfg(windows)]
    let mut finished_camera = None;
    let mut known_generation = None;
    let mut unavailable = catalog::Unavailable::new();
    let mut retry_catalogs = Instant::now();
    let mut checked_lists = local_work::CheckedLists::default();
    loop {
        let generation = catalog::generation()?;
        if known_generation != Some(generation)
            || (!unavailable.is_empty() && Instant::now() >= retry_catalogs)
        {
            unavailable = catalog::synchronize(&db, catalog::album_path)?;
            {
                let mut live = view.lock().unwrap();
                live.target = target(&db)?;
            }
            known_generation = Some(generation);
            retry_catalogs = Instant::now() + Duration::from_secs(20);
            completed.clear();
            checked_lists.clear();
            #[cfg(windows)]
            {
                next_unlisted_probe = Instant::now();
                camera_delay = Duration::from_secs(30);
                camera_error.clear();
                view.lock().unwrap().apple_retry_at = None;
                finished_camera = None;
            }
        }
        local_work::process(app, view, &db, &unavailable, &mut checked_lists)?;
        if let Some((id, path, dest)) = next_folder(&db, &unavailable)? {
            match catalog::album_path(&dest.library, dest.album) {
                Ok(Some(current)) if current == dest.root => (),
                Ok(_) => {
                    known_generation = None;
                    continue;
                }
                Err(error) => {
                    unavailable.insert((dest.library, dest.album), error.to_string());
                    retry_catalogs = Instant::now() + Duration::from_secs(20);
                    continue;
                }
            }
            job(
                app, view, &dest, ImportSource::Folder(Path::new(&path)), || Ok(()),
            )?;
            // A rename/remove during a transfer is resolved at the top of the next pass.
            // Keep the durable request so that a moved destination resumes automatically.
            if generation == catalog::generation()? {
                db.execute("DELETE FROM queue WHERE id=?", [id])?;
            }
            continue;
        }
        #[cfg(windows)]
        let detected = apartment
            .as_ref()
            .map_err(|e| e.to_string())
            .and_then(|_| wpd::devices().map_err(|e| e.to_string()));
        #[cfg(windows)]
        let windows_unlisted = detected.as_ref().map_or(true, |devices| !devices.iter().any(wpd::Device::is_apple));
        #[cfg(windows)]
        match detected {
            Err(error) => {
                view.lock().unwrap().device_error = error;
            }
            Ok(devices) => {
                {
                    let mut live = view.lock().unwrap();
                    if !windows_unlisted {
                        camera_error.clear();
                        live.apple_retry_at = None;
                    }
                    live.device_error = unavailable
                        .values()
                        .next()
                        .map(|message| format!("暂时无法读取相册索引，将自动重试。{message}"))
                        .unwrap_or_else(|| camera_error.clone());
                    live.devices = devices.iter().map(|d| d.name.clone()).collect();
                    if windows_unlisted && finished_camera.as_ref().is_some_and(|camera: &afc::desktop::FinishedCamera| camera.belongs_to(live.target.as_ref())) {
                        live.devices.push(afc::desktop::CAMERA_LABEL.into());
                    }
                    if !live.devices.contains(&live.report.source) {
                        live.next_retry_at = None;
                    }
                }
                completed.retain(|id, _| devices.iter().any(|d| &d.id == id));
                native_checks.retain(|id, _| devices.iter().any(|d| &d.id == id));
                if let Some(dest) = target(&db)?
                    .filter(|t| t.auto && !unavailable.contains_key(&(t.library.clone(), t.album)))
                {
                    for d in devices {
                        if view.lock().unwrap().target.as_ref() != Some(&dest) {
                            break;
                        }
                        if completed
                            .get(&d.id)
                            .is_some_and(|retry| retry.is_none_or(|(at, _)| at > Instant::now()))
                        {
                            continue;
                        }
                        let report = job(
                            app,
                            view,
                            &dest,
                            ImportSource::Phone(&d),
                            || {
                                {
                                    let live = view.lock().unwrap();
                                    match live.target.as_ref() {
                                        Some(current) if current == &dest => (),
                                        Some(current) if current.auto => {
                                            return Err(TransferStop::DestinationChanged.into());
                                        }
                                        _ => return Err(TransferStop::AutoDisabled.into()),
                                    }
                                }
                                if removed.swap(false, Ordering::AcqRel)
                                    && wpd::devices().is_ok_and(|list| {
                                        list.iter().all(|device| device.id != d.id)
                                    })
                                {
                                    return Err(TransferStop::Disconnected.into());
                                }
                                Ok(())
                            },
                        )?;
                        let find_camera = report.phase == Phase::AwaitAppleDirectory
                            || report.device_key.is_none();
                        if (find_camera || afc::supplement::stalled(&report))
                            && d.is_apple()
                            && native_checks.get(&d.id).is_none_or(|last| last.elapsed() >= Duration::from_secs(120)) {
                            native_checks.insert(d.id.clone(), Instant::now());
                            let windows_report = if find_camera { None } else { Some(&report) };
                            if let Err(error) = afc::desktop::receive(app,view,&db,&dest,windows_report,&mut finished_camera) {
                                view.lock().unwrap().device_error = format!("Apple 相机原件暂时无法读取。{error}");
                            }
                        }
                        let previous = completed.get(&d.id).copied().flatten().map(|(_, d)| d);
                        let retry = next_phone_retry(&report, previous)
                            .map(|delay| (Instant::now() + delay, delay));
                        view.lock().unwrap().next_retry_at = retry.map(|(_, delay)| {
                            chrono::Utc::now().timestamp() + delay.as_secs() as i64
                        });
                        completed.insert(d.id, retry);
                    }
                }
            }
        }
        // A paired camera may be the only source Apple publishes on Windows.
        #[cfg(windows)]
        if windows_unlisted && Instant::now() >= next_unlisted_probe {
            let mut delay = Duration::from_secs(30);
            if let Some(dest) = target(&db)?.filter(|t| t.auto && !unavailable.contains_key(&(t.library.clone(), t.album))) {
                view.lock().unwrap().apple_retry_at = None;
                camera_error.clear();
                let interrupted = afc::desktop::receive(app,view,&db,&dest,None,&mut finished_camera).unwrap_or_else(|error| {
                    camera_error = format!("Apple 相机原件暂时无法读取。{error}");
                    true
                });
                let mut live = view.lock().unwrap();
                if live.target.as_ref() == Some(&dest) {
                    live.device_error = camera_error.clone();
                    if interrupted {
                        delay = camera_delay;
                        camera_delay = (camera_delay*2).min(Duration::from_secs(300));
                        live.apple_retry_at = Some(chrono::Utc::now().timestamp() + delay.as_secs() as i64);
                    } else { camera_delay = Duration::from_secs(30); }
                }
            }
            next_unlisted_probe = Instant::now() + delay;
        }
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(wake) => {
                let mut retry = matches!(wake, Wake::Retry);
                for wake in rx.try_iter() {
                    retry |= matches!(wake, Wake::Retry);
                }
                if retry {
                    known_generation = None;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            _ => (),
        }
    }
}

pub fn start(app: &AppHandle) {
    let (signal, rx) = mpsc::channel();
    let mut initial = View::default();
    if let Ok(db) = control() {
        initial.target = target(&db).unwrap_or_default();
        initial.recent = recent(&db).unwrap_or_default();
    }
    let view = Arc::new(Mutex::new(initial));
    app.manage(Importer {
        signal,
        view: view.clone(),
    });
    let app = app.clone();
    let runtime = tauri::async_runtime::handle();
    std::thread::spawn(move || {
        // The metadata reader runs ffprobe on Tokio. Keep COM and file transfers on this thread.
        let _runtime_context = runtime.inner().enter();
        while let Err(e) = worker(&app, &view, &rx) {
            {
                let mut live = view.lock().unwrap();
                live.busy = false;
                live.device_error = format!("导入服务暂时不可用，将自动重试。{e}");
            }
            if matches!(
                rx.recv_timeout(Duration::from_secs(5)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ) {
                return;
            }
        }
    });
}

#[cfg(windows)]
pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg(windows)]
pub fn tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri::{
        menu::{Menu, MenuItem},
        tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    };
    let open = MenuItem::with_id(app, "timecut-open", "打开 Time-cut", true, None::<&str>)?;
    let exit = MenuItem::with_id(app, "timecut-exit", "退出 Time-cut", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &exit])?;
    let mut builder = TrayIconBuilder::with_id("timecut")
        .tooltip("Time-cut · 本地相簿")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, e| match e.id.as_ref() {
            "timecut-open" => show_window(app),
            "timecut-exit" => app.exit(0),
            _ => (),
        })
        .on_tray_icon_event(move |t, e| {
            if matches!(
                e,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_window(t.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// The portable build exposes the same ingest path for diagnostics and unattended testing.
pub fn command_line() -> Option<i32> {
    let args: Vec<_> = std::env::args_os().collect();
    match args.get(1)?.to_str()? {
        "--verify-archive" => {
            if args.len() != 3 { return Some(1); }
            match audit_archive::verify(Path::new(&args[2])) {
                Ok(result) => {
                    println!("{}", serde_json::to_string(&result).expect("serialize archive totals"));
                    Some(i32::from(result.failed != 0 || result.files_verified == 0))
                }
                Err(error) => { eprintln!("{error}"); Some(1) }
            }
        }
        #[cfg(windows)]
        mode @ ("--probe-iphone-originals" | "--check-iphone-originals") => {
            let root=args.get(2).map(PathBuf::from);
            let probe=mode=="--probe-iphone-originals";
            if args.len() != if probe {2} else {3} { return Some(1); }
            // main() owns a Tokio runtime. The USB source runs on a dedicated sync worker,
            // exactly like capture in the desktop, including for standalone diagnostics.
            let result=std::thread::spawn(move || -> Result<i32> {
                let phone=afc::Phone::only()?;
                if probe {
                    let count = phone.camera_entry_count()?;
                    println!("{}", serde_json::json!({"usb":true,"cameraDirectoryReadable":true,"cameraEntryCount":count}));
                    return Ok(0);
                }
                let root=root.ok_or("请指定现有图库")?;
                afc::verify_gallery(&root,&phone)?;
                let result=afc::audit(&root,&phone,|status| {
                    if let Ok(line)=serde_json::to_string(status) { println!("{line}"); }
                })?;
                Ok(if result.without_local_proof==0 {0} else {2})
            }).join();
            Some(match result {
                Ok(Ok(code))=>code,
                Ok(Err(e))=>{eprintln!("{e}");1},
                Err(_)=>{eprintln!("iPhone 原件核对线程意外退出");1},
            })
        }
        "--index-original-checksums" if args.len() == 3 => {
            let result = inbox::fingerprints::prepare(Path::new(&args[2]), |progress| {
                if let Ok(line) = serde_json::to_string(progress) { println!("{line}"); }
            });
            Some(match result {
                Ok(done) if done.unavailable == 0 => 0,
                Ok(_) => 2,
                Err(e) => { eprintln!("{e}"); 1 }
            })
        }
        #[cfg(windows)]
        option @ ("--resume-verify-phone" | "--resume-receive-phone") => {
            let requested = (|| -> Result<()> {
                if args.len() != 3 {
                    return Err("请指定上次已发现的素材数".into());
                }
                let minimum = args[2].to_str().ok_or("无效的素材数")?.parse::<u64>()?;
                if minimum == 0 {
                    return Err("素材数必须大于零".into());
                }
                crate::t_config::set_app_identifier("app.timecut.album");
                verification::arm_existing(&control()?, minimum, option == "--resume-verify-phone")
            })();
            match requested {
                Ok(()) => None, // Continue into the ordinary desktop and tray application.
                Err(e) => {
                    eprintln!("{e}");
                    Some(1)
                }
            }
        }
        "--import-folder" if args.len() == 4 => {
            crate::t_config::set_app_identifier("app.timecut.album");
            crate::t_video::init_portable_ffmpeg_path();
            let src = Path::new(&args[2]);
            let root = Path::new(&args[3]);
            let report = copy_folder(root, src, |_| {}, || Ok(()), || Ok(()));
            println!("{}", serde_json::to_string(&report).unwrap());
            Some(if report.failed == 0 { 0 } else { 1 })
        }
        #[cfg(windows)]
        "--probe-devices" => {
            let probe = (|| -> Result<_> {
                let _apt = wpd::Apartment::new()?;
                Ok(wpd::devices()?
                    .into_iter()
                    .map(|d| d.name)
                    .collect::<Vec<_>>())
            })();
            match probe {
                Ok(list) => {
                    println!("{}", serde_json::to_string(&list).unwrap());
                    Some(0)
                }
                Err(e) => {
                    eprintln!("{e}");
                    Some(1)
                }
            }
        }
        _ => None,
    }
}
