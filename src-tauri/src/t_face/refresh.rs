use super::{FaceBox, FaceData};
use crate::t_sqlite::FaceTarget;
use rusqlite::{Connection, Result, params};

pub(crate) fn init(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS face_refresh_assignments (
            old_face INTEGER PRIMARY KEY,
            file_id INTEGER NOT NULL REFERENCES afiles(id) ON DELETE CASCADE,
            person_id INTEGER REFERENCES persons(id) ON DELETE SET NULL,
            excluded INTEGER NOT NULL,
            bbox TEXT NOT NULL,
            embedding BLOB NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_face_refresh_file ON face_refresh_assignments(file_id);
        CREATE TABLE IF NOT EXISTS face_retries (
            file_id INTEGER PRIMARY KEY REFERENCES afiles(id) ON DELETE CASCADE,
            attempts INTEGER NOT NULL,
            retry_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_face_retries_at ON face_retries(retry_at);",
    )
}

/// Preserve previous assignments privately while the new original awaits detection.
/// Manual edits remain authoritative even if a rescan is interrupted or restarted.
pub(crate) fn remember_assignments(db: &Connection, file: i64) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO face_refresh_assignments
         SELECT f.id,f.file_id,f.person_id,e.face_id IS NOT NULL,f.bbox,f.embedding
         FROM faces f LEFT JOIN face_group_exclusions e ON e.face_id=f.id
         WHERE f.file_id=?1 AND (f.person_id IS NOT NULL OR e.face_id IS NOT NULL)
         AND f.bbox IS NOT NULL AND f.embedding IS NOT NULL",
        [file],
    )?;
    Ok(())
}

struct PreviousFace {
    face_id: i64,
    person: Option<i64>,
    excluded: bool,
    bbox: FaceBox,
    embedding: Vec<f32>,
}

impl PreviousFace {
    fn matches(&self, new: &FaceData) -> bool {
        if self.embedding.len() != new.embedding.len() || self.embedding.is_empty() {
            return false;
        }
        let a = &self.bbox;
        let b = &new.bbox;
        let width = ((a.x + a.width).min(b.x + b.width) - a.x.max(b.x)).max(0.);
        let height = ((a.y + a.height).min(b.y + b.height) - a.y.max(b.y)).max(0.);
        let overlap = width * height;
        if overlap <= 0. || overlap < 0.5 * (a.width * a.height + b.width * b.height - overlap) {
            return false;
        }
        // Restore only near-identical model output at the same position, so a
        // replacement photo never inherits a name on position alone.
        let (mut dot, mut old_norm, mut new_norm) = (0., 0., 0.);
        for (&old, &new) in self.embedding.iter().zip(&new.embedding) {
            dot += old * new;
            old_norm += old * old;
            new_norm += new * new;
        }
        old_norm > 0. && new_norm > 0. && dot >= 0.98 * (old_norm * new_norm).sqrt()
    }
}

pub(crate) fn restore(db: &Connection, file: i64, ids: &[i64], faces: &[FaceData]) -> Result<()> {
    let mut query = db.prepare_cached(
        "SELECT old_face,person_id,excluded,bbox,embedding FROM face_refresh_assignments WHERE file_id=?1",
    )?;
    let rows = query.query_map([file], |row| {
        let face_id = row.get(0)?;
        let person = row.get(1)?;
        let excluded = row.get(2)?;
        let encoded: String = row.get(3)?;
        let bytes: Vec<u8> = row.get(4)?;
        let previous = serde_json::from_str(&encoded)
            .ok()
            .map(|bbox| PreviousFace {
                face_id,
                person,
                excluded,
                bbox,
                embedding: bytes
                    .chunks_exact(4)
                    .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
                    .collect(),
            });
        Ok(previous)
    })?;
    let previous = rows
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    for old in &previous {
        let mut candidates = faces
            .iter()
            .enumerate()
            .filter(|(_, face)| old.matches(face));
        let Some((index, face)) = candidates.next() else {
            continue;
        };
        if candidates.next().is_some()
            || previous.iter().filter(|old| old.matches(face)).count() != 1
        {
            continue;
        }
        let id = ids[index];
        if old.excluded {
            db.execute(
                "INSERT OR IGNORE INTO face_group_exclusions(face_id) VALUES(?1)",
                [id],
            )?;
        } else if let Some(person) = old.person {
            db.execute(
                "UPDATE faces SET person_id=?1 WHERE id=?2",
                params![person, id],
            )?;
            db.execute(
                "UPDATE persons SET cover_face_id=?1 WHERE id=?2 AND cover_face_id=?3",
                params![id, person, old.face_id],
            )?;
        }
    }
    db.execute(
        "DELETE FROM face_refresh_assignments WHERE file_id=?1",
        [file],
    )?;
    db.execute("DELETE FROM face_retries WHERE file_id=?1", [file])?;
    Ok(())
}

/// Retry transient failures after five and thirty minutes, then wait for a file
/// revision or a manual scan. An in-flight result cannot suppress a newer file.
pub(crate) fn defer(db: &Connection, target: &FaceTarget) -> Result<()> {
    db.execute(
        "INSERT INTO face_retries(file_id,attempts,retry_at)
        SELECT id,1,unixepoch()+300 FROM afiles WHERE id=?1 AND COALESCE(has_faces,0)=0
            AND modified_at IS ?2 AND size=?3
        ON CONFLICT(file_id) DO UPDATE SET
            retry_at=CASE WHEN attempts=1 THEN unixepoch()+1800 ELSE NULL END,
            attempts=min(attempts+1,3)",
        params![target.id, target.modified_at, target.size],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::t_sqlite::Face;

    fn library() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON;
            CREATE TABLE afiles(id INTEGER PRIMARY KEY, size INTEGER, modified_at INTEGER, has_faces INTEGER);
            CREATE TABLE persons(id INTEGER PRIMARY KEY, cover_face_id INTEGER);
            CREATE TABLE faces(id INTEGER PRIMARY KEY AUTOINCREMENT, file_id INTEGER, bbox TEXT, embedding BLOB,
                person_id INTEGER REFERENCES persons(id), created_at INTEGER);
            CREATE TABLE face_group_exclusions(face_id INTEGER PRIMARY KEY REFERENCES faces(id) ON DELETE CASCADE);
            INSERT INTO afiles VALUES(1,100,10,0);
            INSERT INTO persons VALUES(1,NULL);").unwrap();
        init(&db).unwrap();
        db
    }

    fn detection(embedding: Vec<f32>) -> FaceData {
        FaceData {
            bbox: FaceBox {
                x: 10.,
                y: 20.,
                width: 50.,
                height: 50.,
                confidence: 0.95,
                landmarks: None,
            },
            embedding,
        }
    }

    fn add(db: &Connection, face: &FaceData) -> i64 {
        Face::add_with_conn(
            db,
            1,
            &serde_json::to_string(&face.bbox).unwrap(),
            &face.embedding,
        )
        .unwrap()
    }

    #[test]
    fn matching_redetection_recovers_person_cover_and_exclusion_only_for_the_same_face() {
        for (old, new, excluded, expect_match) in [
            (vec![1., 0.], vec![0.999, 0.001], false, true),
            (vec![1., 0.], vec![1., 0.], true, true),
            (vec![1., 0.], vec![0., 1.], false, false),
        ] {
            let db = library();
            let before = add(&db, &detection(old));
            if excluded {
                db.execute("INSERT INTO face_group_exclusions VALUES(?1)", [before])
                    .unwrap();
            } else {
                db.execute("UPDATE faces SET person_id=1 WHERE id=?1", [before])
                    .unwrap();
                db.execute("UPDATE persons SET cover_face_id=?1 WHERE id=1", [before])
                    .unwrap();
            }
            remember_assignments(&db, 1).unwrap();
            db.execute("DELETE FROM faces WHERE file_id=1", []).unwrap();
            // A second refresh before detection must not lose the original edit.
            remember_assignments(&db, 1).unwrap();
            let face = detection(new);
            let after = add(&db, &face);
            restore(&db, 1, &[after], &[face]).unwrap();
            let result: (bool, bool, bool) = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM faces WHERE id=?1 AND person_id=1),
                    EXISTS(SELECT 1 FROM persons WHERE cover_face_id=?1),
                    EXISTS(SELECT 1 FROM face_group_exclusions WHERE face_id=?1)",
                    [after],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(
                result,
                (
                    expect_match && !excluded,
                    expect_match && !excluded,
                    expect_match && excluded
                )
            );
        }
    }

    #[test]
    fn failure_backoff_stops_and_an_old_revision_cannot_delay_a_replacement() {
        let db = library();
        let mut target = FaceTarget {
            id: 1,
            path: String::new(),
            width: 100,
            height: 100,
            modified_at: Some(10),
            size: 100,
            file_type: 1,
            orientation: 1,
        };
        for (attempts, delay) in [(1, Some(300i64)), (2, Some(1800)), (3, None), (3, None)] {
            defer(&db, &target).unwrap();
            let (n, wait): (i64, Option<i64>) = db
                .query_row(
                    "SELECT attempts,retry_at-unixepoch() FROM face_retries WHERE file_id=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(n, attempts);
            assert!(wait == delay || wait == delay.map(|value| value - 1));
        }
        db.execute("DELETE FROM face_retries", []).unwrap();
        target.modified_at = Some(9);
        defer(&db, &target).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM face_retries", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
