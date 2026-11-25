[CmdletBinding()]
param(
    [string]$Binary,
    [string]$OnnxRuntime = $env:ORT_LIB_LOCATION,
    [string]$CrtDirectory,
    [switch]$SkipZip
)
$ErrorActionPreference = 'Stop'
$project = Split-Path $PSScriptRoot -Parent
if (!$Binary) { $Binary = Join-Path $project 'src-tauri\target\release\Lap.exe' }
if (!$OnnxRuntime) { throw 'Set ORT_LIB_LOCATION to the Microsoft ONNX Runtime 1.22.0 win-x64 native directory.' }
if (!$CrtDirectory) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    $CrtDirectory = (Get-Item "$vs\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT" | Sort-Object FullName -Descending | Select-Object -First 1).FullName
}
if (!(Test-Path -LiteralPath (Join-Path $CrtDirectory 'vcruntime140.dll'))) { throw 'App-local VC++ x64 runtime not found.' }
$version = (Get-Content -Raw -LiteralPath (Join-Path $project 'src-tauri\tauri.conf.json') | ConvertFrom-Json).version
$label = "Time-cut-$version-windows-x64"
$release = Join-Path $project 'release-test'
$dest = Join-Path $release $label
$licenses = Join-Path $dest 'licenses'
$models = Join-Path $dest 'models'
$ffmpeg = Join-Path $dest 'ffmpeg'
New-Item -ItemType Directory -Force -Path $dest,$licenses,$models,$ffmpeg | Out-Null
Copy-Item -LiteralPath $Binary -Destination (Join-Path $dest 'Time-cut.exe') -Force
foreach ($name in 'onnxruntime.dll','onnxruntime_providers_shared.dll') {
    Copy-Item -LiteralPath (Join-Path $OnnxRuntime $name) -Destination $dest -Force
}
Get-ChildItem -LiteralPath $CrtDirectory -File -Filter '*.dll' | Copy-Item -Destination $dest -Force
foreach ($name in 'text_model.onnx','vision_model.onnx','tokenizer.json','face_detection_yunet_2026may.onnx','face_recognition_sface_2021dec.onnx') {
    Copy-Item -LiteralPath (Join-Path $project "src-tauri\resources\models\$name") -Destination $models -Force
}
foreach ($name in 'YuNet-LICENSE.txt','SFace-LICENSE.txt') {
    Copy-Item -LiteralPath (Join-Path $project "src-tauri\resources\models\$name") -Destination $licenses -Force
}
foreach ($name in 'ffmpeg-x86_64-pc-windows-msvc.exe','ffprobe-x86_64-pc-windows-msvc.exe') {
    Copy-Item -LiteralPath (Join-Path $project "src-tauri\resources\ffmpeg\$name") -Destination $ffmpeg -Force
}
Copy-Item -LiteralPath (Join-Path $project LICENSE),(Join-Path $project THIRD_PARTY_NOTICES.txt) -Destination $licenses -Force
Copy-Item -LiteralPath (Join-Path $project 'src-tauri\third_party\idevice-LICENSE.txt') -Destination $licenses -Force
Copy-Item -LiteralPath (Join-Path $OnnxRuntime 'LICENSE') -Destination (Join-Path $licenses 'ONNX-Runtime-LICENSE.txt') -Force
Copy-Item -LiteralPath (Join-Path $OnnxRuntime 'ThirdPartyNotices.txt') -Destination (Join-Path $licenses 'ONNX-Runtime-ThirdPartyNotices.txt') -Force
Copy-Item -LiteralPath (Join-Path $project 'docs\Time-cut-Windows.md') -Destination (Join-Path $dest '使用说明.md') -Force
if (!$SkipZip) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $temporary = Join-Path $release ("$label-" + [guid]::NewGuid().ToString('N') + '.zip')
    [System.IO.Compression.ZipFile]::CreateFromDirectory($dest,$temporary,[System.IO.Compression.CompressionLevel]::Fastest,$true)
    Move-Item -LiteralPath $temporary -Destination (Join-Path $release "$label.zip") -Force
    Get-FileHash -LiteralPath (Join-Path $release "$label.zip") | Select-Object Algorithm,Hash,Path
}
