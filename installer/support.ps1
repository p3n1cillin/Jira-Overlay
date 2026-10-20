# Installer lifecycle only. No Jira calls and no site-specific configuration.
param(
    [ValidateSet('Discover','Prepare','Finish','Stop','RemoveStartup')][string]$Action,
    [string]$Target,
    [string]$Source,
    [string]$ResultFile,
    [string]$Version,
    [switch]$EnableStartup,
    [ValidateRange(1,60)][int]$TimeoutSeconds = 30
)
$ErrorActionPreference = 'Stop'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$names = @('JiraOverlay', 'JiraTriageLearner')
$script:FailureCode = 'JIRA-I100'

function Progress([int]$Percent, [string]$Message) {
    Write-Output ('JIRA_PROGRESS|{0}|{1}' -f $Percent, $Message)
}

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
        if ($cursor -is [IO.DirectoryInfo]) { $cursor = $cursor.Parent }
        else { $cursor = $cursor.Directory }
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

function Running-Companions([string[]]$MutexNames) {
    foreach ($mutexName in $MutexNames) {
        try {
            $mutex = [Threading.Mutex]::OpenExisting($mutexName)
            $mutex.Dispose()
            return $true
        } catch [Threading.WaitHandleCannotBeOpenedException] { }
    }
    return $false
}

function Request-LegacyQuit([string[]]$Roots, [bool]$DiscoverRunning = $false) {
    $script:FailureCode = 'JIRA-I113'
    $bridge = Join-Path $PSScriptRoot 'legacy_shutdown.cs'
    if (-not ('JiraLegacyShutdown' -as [type])) { Add-Type -Path $bridge }
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $session = [Diagnostics.Process]::GetCurrentProcess().SessionId
    $sent = 0
    foreach ($process in Get-CimInstance Win32_Process -Filter "Name='python.exe' OR Name='pythonw.exe'") {
        if ($process.SessionId -ne $session -or -not $process.CommandLine) { continue }
        $arguments = [JiraLegacyShutdown]::Arguments($process.CommandLine)
        # Only a direct script invocation, never 'python other.py <app path>',
        # -c, -m or unrelated Python programs. Unrecognised forms fail closed.
        $index = 1
        while ($index -lt $arguments.Count -and $arguments[$index] -in @('-B','-u','-I','-E','-s')) { $index++ }
        if ($index -ge $arguments.Count -or -not [IO.Path]::IsPathRooted($arguments[$index])) { continue }
        $scriptPath = Canonical $arguments[$index]
        $matched = $false
        foreach ($root in $Roots) {
            foreach ($name in @('jira_overlay.pyw','triage_learner.pyw')) {
                if ($scriptPath -eq (Join-Path (Canonical $root) $name)) { $matched = $true }
            }
        }
        if ($DiscoverRunning -and -not $matched -and [IO.Path]::GetFileName($scriptPath) -in @('jira_overlay.pyw','triage_learner.pyw')) {
            # Fresh-install mode may have neither a selected old folder nor a
            # startup entry. Identify a real legacy bundle, not a bare script.
            $directory = Split-Path -Parent $scriptPath
            $matched = (Test-Path -LiteralPath (Join-Path $directory 'jira_overlay.pyw') -PathType Leaf) -and
                (Test-Path -LiteralPath (Join-Path $directory 'triage_learner.pyw') -PathType Leaf) -and
                (Test-Path -LiteralPath (Join-Path $directory 'jira_config.json') -PathType Leaf)
        }
        if (-not $matched) { continue }
        $owner = Invoke-CimMethod -InputObject $process -MethodName GetOwnerSid
        if ($owner.ReturnValue -ne 0 -or $owner.Sid -ne $identity) { continue }
        # Never trust PID alone: obtain a process handle before rechecking its
        # identity, and retain it until the window messages have been posted.
        try { $live = [Diagnostics.Process]::GetProcessById([int]$process.ProcessId) }
        catch [ArgumentException] { continue }
        try {
            $null = $live.Handle
            $fresh = Get-CimInstance Win32_Process -Filter ('ProcessId=' + $process.ProcessId)
            if (-not $fresh -or $fresh.CreationDate -ne $process.CreationDate -or
                $fresh.CommandLine -ne $process.CommandLine) { continue }
            $sent += [JiraLegacyShutdown]::Request($process.ProcessId, ([IO.Path]::GetFileName($scriptPath) -eq 'triage_learner.pyw'))
        } finally { $live.Dispose() }
    }
    if ($sent -gt 0) {
        Progress 15 'Requested Quit for the selected older app windows. Waiting for them to close...'
    }
    return $sent
}

function Stop-Companions([string[]]$Roots,
    [string[]]$MutexNames = @('JiraOverlaySingleInstance','JiraTriageLearner')) {
    $script:FailureCode = 'JIRA-I110'
    Progress 10 'Checking whether the overlay and learner are running...'
    if (-not (Running-Companions $MutexNames)) { return }
    $handles = @()
    try {
        foreach ($root in ($Roots | Select-Object -Unique)) {
            $sha = [Security.Cryptography.SHA256]::Create()
            try { $bytes = $sha.ComputeHash([Text.Encoding]::UTF8.GetBytes((Canonical $root).ToLowerInvariant())) }
            finally { $sha.Dispose() }
            $hash = ([BitConverter]::ToString($bytes)).Replace('-','').ToLowerInvariant().Substring(0,24)
            # Open ONLY an existing listener. Creating a new event used to look
            # successful even when a legacy app could never hear the request.
            try {
                $handle = [Threading.EventWaitHandle]::OpenExisting("Local\JiraOverlay-$hash-stop",
                    [Security.AccessControl.EventWaitHandleRights]::Modify)
                $handles += $handle
                if (-not $handle.Set()) { throw 'Cannot signal the application.' }
            } catch [Threading.WaitHandleCannotBeOpenedException] { }
        }
        # Listener builds save/join their worker before exiting. Only use the
        # compatibility bridge if there was NO supported listener to signal.
        if ($handles.Count -eq 0 -and (Running-Companions $MutexNames)) {
            # Windows startup can identify an old running ZIP even when a
            # fresh install was chosen. Never import data from these roots.
            $discoverRunning = ($MutexNames.Count -eq 2 -and $MutexNames[0] -eq 'JiraOverlaySingleInstance' -and $MutexNames[1] -eq 'JiraTriageLearner')
            $legacyRoots = @($Roots)
            if ($discoverRunning) { $legacyRoots += @(Legacy-Paths) }
            $requested = @(Request-LegacyQuit $legacyRoots $discoverRunning)
            # Relay progress separately from the integer return value.
            foreach ($notice in $requested) { if ($notice -is [string]) { Write-Output $notice } }
            $script:FailureCode = 'JIRA-I111'
            if ($requested[-1] -eq 0 -and (Running-Companions $MutexNames)) {
                throw 'A Jira copy is running but could not be safely identified or asked to quit. Select its previous-install folder, or choose Quit on BOTH Jira tray icons, then Retry. Hiding the overlay is not enough. No application code or personal data was replaced.'
            }
        }
        $timer = [Diagnostics.Stopwatch]::StartNew()
        $lastNotice = -5
        do {
            if (-not (Running-Companions $MutexNames)) { return }
            if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) {
                $script:FailureCode = 'JIRA-I112'
                throw 'The apps are still saving or running. Choose Quit on BOTH Jira tray icons, wait for them to disappear, then Retry. Learner tuning can take several minutes. Nothing was force-killed and no application code or personal data was replaced.'
            }
            if ($timer.Elapsed.TotalSeconds - $lastNotice -ge 5) {
                Progress (15 + [int](15 * $timer.Elapsed.TotalSeconds / $TimeoutSeconds)) ('Waiting for the companions to save and close ({0}s). Never force-killing the learner...' -f [int]$timer.Elapsed.TotalSeconds)
                $lastNotice = $timer.Elapsed.TotalSeconds
            }
            Start-Sleep -Milliseconds 200
        } while ($true)
    } finally {
        foreach ($handle in $handles) { $handle.Dispose() }
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
    $script:FailureCode = 'JIRA-I120'
    if ($Old -eq $New) { return }
    $files = @(Personal-Files $Old)
    foreach ($file in $files) { Safe-Tree $file.FullName $false }
    if ($files.Count -eq 0) { return }
    if ((Test-Path -LiteralPath $New) -and (@(Personal-Files $New).Count -gt 0)) {
        throw 'The destination already has personal data. Refusing to merge two learners. Use the existing installation or choose an empty destination.'
    }
    [void][IO.Directory]::CreateDirectory($New)
    $stage = Join-Path $New ('.migration-' + [Guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($stage)
    $moved = @()
    try {
        $number = 0
        foreach ($file in $files) {
            Progress (35 + [int](55 * $number / $files.Count)) ('Copying and verifying personal data: ' + $file.Name)
            $copy = Join-Path $stage $file.Name
            Copy-Item -LiteralPath $file.FullName -Destination $copy
            if ((File-Digest $copy) -ne (File-Digest $file.FullName)) {
                throw 'A personal-data copy did not verify; the original installation is untouched.'
            }
            $number++
        }
        # Historical backups remain at the original path (which is never deleted).
        foreach ($file in $files) {
            Move-Item -LiteralPath (Join-Path $stage $file.Name) -Destination (Join-Path $New $file.Name)
            $moved += (Join-Path $New $file.Name)
        }
        Progress 95 'Personal data copied and verified. The original folder is unchanged.'
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
    $script:FailureCode = 'JIRA-I101'
    Progress 0 ('Installer phase: ' + $Action)
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
        # Only allowlisted data files are read; do not traverse an old clone's
        # virtual environments, caches or private backup trees.
        Safe-Tree $Source $false
        foreach ($name in @('jira_overlay.pyw','triage_learner.pyw')) {
            if (-not (Test-Path -LiteralPath (Join-Path $Source $name) -PathType Leaf)) { throw 'Select the old application folder, containing both scripts.' }
        }
        $roots += $Source
    }
    switch ($Action) {
        'Prepare' {
            Stop-Companions $roots
            if ($Source) { Migrate-Data $Source $Target }
            Progress 100 'Preparation complete. Ready to install program files.'
        }
        'Stop' { Stop-Companions @($Target) }
        'Finish' {
            $script:FailureCode = 'JIRA-I130'
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
        'RemoveStartup' { $script:FailureCode = 'JIRA-I130'; Remove-OwnedStartup @($Target) }
    }
    exit 0
} catch {
    $detail = '[' + $script:FailureCode + '] ' + $_.Exception.Message
    if ($script:FailureCode -eq 'JIRA-I113') { $detail += ' Windows could not run the legacy quit helper. Quit both tray icons manually and Retry; ask IT if a policy blocks the helper.' }
    if ($ResultFile) { [IO.File]::WriteAllText($ResultFile, $detail, (New-Object Text.UTF8Encoding $false)) }
    Write-Error $detail -ErrorAction Continue
    exit 1
}
}

if ($MyInvocation.InvocationName -ne '.') { Invoke-Lifecycle }
