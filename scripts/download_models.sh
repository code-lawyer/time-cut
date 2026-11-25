#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="$ROOT_DIR/src-tauri/resources/models"
mkdir -p "$TARGET_DIR"

MODELS=(
  "https://huggingface.co/openai/clip-vit-base-patch32/resolve/3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268/tokenizer.json|tokenizer.json|b556ac8c99757ffb677208af34bc8c6721572114111a6e0aaf5fa69ff0b8d842"
  "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/d15189d7028b43f1d3e65039190477f6af591c2a/onnx/text_model_quantized.onnx|text_model.onnx|73baab855d406190da9faa498cfedf65f15cf309f4cc7385b7b032e6d08e5c3a"
  "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/d15189d7028b43f1d3e65039190477f6af591c2a/onnx/vision_model_quantized.onnx|vision_model.onnx|583fd1110a514667812fee7d684952aaf82a99b959760c8d7dca7e0ab9839299"
  "https://media.githubusercontent.com/media/opencv/opencv_zoo/47534e27c9851bb1128ccc0102f1145e27f23f98/models/face_detection_yunet/face_detection_yunet_2026may.onnx|face_detection_yunet_2026may.onnx|ebafce4e3c118d6554634be5c27ab333b4c047a9a8c3faf1d7cf93101c22f0f0"
  "https://media.githubusercontent.com/media/opencv/opencv_zoo/47534e27c9851bb1128ccc0102f1145e27f23f98/models/face_recognition_sface/face_recognition_sface_2021dec.onnx|face_recognition_sface_2021dec.onnx|0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79"
  "https://raw.githubusercontent.com/opencv/opencv_zoo/47534e27c9851bb1128ccc0102f1145e27f23f98/models/face_detection_yunet/LICENSE|YuNet-LICENSE.txt|c83b8120c50ccbd4c4f96edf53141bdd566ebb8f8e9227e415326aa1b1aba958"
  "https://raw.githubusercontent.com/opencv/opencv_zoo/47534e27c9851bb1128ccc0102f1145e27f23f98/models/face_recognition_sface/LICENSE|SFace-LICENSE.txt|cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
)

# Keep this contract in sync with contrib/aur/README.md.
if command -v sha256sum >/dev/null 2>&1; then
  SHA256=(sha256sum)
else
  SHA256=(shasum -a 256)
fi

verify_file() {
  if ! printf '%s  %s\n' "$2" "$1" | "${SHA256[@]}" -c - >/dev/null; then
    echo "SHA-256 mismatch: $1; remove the rejected file before downloading again." >&2
    return 1
  fi
}

TEMP_FILE=""
trap 'if [ -n "$TEMP_FILE" ]; then rm -f -- "$TEMP_FILE"; fi' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo "Downloading ${#MODELS[@]} models into $TARGET_DIR..."

for entry in "${MODELS[@]}"; do
  IFS='|' read -r URL FILENAME EXPECTED_SHA256 <<< "$entry"
  FILEPATH="$TARGET_DIR/$FILENAME"

  if [ -f "$FILEPATH" ]; then
    verify_file "$FILEPATH" "$EXPECTED_SHA256"
    echo "✓ $FILENAME verified, skipping download."
  else
    echo "⬇ Downloading $FILENAME..."
    TEMP_FILE="$(mktemp "$FILEPATH.part.XXXXXX")"
    CURL_ARGS=(-fL)
    if [ -n "${HF_TOKEN:-}" ] && [[ "$URL" == https://huggingface.co/* ]]; then
      CURL_ARGS+=(-H "Authorization: Bearer $HF_TOKEN")
    fi
    curl "${CURL_ARGS[@]}" -o "$TEMP_FILE" "$URL"
    verify_file "$TEMP_FILE" "$EXPECTED_SHA256"
    chmod 644 "$TEMP_FILE"
    mv -- "$TEMP_FILE" "$FILEPATH"
    TEMP_FILE=""
    echo "✓ $FILENAME done."
  fi
done

echo "All downloads complete!"
