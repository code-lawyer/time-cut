param([switch]$ClipOnly, [switch]$VerifyOnly)
$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir
$TargetDir = Join-Path $RootDir "src-tauri\resources\models"
if (-not (Test-Path $TargetDir)) {
    New-Item -ItemType Directory -Path $TargetDir -Force | Out-Null
}

$Models = @(
    @{ Url = "https://huggingface.co/openai/clip-vit-base-patch32/resolve/3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268/tokenizer.json"; File = "tokenizer.json"; Sha256 = "b556ac8c99757ffb677208af34bc8c6721572114111a6e0aaf5fa69ff0b8d842" },
    @{ Url = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/d15189d7028b43f1d3e65039190477f6af591c2a/onnx/text_model_quantized.onnx"; File = "text_model.onnx"; Sha256 = "73baab855d406190da9faa498cfedf65f15cf309f4cc7385b7b032e6d08e5c3a" },
    @{ Url = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/d15189d7028b43f1d3e65039190477f6af591c2a/onnx/vision_model_quantized.onnx"; File = "vision_model.onnx"; Sha256 = "583fd1110a514667812fee7d684952aaf82a99b959760c8d7dca7e0ab9839299" },
    @{ Url = "https://media.githubusercontent.com/media/opencv/opencv_zoo/47534e27c9851bb1128ccc0102f1145e27f23f98/models/face_detection_yunet/face_detection_yunet_2026may.onnx"; File = "face_detection_yunet_2026may.onnx"; Sha256 = "ebafce4e3c118d6554634be5c27ab333b4c047a9a8c3faf1d7cf93101c22f0f0" },
    @{ Url = "https://media.githubusercontent.com/media/opencv/opencv_zoo/47534e27c9851bb1128ccc0102f1145e27f23f98/models/face_recognition_sface/face_recognition_sface_2021dec.onnx"; File = "face_recognition_sface_2021dec.onnx"; Sha256 = "0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79" },
    @{ Url = "https://raw.githubusercontent.com/opencv/opencv_zoo/47534e27c9851bb1128ccc0102f1145e27f23f98/models/face_detection_yunet/LICENSE"; File = "YuNet-LICENSE.txt"; Sha256 = "c83b8120c50ccbd4c4f96edf53141bdd566ebb8f8e9227e415326aa1b1aba958" },
    @{ Url = "https://raw.githubusercontent.com/opencv/opencv_zoo/47534e27c9851bb1128ccc0102f1145e27f23f98/models/face_recognition_sface/LICENSE"; File = "SFace-LICENSE.txt"; Sha256 = "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30" }
)

if ($ClipOnly) { $Models = $Models | Where-Object { $_.File -in 'tokenizer.json','text_model.onnx','vision_model.onnx' } }
Write-Host "Downloading $($Models.Count) models into $TargetDir..."

function Test-ModelIntegrity {
    param([string]$Path, [string]$ExpectedSha256)

    $ActualSha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    if ($ActualSha256 -ne $ExpectedSha256) {
        throw "SHA-256 mismatch: $Path; remove the rejected file before downloading again."
    }
}

foreach ($model in $Models) {
    $FilePath = Join-Path $TargetDir $model.File
    if (Test-Path -LiteralPath $FilePath) {
        Test-ModelIntegrity -Path $FilePath -ExpectedSha256 $model.Sha256
        Write-Host "# $($model.File) verified, skipping download."
    } else {
        if ($VerifyOnly) { throw "Missing model input: $FilePath" }
        Write-Host "Downloading $($model.File)..."
        $Headers = @{}
        if ($env:HF_TOKEN -and ([Uri]$model.Url).Host -eq 'huggingface.co') {
            $Headers["Authorization"] = "Bearer $($env:HF_TOKEN)"
        }
        $TempFile = Join-Path $TargetDir ($model.File + ".part." + [guid]::NewGuid().ToString("N"))
        try {
            Invoke-WebRequest -Uri $model.Url -OutFile $TempFile -UseBasicParsing -Headers $Headers
            Test-ModelIntegrity -Path $TempFile -ExpectedSha256 $model.Sha256
            Move-Item -LiteralPath $TempFile -Destination $FilePath -ErrorAction Stop
            Write-Host "# $($model.File) done."
        } finally {
            if (Test-Path -LiteralPath $TempFile) {
                Remove-Item -LiteralPath $TempFile -Force
            }
        }
    }
}

Write-Host "All downloads complete!"
