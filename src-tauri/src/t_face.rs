/**
 * Face Recognition module
 * Face detection and clustering with local YuNet and SFace inference.
 */
mod model;
pub(crate) mod refresh;

use crate::{t_cluster, t_common};
use image::DynamicImage;
use ort::session::{Session, builder::GraphOptimizationLevel};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

// cancellation token for face indexing
#[derive(Clone)]
pub struct FaceIndexCancellation(pub Arc<Mutex<bool>>);

// detailed status for face indexing
#[derive(Clone)]
pub struct FaceIndexingStatus(pub Arc<Mutex<bool>>);

// face indexing progress
#[derive(Clone, serde::Serialize)]
pub struct FaceIndexProgress {
    pub library_id: String,
    pub current: usize,
    pub total: usize,
    pub faces_found: usize,
    pub phase: String,
}

#[derive(Clone)]
pub struct FaceIndexProgressState(pub Arc<Mutex<FaceIndexProgress>>);

// face stats
#[derive(Clone, serde::Serialize)]
pub struct FaceStats {
    pub total: usize,
    pub processed: usize,
    pub unprocessed: usize,
    pub faces: usize,
}

/// Detected face bounding box and landmarks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaceBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub confidence: f32,
    pub landmarks: Option<Vec<(f32, f32)>>, // 5 facial landmarks
}

/// Face with embedding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaceData {
    pub bbox: FaceBox,
    pub embedding: Vec<f32>,
}

pub struct FaceEngine {
    detection_model: Option<Session>,
    embedding_model: Option<Session>,
}

impl FaceEngine {
    pub fn new() -> Self {
        Self {
            detection_model: None,
            embedding_model: None,
        }
    }

    fn load_in(directory: &std::path::Path) -> Result<Self, String> {
        let load = |file: &str| {
            let path = directory.join(file);
            Session::builder()
                .map_err(|e| e.to_string())?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|e| e.to_string())?
                .with_intra_threads(2)
                .map_err(|e| e.to_string())?
                .commit_from_file(&path)
                .map_err(|e| format!("Failed to load face model {}: {e}", path.display()))
        };
        Ok(Self {
            detection_model: Some(load(t_common::DETECTION_MODEL)?),
            embedding_model: Some(load(t_common::EMBEDDING_MODEL)?),
        })
    }

    pub fn load_models(&mut self, app: &AppHandle) -> Result<(), String> {
        if !self.is_loaded() {
            let directory = app
                .path()
                .resolve("models", tauri::path::BaseDirectory::Resource)
                .map_err(|e| e.to_string())?;
            #[cfg(debug_assertions)]
            let directory = if directory.join(t_common::DETECTION_MODEL).exists() {
                directory
            } else {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/models")
            };
            *self = Self::load_in(&directory)?;
        }
        Ok(())
    }

    pub fn is_loaded(&self) -> bool {
        self.detection_model.is_some() && self.embedding_model.is_some()
    }

    fn detect_faces(&mut self, image: &DynamicImage) -> Result<Vec<FaceBox>, String> {
        let session = self
            .detection_model
            .as_mut()
            .ok_or("Face detector is unavailable")?;
        let candidates = model::detect(session, image)?;
        Ok(self.nms(candidates, 0.3))
    }

    fn get_face_embedding(
        &mut self,
        image: &DynamicImage,
        face: &FaceBox,
    ) -> Result<Vec<f32>, String> {
        let session = self
            .embedding_model
            .as_mut()
            .ok_or("Face embedding model is unavailable")?;
        model::embed(session, image, face)
    }

    /// Compute cosine similarity between two embeddings
    #[allow(dead_code)]
    pub fn compare_faces(emb1: &[f32], emb2: &[f32]) -> f32 {
        if emb1.len() != emb2.len() {
            return 0.0;
        }
        // Embeddings are already normalized, so dot product = cosine similarity
        emb1.iter().zip(emb2.iter()).map(|(a, b)| a * b).sum()
    }

    pub fn process_image_from_bytes(
        &mut self,
        image_bytes: &[u8],
    ) -> Result<(Vec<FaceData>, (u32, u32)), String> {
        let img = image::load_from_memory(image_bytes)
            .map_err(|e| format!("Failed to load image from memory: {}", e))?;
        self.process_dynamic_image(&img)
    }

    fn process_dynamic_image(
        &mut self,
        img: &DynamicImage,
    ) -> Result<(Vec<FaceData>, (u32, u32)), String> {
        let faces = self.detect_faces(img)?;

        let mut results = Vec::new();
        for face in faces {
            // Filter 1: Skip low confidence faces
            if face.confidence < t_common::MIN_CONFIDENCE {
                continue;
            }

            // Filter 2: Skip very small faces (likely background people)
            // let face_area = face.width * face.height;
            // let img_width = img.width() as f32;
            // let img_height = img.height() as f32;
            // let img_area = img_width * img_height;
            // if face_area / img_area < t_common::MIN_FACE_RATIO {
            //     continue;
            // }

            // Filter 3: Skip faces smaller than minimum pixel size
            // if face.width < t_common::MIN_FACE_SIZE || face.height < t_common::MIN_FACE_SIZE {
            //     continue;
            // }

            // Filter 4: Skip blurry faces
            let blur_score = self.calculate_blur_score(img, &face);
            if blur_score < t_common::MIN_BLUR_SCORE {
                continue;
            }

            // Get embedding for quality face
            let embedding = self.get_face_embedding(img, &face)?;
            results.push(FaceData {
                bbox: face,
                embedding,
            });
        }

        Ok((results, (img.width(), img.height())))
    }

    /// Calculate blur score using Variance of Laplacian
    /// Optimized: Uses Welford's online algorithm to avoid allocating a large vector
    fn calculate_blur_score(&self, img: &DynamicImage, bbox: &FaceBox) -> f32 {
        let x = bbox.x.max(0.0) as u32;
        let y = bbox.y.max(0.0) as u32;
        // Check bounds to ensure we don't crash on cropping
        let w = bbox.width.min(img.width() as f32 - bbox.x) as u32;
        let h = bbox.height.min(img.height() as f32 - bbox.y) as u32;

        if w < 3 || h < 3 {
            return 0.0;
        }

        let crop = img.crop_imm(x, y, w, h).to_luma8();
        let (width, height) = crop.dimensions();

        // Online variance calculation (Welford's algorithm)
        let mut count = 0usize;
        let mut m2 = 0.0;
        let mut mean = 0.0;

        for y in 1..height - 1 {
            for x in 1..width - 1 {
                let p = crop.get_pixel(x, y).0[0] as i16;
                let top = crop.get_pixel(x, y - 1).0[0] as i16;
                let bottom = crop.get_pixel(x, y + 1).0[0] as i16;
                let left = crop.get_pixel(x - 1, y).0[0] as i16;
                let right = crop.get_pixel(x + 1, y).0[0] as i16;

                let sum = top + bottom + left + right - 4 * p;
                let val = sum as f32;

                count += 1;
                let delta = val - mean;
                mean += delta / count as f32;
                let delta2 = val - mean;
                m2 += delta * delta2;
            }
        }

        if count < 2 {
            return 0.0;
        }

        // Variance
        m2 / (count as f32)
    }

    /// Non-maximum suppression
    fn nms(&self, mut boxes: Vec<FaceBox>, iou_threshold: f32) -> Vec<FaceBox> {
        boxes.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));

        let mut keep = Vec::new();
        let mut suppressed = vec![false; boxes.len()];

        for i in 0..boxes.len() {
            if suppressed[i] {
                continue;
            }
            keep.push(boxes[i].clone());

            for j in (i + 1)..boxes.len() {
                if suppressed[j] {
                    continue;
                }
                if self.iou(&boxes[i], &boxes[j]) > iou_threshold {
                    suppressed[j] = true;
                }
            }
        }

        keep
    }

    /// Intersection over Union
    /// Optimized: Simplified redundant max(0.0) for valid boxes
    fn iou(&self, a: &FaceBox, b: &FaceBox) -> f32 {
        let x1 = a.x.max(b.x);
        let y1 = a.y.max(b.y);
        let x2 = (a.x + a.width).min(b.x + b.width);
        let y2 = (a.y + a.height).min(b.y + b.height);

        if x2 <= x1 || y2 <= y1 {
            return 0.0;
        }

        let inter_area = (x2 - x1) * (y2 - y1);
        let a_area = a.width * a.height;
        let b_area = b.width * b.height;

        inter_area / (a_area + b_area - inter_area)
    }
}

#[derive(Clone)]
pub struct FaceState(pub std::sync::Arc<Mutex<FaceEngine>>);

pub fn run_face_indexing(
    app_handle: AppHandle,
    face_state: FaceState,
    cancel_token: FaceIndexCancellation,
    status: FaceIndexingStatus,
    progress: FaceIndexProgressState,
    cluster_epsilon: f32,
    automatic: bool,
) -> Result<(), String> {
    let library_id = crate::t_config::current_library_id()?;
    {
        let mut running = status.0.lock().map_err(|e| e.to_string())?;
        if *running {
            return Err("Face indexing is already running".into());
        }
        *cancel_token.0.lock().map_err(|e| e.to_string())? = false;
        *progress.0.lock().map_err(|e| e.to_string())? = FaceIndexProgress {
            library_id: library_id.clone(),
            current: 0,
            total: 0,
            faces_found: 0,
            phase: "indexing".to_owned(),
        };
        *running = true;
    }

    tauri::async_runtime::spawn_blocking(move || {
        use crate::t_cmds::with_current_library;
        use crate::t_sqlite::{AThumb, Face};
        let cancelled = || {
            *cancel_token.0.lock().unwrap()
                || !crate::t_config::current_library_id().is_ok_and(|id| id == library_id)
        };
        let mut faces_found = 0;
        let mut pending_offline = 0;
        let mut failures = 0;
        let publish = |current, total, faces_found, phase: &str| {
            let update = FaceIndexProgress {
                library_id: library_id.clone(),
                current,
                total,
                faces_found,
                phase: phase.to_owned(),
            };
            *progress.0.lock().unwrap() = update.clone();
            let _ = app_handle.emit("face_index_progress", update);
        };
        publish(0, 0, 0, "indexing");
        let run = (|| -> Result<usize, String> {
            let ((_, processed, _, existing), files) = with_current_library(&library_id, || {
                Ok((
                    Face::get_stats_full()?,
                    Face::get_unprocessed_image_files(automatic)?,
                ))
            })?;
            faces_found = existing;
            let total = processed + files.len();
            if !files.is_empty() {
                face_state.0.lock().unwrap().load_models(&app_handle)?;
            }
            publish(processed, total, faces_found, "indexing");
            for (index, target) in files.into_iter().enumerate() {
                if cancelled() {
                    return Ok(0);
                }
                let thumb = with_current_library(&library_id, || {
                    Ok(AThumb::analysis_cached(target.id, &target.path))
                })?;
                let result = if thumb.is_some() || crate::t_utils::content_readable(&target.path) {
                    let bytes = match thumb {
                        Some(bytes) => Ok(bytes),
                        None => crate::t_image::analysis_thumbnail(
                            &target.path,
                            target.file_type,
                            target.orientation,
                            640,
                        )
                        .and_then(|image| image.ok_or_else(|| "No image preview available".into())),
                    };
                    Some(bytes.and_then(|bytes| {
                        let (mut faces, (w, h)) = face_state
                            .0
                            .lock()
                            .unwrap()
                            .process_image_from_bytes(&bytes)?;
                        let x_scale = target.width as f32 / w as f32;
                        let y_scale = target.height as f32 / h as f32;
                        for face in &mut faces {
                            face.bbox.x *= x_scale;
                            face.bbox.y *= y_scale;
                            face.bbox.width *= x_scale;
                            face.bbox.height *= y_scale;
                            if let Some(points) = &mut face.bbox.landmarks {
                                for (x, y) in points {
                                    *x *= x_scale;
                                    *y *= y_scale;
                                }
                            }
                        }
                        Ok(faces)
                    }))
                } else {
                    None
                };
                if cancelled() {
                    return Ok(0);
                }
                let count = match result {
                    Some(Ok(faces)) => {
                        let stored = with_current_library(&library_id, || {
                            Face::store_detection(&target, &faces)
                        })?;
                        if stored { faces.len() } else { 0 }
                    }
                    Some(Err(_)) => {
                        with_current_library(&library_id, || Face::defer_detection(&target))?;
                        failures += 1;
                        0
                    }
                    None => {
                        with_current_library(&library_id, || Face::defer_detection(&target))?;
                        pending_offline += 1;
                        0
                    }
                };
                faces_found += count;
                let current = processed + index + 1;
                if index % 10 == 0 || current == total {
                    publish(current, total, faces_found, "indexing");
                }
            }
            publish(total, total, faces_found, "clustering");
            t_cluster::cluster_faces(
                &library_id,
                cluster_epsilon,
                |progress| {
                    let _ = app_handle.emit("cluster_progress", progress);
                },
                &cancelled,
            )
        })();
        let was_cancelled = cancelled();
        *status.0.lock().unwrap() = false;
        let (new_people, error) = match run {
            Ok(count) => (count, None),
            Err(_) if was_cancelled => (0, None),
            Err(error) => (0, Some(error)),
        };
        let _ = app_handle.emit("face_index_finished", serde_json::json!({
            "library_id": library_id, "total_faces": faces_found, "new_persons": new_people,
            "cancelled": was_cancelled, "failed": failures, "offline": pending_offline, "error": error
        }));
    });
    Ok(())
}
