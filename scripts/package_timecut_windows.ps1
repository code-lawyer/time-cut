[CmdletBinding()]
param(
    [string]$Binary,
    [string]$OnnxRuntime = $env:ORT_LIB_LOCATION,
    [string]$CrtDirectory,
    [string]$OutputDirectory,
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
if ($version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$') { throw 'Invalid package version' }
& (Join-Path $PSScriptRoot 'download_ffmpeg_sidecar.ps1') -Arch x64 -VerifyOnly
& (Join-Path $PSScriptRoot 'download_models.ps1') -VerifyOnly
if (Test-Path -LiteralPath (Join-Path $project '.git')) {
    $commit = git -C $project rev-parse --verify HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify Git source revision' }
    $changes = @(git -C $project status --porcelain --untracked-files=all)
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify source worktree state' }
    $modified = $changes.Count -gt 0
    $modules = @(git -C $project submodule status --recursive)
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify submodule revisions' }
} else {
    $export = Get-Content -Raw -LiteralPath (Join-Path $project TIME-CUT-SOURCE.json) | ConvertFrom-Json
    if ($export.version -ne $version -or $export.commit -notmatch '^[a-f0-9]{40}$') { throw 'Invalid release source metadata' }
    $commit = $export.commit
    $modules = $export.submodules
    $modified = $null # An extracted source tree has no Git index against which to check edits.
}
if (!$OutputDirectory) { $OutputDirectory = Join-Path $project 'release-test' }
$release = [IO.Directory]::CreateDirectory(
    $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
).FullName
$work = Join-Path $release ('.package-' + [guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($work)
$dest = Join-Path $work $label
$published = Join-Path $release $label
$archive = Join-Path $release "$label.zip"
$lock = $null
function Test-IdenticalDirectory([string]$candidate, [string]$existing) {
    if (!(Test-Path -LiteralPath $existing -PathType Container)) { return $false }
    $entries = @(Get-ChildItem -Force -Recurse -LiteralPath $existing)
    if ($entries.Count -ne @(Get-ChildItem -Force -Recurse -LiteralPath $candidate).Count) { return $false }
    foreach ($entry in $entries) {
        if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { return $false }
        $other = Join-Path $candidate ($entry.FullName.Substring($existing.Length + 1))
        if ($entry.PSIsContainer) {
            if (!(Test-Path -LiteralPath $other -PathType Container)) { return $false }
        } elseif (!(Test-Path -LiteralPath $other -PathType Leaf) -or
            $entry.Length -ne (Get-Item -LiteralPath $other).Length -or
            (Get-FileHash -LiteralPath $other).Hash -ne (Get-FileHash -LiteralPath $entry.FullName).Hash) {
            return $false
        }
    }
    return $true
}
try {
    # DeleteOnClose releases the per-version writer lock even if PowerShell exits.
    $lock = [IO.FileStream]::new((Join-Path $release ".$label.lock"), [IO.FileMode]::OpenOrCreate,
        [IO.FileAccess]::ReadWrite, [IO.FileShare]::None, 4096, [IO.FileOptions]::DeleteOnClose)
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
    foreach ($name in 'FFmpeg-Windows-NOTICES.txt','LibRaw\COPYRIGHT','LibRaw\LICENSE.LGPL','libheif\COPYING',
                       'libde265\COPYING','libjpeg-turbo\LICENSE.md','libjpeg-turbo\README.ijg') {
        Copy-Item -LiteralPath (Join-Path $project "src-tauri\third_party\$name") -Destination (Join-Path $licenses ($name -replace '\\','-'))
    }
    & python (Join-Path $PSScriptRoot 'windows_dependency_notices.py') --project $project --output (Join-Path $licenses 'Windows-DEPENDENCIES.txt')
    if ($LASTEXITCODE -ne 0) { throw 'Could not generate notices for the actual Windows dependencies.' }
    Copy-Item -LiteralPath (Join-Path $project 'docs\Windows-source.md') -Destination $licenses
    Copy-Item -LiteralPath (Join-Path $OnnxRuntime 'LICENSE') -Destination (Join-Path $licenses 'ONNX-Runtime-LICENSE.txt') -Force
    Copy-Item -LiteralPath (Join-Path $OnnxRuntime 'ThirdPartyNotices.txt') -Destination (Join-Path $licenses 'ONNX-Runtime-ThirdPartyNotices.txt') -Force
    Copy-Item -LiteralPath (Join-Path $project 'docs\Time-cut-Windows.md') -Destination (Join-Path $dest '使用说明.md') -Force
    Copy-Item -LiteralPath (Join-Path $project 'PRIVACY.md') -Destination $dest
    $source = [ordered]@{
        version = $version
        commit = $commit
        workingTreeModified = $modified
        submodules = $modules
        executableSHA256 = (Get-FileHash -LiteralPath (Join-Path $dest 'Time-cut.exe')).Hash
    }
    $source | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $dest 'BUILD-INFO.json') -Encoding utf8
    if (!$SkipZip) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $temporary = Join-Path $work "$label.zip"
        [IO.Compression.ZipFile]::CreateFromDirectory($dest, $temporary, [IO.Compression.CompressionLevel]::Fastest, $true)
        $hash = (Get-FileHash -LiteralPath $temporary).Hash.ToLowerInvariant()
        [IO.File]::WriteAllText("$temporary.sha256", "$hash  $label.zip`n")
    }
    # Only the freshly assembled tree is shipped. Preserve anything a user may
    # have placed alongside an earlier unpacked binary when rebuilding a version.
    $previous = $null
    $unchanged = Test-IdenticalDirectory $dest $published
    if (!$unchanged -and [IO.Directory]::Exists($published)) {
        $previous = Join-Path $release ("$label.previous-" + [guid]::NewGuid().ToString('N'))
        [IO.Directory]::Move($published, $previous)
        Write-Host "Previous unpacked directory preserved at $previous"
    }
    $updated = @()
    try {
        if (!$SkipZip) {
            foreach ($suffix in @('', '.sha256')) {
                $from = "$temporary$suffix"
                $to = "$archive$suffix"
                $backup = Join-Path $work ("previous.zip$suffix")
                if ([IO.File]::Exists($to)) { [IO.File]::Replace($from, $to, $backup) }
                else { [IO.File]::Move($from, $to) }
                $updated += $suffix
            }
        }
        if (!$unchanged) { [IO.Directory]::Move($dest, $published) }
    } catch {
        foreach ($suffix in $updated) {
            $backup = Join-Path $work ("previous.zip$suffix")
            [IO.File]::Delete("$archive$suffix")
            if ([IO.File]::Exists($backup)) { [IO.File]::Move($backup, "$archive$suffix") }
        }
        if ($previous) { [IO.Directory]::Move($previous, $published) }
        throw
    }
    if (!$SkipZip) { Get-FileHash -LiteralPath $archive | Select-Object Algorithm,Hash,Path }
} finally {
    if ($lock) { $lock.Dispose() }
    $checkedWork = [IO.Path]::GetFullPath($work)
    if ([IO.Path]::GetDirectoryName($checkedWork) -eq $release -and
        [IO.Path]::GetFileName($checkedWork) -match '^\.package-[0-9a-f]{32}$') {
        Remove-Item -LiteralPath $checkedWork -Recurse -Force
    }
}
