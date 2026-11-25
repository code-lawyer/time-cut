#Requires -Version 7.2
<#
.SYNOPSIS
Compare an independent Windows Shell MTP inventory with a Time-cut WPD list.
.EXAMPLE
pwsh -STA -File scripts/measure_timecut_android_inventory.ps1 -Snapshot D:\QA\galaxy.json
.EXAMPLE
pwsh -STA -File scripts/measure_timecut_android_inventory.ps1 -Scope Gallery -Snapshot D:\QA\gallery.json
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
    [Parameter(ParameterSetName='Capture')][ValidateSet('Full','Gallery')][string]$Scope = 'Full',
    [Parameter(ParameterSetName='Capture')][switch]$HonorNoMedia = $true,
    [Parameter(ParameterSetName='Capture')][string]$FixtureDeviceRoot,
    [Parameter(ParameterSetName='Capture')][string[]]$FixtureStorageNames = @(),
    [Parameter(Mandatory, ParameterSetName='Compare')][string]$ArchiveRoot,
    [Parameter(Mandatory, ParameterSetName='Compare')][ValidateRange(1, [long]::MaxValue)][long]$RunId
)

$ErrorActionPreference = 'Stop'
$format = 'timecut-shell-path-size-sha256-v1'
$galleryFormat = 'timecut-shell-gallery-path-size-sha256-v1'
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
    snapshot, root, run, schema, gallery_schema = sys.argv[1:]
    saved = json.loads(pathlib.Path(snapshot).read_text(encoding="utf-8"))
    tokens = saved["tokens"]
    gallery = saved["format"] == gallery_schema
    if (saved["format"] not in (schema, gallery_schema) or not isinstance(tokens, list) or (not gallery and not tokens)
            or saved["entries"] != len(tokens) or tokens != sorted(tokens)
            or any(len(key) != 64 or any(c not in "0123456789abcdef" for c in key) for key in tokens)):
        raise ValueError("Invalid snapshot")
    public = {"dcim", "pictures", "movies"}
    if gallery and (saved.get("galleryRoots") != ["DCIM", "Pictures", "Movies"] or saved.get("rootFolders", 0) < 1
            or not isinstance(saved.get("storageKeys"), list) or not isinstance(saved.get("honorNoMedia"), bool)):
        raise ValueError("Invalid gallery scope")
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
    unknown = rows = outside = 0
    for path, size in db.execute("SELECT entry,size FROM phone_list_items WHERE run_id=?", (int(run),)):
        rows += 1
        path = path.replace("\\", "/")
        if gallery:
            components = path.split("/")
            if len(components) >= 2 and sha(components[0].encode("utf-8")) in saved["storageKeys"]:
                components = components[1:]
            if (len(components) < 2 or components[0].lower() not in public or
                    any(not item or item.startswith(".") for item in components)):
                outside += 1
        if not isinstance(size, int) or size < 0:
            unknown += 1
            continue
        app.append(sha(path.encode("utf-8") + b"\0" + str(size).encode("ascii")))
    db.close()
    app.sort()
    app_hash = sha("".join(app).encode("ascii"))
    shell_count, app_count = collections.Counter(tokens), collections.Counter(app)
    missing = sum((shell_count - app_count).values())
    extra = sum((app_count - shell_count).values())
    print(json.dumps(dict(runId=int(run), shellEntries=len(tokens), appListed=info[1],
                          appRows=rows, appUnknownSizes=unknown, shellOnly=missing, appOnly=extra,
                          matched=len(tokens)-missing, scope="gallery" if gallery else "full",
                          appOutsideSelectedRoots=outside if gallery else None, shellSha256=shell_hash, appSha256=app_hash)))
    sys.exit(0 if rows == info[1] and unknown == missing == extra == outside == 0 else 2)
except (OSError, sqlite3.Error, ValueError, KeyError, TypeError) as exc:
    print("Offline comparison failed (" + type(exc).__name__ + "). Check the snapshot, archive and run ID.", file=sys.stderr)
    sys.exit(1)
'@
    $compare | & python - $snapshotPath $ArchiveRoot $RunId $format $galleryFormat
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
if ($HonorNoMedia -and $Scope -ne 'Gallery') { throw '-HonorNoMedia requires -Scope Gallery.' }
if ($FixtureStorageNames.Count -and !$FixtureDeviceRoot) { throw 'Fixture storage names can only describe a local test directory.' }
$gallery = $Scope -eq 'Gallery'
$public = @('DCIM', 'Pictures', 'Movies')
$extensions = Get-MediaExtensions
$shell = New-Object -ComObject Shell.Application
if ($FixtureDeviceRoot) {
    if ($DeviceName) { throw 'Choose either a physical device or a local fixture.' }
    $local = (Resolve-Path -LiteralPath $FixtureDeviceRoot).ProviderPath
    $device = $shell.NameSpace($local).Self
    if (!$device) { throw 'No local fixture namespace.' }
} else {
    $devices = $shell.NameSpace(17).Items()
    $phones = [Collections.Generic.List[object]]::new()
    for ($index = 0; $index -lt $devices.Count; $index++) {
        $item = $devices.Item($index)
        $selected = if ($DeviceName) { $item.Name -eq $DeviceName } else { $item.Name -match 'Galaxy' }
        if ($selected -and $item.IsFolder -and !$item.IsFileSystem) { $phones.Add($item) }
    }
    if ($phones.Count -ne 1) { throw "Expected one matching portable device, found $($phones.Count)." }
    $device = $phones[0]
}

function Test-Storage($Item) {
    if ($FixtureDeviceRoot) { return $Item.Name -cin $FixtureStorageNames }
    # WPD storage functional category from PortableDevice.h. Shell namespace providers
    # also expose capacity or use an opaque SID storage component in the parsing path.
    $category = [string]$Item.ExtendedProperty('{8F052D93-ABCA-4FC5-A5AC-B01DF4DBE598} 2')
    if ($category.Trim('{}') -eq '23F05BBC-15DE-4C2A-A55B-A9AF5CE412EF') { return $true }
    foreach ($property in 'System.Capacity','{01A3057A-74D6-4E80-BEA7-DC4C212CE50A} 4') {
        $capacity = 0L
        if ([long]::TryParse([string]$Item.ExtendedProperty($property), [ref]$capacity) -and $capacity -gt 0) { return $true }
    }
    return [string]$Item.Path -match '[\\/]SID-\{[0-9a-f]+,[^{}\\/]*\}$'
}

# Keep names and opaque device IDs in memory only. Stage each directory's files and
# descendants until its full listing has ruled out a visible .nomedia marker.
$stack = [Collections.Generic.Stack[object]]::new()
$stack.Push(@{Folder=$device; Path=''; Role='Device'; Category=''})
$tokens = [Collections.Generic.List[string]]::new()
$storageKeys = [Collections.Generic.List[string]]::new()
$categoryCounts = [ordered]@{DCIM=0; Pictures=0; Movies=0}
$folders = $stores = $errors = $unknownSizes = $rootFolders = $noMediaFolders = 0
$measuredBytes = 0L
$hiddenEntries = 0
while ($stack.Count) {
    $current = $stack.Pop()
    try {
        $children = $current.Folder.GetFolder.Items()
        if ($null -eq $children) { throw 'Folder unavailable' }
        $count = $children.Count
        $queue = [Collections.Generic.List[object]]::new()
        $files = [Collections.Generic.List[object]]::new()
        $suppressed = $false
        for ($index = 0; $index -lt $count; $index++) {
            $item = $children.Item($index)
            if ($null -eq $item) { throw 'Item unavailable' }
            $name = [string]$item.Name
            if (!$item.IsFolder) {
                $original = [string]$item.ExtendedProperty('System.FileName')
                if ($original) { $name = $original }
                else {
                    $suffix = [string]$item.ExtendedProperty('System.FileExtension')
                    if ($suffix -and !$suffix.StartsWith('.')) { $suffix = '.' + $suffix }
                    if ($suffix -and !$name.EndsWith($suffix,[StringComparison]::OrdinalIgnoreCase)) { $name += $suffix }
                }
            }
            if (!$name) { throw 'Item name unavailable' }
            if ($HonorNoMedia -and $current.Role -eq 'Gallery' -and !$item.IsFolder -and $name -eq '.nomedia') { $suppressed=$true; break }
            if ($name -eq '.' -or $name -eq '..') { continue }
            if ($gallery -and $name.StartsWith('.')) { $hiddenEntries++; continue }
            $leaf = $name -replace '[/\\:]', '_'
            $path = if ($current.Path) { $current.Path + '/' + $leaf } else { $leaf }
            if ($item.IsFolder) {
                $role=$current.Role; $category=$current.Category
                if (!$gallery) {
                    if (!$current.Path) { $stores++ }
                } elseif ($current.Role -in @('Device','Storage')) {
                    if ($current.Role -eq 'Device' -and (Test-Storage $item)) {
                        $stores++; $storageKeys.Add((Get-Sha256 $leaf)); $role='Storage'
                    } elseif ($leaf -in $public) {
                        $role='Gallery'; $category=$public[$public.ToLowerInvariant().IndexOf($leaf.ToLowerInvariant())]; $rootFolders++
                    } else { continue }
                }
                $queue.Add(@{ Folder=$item; Path=$path; Role=$role; Category=$category })
            } elseif ((!$gallery -or $current.Role -eq 'Gallery') -and $extensions.Contains([IO.Path]::GetExtension($leaf).TrimStart('.'))) {
                $files.Add(@{Item=$item;Path=$path})
            }
        }
        if ($children.Count -ne $count) { throw 'Directory changed during enumeration' }
        if ($suppressed) { $noMediaFolders++; continue }
        foreach ($next in $queue) { $stack.Push($next); $folders++ }
        foreach ($file in $files) {
            $rawSize = $file.Item.ExtendedProperty('System.Size')
            $size = 0L
            if ($null -eq $rawSize -or ![long]::TryParse([string]$rawSize, [ref]$size) -or $size -lt 0) { $unknownSizes++; continue }
            $tokens.Add((Get-Sha256 ($file.Path + [char]0 + $size.ToString([Globalization.CultureInfo]::InvariantCulture))))
            $measuredBytes += $size
            if ($gallery) { $categoryCounts[$current.Category]++ }
        }
    } catch { $errors++ }
}
if (($gallery -and !$rootFolders) -or (!$gallery -and (!$stores -or !$tokens.Count)) -or $errors -or $unknownSizes) {
    throw "Incomplete capture (storage folders $stores, gallery roots $rootFolders, measured entries $($tokens.Count), enumeration errors $errors, unknown sizes $unknownSizes)."
}
$tokens.Sort([StringComparer]::Ordinal)
$summary = [ordered]@{ format=if($gallery){$galleryFormat}else{$format}; storageFolders=$stores; folders=$folders; entries=$tokens.Count
                       inventorySha256=(Get-Sha256 ([string]::Join('', $tokens))) }
if ($gallery) {
    $summary['galleryRoots']=$public
    $summary['rootFolders']=$rootFolders
    $summary['knownBytes']=$measuredBytes
    $summary['hiddenEntriesSkipped']=$hiddenEntries
    $summary['entriesByRoot']=$categoryCounts
    $summary['honorNoMedia']=[bool]$HonorNoMedia
    $summary['noMediaFolders']=$noMediaFolders
    $summary['storageKeys']=@($storageKeys | Sort-Object -Unique)
}
$snapshotData = [ordered]@{}
foreach ($key in $summary.Keys) { $snapshotData[$key] = $summary[$key] }
$snapshotData['tokens'] = $tokens
$bytes = [Text.Encoding]::UTF8.GetBytes(($snapshotData | ConvertTo-Json -Compress -Depth 3))
$output = [IO.File]::Open($snapshotPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try { $output.Write($bytes, 0, $bytes.Length) } finally { $output.Dispose() }
$summary | ConvertTo-Json -Compress
