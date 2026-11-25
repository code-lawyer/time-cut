#Requires -Version 7.2
<#
.SYNOPSIS
Build, bind, or remove the two empty Android test folders created by test_android_mtp_fixture.ps1.
.EXAMPLE
pwsh -File scripts/remove_android_fixture_dirs.ps1 build
.EXAMPLE
pwsh -STA -File scripts/remove_android_fixture_dirs.ps1 bind -Receipt D:\QA\s21-run.json
.EXAMPLE
pwsh -STA -File scripts/remove_android_fixture_dirs.ps1 remove -Receipt D:\QA\s21-run.json
.DESCRIPTION
Build uses x64 MSVC and the Windows SDK. Bind must run after fixture seed or mutation,
before cleanup removes the ownership marker. It opens WPD read-only, matches a single
Samsung interface and the random marker, and records hashes of the physical serial,
interface and persistent object IDs in an adjacent .wpd.json receipt. After fixture
cleanup, remove checks the same Shell device/storage keys and native bindings and uses
only PORTABLE_DEVICE_DELETE_NO_RECURSION. It refuses an unexpected child, changed ID or
missing property. A driver can refuse deleting even empty folders. Keep both receipts.
#>
[CmdletBinding()]
param([Parameter(Mandatory,Position=0)][ValidateSet('build','bind','remove')][string]$Mode,
      [string]$Receipt)
$ErrorActionPreference='Stop'
$build = Join-Path (Split-Path $PSScriptRoot -Parent) '.timecut-build/wpddir-tool'
$source = Join-Path $PSScriptRoot 'android_fixture_empty_dirs.cpp'
$exe = Join-Path $build 'android_fixture_empty_dirs.exe'
if ($Mode -eq 'build') {
    if (!(Get-Command cl.exe -ErrorAction SilentlyContinue)) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        $vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (!$vs) { throw 'MSVC x64 and the Windows SDK are required.' }
        Import-Module (Join-Path $vs 'Common7/Tools/Microsoft.VisualStudio.DevShell.dll')
        Enter-VsDevShell -VsInstallPath $vs -SkipAutomaticLocation -DevCmdArguments '-arch=x64 -host_arch=x64' | Out-Null
    }
    [void][IO.Directory]::CreateDirectory($build)
    Push-Location $build
    try {
        & cl.exe /nologo /std:c++17 /W4 /EHsc /DUNICODE /D_UNICODE $source /link ole32.lib portabledeviceguids.lib propsys.lib crypt32.lib
        if ($LASTEXITCODE) { throw 'Native helper compilation failed.' }
    } finally { Pop-Location }
    return
}
if (!$Receipt -or ![IO.File]::Exists($exe)) { throw 'Provide the original receipt and build this helper first.' }
if ([Threading.Thread]::CurrentThread.GetApartmentState() -ne 'STA') { throw 'Run the Shell binding with pwsh -STA.' }
function Get-Key([string]$Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) { throw 'Unavailable Shell identity.' }
    [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($Value))).ToLowerInvariant()
}
function Find-Item($Folder, [scriptblock]$Predicate) {
    $items = $Folder.Items()
    if ($null -eq $items) { throw 'Shell cannot enumerate folder.' }
    $count = $items.Count
    $found = $null
    for ($i=0;$i -lt $count;$i++) {
        $entry=$items.Item($i)
        if (!$entry) { throw 'Incomplete Shell enumeration.' }
        if (& $Predicate $entry) {
            if ($found) { throw 'Ambiguous Shell match.' }
            $found=$entry
        }
    }
    if ($items.Count -ne $count) { throw 'Shell directory changed during enumeration.' }
    return $found
}
$receiptPath=(Resolve-Path -LiteralPath $Receipt).ProviderPath
$bindingFile="$receiptPath.wpd.json"
$lock=[IO.FileStream]::new("$receiptPath.lock",'OpenOrCreate','ReadWrite','None',4096,'DeleteOnClose')
try {
    $state=Get-Content -Raw -LiteralPath $receiptPath | ConvertFrom-Json -AsHashtable
    $leaf=$state.ownershipLeaf ?? '.nomedia'
    $mark=Join-Path "$receiptPath.data" 'seed/.nomedia'
    if ($state.format -cne 'timecut-android-fixture-v1' -or $state.folder -cnotmatch '^TimecutQa_[a-f0-9]{32}$' -or
        $leaf -cnotin @('.nomedia','TimecutQaOwner.txt') -or $state.owner -cnotmatch '^[a-f0-9]{64}$' -or
        $state.deviceKey -cnotmatch '^[a-f0-9]{64}$' -or $state.storageKey -cnotmatch '^[a-f0-9]{64}$' -or
        (Get-FileHash -LiteralPath $mark).Hash.ToLowerInvariant() -cne $state.owner -or
        [IO.File]::ReadAllText($mark) -cnotmatch ('^'+$state.folder+'/[a-f0-9]{32}$')) { throw 'Invalid original fixture receipt.' }
    if ($Mode -eq 'bind' -and $state.phase -cnotin @('seeded','mutated')) { throw 'Bind while the verified fixture and owner marker are present.' }
    if ($Mode -eq 'remove' -and $state.phase -cne 'files-removed') { throw 'Use fixture cleanup first.' }
    $binding=$null
    $providerArgs=@()
    if ($state.providerMarkerSha256) {
        if ($leaf -cne 'TimecutQaOwner.txt' -or $state.providerMarkerSha256 -cne (Get-Key ('/storage/emulated/0/'+$state.folder))) {
            throw 'Invalid provider marker for this exact fixture.'
        }
        $providerArgs=@($state.providerMarkerSha256)
    }
    if ([IO.File]::Exists($bindingFile)) {
        $binding=Get-Content -LiteralPath $bindingFile -Raw | ConvertFrom-Json -AsHashtable
        if ($binding.format -cne 'timecut-wpd-empty-dirs-v1' -or $binding.folder -cne $state.folder -or
            $binding.owner -cne $state.owner -or $binding.deviceKey -cne $state.deviceKey -or $binding.storageKey -cne $state.storageKey -or
            @($binding.native).Count -ne 5 -or @($binding.native | Where-Object {$_ -cnotmatch '^[a-f0-9]{64}$'}).Count) { throw 'Invalid native binding.' }
    }
    if ($Mode -eq 'remove' -and !$binding) { throw 'No prior native binding. Leave the empty directories in place.' }
    $shell=New-Object -ComObject Shell.Application
    $phone=Find-Item ($shell.NameSpace(17)) { param($item) $item.IsFolder -and !$item.IsFileSystem -and (Get-Key ([string]$item.Path)) -ceq $state.deviceKey }
    if (!$phone) { throw 'Original Shell device is unavailable.' }
    $store=Find-Item $phone.GetFolder { param($item) $item.IsFolder -and (Get-Key ([string]$item.Path)) -ceq $state.storageKey }
    if (!$store) { throw 'Original Shell storage is unavailable.' }
    $findFolder = { param($item) $item.IsFolder -and $item.Name -ceq $state.folder }
    if ($Mode -eq 'bind') {
        $folder=Find-Item $store.GetFolder $findFolder
        if (!$folder -or !(Find-Item $folder.GetFolder { param($item) $item.IsFolder -and $item.Name -ceq 'nested' })) { throw 'Original fixture hierarchy unavailable.' }
        $result=@(& $exe bind $state.folder $leaf $state.owner @providerArgs)
        if ($LASTEXITCODE -or $result.Count -ne 5 -or @($result | Where-Object { $_ -cnotmatch '^[a-f0-9]{64}$' }).Count) { throw 'Cannot establish native binding. No files changed.' }
        if ($binding) {
            if (($binding.native -join ',') -cne ($result -join ',')) { throw 'The existing native binding differs. Refusing to rebind.' }
        } else {
            $proof=[ordered]@{ format='timecut-wpd-empty-dirs-v1'; folder=$state.folder; owner=$state.owner;
                deviceKey=$state.deviceKey; storageKey=$state.storageKey; native=$result; boundUtc=[datetime]::UtcNow.ToString('o') }
            $temporary="$bindingFile.$([guid]::NewGuid().ToString('N')).tmp"
            [IO.File]::WriteAllText($temporary,($proof | ConvertTo-Json),[Text.UTF8Encoding]::new($false))
            [IO.File]::Move($temporary,$bindingFile,$false)
        }
        Write-Output 'Verified the device, storage and both original directory bindings.'
    } else {
        $native=@($binding.native)
        $result=@(& $exe remove $state.folder $leaf $state.owner @native)
        if ($LASTEXITCODE -or $result.Count -ne 1 -or $result[0] -cnotin @('removed','absent')) { throw 'WPD could not confirm safe non-recursive removal.' }
        $until=[DateTime]::UtcNow.AddSeconds(8)
        do {
            $folder=Find-Item $store.GetFolder $findFolder
            if (!$folder) { break }
            Start-Sleep -Milliseconds 350
        } while ([DateTime]::UtcNow -lt $until)
        if ($folder) { throw 'Shell still lists the folder. Retain the receipts for inspection.' }
        Write-Output "WPD result $($result[0]); the original storage no longer lists the test directory."
    }
} finally { $lock.Dispose() }
