<#
  Jira Overlay - bootstrap installer
  ==================================
  Gets a bare Windows machine from nothing to a running overlay:

      1. finds a usable Python, installing one if there isn't one
      2. hands over to setup.py for packages, startup and first launch

  Run it via setup.bat (double-click) so you don't have to think about
  PowerShell execution policy.

  Deliberately ASCII-only. Windows PowerShell 5.1 - still the default
  'powershell' on most machines - reads a script without a byte-order mark
  as ANSI, so a stray em-dash is enough to break string parsing and take the
  whole installer down before it prints anything.
#>

$ErrorActionPreference = 'Stop'
$ProjectDir  = Split-Path -Parent $MyInvocation.MyCommand.Definition
$MinVersion  = [Version]'3.9'
# Used only when winget isn't available. Any recent 3.x is fine.
$FallbackVer = '3.12.8'

function Write-Step($n, $text) { Write-Host "`n[$n] $text" -ForegroundColor Cyan }
function Write-Ok($text)       { Write-Host "    $text" -ForegroundColor Green }
function Write-Info($text)     { Write-Host "    $text" -ForegroundColor Gray }
function Write-Bad($text)      { Write-Host "    $text" -ForegroundColor Red }

# --------------------------------------------------------------------------
# Find a Python we can use
# --------------------------------------------------------------------------

function Test-StoreStub($exe) {
    # Windows ships a 0-byte "app execution alias" at
    # WindowsApps\python.exe. It is not Python: running it opens the
    # Microsoft Store, which is the last thing anyone wants mid-install. It
    # has to be recognised without being executed.
    try {
        $item = Get-Item -LiteralPath $exe -Force -ErrorAction Stop
        return ($item.Length -eq 0) -or
               ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)
    } catch {
        return $false
    }
}

function Get-PythonVersion($exe) {
    if (Test-StoreStub $exe) { return $null }
    try {
        $out = & $exe -c "import sys;print('%d.%d'%sys.version_info[:2])" 2>$null
        if ($LASTEXITCODE -eq 0 -and $out) { return [Version]$out.Trim() }
    } catch { }
    return $null
}

function Find-Python {
    $candidates = @()

    # The py launcher knows about every install on the machine.
    if (Get-Command py -ErrorAction SilentlyContinue) {
        try {
            $exe = & py -3 -c "import sys;print(sys.executable)" 2>$null
            if ($LASTEXITCODE -eq 0 -and $exe) { $candidates += $exe.Trim() }
        } catch { }
    }

    foreach ($name in 'python', 'python3') {
        $cmd = Get-Command $name -ErrorAction SilentlyContinue
        if ($cmd) { $candidates += $cmd.Source }
    }

    # Freshly installed Pythons that are not on PATH in this session yet.
    foreach ($root in @("$env:LOCALAPPDATA\Programs\Python",
                        "$env:ProgramFiles\Python*",
                        "${env:ProgramFiles(x86)}\Python*")) {
        Get-ChildItem -Path $root -Filter 'python.exe' -Recurse `
            -ErrorAction SilentlyContinue -Depth 2 |
            ForEach-Object { $candidates += $_.FullName }
    }

    foreach ($exe in ($candidates | Where-Object { $_ } | Select-Object -Unique)) {
        $ver = Get-PythonVersion $exe
        if ($ver -and $ver -ge $MinVersion) {
            return [pscustomobject]@{ Exe = $exe; Version = $ver }
        }
    }
    return $null
}

function Install-Python {
    # winget ships with Windows 11 and recent Windows 10, and handles the
    # download, signature check and PATH for us.
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        Write-Info 'Installing Python via winget...'
        try {
            winget install --id Python.Python.3.12 --exact --source winget `
                --accept-package-agreements --accept-source-agreements `
                --scope user --silent | Out-Null
            if (Find-Python) { return $true }
        } catch {
            Write-Info "winget could not do it: $($_.Exception.Message.Trim())"
        }
    }

    # Fall back to the official installer, per-user so there is no admin prompt.
    $url = "https://www.python.org/ftp/python/$FallbackVer/python-$FallbackVer-amd64.exe"
    $dst = Join-Path $env:TEMP "python-$FallbackVer-amd64.exe"
    Write-Info "Downloading Python $FallbackVer from python.org..."
    try {
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $url -OutFile $dst -UseBasicParsing
    } catch {
        Write-Bad "Download failed: $($_.Exception.Message)"
        return $false
    }
    Write-Info 'Running the installer (this takes a minute)...'
    $p = Start-Process -FilePath $dst -Wait -PassThru -ArgumentList @(
        '/quiet', 'InstallAllUsers=0', 'PrependPath=1',
        'Include_launcher=1', 'Include_test=0')
    Remove-Item $dst -ErrorAction SilentlyContinue
    if ($p.ExitCode -ne 0) {
        Write-Bad "The Python installer exited with code $($p.ExitCode)."
        return $false
    }
    return $true
}

function Update-SessionPath {
    # A just-installed Python is on the *stored* PATH, not this process's copy.
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user    = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = (@($machine, $user) | Where-Object { $_ }) -join ';'
}

# --------------------------------------------------------------------------

Write-Host ''
Write-Host '==============================================================' -ForegroundColor White
Write-Host '  Jira Overlay - setup' -ForegroundColor White
Write-Host '==============================================================' -ForegroundColor White

Write-Step 1 'Looking for Python'
$python = Find-Python
if ($python) {
    Write-Ok "Found Python $($python.Version) at $($python.Exe)"
} else {
    Write-Info "No Python $MinVersion or newer found - installing it."
    if (-not (Install-Python)) {
        Write-Bad 'Could not install Python automatically.'
        Write-Bad 'Install it from https://python.org (tick "Add to PATH"),'
        Write-Bad 'then run setup again.'
        Read-Host 'Press Enter to close'
        exit 1
    }
    Update-SessionPath
    $python = Find-Python
    if (-not $python) {
        Write-Bad 'Python installed but could not be found afterwards.'
        Write-Bad 'Close this window, open a new one, and run setup again.'
        Read-Host 'Press Enter to close'
        exit 1
    }
    Write-Ok "Installed Python $($python.Version)"
}

Write-Step 2 'Handing over to the application setup'
$setupPy = Join-Path $ProjectDir 'setup.py'
if (-not (Test-Path $setupPy)) {
    Write-Bad "setup.py is missing from $ProjectDir"
    Read-Host 'Press Enter to close'
    exit 1
}

& $python.Exe $setupPy
exit $LASTEXITCODE
