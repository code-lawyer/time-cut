#Requires -Version 7.2
<#
.SYNOPSIS
Compare an independent Windows Shell MTP inventory with a Time-cut WPD list.
.EXAMPLE
pwsh -STA -File scripts/measure_timecut_android_inventory.ps1 -Snapshot D:\QA\galaxy.json
.EXAMPLE
pwsh -File scripts/measure_timecut_android_inventory.ps1 -Snapshot D:\QA\galaxy.json -ArchiveRoot D:\QA\album -RunId 1
.DESCRIPTION
Capture requires one unlocked Galaxy in USB file-transfer mode. -DeviceName selects an exact
This PC display name when necessary. Capture when the phone's media is stable. The snapshot
is a new local JSON file with sorted per-entry SHA-256 hashes and aggregate counts only.
Compare is offline and needs Python 3 with sqlite3. It returns 0 for matching, complete lists,
2 for a mismatch, and 1 for an unavailable or invalid measurement. File contents, local
archive integrity, and safe disconnect are checked separately by the importer.
#>
[CmdletBinding(DefaultParameterSetName='Capture')]
param(
    [Parameter(Mandatory)][string]$Snapshot,
    [Parameter(ParameterSetName='Capture')][string]$DeviceName,
    [Parameter(Mandatory, ParameterSetName='Compare')][string]$ArchiveRoot,
    [Parameter(Mandatory, ParameterSetName='Compare')][ValidateRange(1, [long]::MaxValue)][long]$RunId
)

$ErrorActionPreference = 'Stop'
$format = 'timecut-shell-path-size-sha256-v1'
$snapshotPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Snapshot)

if ($PSCmdlet.ParameterSetName -eq 'Compare') {
    # Entry encoding is exact UTF-8 path, NUL, invariant unsigned decimal byte count.
    # Only the separator changes between the Shell and the archive. Case, Unicode and
    # the storage folder are significant. Counter subtraction retains duplicate entries.
    $compare = @'
import collections, hashlib, json, pathlib, sqlite3, sys

def sha(value):
    return hashlib.sha256(value).hexdigest()

try:
    snapshot, root, run, schema = sys.argv[1:]
    saved = json.loads(pathlib.Path(snapshot).read_text(encoding="utf-8"))
    tokens = saved["tokens"]
    if (saved["format"] != schema or not isinstance(tokens, list) or not tokens
            or saved["entries"] != len(tokens) or tokens != sorted(tokens)
            or any(len(key) != 64 or any(c not in "0123456789abcdef" for c in key) for key in tokens)):
        raise ValueError("Invalid snapshot")
    shell_hash = sha("".join(tokens).encode("ascii"))
    if saved["inventorySha256"] != shell_hash:
        raise ValueError("Invalid snapshot checksum")
    dbpath = pathlib.Path(root, ".time-cut", "imports.sqlite3").resolve(strict=True)
    db = sqlite3.connect(dbpath.as_uri() + "?mode=ro", uri=True)
    db.execute("BEGIN")
    info = db.execute("""SELECT r.source, l.total FROM phone_runs r JOIN phone_lists l
                         ON l.run_id=r.id WHERE r.id=?""", (int(run),)).fetchone()
    if not info or not info[0].startswith("wpd:"):
        raise ValueError("Selected run has no complete WPD list")
    app = []
    unknown = rows = 0
    for path, size in db.execute("SELECT entry,size FROM phone_list_items WHERE run_id=?", (int(run),)):
        rows += 1
        if not isinstance(size, int) or size < 0:
            unknown += 1
            continue
        path = path.replace("\\", "/")
        app.append(sha(path.encode("utf-8") + b"\0" + str(size).encode("ascii")))
    db.close()
    app.sort()
    app_hash = sha("".join(app).encode("ascii"))
    shell_count, app_count = collections.Counter(tokens), collections.Counter(app)
    missing = sum((shell_count - app_count).values())
    extra = sum((app_count - shell_count).values())
    print(json.dumps(dict(runId=int(run), shellEntries=len(tokens), appListed=info[1],
                          appRows=rows, appUnknownSizes=unknown, shellOnly=missing, appOnly=extra,
                          matched=len(tokens)-missing, shellSha256=shell_hash, appSha256=app_hash)))
    sys.exit(0 if rows == info[1] and unknown == missing == extra == 0 else 2)
except (OSError, sqlite3.Error, ValueError, KeyError, TypeError) as exc:
    print("Offline comparison failed (" + type(exc).__name__ + "). Check the snapshot, archive and run ID.", file=sys.stderr)
    sys.exit(1)
'@
    $compare | & python - $snapshotPath $ArchiveRoot $RunId $format
    exit $LASTEXITCODE
}

function Get-MediaExtensions {
    # Use precisely the selectors in t_utils::get_file_type plus ingest::archive::media.
    $source = [IO.File]::ReadAllText((Join-Path $PSScriptRoot '../src-tauri/src/t_common.rs'))
    $source = [regex]::Replace($source, '(?m)//[^\r\n]*', '')
    $extensions = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($constant in 'NORMAL_IMGS','FFMPEG_BACKED_IMGS','AFFINITY_IMGS','RAW_IMGS','VIDEOS') {
        $match = [regex]::Match($source, "(?s)pub const ${constant}:\s*&\[&str\]\s*=\s*&\[(.*?)\];")
        $values = [regex]::Matches($match.Groups[1].Value, '"([a-zA-Z0-9]+)"')
        if (!$match.Success -or !$values.Count) { throw 'Cannot read the application format registry.' }
        foreach ($value in $values) { [void]$extensions.Add($value.Groups[1].Value) }
    }
    [void]$extensions.Add('aae')
    [void]$extensions.Add('xmp')
    return ,$extensions
}

function Get-Sha256([string]$Value) {
    [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData(
        [Text.Encoding]::UTF8.GetBytes($Value))).ToLowerInvariant()
}

if ([IO.File]::Exists($snapshotPath)) { throw 'Choose a new snapshot file.' }
if ([Threading.Thread]::CurrentThread.GetApartmentState() -ne 'STA') { throw 'Run capture with pwsh -STA.' }
$extensions = Get-MediaExtensions
$shell = New-Object -ComObject Shell.Application
$devices = $shell.NameSpace(17).Items()
$phones = [Collections.Generic.List[object]]::new()
for ($index = 0; $index -lt $devices.Count; $index++) {
    $item = $devices.Item($index)
    $selected = if ($DeviceName) { $item.Name -eq $DeviceName } else { $item.Name -match 'Galaxy' }
    if ($selected -and $item.IsFolder -and !$item.IsFileSystem) { $phones.Add($item) }
}
if ($phones.Count -ne 1) { throw "Expected one matching portable device, found $($phones.Count)." }

# Use the indexed FolderItems COM collection and FolderItem.GetFolder property; PowerShell
# pipeline enumeration of Shell folders can unwrap a single FolderItem instead of a collection.
$stack = [Collections.Generic.Stack[object]]::new()
$stack.Push(@{ Folder = $phones[0]; Path = '' })
$tokens = [Collections.Generic.List[string]]::new()
$folders = $stores = $errors = $unknownSizes = 0
while ($stack.Count) {
    $current = $stack.Pop()
    try {
        $children = $current.Folder.GetFolder.Items()
        if ($null -eq $children) { throw 'Folder unavailable' }
        $count = $children.Count
        for ($index = 0; $index -lt $count; $index++) {
            $item = $children.Item($index)
            if ($null -eq $item) { throw 'Item unavailable' }
            $name = [string]$item.Name
            if (!$item.IsFolder) {
                # Explorer may hide known extensions. System.FileName contains the original leaf.
                $original = [string]$item.ExtendedProperty('System.FileName')
                if ($original) { $name = $original }
                else {
                    $suffix = [string]$item.ExtendedProperty('System.FileExtension')
                    if ($suffix -and !$suffix.StartsWith('.')) { $suffix = '.' + $suffix }
                    if ($suffix -and !$name.EndsWith($suffix, [StringComparison]::OrdinalIgnoreCase)) {
                        $name += $suffix
                    }
                }
            }
            if (!$name) { throw 'Item name unavailable' }
            if ($name -eq '.' -or $name -eq '..') { continue }
            $leaf = $name -replace '[/\\:]', '_'
            $path = if ($current.Path) { $current.Path + '/' + $leaf } else { $leaf }
            if ($item.IsFolder) {
                if (!$current.Path) { $stores++ }
                $folders++
                $stack.Push(@{ Folder = $item; Path = $path })
                continue
            }
            if (!$extensions.Contains([IO.Path]::GetExtension($leaf).TrimStart('.'))) { continue }
            $rawSize = $item.ExtendedProperty('System.Size')
            $size = 0L
            if ($null -eq $rawSize -or ![long]::TryParse([string]$rawSize, [ref]$size) -or $size -lt 0) {
                $unknownSizes++
                continue
            }
            $tokens.Add((Get-Sha256 ($path + [char]0 + $size.ToString([Globalization.CultureInfo]::InvariantCulture))))
        }
        if ($children.Count -ne $count) { throw 'Directory changed during enumeration' }
    } catch { $errors++ }
}
if (!$stores -or !$tokens.Count -or $errors -or $unknownSizes) {
    throw "Incomplete capture (storage folders $stores, measured entries $($tokens.Count), enumeration errors $errors, unknown sizes $unknownSizes)."
}
$tokens.Sort([StringComparer]::Ordinal)
$summary = [ordered]@{ format=$format; storageFolders=$stores; folders=$folders; entries=$tokens.Count
                       inventorySha256=(Get-Sha256 ([string]::Join('', $tokens))) }
$snapshotData = [ordered]@{}
foreach ($key in $summary.Keys) { $snapshotData[$key] = $summary[$key] }
$snapshotData['tokens'] = $tokens
$bytes = [Text.Encoding]::UTF8.GetBytes(($snapshotData | ConvertTo-Json -Compress -Depth 3))
$output = [IO.File]::Open($snapshotPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try { $output.Write($bytes, 0, $bytes.Length) } finally { $output.Dispose() }
$summary | ConvertTo-Json -Compress
