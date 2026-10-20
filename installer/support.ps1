# Installer lifecycle only. No Jira calls and no site-specific configuration.
param(
    [ValidateSet('Discover','Prepare','Finish','Stop','RemoveStartup')][string]$Action,
    [string]$Target,
    [string]$Source,
    [string]$ResultFile,
    [string]$Version,
    [switch]$EnableStartup,
    [int]$TimeoutSeconds = 600
)
$ErrorActionPreference = 'Stop'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$names = @('JiraOverlay', 'JiraTriageLearner')

function Canonical([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path)) { throw 'An application path is required.' }
    return [IO.Path]::GetFullPath($Path).TrimEnd('\')
}

function Safe-Tree([string]$Path, [bool]$Recursive = $true) {
    $item = Get-Item -LiteralPath $Path -Force
    $cursor = $item
    while ($null -ne $cursor) {
        if ($cursor.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw 'Linked/junction application paths are not supported. Choose a normal local folder.'
        }
        $cursor = $cursor.Parent
    }
    if ($item.PSIsContainer -and $Recursive) {
        foreach ($child in Get-ChildItem -LiteralPath $Path -Force -Recurse) {
            if ($child.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw 'The application contains a linked file or folder; no changes were made.'
            }
        }
    }
}

function Legacy-Paths {
    $paths = @()
    if (Test-Path -LiteralPath $runKey) {
        $entries = Get-ItemProperty -LiteralPath $runKey
        foreach ($name in $names) {
            $property = $entries.PSObject.Properties[$name]
            if ($property -and $property.Value -match '"([^"\r\n]+\\(?:jira_overlay|triage_learner)\.pyw)"') {
                $directory = Split-Path -Parent $Matches[1]
                if ((Test-Path -LiteralPath (Join-Path $directory 'jira_overlay.pyw')) -and
                    (Test-Path -LiteralPath (Join-Path $directory 'triage_learner.pyw'))) {
                    $paths += (Canonical $directory)
                }
            }
        }
    }
    return @($paths | Select-Object -Unique)
}

function Stop-Companions([string[]]$Roots) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class JiraInstallerEvents {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern IntPtr CreateEvent(IntPtr attributes, bool manual, bool initial, string name);
    [DllImport("kernel32.dll")] public static extern bool SetEvent(IntPtr handle);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr handle);
}
'@
    $handles = @()
    try {
        foreach ($root in ($Roots | Select-Object -Unique)) {
            $sha = [Security.Cryptography.SHA256]::Create()
            try { $bytes = $sha.ComputeHash([Text.Encoding]::UTF8.GetBytes((Canonical $root).ToLowerInvariant())) }
            finally { $sha.Dispose() }
            $hash = ([BitConverter]::ToString($bytes)).Replace('-','').ToLowerInvariant().Substring(0,24)
            $handle = [JiraInstallerEvents]::CreateEvent([IntPtr]::Zero, $true, $false, "Local\JiraOverlay-$hash-stop")
            if ($handle -eq [IntPtr]::Zero) { throw 'Cannot request a safe application shutdown.' }
            $handles += $handle
            if (-not [JiraInstallerEvents]::SetEvent($handle)) { throw 'Cannot signal the application.' }
        }
        $until = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        do {
            $running = $false
            foreach ($mutexName in @('JiraOverlaySingleInstance','JiraTriageLearner')) {
                try {
                    $mutex = [Threading.Mutex]::OpenExisting($mutexName)
                    $mutex.Dispose()
                    $running = $true
                } catch [Threading.WaitHandleCannotBeOpenedException] { }
            }
            if (-not $running) { return }
            if ([DateTime]::UtcNow -ge $until) {
                throw 'The apps are still running. Close BOTH Jira tray icons, then retry. No running learner is force-killed.'
            }
            Start-Sleep -Milliseconds 200
        } while ($true)
    } finally {
        foreach ($handle in $handles) { [void][JiraInstallerEvents]::CloseHandle($handle) }
    }
}

function Personal-Files([string]$Path) {
    # Exact user-data names, plus their atomic-save backups. Never import code.
    $allowed = @('jira_config.json','triage_state.json','triage_suggestions.json',
        'triage_status.json','triage_reviews.json','triage_history.jsonl',
        'triage_dataset.jsonl','triage_log.jsonl','triage_log.jsonl.metrics.json','triage_learner.log',
        'TRIAGE_REPORT.md','TRIAGE_REVIEW_QUEUE.md')
    return @(Get-ChildItem -LiteralPath $Path -File -Force | Where-Object {
        $fileName = $_.Name
        $baseName = $fileName -replace '\.(bak|tmp|corrupt(?:[-.].*)?)$',''
        ($allowed -contains $fileName) -or ($allowed -contains $baseName)
    })
}

function File-Digest([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($sha.ComputeHash($stream)) }
    finally { $sha.Dispose(); $stream.Dispose() }
}

function Migrate-Data([string]$Old, [string]$New) {
    if ($Old -eq $New) { return }
    $files = @(Personal-Files $Old)
    if ($files.Count -eq 0) { return }
    if ((Test-Path -LiteralPath $New) -and (@(Personal-Files $New).Count -gt 0)) {
        throw 'The destination already has personal data. Refusing to merge two learners. Use the existing installation or choose an empty destination.'
    }
    [void][IO.Directory]::CreateDirectory($New)
    $stage = Join-Path $New ('.migration-' + [Guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($stage)
    $moved = @()
    try {
        foreach ($file in $files) {
            $copy = Join-Path $stage $file.Name
            Copy-Item -LiteralPath $file.FullName -Destination $copy
            if ((File-Digest $copy) -ne (File-Digest $file.FullName)) {
                throw 'A personal-data copy did not verify; the original installation is untouched.'
            }
        }
        # Historical backups remain at the original path (which is never deleted).
        foreach ($file in $files) {
            Move-Item -LiteralPath (Join-Path $stage $file.Name) -Destination (Join-Path $New $file.Name)
            $moved += (Join-Path $New $file.Name)
        }
    } catch {
        foreach ($created in $moved) { Remove-Item -LiteralPath $created -Force }
        throw
    } finally {
        # This is a freshly created, exact staging directory, never the application root.
        if ((Split-Path -Parent $stage) -ne $New -or (Split-Path -Leaf $stage) -notmatch '^\.migration-[a-f0-9]{32}$') {
            throw 'Unsafe migration staging path.'
        }
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
}

function Remove-OwnedStartup([string[]]$Roots) {
    if (-not (Test-Path -LiteralPath $runKey)) { return }
    $entries = Get-ItemProperty -LiteralPath $runKey
    foreach ($name in $names) {
        $property = $entries.PSObject.Properties[$name]
        if (-not $property) { continue }
        foreach ($root in $Roots) {
            $removed = $false
            foreach ($script in @('jira_overlay.pyw','triage_learner.pyw')) {
                if ($property.Value.IndexOf(('"' + (Join-Path $root $script) + '"'), [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                    Remove-ItemProperty -LiteralPath $runKey -Name $name
                    $removed = $true
                    break
                }
            }
            if ($removed) { break }
        }
    }
}

function Invoke-Lifecycle {
try {
    if ($Action -eq 'Discover') {
        $found = @(Legacy-Paths)
        if ($found.Count -eq 1) { [IO.File]::WriteAllText($ResultFile, $found[0], (New-Object Text.UTF8Encoding $false)) }
        exit 0
    }
    $Target = Canonical $Target
    if ($Target -eq [IO.Path]::GetPathRoot($Target).TrimEnd('\') -or
        $Target -eq [Environment]::GetFolderPath('UserProfile') -or
        $Target -eq [Environment]::GetFolderPath('LocalApplicationData')) {
        throw 'Choose a dedicated Jira Overlay application folder, not a drive or profile root.'
    }
    if (Test-Path -LiteralPath $Target) {
        Safe-Tree $Target
        if (Test-Path -LiteralPath (Join-Path $Target '.git')) { throw 'Never install over a Git checkout. Choose a separate installation directory.' }
        if (Test-Path -LiteralPath (Join-Path $Target 'app\app_release.py')) {
            throw 'Never overwrite an organised source tree. Install into a separate directory and use the source folder only as a migration source.'
        }
        if ($Action -eq 'Prepare') {
            $releaseFile = Join-Path $Target 'app_release.py'
            if ($Version -and (Test-Path -LiteralPath $releaseFile)) {
                $declaration = [IO.File]::ReadAllText($releaseFile)
                if ($declaration -match '(?m)^VERSION\s*=\s*"([0-9.]+)"' -and ([Version]$Matches[1] -gt [Version]$Version)) {
                    throw 'A newer build is already installed. Download the latest installer instead of downgrading.'
                }
            }
            if ((@(Get-ChildItem -LiteralPath $Target -Force).Count -gt 0) -and
                -not ((Test-Path -LiteralPath (Join-Path $Target 'jira_overlay.pyw')) -and
                      (Test-Path -LiteralPath (Join-Path $Target 'triage_learner.pyw'))) -and
                -not (Test-Path -LiteralPath (Join-Path $Target '.jira-overlay-install')) -and
                (@(Personal-Files $Target).Count -eq 0)) {
                throw 'The destination is a non-empty unrelated folder. Choose a dedicated application folder.'
            }
        }
    } else {
        $ancestor = Split-Path -Parent $Target
        while (-not (Test-Path -LiteralPath $ancestor)) { $ancestor = Split-Path -Parent $ancestor }
        Safe-Tree $ancestor $false
    }
    $roots = @($Target)
    if (-not [string]::IsNullOrWhiteSpace($Source)) {
        $Source = Canonical $Source
        Safe-Tree $Source
        foreach ($name in @('jira_overlay.pyw','triage_learner.pyw')) {
            if (-not (Test-Path -LiteralPath (Join-Path $Source $name) -PathType Leaf)) { throw 'Select the old application folder, containing both scripts.' }
        }
        $roots += $Source
    }
    switch ($Action) {
        'Prepare' {
            Stop-Companions $roots
            if ($Source) { Migrate-Data $Source $Target }
        }
        'Stop' { Stop-Companions @($Target) }
        'Finish' {
            Remove-OwnedStartup $roots
            if ($EnableStartup) {
                [void](New-Item -Path $runKey -Force)
                $python = Join-Path $Target 'runtime\pythonw.exe'
                New-ItemProperty -LiteralPath $runKey -Name 'JiraOverlay' -Value ('"' + $python + '" "' + (Join-Path $Target 'jira_overlay.pyw') + '"') -PropertyType String -Force | Out-Null
                # The overlay starts the learner only after credentials exist.
                if ((Get-ItemProperty -LiteralPath $runKey).PSObject.Properties['JiraTriageLearner']) {
                    Remove-ItemProperty -LiteralPath $runKey -Name 'JiraTriageLearner'
                }
            }
        }
        'RemoveStartup' { Remove-OwnedStartup @($Target) }
    }
    exit 0
} catch {
    if ($ResultFile) { [IO.File]::WriteAllText($ResultFile, $_.Exception.Message, (New-Object Text.UTF8Encoding $false)) }
    Write-Error $_.Exception.Message -ErrorAction Continue
    exit 1
}
}

if ($MyInvocation.InvocationName -ne '.') { Invoke-Lifecycle }
