use crate::t_ai::{AiEngine, AiState};
use crate::t_cmds::with_current_library;
use crate::t_face::{
    self, FaceIndexCancellation, FaceIndexProgressState, FaceIndexingStatus, FaceState,
};
use crate::t_sqlite::{self, AFile, AThumb, Face};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Listener, Manager, State};

pub mod agent;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scene {
    Travel,
    Work,
    Saved,
    Daily,
    Review,
}

impl Scene {
    fn key(self) -> &'static str {
        match self {
            Self::Travel => "travel",
            Self::Work => "work",
            Self::Saved => "saved",
            Self::Daily => "daily",
            Self::Review => "review",
        }
    }
}

const MODEL_VERSION: i64 = 1;
const SCREENSHOT_MODEL_VERSION: i64 = 2;
const SCENE_RETRY_SECONDS: i64 = 300;
const SCREENSHOT_CANDIDATE: &str = "a.file_type=1 AND a.format_label='PNG'
    AND COALESCE(a.e_make,'')='' AND a.gps_latitude IS NULL AND a.gps_longitude IS NULL
    AND a.width>0 AND a.height*9>=a.width*16";
const PROMPTS: &[(Scene, &[&str])] = &[
    (
        Scene::Travel,
        &[
            "a vacation photograph of a famous landmark or historic site",
            "a tourist sightseeing picture taken while visiting a different city",
            "a travel photograph showing a scenic destination or a railway journey",
        ],
    ),
    (
        Scene::Work,
        &[
            "a work record, business document, whiteboard or spreadsheet",
            "a photo recording an office meeting, work desk or presentation slide",
            "a document scan, work receipt, report or labeled technical diagram",
        ],
    ),
    (
        Scene::Saved,
        &[
            "a downloaded internet image, meme, cartoon, poster or online advertisement",
            "a screenshot of a social media post or web page on a phone",
            "a marketing image or graphic saved from an internet website",
        ],
    ),
    (
        Scene::Daily,
        &[
            "a personal snapshot of normal daily life at home, family or friends",
            "a photo of a pet, a meal, a child playing or an ordinary outing",
            "a casual portrait or everyday picture taken with a camera",
        ],
    ),
];

const MOBILE_SCREENSHOT_PROMPTS: &[&str] = &[
    "a screenshot of a mobile phone app interface showing menus, buttons and text",
    "a smartphone screen capture of an app, online chat or messaging conversation",
    "a digital screenshot showing an app interface or a map on a phone screen",
    "a smartphone screenshot of mobile app content with status and navigation bars",
];

type PromptVectors = Vec<(Scene, Vec<Vec<f32>>)>;

struct ScenePrompts {
    standard: PromptVectors,
    mobile_screenshot: PromptVectors,
}

impl ScenePrompts {
    fn classify(&self, vector: &[f32], capture: CaptureContext, version: i64) -> (Scene, f64) {
        let result = classify(vector, &self.standard, capture);
        if result.0 == Scene::Review && version == SCREENSHOT_MODEL_VERSION {
            let reconsidered = classify(vector, &self.mobile_screenshot, capture);
            if reconsidered.0 == Scene::Saved {
                return reconsidered;
            }
        }
        result
    }
}

#[derive(Clone, Copy, Default, Serialize)]
struct CaptureContext {
    camera: bool,
    gps: bool,
    screenshot_name: bool,
}

fn unit(mut values: Vec<f32>) -> Result<Vec<f32>, String> {
    let length = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !length.is_finite() || length <= f32::EPSILON {
        return Err("Invalid image or text feature vector".into());
    }
    for value in &mut values {
        *value /= length;
    }
    Ok(values)
}

fn prompts(engine: &mut AiEngine) -> Result<ScenePrompts, String> {
    let standard: PromptVectors = PROMPTS
        .iter()
        .map(|(scene, text)| {
            let vectors = text
                .iter()
                .map(|prompt| unit(engine.encode_text(prompt)?))
                .collect::<Result<_, _>>()?;
            Ok((*scene, vectors))
        })
        .collect::<Result<_, String>>()?;
    let mut mobile_screenshot = standard.clone();
    let saved = mobile_screenshot
        .iter_mut()
        .find(|(scene, _)| *scene == Scene::Saved)
        .expect("the standard model includes saved images");
    for prompt in MOBILE_SCREENSHOT_PROMPTS {
        saved.1.push(unit(engine.encode_text(prompt)?)?);
    }
    Ok(ScenePrompts {
        standard,
        mobile_screenshot,
    })
}

fn classify(vector: &[f32], prototypes: &PromptVectors, capture: CaptureContext) -> (Scene, f64) {
    let mut scores = Vec::new();
    for (scene, options) in prototypes {
        let similarity = options
            .iter()
            .filter(|candidate| candidate.len() == vector.len())
            .map(|candidate| {
                candidate
                    .iter()
                    .zip(vector)
                    .map(|(a, b)| a * b)
                    .sum::<f32>()
            })
            .fold(-1.0f32, f32::max);
        // Metadata only nudges close visual matches. Its absence alone cannot
        // identify a download, and GPS by itself cannot identify a vacation.
        let adjustment = match scene {
            Scene::Saved if capture.screenshot_name => 0.032,
            Scene::Saved if capture.camera => -0.016,
            Scene::Travel if capture.gps => 0.009,
            _ => 0.0,
        };
        scores.push((*scene, similarity, similarity + adjustment));
    }
    scores.sort_by(|a, b| b.2.total_cmp(&a.2));
    if let [best, second, ..] = scores.as_slice() {
        let margin = (best.2 - second.2).max(0.0);
        if best.1 >= 0.15 && margin >= 0.016 {
            return (best.0, f64::from(margin));
        }
        return (Scene::Review, f64::from(margin));
    }
    (Scene::Review, 0.0)
}

pub(crate) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS timecut_scenes (
          file_id INTEGER PRIMARY KEY REFERENCES afiles(id) ON DELETE CASCADE,
          model_version INTEGER NOT NULL DEFAULT 0, modified_at INTEGER,
          size INTEGER, category TEXT, margin REAL, manual_category TEXT,
          attempted_at INTEGER NOT NULL DEFAULT 0, issue TEXT
        );
        CREATE TABLE IF NOT EXISTS timecut_organizer_options (
          id INTEGER PRIMARY KEY CHECK(id=1), auto_scenes INTEGER NOT NULL DEFAULT 1
        );",
    )
}

fn automatic_expression() -> String {
    format!(
        "CASE WHEN (s.model_version={MODEL_VERSION} OR
         (s.model_version={SCREENSHOT_MODEL_VERSION} AND ({SCREENSHOT_CANDIDATE}))) AND
        s.modified_at IS a.modified_at AND s.size=a.size THEN s.category END"
    )
}

pub(crate) fn category_expression() -> String {
    format!(
        "COALESCE(s.manual_category, {}, 'pending')",
        automatic_expression()
    )
}

fn visible_media() -> String {
    format!(
        "a.file_type IN (1,2,3) AND {} AND {} {}",
        AFile::live_photo_companion_exclusion_condition(),
        AFile::search_exclusion_condition("b"),
        AFile::album_filter_sql("a")
    )
}

fn auto_scenes(db: &Connection) -> rusqlite::Result<bool> {
    Ok(db
        .query_row(
            "SELECT auto_scenes FROM timecut_organizer_options WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(true))
}

struct Candidate {
    id: i64,
    path: String,
    size: i64,
    modified: Option<i64>,
    file_type: i64,
    orientation: i32,
    model_version: i64,
    capture: CaptureContext,
}

fn candidates(db: &Connection) -> Result<Vec<Candidate>, String> {
    // Only eligible PNGs awaiting a decision need a new inference after the upgrade.
    let model_version = format!(
        "CASE WHEN {SCREENSHOT_CANDIDATE} THEN {SCREENSHOT_MODEL_VERSION} ELSE {MODEL_VERSION} END"
    );
    let mut query = db.prepare(&format!(
        "SELECT a.id, b.path || '/' || a.name, a.name, a.size, a.modified_at, a.file_type,
                 COALESCE(a.e_make,'') <> '', a.gps_latitude IS NOT NULL AND a.gps_longitude IS NOT NULL,
                 COALESCE(a.e_orientation,1), {model_version}
           FROM afiles a JOIN afolders b ON b.id=a.folder_id
                         LEFT JOIN timecut_scenes s ON s.file_id=a.id
           WHERE {} AND s.manual_category IS NULL AND (
             s.file_id IS NULL OR s.modified_at IS NOT a.modified_at OR s.size IS NOT a.size
             OR (s.model_version<>({model_version}) AND
                 (s.model_version<>{MODEL_VERSION} OR s.category IS NULL OR s.category='review'))
             OR (s.category IS NULL AND s.attempted_at < unixepoch()-{SCENE_RETRY_SECONDS})
           ) ORDER BY a.id LIMIT 32", visible_media()
    )).map_err(|e| e.to_string())?;
    let rows = query
        .query_map([], |row| {
            let name = row.get::<_, String>(2)?.to_lowercase();
            Ok(Candidate {
                id: row.get(0)?,
                path: row.get(1)?,
                size: row.get(3)?,
                modified: row.get(4)?,
                file_type: row.get(5)?,
                orientation: row.get(8)?,
                model_version: row.get(9)?,
                capture: CaptureContext {
                    camera: row.get(6)?,
                    gps: row.get(7)?,
                    screenshot_name: [
                        "screenshot",
                        "screen shot",
                        "屏幕快照",
                        "螢幕快照",
                        "截屏",
                        "截图",
                    ]
                    .iter()
                    .any(|v| name.contains(v)),
                },
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())
}

fn encode(app: &AppHandle, library: &str, file: &Candidate) -> Result<Vec<f32>, &'static str> {
    let thumb = with_current_library(library, || Ok(AThumb::analysis_cached(file.id, &file.path)))
        .map_err(|_| "library_changed")?;
    let state = app.state::<AiState>();
    if let Some(bytes) = &thumb {
        if let Ok(vector) = state.0.lock().unwrap().encode_image_from_bytes(bytes) {
            return unit(vector).map_err(|_| "decode");
        }
    }
    if !crate::t_utils::content_readable(&file.path) {
        return Err("offline");
    }
    let jpeg =
        crate::t_image::analysis_thumbnail(&file.path, file.file_type, file.orientation, 384)
            .map_err(|_| "decode")?
            .ok_or("decode")?;
    let vector = state.0.lock().unwrap().encode_image_from_bytes(&jpeg);
    unit(vector.map_err(|_| "decode")?).map_err(|_| "decode")
}

fn save_result(
    library: &str,
    file: &Candidate,
    result: Result<(Scene, f64), &'static str>,
) -> Result<(), String> {
    let (category, margin, issue) = match result {
        Ok((category, margin)) => (Some(category.key()), Some(margin), None),
        Err(issue) => (None, None, Some(issue)),
    };
    with_current_library(library, || {
        t_sqlite::open_conn()?.execute(
            "INSERT INTO timecut_scenes(file_id,model_version,modified_at,size,category,margin,attempted_at,issue)
             SELECT id,?1,modified_at,size,?5,?6,unixepoch(),?7 FROM afiles
             WHERE id=?2 AND modified_at IS ?3 AND size=?4
             ON CONFLICT(file_id) DO UPDATE SET model_version=excluded.model_version,
              modified_at=excluded.modified_at,size=excluded.size, category=excluded.category,
              margin=excluded.margin,attempted_at=excluded.attempted_at, issue=excluded.issue",
            params![file.model_version, file.id, file.modified, file.size, category, margin, issue]
        ).map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[derive(Clone, Default)]
struct WorkerView {
    library: String,
    running: bool,
    error: Option<String>,
}

pub struct Organizer {
    wake: mpsc::Sender<()>,
    view: Arc<Mutex<WorkerView>>,
    auto_faces: Arc<Mutex<Option<f32>>>,
    scene_counts: Mutex<SceneCountCache>,
}

#[derive(Clone, Serialize)]
struct SceneCounts {
    counts: BTreeMap<String, usize>,
    unavailable: usize,
    failed: usize,
    automatic: bool,
}

#[derive(Default)]
struct SceneCountCache {
    snapshot: Option<((u64, i64), SceneCounts)>,
}

impl SceneCountCache {
    fn get(
        &mut self,
        revision: Option<(u64, i64)>,
        load: impl FnOnce() -> Result<SceneCounts, String>,
    ) -> Result<SceneCounts, String> {
        if let Some((previous, counts)) = &self.snapshot
            && Some(*previous) == revision
        {
            return Ok(counts.clone());
        }
        let counts = load()?;
        self.snapshot = revision.map(|revision| (revision, counts.clone()));
        Ok(counts)
    }
}

#[derive(Serialize)]
pub struct SceneStatus {
    library: String,
    #[serde(flatten)]
    summary: SceneCounts,
    running: bool,
    error: Option<String>,
}

#[derive(Serialize)]
pub struct FileScene {
    automatic: Option<String>,
    manual: Option<String>,
}

#[tauri::command]
pub fn timecut_file_scene(library: String, file_id: i64) -> Result<FileScene, String> {
    with_current_library(&library, || {
        t_sqlite::open_conn()?.query_row(&format!(
            "SELECT {}, s.manual_category FROM afiles a LEFT JOIN timecut_scenes s ON s.file_id=a.id WHERE a.id=?1",
            automatic_expression()), [file_id], |row| Ok(FileScene { automatic: row.get(0)?, manual: row.get(1)? }))
            .map_err(|e| e.to_string())
    })
}

fn read_scene_counts() -> Result<SceneCounts, String> {
    let conn = t_sqlite::open_conn()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {}, COUNT(*), sum(s.issue='offline'), sum(s.issue='decode')
            FROM afiles a JOIN afolders b ON b.id=a.folder_id LEFT JOIN timecut_scenes s ON s.file_id=a.id
            WHERE {} GROUP BY 1", category_expression(), visible_media()
    )).map_err(|e| e.to_string())?;
    let mut counts = BTreeMap::new();
    let (mut unavailable, mut failed) = (0, 0);
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let count: usize = row.get(1).map_err(|e| e.to_string())?;
        let category: String = row.get(0).map_err(|e| e.to_string())?;
        if category == "pending" {
            unavailable += row
                .get::<_, Option<usize>>(2)
                .map_err(|e| e.to_string())?
                .unwrap_or(0);
            failed += row
                .get::<_, Option<usize>>(3)
                .map_err(|e| e.to_string())?
                .unwrap_or(0);
        }
        counts.insert(category, count);
    }
    Ok(SceneCounts {
        counts,
        unavailable,
        failed,
        automatic: auto_scenes(&conn).map_err(|e| e.to_string())?,
    })
}

#[tauri::command]
pub fn timecut_scene_status(
    library: String,
    state: State<Organizer>,
) -> Result<SceneStatus, String> {
    with_current_library(&library, || {
        if t_sqlite::is_database_corrupted() {
            return Err(t_sqlite::DB_CORRUPTED_MSG.into());
        }
        let summary = {
            let mut cache = state.scene_counts.lock().map_err(|e| e.to_string())?;
            // Sample before reading the counts so a concurrent commit triggers a reload.
            cache.get(t_sqlite::library_revision().ok(), read_scene_counts)?
        };
        let view = state.view.lock().unwrap();
        Ok(SceneStatus {
            running: view.library == library && view.running,
            error: (view.library == library)
                .then(|| view.error.clone())
                .flatten(),
            library: library.clone(),
            summary,
        })
    })
}

#[tauri::command]
pub fn timecut_set_scene_auto(
    library: String,
    enabled: bool,
    state: State<Organizer>,
) -> Result<(), String> {
    with_current_library(&library, || {
        t_sqlite::open_conn()?.execute(
            "INSERT INTO timecut_organizer_options VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET auto_scenes=excluded.auto_scenes", [enabled]
        ).map_err(|e| e.to_string())?;
        Ok(())
    })?;
    let _ = state.wake.send(());
    Ok(())
}

#[tauri::command]
pub fn timecut_face_auto(
    enabled: bool,
    cluster_epsilon: Option<f32>,
    state: State<Organizer>,
    cancel: State<FaceIndexCancellation>,
) {
    *state.auto_faces.lock().unwrap() = enabled.then_some(cluster_epsilon.unwrap_or(0.55));
    if !enabled {
        *cancel.0.lock().unwrap() = true;
    }
    let _ = state.wake.send(());
}

#[tauri::command]
pub fn timecut_set_scene(
    library: String,
    file_ids: Vec<i64>,
    category: Option<Scene>,
    app: AppHandle,
) -> Result<(), String> {
    with_current_library(&library, || {
        let db = t_sqlite::open_conn()?;
        let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
        for id in file_ids {
            tx.execute("INSERT INTO timecut_scenes(file_id,manual_category) SELECT id,?2 FROM afiles WHERE id=?1
              ON CONFLICT(file_id) DO UPDATE SET manual_category=excluded.manual_category", params![id, category.map(Scene::key)])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    })?;
    let _ = app.emit("timecut-scenes-changed", &library);
    let _ = app.state::<Organizer>().wake.send(());
    Ok(())
}

enum SceneBatch {
    Processed,
    Idle { retry_in: Option<Duration> },
}

fn next_scene_retry(db: &Connection, checked_at: i64) -> rusqlite::Result<Option<Duration>> {
    // Closing the scan across a second boundary must not lose a due retry.
    let seconds: Option<u64> = db.query_row(
        "SELECT max(0, min(attempted_at) + ?2 + 1 - unixepoch()) FROM timecut_scenes
         WHERE manual_category IS NULL AND category IS NULL AND attempted_at >= ?1 - ?2",
        params![checked_at, SCENE_RETRY_SECONDS],
        |row| row.get(0),
    )?;
    Ok(seconds.map(|value| Duration::from_secs(value.min(SCENE_RETRY_SECONDS as u64 + 1))))
}

fn arrange_batch(
    app: &AppHandle,
    library: &str,
    text_vectors: &mut Option<ScenePrompts>,
) -> Result<SceneBatch, String> {
    let (files, retry_in) = with_current_library(library, || {
        let db = t_sqlite::open_conn()?;
        if !auto_scenes(&db).map_err(|e| e.to_string())? {
            return Ok((Vec::new(), None));
        }
        let checked_at = db.query_row("SELECT unixepoch()", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        let files = candidates(&db)?;
        let retry_in = if files.is_empty() {
            next_scene_retry(&db, checked_at).map_err(|e| e.to_string())?
        } else {
            None
        };
        Ok((files, retry_in))
    })?;
    if files.is_empty() {
        return Ok(SceneBatch::Idle { retry_in });
    }
    if text_vectors.is_none() {
        let state = app.state::<AiState>();
        let mut engine = state.0.lock().unwrap();
        engine.load_models(app)?;
        *text_vectors = Some(prompts(&mut engine)?);
    }
    for file in files {
        let enabled = with_current_library(library, || {
            let db = t_sqlite::open_conn()?;
            auto_scenes(&db).map_err(|e| e.to_string())
        })?;
        if !enabled {
            return Ok(SceneBatch::Idle { retry_in: None });
        }
        let result = encode(app, library, &file).map(|vector| {
            text_vectors.as_ref().unwrap().classify(&vector, file.capture, file.model_version)
        });
        save_result(library, &file, result)?;
    }
    let _ = app.emit("timecut-scenes-changed", library);
    Ok(SceneBatch::Processed)
}

fn start_face_batch(app: &AppHandle, library: &str, epsilon: f32) -> Result<(bool, Option<Duration>), String> {
    let status = app.state::<FaceIndexingStatus>();
    if *status.0.lock().unwrap() {
        return Ok((true, None));
    }
    let (ready, retry_in) = with_current_library(library, Face::scheduled_work)?;
    if !ready {
        return Ok((false, retry_in.map(Duration::from_secs)));
    }
    t_face::run_face_indexing(
        app.clone(),
        app.state::<FaceState>().inner().clone(),
        app.state::<FaceIndexCancellation>().inner().clone(),
        status.inner().clone(),
        app.state::<FaceIndexProgressState>().inner().clone(),
        epsilon,
        true,
    )?;
    Ok((true, None))
}

struct IdleLibrary {
    revision: (u64, i64),
    retry_at: Option<Instant>,
}

impl IdleLibrary {
    fn unchanged(&self, revision: Option<(u64, i64)>, now: Instant) -> bool {
        Some(self.revision) == revision && self.retry_at.is_none_or(|deadline| now < deadline)
    }
}

pub fn start(app: AppHandle) {
    let (wake, receiver) = mpsc::channel();
    let view = Arc::new(Mutex::new(WorkerView::default()));
    let auto_faces = Arc::new(Mutex::new(None));
    app.listen("index_finished", {
        let wake = wake.clone();
        move |_| {
            let _ = wake.send(());
        }
    });
    app.manage(Organizer {
        wake,
        view: view.clone(),
        auto_faces: auto_faces.clone(),
        scene_counts: Mutex::default(),
    });
    let runtime = tauri::async_runtime::handle();
    std::thread::spawn(move || {
        let _runtime = runtime.inner().enter();
        let mut text_vectors = None;
        let mut last_faces: Option<(String, Instant)> = None;
        let mut idle: Option<IdleLibrary> = None;
        loop {
            let result = crate::t_config::current_library_id().and_then(|library| {
                // A commit during either search must invalidate the empty result.
                let revision = with_current_library(&library, t_sqlite::library_revision).ok();
                if idle.as_ref().is_some_and(|s| s.unchanged(revision, Instant::now())) {
                    return Ok(false);
                }
                idle = None;
                let auto_threshold = *auto_faces.lock().unwrap();
                let mut faces_idle = auto_threshold.is_none();
                let mut face_retry = None;
                if let Some(epsilon) = auto_threshold
                    && last_faces.as_ref().is_none_or(|(previous, time)| {
                    previous != &library || time.elapsed() >= Duration::from_secs(60)
                }) {
                    match start_face_batch(&app, &library, epsilon) {
                        Ok((false, retry_in)) => { faces_idle = true; face_retry = retry_in; }
                        Ok((true, _)) => last_faces = Some((library.clone(), Instant::now())),
                        Err(_) => {}
                    }
                }
                *view.lock().unwrap() = WorkerView {
                    library: library.clone(),
                    running: true,
                    error: None,
                };
                let result = arrange_batch(&app, &library, &mut text_vectors);
                let error = result.as_ref().err().filter(|_| {
                    crate::t_config::current_library_id().is_ok_and(|id| id == library)
                }).cloned();
                *view.lock().unwrap() = WorkerView { library, running: false, error };
                match result? {
                    SceneBatch::Processed => Ok(true),
                    SceneBatch::Idle { retry_in } => {
                        if faces_idle {
                            idle = revision.map(|revision| IdleLibrary {
                                revision,
                                retry_at: retry_in.into_iter().chain(face_retry).min().map(|delay| Instant::now() + delay),
                            });
                        }
                        Ok(false)
                    }
                }
            });
            let delay = if result.as_ref().is_ok_and(|more| *more) {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(12)
            };
            match receiver.recv_timeout(delay) {
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(()) => {
                    while receiver.try_recv().is_ok() {}
                    idle = None;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_counts_are_reused_until_the_catalog_changes() {
        let mut cache = SceneCountCache::default();
        let mut loaded = 0;
        let mut load = || {
            loaded += 1;
            Ok(SceneCounts {
                counts: BTreeMap::from([("pending".into(), loaded)]),
                unavailable: 0,
                failed: 0,
                automatic: true,
            })
        };
        for (revision, expected) in [
            (Some((1, 1)), 1),
            (Some((1, 1)), 1),
            (Some((1, 2)), 2),
            (Some((2, 2)), 3), // the library was reopened or switched
            (None, 4), // the observer is temporarily unavailable
            (None, 5),
        ] {
            assert_eq!(cache.get(revision, &mut load).unwrap().counts["pending"], expected);
        }
    }

    #[test]
    fn idle_organizer_wakes_for_changes_or_a_due_retry() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE afiles(id INTEGER PRIMARY KEY); INSERT INTO afiles VALUES(1),(2),(3)").unwrap();
        init(&db).unwrap();
        let now: i64 = db.query_row("SELECT unixepoch()", [], |row| row.get(0)).unwrap();
        db.execute("INSERT INTO timecut_scenes(file_id,attempted_at,manual_category) VALUES
            (1,?1-1000,NULL),(2,?1,'work')", [now]).unwrap();
        assert_eq!(next_scene_retry(&db, now).unwrap(), None);
        db.execute("INSERT INTO timecut_scenes(file_id,attempted_at) VALUES(3,?1-280)", [now]).unwrap();
        let delay = next_scene_retry(&db, now).unwrap().unwrap();
        assert!(delay <= Duration::from_secs(21));
        let tick = Instant::now();
        let quiet = IdleLibrary { revision: (1, 7), retry_at: None };
        assert!(quiet.unchanged(Some((1, 7)), tick + Duration::from_secs(3600)));
        assert!(!quiet.unchanged(Some((1, 8)), tick)); // other writer
        assert!(!quiet.unchanged(Some((2, 7)), tick)); // reopened or changed library
        assert!(!quiet.unchanged(None, tick)); // observer unavailable
        let waiting = IdleLibrary { retry_at: Some(tick + delay), ..quiet };
        assert!(!waiting.unchanged(Some((1, 7)), tick + delay));
    }

    #[test]
    fn only_reviewed_phone_shaped_images_use_the_extra_prompts() {
        let standard = vec![
            (Scene::Work, vec![vec![0.27, 0.34, 0.27]]),
            (Scene::Saved, vec![vec![0.26, 0.26, 0.26]]),
        ];
        let mut mobile_screenshot = standard.clone();
        mobile_screenshot[1].1.push(vec![0.30, 0.40, 0.28]);
        let prompts = ScenePrompts {
            standard,
            mobile_screenshot,
        };
        let capture = CaptureContext::default();
        for (vector, version, expected) in [
            ([1.0, 0.0, 0.0], MODEL_VERSION, Scene::Review),
            ([1.0, 0.0, 0.0], SCREENSHOT_MODEL_VERSION, Scene::Saved),
            ([0.0, 1.0, 0.0], SCREENSHOT_MODEL_VERSION, Scene::Work),
            ([0.0, 0.0, 1.0], SCREENSHOT_MODEL_VERSION, Scene::Review),
        ] {
            assert_eq!(prompts.classify(&vector, capture, version).0, expected);
        }
    }

    #[test]
    fn screenshot_upgrade_queues_only_eligible_review_rows() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE albums(id INTEGER, file_types INTEGER, small_image_filter INTEGER,
               path TEXT, excluded_folders TEXT);
             INSERT INTO albums VALUES(1,7,0,'archive','[]');
             CREATE TABLE afolders(id INTEGER, album_id INTEGER, path TEXT, is_excluded_from_search INTEGER);
             INSERT INTO afolders VALUES(1,1,'archive',0);
             CREATE TABLE afiles(id INTEGER PRIMARY KEY, folder_id INTEGER DEFAULT 1,
               name TEXT DEFAULT 'capture.png', modified_at INTEGER DEFAULT 100, size INTEGER DEFAULT 10,
               file_type INTEGER DEFAULT 1, format_label TEXT DEFAULT 'PNG', width INTEGER DEFAULT 1179,
               height INTEGER DEFAULT 2556, e_make TEXT, gps_latitude REAL, gps_longitude REAL,
               e_orientation INTEGER, live_photo_video_id INTEGER);
             INSERT INTO afiles(id) VALUES(1),(2),(3),(4),(5),(6),(7);
             UPDATE afiles SET format_label='HEIC' WHERE id=3;
             UPDATE afiles SET e_make='Apple' WHERE id IN (5,6);",
        )
        .unwrap();
        init(&db).unwrap();
        db.execute_batch(
            "INSERT INTO timecut_scenes(file_id,model_version,modified_at,size,category,manual_category)
             VALUES(1,1,100,10,'review',NULL),(2,1,100,10,'work',NULL),
                   (3,1,100,10,'review',NULL),(4,1,100,10,'review','daily'),
                   (5,1,100,10,'review',NULL),(6,2,100,10,'saved',NULL);",
        )
        .unwrap();
        let queued = candidates(&db)
            .unwrap()
            .into_iter()
            .map(|candidate| (candidate.id, candidate.model_version))
            .collect::<Vec<_>>();
        assert_eq!(
            queued,
            [(1, SCREENSHOT_MODEL_VERSION), (6, MODEL_VERSION), (7, SCREENSHOT_MODEL_VERSION)]
        );
        let category: String = db
            .query_row(
                &format!(
                    "SELECT {} FROM afiles a JOIN timecut_scenes s ON s.file_id=a.id WHERE a.id=1",
                    category_expression()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(category, "review");
    }

    #[test]
    fn scene_scores_keep_ambiguous_media_in_review() {
        let prototypes = vec![
            (Scene::Travel, vec![vec![1.0, 0.0]]),
            (Scene::Work, vec![vec![0.0, 1.0]]),
        ];
        assert_eq!(
            classify(&[0.9, 0.1], &prototypes, CaptureContext::default()).0,
            Scene::Travel
        );
        assert_eq!(
            classify(&[0.1, 0.9], &prototypes, CaptureContext::default()).0,
            Scene::Work
        );
        assert_eq!(
            classify(
                &[0.5, 0.5],
                &prototypes,
                CaptureContext {
                    gps: true,
                    ..Default::default()
                }
            )
            .0,
            Scene::Review
        );
    }
}
