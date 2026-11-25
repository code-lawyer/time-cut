#Requires -Version 7.2
<#
.SYNOPSIS
Prepare and verify a synthetic Galaxy MTP test under one new TimecutQa_<GUID> directory.
.EXAMPLE
pwsh -STA -File scripts/test_android_mtp_fixture.ps1 prepare -Receipt D:\QA\s21-run.json
.EXAMPLE
pwsh -STA -File scripts/test_android_mtp_fixture.ps1 seed -Receipt D:\QA\s21-run.json
.EXAMPLE
pwsh -STA -File scripts/test_android_mtp_fixture.ps1 mutation -Receipt D:\QA\s21-run.json
.EXAMPLE
pwsh -STA -File scripts/test_android_mtp_fixture.ps1 cleanup -Receipt D:\QA\s21-run.json
.PARAMETER EnrollSystemMarker
Recover a previously started seed whose only visible file is Android's .nomedia containing
the exact generated directory's /storage/emulated/0 path. Establish a separate nonce
without overwriting the provider marker. This switch applies only to a creating receipt.
.DESCRIPTION
Prepare is offline. It uses the project's FFmpeg to generate every media sample. Keep the
receipt and its adjacent .data directory together in a trusted local directory. Seed binds
the receipt to the one selected phone and storage. Use exact -DeviceName and -StorageName
if the defaults are ambiguous. No process should edit the generated directory during a run.
After seed and again after mutation, let Time-cut finish a new import before proceeding.
The two expected inventories in the receipt contain paths, lengths and SHA-256 digests.
Mutation changes the bytes of the top-level image at the same path and length; MTP may
assign a new object ID or modification time. Cleanup moves approved leaves into a new local
quarantine and checks both endpoints. Empty test directories stay on the phone because
Shell providers offer no dependable empty-directory-only remove. No recursive move/delete
or overwrite is issued. Seed verifies a fresh TimecutQaOwner.txt nonce before media, then
attempts .nomedia to suppress the gallery. Android may replace .nomedia with the generated
directory's absolute path. The exact path is verified separately from the nonce. A refused
move or an upload timeout can mean an asynchronous copy is pending; inspect before retrying.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory, Position=0)][ValidateSet('prepare','seed','mutation','cleanup')][string]$Mode,
    [Parameter(Mandatory)][string]$Receipt,
    [string]$DeviceName,
    [string]$StorageName,
    [switch]$EnrollSystemMarker
)
$ErrorActionPreference = 'Stop'
$receiptPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Receipt)
$data = "$receiptPath.data"
$ffmpeg = Join-Path (Split-Path $PSScriptRoot -Parent) 'src-tauri/resources/ffmpeg/ffmpeg-x86_64-pc-windows-msvc.exe'
$relative = @('sample.png','alias.png','nested/sample.png','sample.xmp','clip.mp4')
$marker = '.nomedia'
$fallbackMarker = 'TimecutQaOwner.txt'
$flags = 4 + 8 + 16 + 512 + 1024 # silent, rename on collision, no confirmation, no mkdir prompt, no error UI
$utf8 = [Text.UTF8Encoding]::new($false)

function Get-Digest([string]$Path) { (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant() }
function Get-Key([string]$Text) {
    if ([string]::IsNullOrWhiteSpace($Text)) { throw 'Shell did not supply a stable namespace path.' }
    [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($utf8.GetBytes($Text))).ToLowerInvariant()
}
function Save-Receipt {
    $temporary = "$receiptPath.$([guid]::NewGuid().ToString('N')).tmp"
    [IO.File]::WriteAllText($temporary, ($state | ConvertTo-Json -Depth 6), $utf8)
    [IO.File]::Move($temporary, $receiptPath, $true)
}
function Local-File([string]$Name, [bool]$Changed) {
    if ($Name -ceq 'sample.png' -and $Changed) { return (Join-Path $data 'mutation/sample.png') }
    if ($Name -cnotin ($relative + $marker)) { throw 'Unknown test leaf.' }
    Join-Path (Join-Path $data 'seed') $Name
}
function Make-Inventory([bool]$Changed) {
    foreach ($name in $relative) {
        $p = Local-File $name $Changed
        [ordered]@{path=$name; bytes=(Get-Item -LiteralPath $p).Length; sha256=(Get-Digest $p)}
    }
}
function Invoke-Encoder([string[]]$Arguments) {
    & $ffmpeg -nostdin -hide_banner -loglevel error -n @Arguments
    if ($LASTEXITCODE) { throw 'Unable to generate synthetic media.' }
}
function New-LocalFixture {
    if (!(Test-Path -LiteralPath $ffmpeg -PathType Leaf)) { throw 'Prepare the project Windows x64 FFmpeg sidecar first.' }
    if ([IO.File]::Exists($receiptPath) -or (Test-Path -LiteralPath $data)) { throw 'Select a new unused local receipt path.' }
    $null = [IO.Directory]::CreateDirectory((Split-Path $receiptPath -Parent))
    $null = [IO.Directory]::CreateDirectory((Join-Path $data 'seed/nested'))
    $null = [IO.Directory]::CreateDirectory((Join-Path $data 'mutation'))
    $id = [guid]::NewGuid().ToString('N')
    $nonce = [guid]::NewGuid().ToString('N')
    [IO.File]::WriteAllText((Local-File $marker $false), "TimecutQa_$id/$nonce", $utf8)
    foreach ($pair in @(@('sample.png',$id.Substring(0,6)), @('nested/sample.png',$id.Substring(6,6)), @('mutation',$id.Substring(12,6)))) {
        $png = if ($pair[0] -eq 'mutation') { Local-File 'sample.png' $true } else { Local-File $pair[0] $false }
        Invoke-Encoder @('-f','lavfi','-i',("color=c=0x" + $pair[1] + ':s=16x16:d=0.1'),'-frames:v','1','-pix_fmt','rgb24','-threads','1','-compression_level','0',$png)
    }
    [IO.File]::Copy((Local-File 'sample.png' $false), (Local-File 'alias.png' $false))
    $xmp = '<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier>' + $id + '</dc:identifier></rdf:Description></rdf:RDF></x:xmpmeta>'
    [IO.File]::WriteAllText((Local-File 'sample.xmp' $false), $xmp, $utf8)
    Invoke-Encoder @('-f','lavfi','-i','color=c=steelblue:s=32x32:r=4:d=1','-c:v','mpeg4','-pix_fmt','yuv420p','-metadata',"comment=$id",'-movflags','+faststart',(Local-File 'clip.mp4' $false))
    $seed = @(Make-Inventory $false)
    $mutation = @(Make-Inventory $true)
    if ($seed[0].bytes -ne $mutation[0].bytes -or $seed[0].sha256 -eq $mutation[0].sha256 -or
        $seed[0].sha256 -ne $seed[1].sha256 -or $seed[2].sha256 -in @($seed[0].sha256,$mutation[0].sha256)) {
        throw 'Generated PNGs failed the identity, difference or equal-length check. Use a new receipt.'
    }
    [ordered]@{format='timecut-android-fixture-v1'; folder="TimecutQa_$id"; owner=(Get-Digest (Local-File $marker $false)); ownershipLeaf=$marker
        deviceKey=$null; storageKey=$null; phase='prepared'; seed=$seed; mutation=$mutation}
}

# Always use indexed COM collections and the actual leaf name, even when Explorer hides its suffix.
function Get-Children($Folder) {
    $items = $Folder.Items()
    if ($null -eq $items) { throw 'MTP folder unavailable.' }
    $count = $items.Count
    for ($i=0; $i -lt $count; $i++) {
        $entry = $items.Item($i)
        if ($null -eq $entry) { throw 'Incomplete MTP enumeration.' }
        $name = [string]$entry.Name
        if (!$entry.IsFolder) {
            $raw = [string]$entry.ExtendedProperty('System.FileName')
            if ($raw) { $name = $raw } else {
                $ext = [string]$entry.ExtendedProperty('System.FileExtension')
                if ($ext -and !$ext.StartsWith('.')) { $ext = '.' + $ext }
                if ($ext -and !$name.EndsWith($ext,[StringComparison]::OrdinalIgnoreCase)) { $name += $ext }
            }
        }
        if (!$name) { throw 'Unknown MTP leaf.' }
        [pscustomobject]@{Name=$name; Item=$entry}
    }
    if ($count -ne $items.Count) { throw 'MTP directory changed during enumeration.' }
}
function Find-Leaf($Folder, [string]$Name) {
    $matches = @(Get-Children $Folder | Where-Object Name -eq $Name)
    if ($matches.Count -gt 1 -or ($matches.Count -and $matches[0].Name -cne $Name)) { throw 'Ambiguous or differently cased MTP name.' }
    if ($matches.Count) { return $matches[0].Item }
}
function Wait-Result([scriptblock]$Probe, [string]$Description) {
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do { $result = & $Probe; if ($result) { return $result }; Start-Sleep -Milliseconds 300 } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out during $Description. Inspect before retrying."
}
function New-Staging {
    $p = Join-Path $data ('transfer-' + [guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($p)
    return $p
}
function Get-NativeHash($Item) {
    if (!$Item -or $Item.IsFolder) { throw 'Expected one file for native readback.' }
    $temporary = Join-Path $data ('direct-' + [guid]::NewGuid().ToString('N') + '.bin')
    try {
        [void][TimecutShellRead]::Copy($Item, $temporary, 1048576, $true)
        return Get-Digest $temporary
    } finally { if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) } }
}
function Receive-Leaf($Item, [string]$Name, [string[]]$Hashes, [switch]$Move) {
    if (!$Item -or $Item.IsFolder -or [IO.Path]::GetFileName($Name) -cne $Name) { throw 'Expected an exact file leaf.' }
    if (!$Move) {
        $actual = Get-NativeHash $Item
        if ($actual -cnotin $Hashes) { throw "Native stream differs from the fixture receipt for $Name." }
        return $actual
    }
    $staging = New-Staging
    $destination = $shell.NameSpace($staging)
    if (!$destination) { throw 'Local Shell destination is unavailable.' }
    $destination.MoveHere($Item, $flags)
    $download = Join-Path $staging $Name
    $hash = Wait-Result {
        if ([IO.File]::Exists($download)) {
            try { $digest = Get-Digest $download; if ($digest -cin $Hashes) { return $digest } } catch [IO.IOException] { }
        }
    } 'MTP readback and hash verification'
    return $hash
}
function Add-Leaf($Folder, [string]$Name, [string]$Local, [string]$Hash, [string[]]$ProviderHashes=@()) {
    if ((Find-Leaf $Folder $Name) -or [IO.Path]::GetFileName($Local) -cne $Name -or (Get-Digest $Local) -cne $Hash) { throw 'Upload precondition failed.' }
    $Folder.CopyHere($Local, $flags)
    $item = Wait-Result { Find-Leaf $Folder $Name } 'MTP upload'
    $null = Receive-Leaf $item $Name (@($Hash) + $ProviderHashes)
}
function Move-Leaf($Folder, [string]$Name, [string]$Hash) {
    $current = Find-Leaf $Folder $Name
    $null = Receive-Leaf $current $Name @($Hash)
    $current = Find-Leaf $Folder $Name
    $null = Receive-Leaf $current $Name @($Hash) -Move
    $null = Wait-Result { !(Find-Leaf $Folder $Name) } 'source removal after verified move'
}
function Read-OwnedTree {
    $ownershipLeaf = $state.ownershipLeaf ?? $marker
    if ($ownershipLeaf -cnotin @($marker,$fallbackMarker)) { throw 'Unknown ownership marker.' }
    $owner = Find-Leaf $qa $ownershipLeaf
    if (!$owner) { throw 'Missing ownership marker; refusing changes.' }
    $null = Receive-Leaf $owner $ownershipLeaf @($state.owner)
    $entries = @{}
    $sub = $null
    $lateMarker = $false
    foreach ($child in @(Get-Children $qa)) {
        if ($child.Name -ceq $ownershipLeaf) { continue }
        if ($ownershipLeaf -ceq $fallbackMarker -and $child.Name -ceq $marker) {
            $null = Receive-Leaf $child.Item $marker @(($state.providerMarkerSha256 ?? $state.owner))
            $lateMarker = $true
            continue
        }
        if ($child.Name -ceq 'nested' -and $child.Item.IsFolder -and !$sub) { $sub = $child.Item.GetFolder; continue }
        if ($child.Item.IsFolder -or $child.Name -cnotin $relative -or $entries.ContainsKey($child.Name)) { throw 'Unexpected entry in the test folder; refusing changes.' }
        $entries[$child.Name] = $child.Item
    }
    if ($sub) {
        foreach ($child in @(Get-Children $sub)) {
            if ($child.Item.IsFolder -or $child.Name -cne 'sample.png' -or $entries.ContainsKey('nested/sample.png')) { throw 'Unexpected entry in the test subfolder; refusing changes.' }
            $entries['nested/sample.png'] = $child.Item
        }
    }
    $verified = @{}
    foreach ($path in $entries.Keys) {
        $allowed = @($state.seed + $state.mutation | Where-Object { $_.path -ceq $path } | ForEach-Object sha256 | Sort-Object -Unique)
        $verified[$path] = Receive-Leaf $entries[$path] ([IO.Path]::GetFileName($path)) $allowed
    }
    [pscustomobject]@{ hashes=$verified; sub=$sub; lateMarker=$lateMarker }
}
function Require-Inventory($Tree, $Inventory) {
    if ($Tree.hashes.Count -ne $Inventory.Count) { throw 'Unexpected fixture count.' }
    foreach ($file in $Inventory) {
        if ($Tree.hashes[$file.path] -cne $file.sha256) { throw 'Unexpected fixture hash.' }
    }
}

if ($Mode -eq 'prepare') {
    $lockPath = "$receiptPath.lock"
    [void][IO.Directory]::CreateDirectory((Split-Path $receiptPath -Parent))
} else {
    if (![IO.File]::Exists($receiptPath) -or ![IO.Directory]::Exists($data)) { throw 'Prepare a local receipt first.' }
    $lockPath = "$receiptPath.lock"
}
$lock = [IO.FileStream]::new($lockPath, 'OpenOrCreate', 'ReadWrite', 'None', 4096, 'DeleteOnClose')
try {
    if ($Mode -eq 'prepare') { $state = New-LocalFixture; Save-Receipt; Write-Output "Prepared $($state.folder)."; return }
    $state = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json -AsHashtable
    if ($EnrollSystemMarker -and ($Mode -ne 'seed' -or $state.phase -cne 'creating')) {
        throw 'Enrollment requires an already interrupted initial seed.'
    }
    if ($state.format -cne 'timecut-android-fixture-v1' -or $state.folder -cnotmatch '^TimecutQa_[a-f0-9]{32}$' -or
        $state.owner -cne (Get-Digest (Local-File $marker $false)) -or
        !([IO.File]::ReadAllText((Local-File $marker $false)) -cmatch ('^'+$state.folder+'/[a-f0-9]{32}$'))) { throw 'Invalid fixture receipt or marker.' }
    foreach ($round in 'seed','mutation') {
        $inventory = @(Make-Inventory ($round -eq 'mutation'))
        if (($inventory | ConvertTo-Json -Compress) -cne ($state[$round] | ConvertTo-Json -Compress)) { throw 'Local fixtures differ from the receipt.' }
    }
    if ($state.providerMarkerSha256 -and $state.providerMarkerSha256 -cne (Get-Key ('/storage/emulated/0/' + $state.folder))) {
        throw 'Provider marker does not identify this exact generated directory.'
    }
    if ([Threading.Thread]::CurrentThread.GetApartmentState() -ne 'STA') { throw 'Use pwsh -STA for Shell MTP operations.' }
    Add-Type -Path (Join-Path $PSScriptRoot 'TimecutShellRead.cs')
    $shell = New-Object -ComObject Shell.Application
    $phones = @(Get-Children ($shell.NameSpace(17)) | Where-Object { $_.Item.IsFolder -and !$_.Item.IsFileSystem -and
        $(if ($DeviceName) { $_.Name -ceq $DeviceName } else { $_.Name -match 'Galaxy.*\bS21\b' }) })
    if ($phones.Count -ne 1) { throw 'Expected one matching Galaxy. Specify the exact -DeviceName if needed.' }
    $phone = $phones[0].Item
    $stores = @(Get-Children $phone.GetFolder | Where-Object { $_.Item.IsFolder -and (!$StorageName -or $_.Name -ceq $StorageName) })
    if ($stores.Count -ne 1) { throw 'Expected one storage. Specify the exact -StorageName if needed.' }
    $store = $stores[0].Item
    $deviceKey = Get-Key ([string]$phone.Path)
    $storageKey = Get-Key ([string]$store.Path)
    $root = $store.GetFolder
    $folder = Find-Leaf $root $state.folder
    if ($state.phase -ceq 'prepared') {
        if ($Mode -ne 'seed' -or $folder) { throw 'Seed requires an unused new remote folder name.' }
        $state.deviceKey = $deviceKey; $state.storageKey = $storageKey; $state.phase = 'creating'
        $state.ownershipLeaf = $fallbackMarker; Save-Receipt
        $root.NewFolder($state.folder)
        $folder = Wait-Result { Find-Leaf $root $state.folder } 'creating the owned directory'
        if (!$folder.IsFolder -or @(Get-Children $folder.GetFolder).Count) { throw 'New root is not empty.' }
        $owned = Join-Path $data $fallbackMarker
        [IO.File]::Copy((Local-File $marker $false), $owned)
        Add-Leaf $folder.GetFolder $fallbackMarker $owned $state.owner
        $systemHash=Get-Key ('/storage/emulated/0/' + $state.folder)
        try {
            Add-Leaf $folder.GetFolder $marker (Local-File $marker $false) $state.owner @($systemHash)
            if ((Get-NativeHash (Find-Leaf $folder.GetFolder $marker)) -ceq $systemHash) {
                $state.providerMarkerSha256=$systemHash; Save-Receipt
            }
        }
        catch {
            $now=@(Get-Children $folder.GetFolder)
            if ($now.Count -ne 1 -or $now[0].Name -cne $fallbackMarker) { throw }
            $null=Receive-Leaf $now[0].Item $fallbackMarker @($state.owner)
            Write-Warning 'MTP did not expose .nomedia. Gallery suppression is unverified.'
        }
    }
    if ($deviceKey -cne $state.deviceKey -or $storageKey -cne $state.storageKey -or !$folder -or !$folder.IsFolder) { throw 'Device, storage, or fixture folder differs from the receipt.' }
    $qa = $folder.GetFolder
    if ($EnrollSystemMarker) {
        $entries=@(Get-Children $qa)
        $resume=$state.ownershipEnrollment -ceq 'pending' -and $entries.Count -eq 2
        if ((!$resume -and $entries.Count -ne 1) -or !(Find-Leaf $qa $marker) -or ($resume -and !(Find-Leaf $qa $fallbackMarker))) {
            throw 'Expected only the exact provider marker, plus the pending QA marker on resume.'
        }
        $systemHash=Get-Key ('/storage/emulated/0/' + $state.folder)
        $null=Receive-Leaf (Find-Leaf $qa $marker) $marker @($systemHash)
        $fallback=Join-Path $data $fallbackMarker
        if (![IO.File]::Exists($fallback)) { [IO.File]::Copy((Local-File $marker $false),$fallback) }
        if ((Get-Digest $fallback) -cne $state.owner) { throw 'Local enrollment payload mismatch.' }
        $state.ownershipEnrollment='pending'; $state.providerMarkerSha256=$systemHash; Save-Receipt
        if (!$resume) { Add-Leaf $qa $fallbackMarker $fallback $state.owner }
        $null=Receive-Leaf (Find-Leaf $qa $fallbackMarker) $fallbackMarker @($state.owner)
        $state.ownershipLeaf=$fallbackMarker; $state.ownershipEnrollment='verified'; Save-Receipt
    }
    $tree = Read-OwnedTree
    switch ($Mode) {
        'seed' {
            if ($state.phase -cnotin @('creating','seeded')) { throw 'Cannot seed this phase.' }
            foreach ($path in $tree.hashes.Keys) { if ($tree.hashes[$path] -cne ($state.seed | Where-Object path -ceq $path).sha256) { throw 'Conflicting existing sample.' } }
            if (!$tree.sub) { $qa.NewFolder('nested'); $tree.sub = (Wait-Result { Find-Leaf $qa 'nested' } 'creating the subfolder').GetFolder }
            foreach ($file in $state.seed) {
                if ($tree.hashes.ContainsKey($file.path)) { continue }
                $dest = if ($file.path.StartsWith('nested/')) { $tree.sub } else { $qa }
                Add-Leaf $dest ([IO.Path]::GetFileName($file.path)) (Local-File $file.path $false) $file.sha256
            }
            Require-Inventory (Read-OwnedTree) $state.seed
            $state.phase = 'seeded'
        }
        'mutation' {
            if ($state.phase -ceq 'seeded') { Require-Inventory $tree $state.seed; $state.phase = 'mutating'; Save-Receipt }
            if ($state.phase -cnotin @('mutating','mutated')) { throw 'Complete seed before mutation.' }
            foreach ($file in $state.seed | Where-Object path -cne 'sample.png') {
                if ($tree.hashes[$file.path] -cne $file.sha256) { throw 'Other fixture entries changed.' }
            }
            if ($tree.hashes['sample.png'] -ceq $state.seed[0].sha256) { Move-Leaf $qa 'sample.png' $state.seed[0].sha256 }
            if (!(Find-Leaf $qa 'sample.png')) { Add-Leaf $qa 'sample.png' (Local-File 'sample.png' $true) $state.mutation[0].sha256 }
            Require-Inventory (Read-OwnedTree) $state.mutation
            $state.phase = 'mutated'
        }
        'cleanup' {
            if ($state.phase -cnotin @('creating','seeded','mutating','mutated','cleaning')) { throw 'Cannot clean this phase.' }
            $state.phase = 'cleaning'; Save-Receipt
            foreach ($path in $relative) {
                if (!$tree.hashes.ContainsKey($path)) { continue }
                $source = if ($path.StartsWith('nested/')) { $tree.sub } else { $qa }
                Move-Leaf $source ([IO.Path]::GetFileName($path)) $tree.hashes[$path]
            }
            $remaining = Read-OwnedTree
            if ($remaining.hashes.Count) { throw 'Fixture changed during cleanup.' }
            if ($remaining.lateMarker) { Move-Leaf $qa $marker ($state.providerMarkerSha256 ?? $state.owner) }
            Move-Leaf $qa ($state.ownershipLeaf ?? $marker) $state.owner
            $state.phase = 'files-removed'
            Write-Output "Known files moved to local quarantine under $data. Remaining phone directories: $($state.folder)$(if ($remaining.sub) {', ' + $state.folder + '/nested'})."
            if ($state.ownershipLeaf -ceq $fallbackMarker -and !$remaining.lateMarker) {
                Write-Output 'A .nomedia hidden by the provider may remain in the test root.'
            }
        }
    }
    Save-Receipt
    Write-Output "Verified $($state.phase) for $($state.folder)."
} finally { $lock.Dispose() }
