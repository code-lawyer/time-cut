use super::*;
use crate::ingest::tests::{Sandbox, originals};

fn control_database() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    initialize_control(&db).unwrap();
    db
}

fn target_for(library: &str, root: &str) -> Target {
    Target {
        library: library.into(),
        album: 1,
        root: root.into(),
        auto: true,
    }
}

fn enqueue(db: &Connection, dest: &Target, source: &str) {
    db.execute(
        "INSERT INTO queue(path,target) VALUES(?,?)",
        params![source, serde_json::to_string(dest).unwrap()],
    )
    .unwrap();
    request_index(db, &dest.library, dest.album).unwrap();
}

#[test]
fn renamed_then_removed_album_updates_durable_jobs_only_in_its_own_library() {
    let db = control_database();
    let original = target_for("first", "D:/Original");
    db.execute(
        "INSERT INTO config VALUES(1,?)",
        [serde_json::to_string(&original).unwrap()],
    )
    .unwrap();
    enqueue(&db, &original, "Camera");
    let moved = target_for("first", "E:/Renamed");
    enqueue(&db, &moved, "Camera");
    enqueue(&db, &target_for("other", "D:/Other"), "Phone");

    catalog::synchronize(&db, |library, _| {
        Ok(Some(
            if library == "first" {
                "E:/Renamed"
            } else {
                "D:/Other"
            }
            .into(),
        ))
    })
    .unwrap();
    assert_eq!(target(&db).unwrap().unwrap().root, moved.root);
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM queue", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2); // The two requests for the same renamed destination were merged.
    assert_eq!(
        next_folder(&db, &catalog::Unavailable::new())
            .unwrap()
            .unwrap()
            .2
            .root,
        moved.root
    );

    catalog::synchronize(&db, |library, _| {
        Ok((library == "other").then(|| "D:/Other".into()))
    })
    .unwrap();
    assert!(target(&db).unwrap().is_none());
    assert_eq!(
        next_folder(&db, &catalog::Unavailable::new())
            .unwrap()
            .unwrap()
            .2
            .library,
        "other"
    );
    let pending: Vec<String> = db
        .prepare("SELECT library FROM indexing")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(pending, ["other"]);
}

#[test]
fn an_offline_catalog_retains_its_pending_requests_and_does_not_block_another_library() {
    let db = control_database();
    let offline = target_for("disconnected", "E:/Archive");
    db.execute(
        "INSERT INTO config VALUES(1,?)",
        [serde_json::to_string(&offline).unwrap()],
    )
    .unwrap();
    enqueue(&db, &offline, "First");
    enqueue(&db, &target_for("online", "D:/Second"), "Second");
    let unavailable = catalog::synchronize(&db, |library, _| {
        if library == "disconnected" {
            Err("offline library database".into())
        } else {
            Ok(Some("D:/Second".into()))
        }
    })
    .unwrap();
    assert_eq!(target(&db).unwrap().unwrap().root, offline.root);
    assert_eq!(next_folder(&db, &unavailable).unwrap().unwrap().1, "Second");

    let restored = catalog::synchronize(&db, |library, _| {
        Ok(Some(
            if library == "disconnected" {
                "E:/Archive"
            } else {
                "D:/Second"
            }
            .into(),
        ))
    })
    .unwrap();
    assert!(restored.is_empty());
    assert_eq!(next_folder(&db, &restored).unwrap().unwrap().1, "First");
}

#[test]
fn interrupted_job_requests_an_index_even_without_reconnecting_the_phone() {
    let db = control_database();
    db.execute("INSERT INTO importing VALUES('library',17)", [])
        .unwrap();
    recover_interrupted_imports(&db).unwrap();
    let version: i64 = db
        .query_row(
            "SELECT revision FROM indexing WHERE library='library' AND album=17",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(version > 0);
    recover_interrupted_imports(&db).unwrap();
    let same: i64 = db
        .query_row(
            "SELECT revision FROM indexing WHERE library='library' AND album=17",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(same, version);
}

#[tokio::test]
async fn a_library_change_cancels_and_waits_for_the_old_scan() {
    let gate = tokio::sync::RwLock::new(());
    let flags = crate::t_cmds::IndexCancellation(Arc::new(Mutex::new(HashMap::from([(7, false)]))));
    let active = gate.read().await;
    let change = crate::t_cmds::drain_library_scans(&gate, &flags);
    tokio::pin!(change);
    tokio::select! {
        biased;
        _ = &mut change => panic!("database changed while the outgoing scan was still active"),
        _ = std::future::ready(()) => (),
    }
    assert_eq!(flags.0.lock().unwrap().get(&7), Some(&true));
    drop(active);
    let _exclusive = change.await.unwrap();
    assert!(gate.try_read().is_err());
}

#[test]
fn reopening_a_preview2_control_database_clears_jobs_for_a_deleted_album() {
    let temp = Sandbox::new();
    let path = temp.dir("state").join("import-control.sqlite3");
    let dest = target_for("gone", "D:/Old archive");
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE config(id INTEGER PRIMARY KEY, data TEXT NOT NULL);
            CREATE TABLE queue(id INTEGER PRIMARY KEY, path TEXT NOT NULL, target TEXT NOT NULL, UNIQUE(path,target));
            CREATE TABLE indexing(library TEXT, album INTEGER, revision INTEGER, PRIMARY KEY(library,album));").unwrap();
        db.execute(
            "INSERT INTO config VALUES(1,?)",
            [serde_json::to_string(&dest).unwrap()],
        )
        .unwrap();
        enqueue(&db, &dest, "old folder");
    }
    {
        let reopened = Connection::open(&path).unwrap();
        initialize_control(&reopened).unwrap();
        recover_interrupted_imports(&reopened).unwrap();
        catalog::synchronize(&reopened, |_, _| Ok(None)).unwrap();
    }
    let db = Connection::open(&path).unwrap();
    assert!(target(&db).unwrap().is_none());
    assert!(
        next_folder(&db, &catalog::Unavailable::new())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM indexing", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn removing_the_active_catalog_waits_for_the_current_original_and_rejects_the_next() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dest = target_for("old library", "E:/Archive");
    let removed = AtomicBool::new(false);
    let mut guard = catalog::guard_destination(&dest, |_, _| {
        Ok((!removed.load(Ordering::Acquire)).then(|| dest.root.clone()))
    });
    let current_original = guard().unwrap();
    std::thread::scope(|scope| {
        let (started, waiting) = mpsc::channel();
        let removed = &removed;
        let retire = scope.spawn(move || {
            started.send(()).unwrap();
            catalog::changing(|| {
                removed.store(true, Ordering::Release);
                Ok(())
            })
            .unwrap();
        });
        waiting.recv().unwrap();
        assert!(!removed.load(Ordering::Acquire));
        drop(current_original);
        retire.join().unwrap();
    });
    assert!(guard().is_err());
}

#[test]
fn catalog_change_stops_the_remaining_files_before_publication() {
    let temp = Sandbox::new();
    let (source, dest) = (temp.dir("source"), temp.dir("archive"));
    std::fs::write(source.join("01.jpg"), b"first completed photo").unwrap();
    std::fs::write(source.join("02.jpg"), b"second photo").unwrap();
    let mut checks = 0;
    let result = transfer_guarded(
        &dest,
        "folder",
        || Ok(Box::new(Folder::new(&source, &dest)?)),
        |_| {},
        || {
            checks += 1;
            if checks > 2 {
                return Err("target removed".into());
            }
            Ok(())
        },
        || Ok(()),
    );
    assert_eq!((result.added, result.failed), (1, 1));
    assert_eq!(originals(&dest).len(), 1);
}
