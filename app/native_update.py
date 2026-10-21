"""External Windows installer supervisor; never holds the private runtime open.

The short-lived Python caller starts Windows PowerShell from a verified staging
directory. That external process waits for setup, records its result, then
relaunches the app after success or a failed/rolled-back installation.
"""
from pathlib import Path
import hashlib
import os
import subprocess

import app_updates as updates


MONITOR_SCRIPT = r'''
param([string]$Installer, [string]$Target, [string]$Version, [string]$Digest, [switch]$NoRelaunch)
$ErrorActionPreference = 'Stop'
$stateFile = Join-Path $Target 'app_update_status.json'
$logFile = Join-Path $PSScriptRoot 'installer.log'
$mutex = $null
$held = $false
$code = -1
$detail = ''
$relaunch = $false
$attempted = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()

function Save-Result([string]$Phase, [string]$Detail) {
    if ((Test-Path -LiteralPath $stateFile) -and
        ((Get-Item -LiteralPath $stateFile -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Refusing a linked update-status file.'
    }
    $value = @{schema=1; version=$Version; attemptedAt=$attempted; phase=$Phase;
        detail=$Detail; notified=$false; log=$logFile} | ConvertTo-Json -Compress
    $temporary = Join-Path $Target ('.update-' + [Guid]::NewGuid().ToString('N'))
    try {
        [IO.File]::WriteAllText($temporary, $value, (New-Object Text.UTF8Encoding $false))
        if (Test-Path -LiteralPath $stateFile) { [IO.File]::Replace($temporary, $stateFile, [NullString]::Value) }
        else { [IO.File]::Move($temporary, $stateFile) }
    } finally { if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force } }
}

try {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { $hash = ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($Target.ToLowerInvariant())))).Replace('-','') }
    finally { $sha.Dispose() }
    $mutex = New-Object Threading.Mutex($false, ('Local\JiraOverlayInstallerMonitor-' + $hash.Substring(0,24)))
    try { $held = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $held = $true }
    if (-not $held) { exit 3 }
    $stream = [IO.File]::OpenRead($Installer)
    $sha = [Security.Cryptography.SHA256]::Create()
    try { $actual = ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-','') }
    finally { $sha.Dispose(); $stream.Dispose() }
    if ($actual -ne $Digest) {
        throw '[JIRA-U201] The staged installer changed after verification. Nothing was installed.'
    }
    Save-Result 'installing' 'Installing the verified application update.'
    # /SILENT retains the requested progress display; suppressed message boxes
    # prevent an unattended upgrade waiting for a hidden confirmation dialog.
    $arguments = @('/SILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/NOMIGRATE',
        '/NOLAUNCH', '/RESTARTEXITCODE=3010', ('/DIR="' + $Target + '"'), ('/LOG="' + $logFile + '"'))
    $setup = Start-Process -FilePath $Installer -ArgumentList $arguments -PassThru -Wait -WindowStyle Normal
    $code = $setup.ExitCode
    if ($code -eq 3010) { throw '[JIRA-U205] Windows needs a restart to finish the update. Setup did not reboot your computer.' }
    $relaunch = $true
    if ($code -ne 0) {
        $reason = ''
        if (Test-Path -LiteralPath $logFile) {
            $tail = Get-Content -LiteralPath $logFile -Tail 100
            foreach ($line in $tail) { if ($line -match '\[JIRA-I[0-9]+\][^\r\n]*') { $reason = $Matches[0] } }
        }
        throw ('[JIRA-U202] Installer exited with status ' + $code + '. ' + $reason + ' See ' + $logFile)
    }
    $declaration = [IO.File]::ReadAllText((Join-Path $Target 'app_release.py'))
    if ($declaration -notmatch '(?m)^VERSION\s*=\s*"([0-9.]+)"' -or $Matches[1] -ne $Version) {
        throw '[JIRA-U203] Setup finished but the expected application version was not found. Run the latest installer to repair it.'
    }
    Save-Result 'completed' ('Updated successfully to ' + $Version)
} catch {
    $detail = $_.Exception.Message
    if ($detail -notmatch '\[JIRA-') { $detail = '[JIRA-U200] ' + $detail }
    [Console]::Error.WriteLine($detail)
    try { Save-Result 'failed' $detail } catch { }
} finally {
    if ($held) {
        if ($relaunch -and -not $NoRelaunch) {
            try {
                $python = Join-Path $Target 'runtime\pythonw.exe'
                $launcher = Join-Path $Target 'launch_installed.pyw'
                Start-Process -FilePath $python -ArgumentList ('"' + $launcher + '" --resume') -WorkingDirectory $Target -WindowStyle Hidden | Out-Null
            } catch {
                $detail = '[JIRA-U204] Could not relaunch the application. Open Jira Overlay from Start. ' + $_.Exception.Message
                try { Save-Result 'failed' $detail } catch { }
                # There may be no surviving app to display a relaunch failure.
                Add-Type -AssemblyName System.Windows.Forms
                [void][Windows.Forms.MessageBox]::Show($detail, 'Jira Overlay update')
            }
        } elseif ($code -eq 3010) {
            Add-Type -AssemblyName System.Windows.Forms
            [void][Windows.Forms.MessageBox]::Show($detail, 'Jira Overlay update')
        }
        $mutex.ReleaseMutex()
    }
    if ($mutex) { $mutex.Dispose() }
}
if ($detail) { exit 1 }
exit 0
'''


def launch(root, installer, release, *, relaunch=True):
    root, installer = Path(root).resolve(), Path(installer).resolve()
    if updates.source_checkout(root) or not updates.native_install(root):
        raise updates.UpdateError("[JIRA-U101] Automatic native updates require a managed installation, not a Git/source tree")
    updates.version_key(release["version"])
    expected = release["installer"]["sha256"]
    if hashlib.sha256(installer.read_bytes()).hexdigest() != expected:
        raise updates.UpdateError("[JIRA-U201] Staged installer checksum mismatch; nothing was installed")
    # All supervisor files are outside the target/runtime being replaced.
    if root == installer.parent or root in installer.parents:
        raise updates.UpdateError("[JIRA-U101] Installer staging must be outside the application folder")
    script = installer.parent / "monitor.ps1"
    with script.open("x", encoding="utf-8") as stream:
        stream.write(MONITOR_SCRIPT)
    executable = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    command = [str(executable), "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
            "-File", str(script), "-Installer", str(installer), "-Target", str(root),
            "-Version", release["version"], "-Digest", expected]
    if not relaunch:
        command.append("-NoRelaunch")
    with (installer.parent / "monitor.log").open("xb") as output:
        return subprocess.Popen(command, cwd=installer.parent, creationflags=0x08000000,
            stdout=output, stderr=subprocess.STDOUT)
