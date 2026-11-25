#Requires -Version 7.2
<#
.SYNOPSIS
Compare bounded Windows Shell MTP reads with originals from a completed USB import.
.EXAMPLE
pwsh -STA -File scripts/verify_android_source_samples.ps1 -ArchiveRoot D:\QA\album -RunId 1
.EXAMPLE
pwsh -STA -File scripts/verify_android_source_samples.ps1 -ArchiveRoot D:\QA\album -RunId 1 -InboxOnly
.DESCRIPTION
Reads up to two different-sized items per supported format, spending at most 128 MiB
across all copies. Exit 2 means a selected item could not be verified. The summary
counts ambiguous and over-budget candidates. -FixtureSourceRoot exercises Shell
against a local synthetic directory tree in place of a connected device.
-InboxOnly checks committed, unreleased receipts from the selected open run. A
successful byte sample does not establish inventory completeness or safe disconnect.
-RepeatSamples accepts an earlier representative report to reconstruct the original
selection. -SampleGroup Camera checks up to three files under DCIM/Camera.
-SampleGroup Pending reads at most five unresolved entries, capped at 16 MiB each.
-SampleGroup NonCameraVideo with -LocalSampleReport checks a specific video against its
previously measured SHA-256. Source-only reads and zero-byte streams are reported separately.
#>
[CmdletBinding(DefaultParameterSetName='Phone')]
param(
    [Parameter(Mandatory)][string]$ArchiveRoot,
    [Parameter(Mandatory)][ValidateRange(1,[long]::MaxValue)][long]$RunId,
    [switch]$InboxOnly,
    [ValidateSet('Auto','Stream','Explorer')][string]$Reader='Auto',
    [ValidateSet('Representative','Camera','Pending','NonCameraVideo')][string]$SampleGroup='Representative',
    [string]$RepeatSamples,
    [string]$LocalSampleReport,
    [Parameter(ParameterSetName='Phone')][string]$DeviceName,
    [Parameter(Mandatory,ParameterSetName='Fixture')][string]$FixtureSourceRoot
)
$ErrorActionPreference = 'Stop'
if ([Threading.Thread]::CurrentThread.GetApartmentState() -ne 'STA') { throw 'Use pwsh -STA.' }

# Keep file names in process memory only. Bind each digest to its exact recorded
# destination when possible. Duplicate WPD names or ambiguous destinations are skipped.
$selectSamples = @'
import collections, hashlib, json, pathlib, re, sqlite3, sys, uuid
try:
    root = pathlib.Path(sys.argv[1]).resolve(strict=True)
    run = int(sys.argv[2])
    db = sqlite3.connect((root / ".time-cut" / "imports.sqlite3").as_uri()+"?mode=ro", uri=True)
    db.execute("BEGIN")
    inbox_only = sys.argv[3] == "inbox"
    sample_group = sys.argv[4]
    reference = json.loads(pathlib.Path(sys.argv[5]).read_text(encoding="utf-8-sig")) if sys.argv[5]!="-" else None
    if reference and (reference.get("sampleGroup","Representative")!=sample_group or sample_group!="Representative"):
        raise ValueError()
    row = db.execute("SELECT source,discovered,safe_at FROM phone_runs WHERE id=?", (run,)).fetchone()
    if not row or not row[0].startswith("wpd:"):
        raise ValueError()
    source, count, safe_at = row
    pending_receipted=0
    reference_video = None
    if sample_group=="NonCameraVideo":
        local=json.loads(pathlib.Path(sys.argv[6]).read_text(encoding="utf-8-sig"))
        videos=[s for s in local["samples"] if s["kind"]=="non-camera-video" and s["extension"].lower() in (".mp4","mp4")]
        if len(videos)!=1 or not re.fullmatch(r"[0-9a-fA-F]{64}",videos[0]["sha256"]): raise ValueError()
        reference_video=videos[0]
    if inbox_only:
        if safe_at is not None:
            raise ValueError()
        entries = db.execute("""SELECT entry,version,digest,size,blob,archive_path FROM phone_inbox
            WHERE run_id=? AND source=? AND released=0 ORDER BY id""",(run,source)).fetchall()
        if reference:
            prior_count=reference["receiptRows"]
            if not 0 < prior_count <= len(entries): raise ValueError()
            entries=entries[:prior_count]
        if sample_group=="Pending":
            pending=db.execute("SELECT entry,device_version,signature,occurrence FROM phone_pending WHERE source=? ORDER BY entry",(source,)).fetchall()
            if len(pending)>5:
                print(json.dumps(dict(skippedPending=len(pending),reason="pendingSetAboveFive")))
                sys.exit(0)
            entries=[]
            for entry,version,signature,occurrence in pending:
                size=signature.partition("/")[0]
                size=int(size) if size.isdecimal() else None
                candidates=db.execute("""SELECT entry,version,digest,size,blob,archive_path FROM phone_inbox
                    WHERE run_id=? AND source=? AND entry=? AND version IS ? AND size=? AND released=0""",
                    (run,source,entry,version,size)).fetchall() if version else []
                if occurrence==1 and len(candidates)==1:
                    pending_receipted+=1
                else:
                    entries.append((entry,version,None,size,None,None))
    else:
        if safe_at is None or sample_group=="Pending":
            raise ValueError()
        entries = db.execute("SELECT entry,version,digest,size,NULL,NULL FROM phone_manifest WHERE run_id=? ORDER BY ordinal", (run,)).fetchall()
        if len(entries) != count or db.execute("SELECT 1 FROM phone_inbox WHERE run_id=? LIMIT 1",(run,)).fetchone():
            raise ValueError()
    if not entries and sample_group!="Pending":
        raise ValueError()
    repeats = collections.Counter(row[0].replace("\\","/") for row in entries)
    aliases = {"jpg":"JPG","jpeg":"JPG","png":"PNG","heic":"HEIC","heif":"HEIC","mp4":"MP4","mov":"MOV","webp":"WEBP"}
    groups = {kind:[] for kind in ("JPG","HEIC","PNG","MP4","WEBP","MOV")}
    ambiguous = too_big = empty = unsupported = invalid_backing = 0
    tables = {r[0] for r in db.execute("SELECT name FROM sqlite_schema WHERE type='table'")}
    budget = 128 * 1024**2
    for entry,version,digest,size,blob,archive_path in entries:
        item = entry.replace("\\","/")
        parts = item.split("/")
        if reference_video and (digest!=reference_video["blake3"] or size!=reference_video["bytes"]):
            continue
        if sample_group=="Camera" and [p.casefold() for p in parts[1:3]] != ["dcim","camera"]:
            continue
        kind = aliases.get(pathlib.PurePosixPath(item).suffix[1:].lower())
        if sample_group=="Pending" and not kind: kind="OTHER"; groups.setdefault(kind,[])
        if not kind:
            unsupported += 1
            continue
        if any(p in ("", ".", "..") for p in parts) or len(parts)<2 or repeats[item]!=1:
            ambiguous += 1
            continue
        if sample_group=="Pending" and (size is None or size==0):
            marker=hashlib.sha256(json.dumps([source,item,version,None],ensure_ascii=False).encode("utf8")).hexdigest()
            groups[kind].append(dict(kind=kind,parts=parts,archive=None,size=0,declaredBytes=size,unknownLength=True,evidenceId=marker))
            continue
        if not isinstance(size,int) or size<=0:
            empty += 1
            continue
        if size>(budget if sample_group=="Representative" else 16*1024**2):
            too_big += 1
            continue
        evidence_id=hashlib.sha256(json.dumps([source,item,version,digest],ensure_ascii=False).encode("utf8")).hexdigest()
        if sample_group=="Pending" and digest is None:
            groups[kind].append(dict(kind=kind,parts=parts,archive=None,size=size,evidenceId=evidence_id))
            continue
        if inbox_only and (not re.fullmatch(r"[0-9a-f]{64}",digest or "") or (blob is None)==(archive_path is None)):
            invalid_backing += 1
            continue
        if inbox_only and blob is not None:
            name=pathlib.PureWindowsPath(blob)
            if name.name!=blob or not re.fullmatch(r"[0-9a-fA-F-]{36}",name.stem):
                invalid_backing += 1
                continue
            try:
                uuid.UUID(name.stem)
            except ValueError:
                invalid_backing += 1
                continue
            relative=pathlib.Path(blob)
            base=root/".time-cut"/"inbox"
        else:
            if inbox_only:
                candidates={r[0] for r in db.execute("SELECT path FROM files WHERE path=? AND digest=? AND size=?",(archive_path,digest,size))}
            else:
                candidates={r[0] for r in db.execute("SELECT path FROM files WHERE digest=? AND size=?",(digest,size))}
                if len(candidates)>1:
                    mapped=set()
                    for table in ("sources","source_versions"):
                        if table in tables:
                            mapped.update(r[0] for r in db.execute("SELECT path FROM "+table+" WHERE source=? AND entry=? AND version IS ? AND digest=?",(source,entry,version,digest)))
                    candidates &= mapped
            if len(candidates)!=1:
                if inbox_only: invalid_backing += 1
                else: ambiguous += 1
                continue
            relative=pathlib.Path(candidates.pop())
            base=root
        archive=(base/relative).resolve()
        if (relative.is_absolute() or ".." in relative.parts or not relative.parts
                or relative.parts[0].casefold()==".time-cut" or not archive.is_relative_to(base)
                or not archive.is_file()):
            invalid_backing += 1
            continue
        groups[kind].append(dict(kind=kind,parts=parts,archive=str(archive),size=size,evidenceId=evidence_id,
                                 expectedSha256=reference_video["sha256"].upper() if reference_video else None))
    db.close()
    selected=[]
    remaining=budget
    for round_no in range(5 if sample_group=="Pending" else 2):
        for kind,group in groups.items():
            fit=[item for item in group if item["size"]<=remaining]
            if not fit: continue
            fit.sort(key=lambda item:(item["size"],item["parts"]))
            choice=fit[0 if round_no==0 else len(fit)//2]
            selected.append(choice)
            group.remove(choice)
            remaining-=choice["size"]
            if sample_group=="Camera" and len(selected)>=3: break
        if sample_group=="Camera" and len(selected)>=3: break
    if reference:
        previous=reference["samples"]
        if (len(previous)!=len(selected) or any(p["type"]!=s["kind"] or p["bytes"]!=s["size"]
                or p.get("evidenceId",s["evidenceId"])!=s["evidenceId"] for p,s in zip(previous,selected))):
            raise ValueError("Prior samples cannot be reconstructed")
    if reference_video and len(selected)!=1: raise ValueError("Selected local video has no unique phone receipt")
    too_big+=sum(item["size"]>remaining for group in groups.values() for item in group)
    limited=sum(item["size"]<=remaining for group in groups.values() for item in group)
    print(json.dumps(dict(samples=selected,receiptRows=len(entries),pendingWithReceipt=pending_receipted,invalidBacking=invalid_backing,ambiguous=ambiguous,budgetExcluded=too_big,
                          selectionLimit=limited,unsupported=unsupported,empty=empty)))
except (OSError,sqlite3.Error,KeyError,ValueError,TypeError):
    print("Cannot obtain matching receipts for the selected WPD run and evidence mode.",file=sys.stderr)
    sys.exit(1)
'@
$selectionMode=if ($InboxOnly) { 'inbox' } else { 'archive' }
$reference=if ($RepeatSamples) { $RepeatSamples } else { '-' }
$localReport=if ($LocalSampleReport) { $LocalSampleReport } else { '-' }
$selection = $selectSamples | & python - $ArchiveRoot $RunId $selectionMode $SampleGroup $reference $localReport
if ($LASTEXITCODE -ne 0) { exit 1 }
$plan = $selection | ConvertFrom-Json
if ($plan.skippedPending -or ($SampleGroup -eq 'Pending' -and !$plan.samples.Count)) {
    [ordered]@{mode=$PSCmdlet.ParameterSetName;sampleGroup='Pending';evidenceScope='unreleasedPendingSnapshot';
        safeDisconnectVerified=$false;selected=0;pendingRows=($plan.skippedPending ?? $plan.receiptRows);
        reason=($plan.reason ?? 'noEligiblePendingItem');matched=0;sourceReadableOnly=0} | ConvertTo-Json -Compress
    exit 2
}

# Prefer a bounded synchronous read. Some MTP providers expose files only through
# the Explorer copy handler, so keep the failing operation and HRESULT in the evidence.
Add-Type -Path (Join-Path $PSScriptRoot 'TimecutShellRead.cs')

function Find-Children($Folder,$WantedNames) {
    $items=$Folder.Items()
    $length=$items.Count
    $found=[Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
    for ($i=0; $i -lt $length; $i++) {
        $candidate=$items.Item($i)
        $name=[string]$candidate.Name
        if (!$candidate.IsFolder) {
            $original=[string]$candidate.ExtendedProperty('System.FileName')
            if ($original) { $name=$original }
            else {
                $suffix=[string]$candidate.ExtendedProperty('System.FileExtension')
                if ($suffix -and !$suffix.StartsWith('.')) { $suffix='.'+$suffix }
                if ($suffix -and !$name.EndsWith($suffix,[StringComparison]::OrdinalIgnoreCase)) { $name+=$suffix }
            }
        }
        $leaf=$name -replace '[/\\:]','_'
        if ($WantedNames.Contains($leaf)) {
            if ($found.ContainsKey($leaf)) { $found[$leaf]=$null } else { $found[$leaf]=$candidate }
        }
    }
    if ($items.Count -ne $length) { throw 'Source directory changed during enumeration.' }
    return ,$found
}
function Find-ExactItem($Folder,[string[]]$Parts) {
    $parent=''
    for ($pos=0; $pos -lt $Parts.Length; $pos++) {
        if (!$listings.ContainsKey($parent)) {
            $listings[$parent]=Find-Children $Folder $wanted[$parent]
        }
        $found=$listings[$parent][$Parts[$pos]]
        if (!$found) { return $null }
        if ($pos -eq $Parts.Length-1) {
            if (!$found.IsFolder) { return $found }
            return $null
        }
        if (!$found.IsFolder) { return $null }
        $Folder=$found.GetFolder
        $parent+='/'+$Parts[$pos]
    }
}

function Get-ReadError($Record,[string]$Where) {
    $exception=$Record.Exception.GetBaseException()
    if ($exception.Message -cin @('shellItem','bindStream','explorerRoot','explorerResolve','explorerCopy','explorerWrite')) { $Where=$exception.Message }
    return [ordered]@{ stage=$Where; hresult=('0x{0:X8}' -f $exception.HResult); errorType=$exception.GetType().Name }
}
function Get-ExplorerDigest($FolderItem,[long]$Length,[string]$Staging,[string[]]$Parts) {
    # The MTP handler can block inside CopyHere before it returns. Isolate the call
    # and pass virtual paths over stdin; the command line and diagnostics stay anonymous.
    $worker="function Find-Children {"+(Get-Command Find-Children).Definition+"}`nfunction Find-ExactItem {"+(Get-Command Find-ExactItem).Definition+"}`n"+@'
$ErrorActionPreference='Stop'
$where='explorerRoot'
try {
    $plan=[Console]::In.ReadToEnd() | ConvertFrom-Json
    $shell=New-Object -ComObject Shell.Application
    $folder=$shell.NameSpace([string]$plan.parent)
    $dest=$shell.NameSpace([string]$plan.staging)
    if (!$dest) { throw 'missing destination' }
    $matches=@()
    if ($folder) {
        $items=$folder.Items()
        for ($i=0;$i -lt $items.Count;$i++) {
            $item=$items.Item($i)
            if ([string]::Equals([string]$item.Path,[string]$plan.item,[StringComparison]::Ordinal)) { $matches+=$item }
        }
    }
    if ($matches.Count -ne 1) {
        $devices=$shell.NameSpace(17).Items()
        $roots=@()
        for ($i=0;$i -lt $devices.Count;$i++) {
            $item=$devices.Item($i)
            if ($item.IsFolder -and !$item.IsFileSystem -and $item.Path -ceq $plan.root) { $roots+=$item }
        }
        if ($roots.Count -ne 1) { throw 'root unavailable' }
        $where='explorerResolve'
        $wanted=[Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
        $listings=[Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
        $parent=''
        foreach($part in $plan.parts) {
            $wanted[$parent]=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
            [void]$wanted[$parent].Add($part)
            $parent+='/'+$part
        }
        $exact=Find-ExactItem $roots[0].GetFolder $plan.parts
        if ($exact -and [long]$exact.ExtendedProperty('System.Size') -eq [long]$plan.length) { $matches=@($exact) }
    }
    $where='explorerResolve'
    if ($matches.Count -ne 1 -or $matches[0].IsFolder) { throw 'unique item unavailable' }
    $where='explorerCopy'
    $dest.CopyHere($matches[0],(4+16+512+1024))
    exit 0
} catch {
    $ex=$_.Exception.GetBaseException()
    [Console]::Out.WriteLine((@{stage=$where;hr=$ex.HResult} | ConvertTo-Json -Compress))
    exit 5
}
'@
    $process=[Diagnostics.ProcessStartInfo]::new((Join-Path $PSHOME 'pwsh.exe'))
    $process.UseShellExecute=$false
    $process.CreateNoWindow=$true
    $process.RedirectStandardInput=$true
    $process.RedirectStandardOutput=$true
    $process.RedirectStandardError=$true
    foreach ($arg in '-NoProfile','-NoLogo','-NonInteractive','-STA','-EncodedCommand',
            [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($worker))) { $process.ArgumentList.Add($arg) }
    $proc=[Diagnostics.Process]::Start($process)
    $timer=[Diagnostics.Stopwatch]::StartNew()
    $previous=$null
    try {
        $proc.StandardInput.WriteLine((@{parent=$FolderItem.Parent.Self.Path;item=$FolderItem.Path;staging=$Staging;
            root=$store.Self.Path;parts=$Parts;length=$Length} | ConvertTo-Json -Compress -EscapeHandling EscapeNonAscii))
        $proc.StandardInput.Close()
        while ($timer.Elapsed.TotalSeconds -lt 30) {
        if ($proc.HasExited -and $proc.ExitCode -ne 0) {
            $why=$proc.StandardOutput.ReadToEnd() | ConvertFrom-Json
            throw [Runtime.InteropServices.COMException]::new([string]$why.stage,[int]$why.hr)
        }
        $files=[IO.Directory]::GetFiles($Staging)
        if ($files.Length -gt 1) { throw 'Ambiguous temporary copy output.' }
        if ($files.Length -eq 1) {
            try {
                $inputFile=[IO.File]::Open($files[0],'Open','Read','None')
                try {
                    if ($inputFile.Length -eq $Length) {
                        $digest=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($inputFile))
                        if ($previous -ceq $digest -and $proc.HasExited) { return $digest }
                        $previous=$digest
                    } else { $previous=$null }
                } finally { $inputFile.Dispose() }
            } catch [IO.IOException] { $previous=$null }
        }
        Start-Sleep -Milliseconds 350
        }
    } finally {
        if (!$proc.HasExited) { $proc.Kill($true); [void]$proc.WaitForExit(3000) }
        $proc.Dispose()
    }
    throw [TimeoutException]::new('Timed out waiting for the completed Explorer copy.')
}
function Remove-LocalCopy([string]$Staging) {
    if ([IO.Path]::GetDirectoryName($Staging) -ne $scratch) { throw 'Unexpected local copy directory.' }
    $timer=[Diagnostics.Stopwatch]::StartNew()
    do {
        try {
            foreach ($file in [IO.Directory]::GetFiles($Staging)) { [IO.File]::Delete($file) }
            [IO.Directory]::Delete($Staging)
            return
        } catch [IO.IOException] { Start-Sleep -Milliseconds 200 }
    } while ($timer.Elapsed.TotalSeconds -lt 15)
    throw 'The Explorer temporary copy is still busy.'
}
$shell=New-Object -ComObject Shell.Application
if ($PSCmdlet.ParameterSetName -eq 'Fixture') {
    $store=$shell.NameSpace((Resolve-Path -LiteralPath $FixtureSourceRoot).Path)
} else {
    $items=$shell.NameSpace(17).Items()
    $phones=[Collections.Generic.List[object]]::new()
    for ($i=0; $i -lt $items.Count; $i++) {
        $item=$items.Item($i)
        $match=if ($DeviceName) { $item.Name -eq $DeviceName } else { $item.Name -match 'Galaxy' }
        if ($match -and $item.IsFolder -and !$item.IsFileSystem) { $phones.Add($item) }
    }
    if ($phones.Count -ne 1) { throw 'A unique matching MTP device is required.' }
    $store=$phones[0].GetFolder
}

$rows=[Collections.Generic.List[object]]::new()
# Large camera folders are enumerated once, retaining only the selected children.
$wanted=[Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
$listings=[Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
foreach ($sample in $plan.samples) {
    $parent=''
    foreach ($part in $sample.parts) {
        if (!$wanted.ContainsKey($parent)) { $wanted[$parent]=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal) }
        [void]$wanted[$parent].Add($part)
        $parent+='/'+$part
    }
}
$root=[IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$scratch=[IO.Directory]::CreateDirectory((Join-Path $root ('timecut-mtp-check-'+[Guid]::NewGuid().ToString('N')))).FullName
$copy=Join-Path $scratch 'sample.bin'
try {
    foreach ($sample in $plan.samples) {
        $status='readError'
        $where='resolve'
        $method='none'
        $diagnostics=@()
        $readLength=$sample.size
        $observedBytes=$null
        $size=$null
        try {
            $found=Find-ExactItem $store $sample.parts
            if (!$found) { $status='unresolved' }
            else {
                $where='shellSize'
                $size=$found.ExtendedProperty('System.Size')
                if ($sample.unknownLength -and ($null -eq $size -or [long]$size -le 16MB)) { $readLength=16MB }
                if ($sample.unknownLength -and $null -ne $size -and [long]$size -gt 16MB) { $status='overBudget' }
                elseif (!$sample.unknownLength -and ($null -eq $size -or [long]$size -ne $sample.size)) { $status='sizeMismatch' }
                else {
                    $sourceDigest=$null
                    if ($Reader -ne 'Explorer' -or $sample.unknownLength) {
                        $where='streamRead'
                        $method='shellStream'
                        for ($attempt=0;$attempt -lt 3;$attempt++) {
                            try {
                                $observedBytes=[TimecutShellRead]::Copy($found,$copy,$readLength,[bool]$sample.unknownLength)
                                $sourceDigest=(Get-FileHash -Algorithm SHA256 -LiteralPath $copy).Hash
                                break
                            } catch {
                                $failure=Get-ReadError $_ $where
                                [IO.File]::Delete($copy)
                                if ($failure.hresult -ceq '0x800700AA' -and $attempt -lt 2) {
                                    $diagnostics+=$failure
                                    Start-Sleep -Milliseconds (500*($attempt+1))
                                    continue
                                }
                                if ($Reader -eq 'Stream' -or $sample.unknownLength) { throw }
                                $diagnostics+=$failure
                                break
                            }
                        }
                    }
                    if (!$sourceDigest) {
                        $where='explorerCopy'
                        $method='explorerCopy'
                        $staging=[IO.Directory]::CreateDirectory((Join-Path $scratch ([guid]::NewGuid().ToString('N')))).FullName
                        try { $sourceDigest=Get-ExplorerDigest $found $sample.size $staging $sample.parts; $observedBytes=$sample.size }
                        finally { Remove-LocalCopy $staging }
                    }
                    if (!$sample.archive) { $status=if ($observedBytes -eq 0) { 'zeroByteSource' } else { 'sourceReadable' } }
                    else {
                        $where='localRead'
                        $original=[IO.File]::Open($sample.archive,'Open','Read','Read')
                        try {
                            if ($original.Length -ne $sample.size) { $status='sizeMismatch' }
                            else {
                                $localDigest=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($original))
                                if ($sample.expectedSha256 -and $sample.expectedSha256 -cne $localDigest) { $status='referenceMismatch' }
                                elseif ($sourceDigest -ceq $localDigest) { $status='matched' } else { $status='hashMismatch' }
                            }
                        } finally { $original.Dispose() }
                    }
                }
            }
        } catch { $diagnostics+=Get-ReadError $_ $where; $status='readError' }
        finally { [IO.File]::Delete($copy) }
        $declaredBytes=if ($sample.unknownLength) { $sample.declaredBytes } else { $sample.size }
        $rows.Add([ordered]@{type=$sample.kind;bytes=$sample.size;declaredBytes=$declaredBytes;shellBytes=$size;observedBytes=$observedBytes;
            evidenceId=$sample.evidenceId;status=$status;method=$method;diagnostics=$diagnostics})
    }
} finally {
    # Only this script's GUID directory in the resolved temp root, already empty.
    if ([IO.Path]::GetDirectoryName($scratch).TrimEnd('\','/') -ne $root.TrimEnd('\','/')) { throw 'Unexpected temporary directory.' }
    [IO.Directory]::Delete($scratch)
}
$passed=@($rows | Where-Object { $_.status -eq 'matched' }).Count
$readOnly=@($rows | Where-Object { $_.status -eq 'sourceReadable' }).Count
$emptyReads=@($rows | Where-Object { $_.status -eq 'zeroByteSource' }).Count
$scope=if ($SampleGroup -eq 'Pending') { 'unreleasedPendingSnapshot' } elseif ($InboxOnly) { 'unreleasedInboxSnapshot' } else { 'completedRunArchive' }
[ordered]@{ mode=$PSCmdlet.ParameterSetName; evidenceScope=$scope; sampleGroup=$SampleGroup; safeDisconnectVerified=$false; receiptRows=$plan.receiptRows; pendingAlreadyReceipted=$plan.pendingWithReceipt;
            selected=$rows.Count; matched=$passed; sourceReadableOnly=$readOnly;emptySourceStreams=$emptyReads;failed=$rows.Count-$passed-$readOnly; invalidBacking=$plan.invalidBacking;
            ambiguous=$plan.ambiguous; overBudget=$plan.budgetExcluded; selectionLimit=$plan.selectionLimit;
            unsupported=$plan.unsupported; zeroOrUnknownSize=$plan.empty; samples=$rows } | ConvertTo-Json -Depth 4 -Compress
if ($rows.Count -eq 0 -or ($passed+$readOnly) -ne $rows.Count) { exit 2 }
