[CmdletBinding()]
param(
    [string]$Package,
    [Parameter(Mandatory)][string]$Destination
)
$ErrorActionPreference = 'Stop'
$expected = 'zb3eqkL6hHPGzZgArsuTH9VWdUs4ooKTJHBXF6ZZfQsPvzModaAEv3u69zwtqKuwUJ/DPc8i+uKoyXMzdwqrPg=='
$expectedHex = [BitConverter]::ToString([Convert]::FromBase64String($expected)).Replace('-', '')
$download = !$Package
try {
    if ($download) {
        $Package = [IO.Path]::GetTempFileName()
        Invoke-WebRequest -UseBasicParsing -OutFile $Package -Uri 'https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/1.22.0/microsoft.ml.onnxruntime.1.22.0.nupkg'
    }
    if ((Get-FileHash -LiteralPath $Package -Algorithm SHA512).Hash -ne $expectedHex) {
        throw 'Microsoft.ML.OnnxRuntime 1.22.0 SHA-512 mismatch.'
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $target = [IO.Directory]::CreateDirectory(
        $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Destination)
    ).FullName
    $zip = [IO.Compression.ZipFile]::OpenRead((Resolve-Path -LiteralPath $Package))
    try {
        foreach ($entry in @(
            'LICENSE', 'ThirdPartyNotices.txt',
            'runtimes/win-x64/native/onnxruntime.dll', 'runtimes/win-x64/native/onnxruntime.lib',
            'runtimes/win-x64/native/onnxruntime_providers_shared.dll', 'runtimes/win-x64/native/onnxruntime_providers_shared.lib'
        )) {
            $out = Join-Path $target ([IO.Path]::GetFileName($entry))
            [IO.Compression.ZipFileExtensions]::ExtractToFile($zip.GetEntry($entry), $out, $true)
        }
    } finally { $zip.Dispose() }
} finally {
    if ($download -and $Package -and (Test-Path -LiteralPath $Package)) { Remove-Item -LiteralPath $Package }
}
