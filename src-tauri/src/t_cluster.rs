use crate::{
    t_cmds::with_current_library,
    t_sqlite::{self, Face, Person},
};
use hnsw_rs::prelude::{DistCosine, Hnsw};
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, serde::Serialize)]
pub struct ClusterProgress {
    pub phase: &'static str,
    pub current: usize,
    pub total: usize,
}

const EMBEDDING_SIZE: usize = 128;
pub(crate) const EMBEDDING_BYTES: i64 = (EMBEDDING_SIZE * std::mem::size_of::<f32>()) as i64;
const EXAMPLES_PER_PERSON: usize = 4;
const MATCH_MARGIN: f32 = 0.045;
const NEAREST_EXAMPLES: usize = 48;

type Embedding = [f32; EMBEDDING_SIZE];

struct Point {
    id: i64,
    file: i64,
    person: Option<i64>,
    embedding: Embedding,
}

#[derive(Default)]
struct Group {
    person: Option<i64>,
    files: HashSet<i64>,
    examples: Vec<Embedding>,
    pending: Vec<(i64, i64)>,
}

impl Group {
    fn add_example(&mut self, file: i64, embedding: Embedding) -> bool {
        self.files.insert(file);
        if self.examples.len() < EXAMPLES_PER_PERSON {
            self.examples.push(embedding);
            return true;
        }
        false
    }

    fn score(&self, point: &Point) -> f32 {
        if self.files.contains(&point.file) {
            return -1.0;
        }
        self.examples
            .iter()
            .map(|example| {
                example
                    .iter()
                    .zip(point.embedding.iter())
                    .map(|(a, b)| a * b)
                    .sum::<f32>()
            })
            .fold(-1.0, f32::max)
    }
}

fn embedding(bytes: &[u8]) -> Option<Embedding> {
    if bytes.len() as i64 != EMBEDDING_BYTES {
        // Preserve existing people using older embeddings. Their features can't be
        // compared with the bundled SFace model.
        return None;
    }
    let mut values = [0.0; EMBEDDING_SIZE];
    for (v, chunk) in values.iter_mut().zip(bytes.chunks_exact(4)) {
        *v = f32::from_le_bytes(chunk.try_into().ok()?);
    }
    let magnitude = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !magnitude.is_finite() || magnitude <= f32::EPSILON {
        return None;
    }
    for value in &mut values {
        *value /= magnitude;
    }
    Some(values)
}

fn read_points(conn: &Connection) -> Result<Vec<Point>, String> {
    let mut query = conn
        .prepare(&format!(
            "SELECT f.id, f.file_id, p.id, f.embedding FROM faces f
         JOIN afiles a ON a.id=f.file_id JOIN afolders b ON a.folder_id=b.id
         LEFT JOIN persons p ON p.id=f.person_id
         WHERE {} AND length(f.embedding)=?1 AND NOT EXISTS
             (SELECT 1 FROM face_group_exclusions e WHERE e.face_id=f.id)
         ORDER BY f.id",
            Face::visible_images(),
        ))
        .map_err(|e| e.to_string())?;
    let rows = query
        .query_map([EMBEDDING_BYTES], |row| {
            let id = row.get(0)?;
            let file = row.get(1)?;
            let person = row.get(2)?;
            let bytes: Vec<u8> = row.get(3)?;
            Ok(embedding(&bytes).map(|embedding| Point {
                id,
                file,
                person,
                embedding,
            }))
        })
        .map_err(|e| e.to_string())?;
    rows.filter_map(|row| row.transpose())
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())
}

fn best_group(
    groups: &[Group],
    candidates: impl IntoIterator<Item = usize>,
    point: &Point,
    minimum: f32,
) -> Option<usize> {
    let mut best = (None, -1.0);
    let mut second = -1.0;
    for index in candidates {
        let score = groups[index].score(point);
        if score > best.1 {
            second = best.1;
            best = (Some(index), score);
        } else {
            second = second.max(score);
        }
    }
    if best.1 >= minimum && best.1 - second >= MATCH_MARGIN {
        best.0
    } else {
        None
    }
}

fn plan(
    points: Vec<Point>,
    minimum: f32,
    mut progress: impl FnMut(ClusterProgress),
    cancelled: impl Fn() -> bool,
) -> Option<Vec<Group>> {
    let index = Hnsw::<f32, DistCosine>::new(16, points.len().max(1), 16, 120, DistCosine {});
    let mut representative_groups = Vec::new();
    let mut groups = BTreeMap::<i64, Group>::new();
    let mut unassigned = Vec::new();
    for point in points {
        if let Some(id) = point.person {
            groups
                .entry(id)
                .or_insert_with(|| Group {
                    person: Some(id),
                    ..Default::default()
                })
                .add_example(point.file, point.embedding);
        } else {
            unassigned.push(point);
        }
    }
    let mut groups: Vec<_> = groups.into_values().collect();
    for (group_id, group) in groups.iter().enumerate() {
        if cancelled() {
            return None;
        }
        for example in &group.examples {
            index.insert((example, representative_groups.len()));
            representative_groups.push(group_id);
        }
    }
    let total = unassigned.len();
    for (current, point) in unassigned.into_iter().enumerate() {
        if cancelled() {
            return None;
        }
        if current % 50 == 0 || current + 1 == total {
            progress(ClusterProgress {
                phase: "assign",
                current: current + 1,
                total,
            });
        }
        let matched = if groups.len() <= NEAREST_EXAMPLES {
            best_group(&groups, 0..groups.len(), &point, minimum)
        } else {
            let mut candidates: Vec<_> = index
                .search(&point.embedding, NEAREST_EXAMPLES, 96)
                .into_iter()
                .map(|neighbor| representative_groups[neighbor.d_id])
                .collect();
            candidates.sort_unstable();
            candidates.dedup();
            best_group(&groups, candidates, &point, minimum)
        };
        let group_id = matched.unwrap_or_else(|| {
            groups.push(Group::default());
            groups.len() - 1
        });
        let group = &mut groups[group_id];
        if group.add_example(point.file, point.embedding) {
            index.insert((&point.embedding, representative_groups.len()));
            representative_groups.push(group_id);
        }
        group.pending.push((point.id, point.file));
    }
    Some(groups)
}

fn save_groups(conn: &Connection, groups: Vec<Group>) -> rusqlite::Result<Vec<i64>> {
    let tx = conn.unchecked_transaction()?;
    let mut created = Vec::new();
    for group in groups.into_iter().filter(|group| !group.pending.is_empty()) {
        let (id, new) = if let Some(id) = group.person {
            (id, false)
        } else {
            tx.execute("INSERT INTO persons(created_at) VALUES (unixepoch())", [])?;
            (tx.last_insert_rowid(), true)
        };
        let mut changes = 0;
        for (face, file) in group.pending {
            // Honor edits and removals made since the snapshot was taken.
            changes += tx.execute(
                "UPDATE faces SET person_id=?1
                 WHERE id=?2 AND file_id=?3 AND person_id IS NULL
                   AND EXISTS(SELECT 1 FROM persons WHERE id=?1)
                   AND NOT EXISTS(SELECT 1 FROM face_group_exclusions WHERE face_id=?2)
                   AND NOT EXISTS(SELECT 1 FROM faces WHERE file_id=?3 AND person_id=?1)",
                params![id, face, file],
            )?;
        }
        if new && changes > 0 {
            created.push(id);
        } else if new {
            tx.execute("DELETE FROM persons WHERE id=?1", [id])?;
        }
    }
    tx.commit()?;
    Ok(created)
}

/// Add new faces to the library's existing person albums. Names and manual
/// corrections survive subsequent imports, cancellation and library switches.
pub fn cluster_faces<F, C>(
    library: &str,
    max_distance: f32,
    mut progress: F,
    cancelled: C,
) -> Result<usize, String>
where
    F: FnMut(ClusterProgress),
    C: Fn() -> bool,
{
    let points = with_current_library(library, || {
        let db = t_sqlite::open_conn()?;
        if Face::has_unassigned(&db)? { read_points(&db) } else { Ok(Vec::new()) }
    })?;
    if points.iter().all(|point| point.person.is_some()) {
        return Ok(0);
    }
    let minimum = 1.0 - max_distance.clamp(0.15, 0.65);
    let Some(groups) = plan(points, minimum, &mut progress, &cancelled) else {
        return Ok(0);
    };
    if cancelled() {
        return Ok(0);
    }
    let new_ids = with_current_library(library, || {
        let db = t_sqlite::open_conn()?;
        save_groups(&db, groups).map_err(|e| e.to_string())
    })?;
    for (i, id) in new_ids.iter().enumerate() {
        if cancelled() {
            break;
        }
        progress(ClusterProgress {
            phase: "thumbnail",
            current: i + 1,
            total: new_ids.len(),
        });
        // Missing originals, offline volumes and library changes don't affect the grouping.
        let _ = with_current_library(library, || Person::update_thumbnail(*id));
    }
    Ok(new_ids.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(id: i64, file: i64, person: Option<i64>, axis: usize) -> Point {
        let mut embedding = [0.0; EMBEDDING_SIZE];
        embedding[axis] = 1.0;
        Point {
            id,
            file,
            person,
            embedding,
        }
    }

    #[test]
    fn auto_grouping_preserves_an_existing_person_and_avoids_same_photo_collisions() {
        let groups = plan(
            vec![
                point(1, 10, Some(7), 0),
                point(2, 11, None, 0),
                point(3, 11, None, 0),
                point(4, 12, None, 63),
            ],
            0.58,
            |_| {},
            || false,
        )
        .unwrap();
        assert_eq!(groups[0].person, Some(7));
        assert_eq!(groups[0].pending, [(2, 11)]);
        assert_eq!(groups[1].pending, [(3, 11)]);
        assert_eq!(groups[2].pending, [(4, 12)]);
        assert!(plan(vec![point(1, 1, None, 0)], 0.58, |_| {}, || true).is_none());
    }
}
