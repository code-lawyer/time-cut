[CmdletBinding()]
param(
    [ValidateSet("x64", "x86_64", "arm64", "aarch64")]
    [string]$Arch,
    [switch]$VerifyOnly
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir
$TargetDir = Join-Path $RootDir "src-tauri\resources\ffmpeg"
if (-not (Test-Path $TargetDir)) {
    New-Item -ItemType Directory -Path $TargetDir -Force | Out-Null
}

if (-not $Arch) {
    $procArch = $env:PROCESSOR_ARCHITECTURE
    if ($procArch -eq "ARM64") {
        $Arch = "arm64"
    }
    else {
        $Arch = "x64"
    }
    Write-Host "No -Arch specified; auto-detected $Arch (PROCESSOR_ARCHITECTURE=$procArch)."
}

switch ($Arch) {
    { $_ -in "x64", "x86_64" } {
        $TripleSuffix = "x86_64-pc-windows-msvc"
    }
    { $_ -in "arm64", "aarch64" } {
        $TripleSuffix = "aarch64-pc-windows-msvc"
    }
    default {
        throw "Unsupported architecture: $Arch"
    }
}

$Downloads = if ($TripleSuffix -eq 'x86_64-pc-windows-msvc') {
    @(
        @{ Name = 'ffmpeg'; Id = 405019646; Hash = '1a65d5b0b10d8d9a81d2824a3538046a40ed3607c906b335a166add87613f705' },
        @{ Name = 'ffprobe'; Id = 405019652; Hash = '54b944d6095c4588d7548424d7acd5118390a9d4f01b1c92f841547fc9a7429c' }
    )
} else {
    @(
        @{ Name = 'ffmpeg'; Id = 405019640; Hash = '2cc705fb32146906817c8bd0dd599f5b6f09146c003728420d0fa60617b32708' },
        @{ Name = 'ffprobe'; Id = 405019641; Hash = '22ad65e855310b2c6c0f8f9858f6fd7fe69788f7265fc12abd8d21455bcfbb5c' }
    )
}

function Assert-SidecarHash([string]$Path, [string]$Expected) {
    if ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -ne $Expected) {
        throw "SHA-256 mismatch for $Path"
    }
}

Write-Host "Downloading FFmpeg sidecar files ($Arch) into $TargetDir..."
foreach ($Asset in $Downloads) {
    $Filename = "$($Asset.Name)-$TripleSuffix.exe"
    $Url = "https://api.github.com/repos/julyx10/lap-binaries/releases/assets/$($Asset.Id)"
    $Dest = Join-Path $TargetDir $Filename

    if (Test-Path -LiteralPath $Dest) {
        Assert-SidecarHash $Dest $Asset.Hash
        Write-Host "# $Filename verified."
        continue
    }
    if ($VerifyOnly) { throw "Missing FFmpeg sidecar: $Dest" }

    Write-Host "# Downloading $Filename..."
    $Partial = "$Dest.part.$([guid]::NewGuid().ToString('N'))"
    try {
        Invoke-WebRequest -Uri $Url -Headers @{ Accept = 'application/octet-stream' } -OutFile $Partial -UseBasicParsing
        Assert-SidecarHash $Partial $Asset.Hash
        [IO.File]::Move($Partial, $Dest)
    } finally {
        if (Test-Path -LiteralPath $Partial) { Remove-Item -LiteralPath $Partial }
    }
    Write-Host "# $Filename downloaded."
}

Write-Host "All FFmpeg sidecar files downloaded into $TargetDir."
