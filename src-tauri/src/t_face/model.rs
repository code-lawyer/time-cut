use super::FaceBox;
use crate::t_common::MIN_CONFIDENCE;
use image::{DynamicImage, GenericImageView, imageops::FilterType};
use ndarray::Array4;
use ort::{inputs, session::Session, value::Value};

/// YuNet output is laid out on stride 8, 16 and 32 grids. Each grid has
/// independent classification, objectness, box and landmark tensors.
pub(super) fn detect(session: &mut Session, image: &DynamicImage) -> Result<Vec<FaceBox>, String> {
    let (width, height) = image.dimensions();
    if width < 16 || height < 16 {
        return Ok(Vec::new());
    }
    let scale = (640.0 / width.max(height) as f32).min(1.0);
    let resized_width = (width as f32 * scale).round().max(1.0) as u32;
    let resized_height = (height as f32 * scale).round().max(1.0) as u32;
    let padded_width = resized_width.next_multiple_of(32) as usize;
    let padded_height = resized_height.next_multiple_of(32) as usize;
    let resized = image
        .resize_exact(resized_width, resized_height, FilterType::Triangle)
        .into_rgb8();
    let mut tensor = Array4::<f32>::zeros((1, 3, padded_height, padded_width));
    for (x, y, pixel) in resized.enumerate_pixels() {
        // YuNet receives the BGR channel order and raw pixel values used by OpenCV.
        for channel in 0..3 {
            tensor[[0, channel, y as usize, x as usize]] = f32::from(pixel[2 - channel]);
        }
    }
    let outputs = session
        .run(inputs![
            Value::from_array(tensor).map_err(|e| e.to_string())?
        ])
        .map_err(|e| format!("YuNet inference: {e}"))?;
    let mut faces = Vec::new();
    let scale_x = resized_width as f32 / width as f32;
    let scale_y = resized_height as f32 / height as f32;
    for stride in [8usize, 16, 32] {
        let columns = padded_width / stride;
        let rows = padded_height / stride;
        let cells = rows * columns;
        let tensor = |prefix: &str, channels: usize| -> Result<&[f32], String> {
            let key = format!("{prefix}_{stride}");
            let output = outputs
                .get(&key)
                .ok_or_else(|| format!("Missing YuNet output {key}"))?;
            let (_, data) = output
                .try_extract_tensor::<f32>()
                .map_err(|e| e.to_string())?;
            if data.len() != cells * channels {
                return Err(format!("Unexpected shape for YuNet output {key}"));
            }
            Ok(data)
        };
        let scores = tensor("cls", 1)?;
        let objects = tensor("obj", 1)?;
        let boxes = tensor("bbox", 4)?;
        let landmarks = tensor("kps", 10)?;
        let step = stride as f32;
        for index in 0..cells {
            let confidence =
                (scores[index].clamp(0.0, 1.0) * objects[index].clamp(0.0, 1.0)).sqrt();
            if !confidence.is_finite() || confidence < MIN_CONFIDENCE {
                continue;
            }
            let col = (index % columns) as f32;
            let row = (index / columns) as f32;
            let b = &boxes[index * 4..][..4];
            let cx = (col + b[0]) * step;
            let cy = (row + b[1]) * step;
            let w = b[2].exp() * step;
            let h = b[3].exp() * step;
            if ![cx, cy, w, h].iter().all(|n| n.is_finite())
                || cx < 0.0
                || cx >= resized_width as f32
                || cy < 0.0
                || cy >= resized_height as f32
            {
                continue;
            }
            let x = ((cx - w / 2.0) / scale_x).max(0.0);
            let y = ((cy - h / 2.0) / scale_y).max(0.0);
            let right = ((cx + w / 2.0) / scale_x).min(width as f32);
            let bottom = ((cy + h / 2.0) / scale_y).min(height as f32);
            if right - x < 12.0 || bottom - y < 12.0 {
                continue;
            }
            let keypoints: Vec<_> = landmarks[index * 10..][..10]
                .chunks_exact(2)
                .map(|p| ((col + p[0]) * step / scale_x, (row + p[1]) * step / scale_y))
                .collect();
            if keypoints
                .iter()
                .all(|&(x, y)| x.is_finite() && y.is_finite())
            {
                faces.push(FaceBox {
                    x,
                    y,
                    width: right - x,
                    height: bottom - y,
                    confidence,
                    landmarks: Some(keypoints),
                });
            }
        }
    }
    Ok(faces)
}

// OpenCV SFace's 112 × 112 alignment template, in YuNet landmark order.
const TEMPLATE: [(f32, f32); 5] = [
    (38.2946, 51.6963),
    (73.5318, 51.5014),
    (56.0252, 71.7366),
    (41.5493, 92.3655),
    (70.7299, 92.2041),
];

/// Fit a similarity transform from aligned pixels back into the original image.
fn align(points: &[(f32, f32)]) -> Option<[f32; 4]> {
    if points.len() != TEMPLATE.len() {
        return None;
    }
    let mean = |points: &[(f32, f32)]| {
        let (x, y) = points
            .iter()
            .fold((0.0, 0.0), |(sum_x, sum_y), &(x, y)| (sum_x + x, sum_y + y));
        (x / points.len() as f32, y / points.len() as f32)
    };
    let (tx, ty) = mean(&TEMPLATE);
    let (sx, sy) = mean(points);
    let mut dot = 0.0;
    let mut cross = 0.0;
    let mut norm = 0.0;
    for (&(x, y), &(u, v)) in TEMPLATE.iter().zip(points) {
        let (x, y) = (x - tx, y - ty);
        let (u, v) = (u - sx, v - sy);
        dot += x * u + y * v;
        cross += x * v - y * u;
        norm += x * x + y * y;
    }
    let (a, b) = (dot / norm, cross / norm);
    if !a.is_finite() || !b.is_finite() || a * a + b * b <= f32::EPSILON {
        return None;
    }
    Some([a, b, sx - a * tx + b * ty, sy - b * tx - a * ty])
}

fn sample(image: &DynamicImage, x: f32, y: f32) -> [f32; 3] {
    let x0 = x.floor() as i64;
    let y0 = y.floor() as i64;
    let mut rgb = [0.0; 3];
    for dy in 0..=1 {
        for dx in 0..=1 {
            let (u, v) = (x0 + dx, y0 + dy);
            if u >= 0 && u < i64::from(image.width()) && v >= 0 && v < i64::from(image.height()) {
                let weight = (1.0 - (x - u as f32).abs()) * (1.0 - (y - v as f32).abs());
                let pixel = image.get_pixel(u as u32, v as u32);
                for channel in 0..3 {
                    rgb[channel] += weight * f32::from(pixel[channel]);
                }
            }
        }
    }
    rgb
}

pub(super) fn embed(
    session: &mut Session,
    image: &DynamicImage,
    face: &FaceBox,
) -> Result<Vec<f32>, String> {
    let [a, b, dx, dy] = face
        .landmarks
        .as_deref()
        .and_then(align)
        .ok_or("Invalid face landmarks")?;
    let mut tensor = Array4::<f32>::zeros((1, 3, 112, 112));
    for y in 0..112 {
        for x in 0..112 {
            let rgb = sample(
                image,
                a * x as f32 - b * y as f32 + dx,
                b * x as f32 + a * y as f32 + dy,
            );
            for channel in 0..3 {
                tensor[[0, channel, y, x]] = rgb[channel];
            }
        }
    }
    // The SFace graph applies normalization. Its inputs are raw RGB pixels.
    let outputs = session
        .run(inputs![
            Value::from_array(tensor).map_err(|e| e.to_string())?
        ])
        .map_err(|e| format!("SFace inference: {e}"))?;
    let (_, vector) = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| e.to_string())?;
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if vector.len() != 128 || !norm.is_finite() || norm <= f32::EPSILON {
        return Err("Invalid SFace embedding".into());
    }
    Ok(vector.iter().map(|x| x / norm).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn face_alignment_recovers_rotation_scale_and_translation() {
        let measured: Vec<_> = TEMPLATE
            .iter()
            .map(|&(x, y)| (x * 1.6 - y * 0.2 + 10.0, x * 0.2 + y * 1.6 - 4.0))
            .collect();
        for (actual, expected) in align(&measured)
            .unwrap()
            .into_iter()
            .zip([1.6, 0.2, 10.0, -4.0])
        {
            assert!((actual - expected).abs() < 0.001);
        }
        assert!(align(&[(0.0, 0.0); 5]).is_none());
    }
}
