# Install, update and uninstall

## One installer for every user

Download **JiraOverlay-<version>-Setup.exe** from
[the latest release](https://github.com/p3n1cillin/Jira-Overlay/releases/latest).
Double-click it. The same EXE installs a new user, repairs missing program files,
or upgrades an existing installation. No ZIP extraction, Python installation,
pip command or administrator account is needed. Supports Windows 10/11 x64
(including x64-compatible systems).

The program installs for the current Windows user under
`%LOCALAPPDATA%\Programs\JiraOverlay`, with its own isolated `runtime/`, a Start
menu shortcut and a single Windows Installed apps entry. A fresh user supplies
their own Jira domain, account/token and service desk at first launch. The
installer contains no credentials, tenant IDs, queue names or trained models.

The EXE is currently **unsigned**: Windows may show an unknown-publisher or
SmartScreen warning. Obtain it only from the repository release page; the release
includes SHA-256 checksums. Corporate application-control policy may require
approval by your IT team. There is no claim that a checksum is code signing.

## Older GitHub ZIP or clone

The optional previous-installation page usually detects the old folder from
Windows startup entries. Otherwise browse to the folder containing
`jira_overlay.pyw`, `triage_learner.pyw` and your personal configuration. Leave it
blank for a new user. The installer copies verified configuration, models,
history, reviews and reports but never imports old executable code. It will not
merge or replace a different dataset already at the destination.

Use the **same Windows account**: encrypted Jira credentials are protected by
Windows DPAPI and should not be copied between users. Original folders and their
historical `triage_backups/` stay untouched for recovery. Do not delete the old
folder until you have checked the new installation. Setup automatically asks
newer builds to save/close. Identifiable older builds are closed through their
Tk/pystray window loops, without terminating other Python programs. Legacy builds
retain their last-saved data; unlike newer builds, they cannot promise to flush
unsaved learning on exit. Unrecognised or security-blocked copies require
**Quit** on both tray icons. If apps are still saving
after 30 seconds, setup returns a clear retry message; tuning can take several
minutes. Wait for both tray icons to disappear, then **Retry**. A fresh install
also requires any old running copy to close. The installer never force-kills a learner.

Setup logs are created automatically in `%TEMP%` as `Setup Log ... .txt`.
If setup closes unexpectedly, share the newest log with the maintainer (review
it for local paths first). An explicit `/LOG="path"` can select the log location.
If security software blocks the PowerShell helper, ask your IT team rather than
disabling security controls.

Preparation shows a progress bar for app shutdown, checks and verified migration;
file installation then uses the normal setup progress bar. Startup registration
and launch are automatic, respecting the user's existing startup preference.

| Failure code | Reason and next step |
| --- | --- |
| `JIRA-I101` | Unsafe/invalid destination, newer installed build, or wrong old folder. Follow the specific path message. |
| `JIRA-I110` | Windows could not verify or signal running apps. Quit both tray icons and retry. |
| `JIRA-I111` | Running copy could not be identified/asked to quit. Select its folder or quit both tray icons. |
| `JIRA-I112` | App is still saving/running after 30 seconds. Wait for both icons to disappear, then retry. |
| `JIRA-I113` | Legacy quit bridge failed or was blocked. Quit both icons manually; ask IT if policy blocked it. |
| `JIRA-I120` | Personal-data migration failed or would merge learners. Original data is retained; follow the specific copy/destination reason. |
| `JIRA-I130` | Windows startup registration/cleanup failed. Check the log and rerun setup to repair. |
| `JIRA-I900` | PowerShell helper could not run/complete. The log includes its Windows/helper status; ask IT about policy restrictions. |

These diagnostic codes are displayed in the message and log; the setup process
still uses standard Inno exit statuses for deployment tools. Code `JIRA-I100`
is reserved for an unexpected failure before a lifecycle phase starts.

Git checkouts are never overwritten. A checkout can be a read-only migration
source, or developers can continue using Git and `setup.bat` instead.

## Updates

Run the newest Setup EXE again: it reuses the installed folder, preserves personal
files, repairs managed files and updates the private runtime. Settings offers
**Check for updates** and **Update and restart** too. Native installations
download the complete installer, verify GitHub's SHA-256 digest, and run the same
upgrade flow. Checks send no Jira credentials or ticket data. Startup preference
is preserved; automatic checking can be disabled. No background update is applied
without confirmation. A newer installed version is not silently downgraded.

Legacy source ZIP users can still use their transactional code-only updater and
`update.bat --rollback`. Native users should rerun the current installer to
repair the installation; do not apply a code-only rollback to its private runtime.

## One uninstaller

Open **Windows Settings > Apps > Installed apps > Jira Overlay > Uninstall**, or
use **Start > Jira Overlay > Uninstall Jira Overlay**. Both run the same generated
uninstaller (`unins000.exe`). It safely stops the companions, removes installed
files/shortcuts and only startup entries pointing at its own folder. It does not
remove another Python installation, another user's files or an old ZIP folder.

**Credentials/settings and learner data are retained by default.** Reinstalling
into the same folder picks them up again. For a permanent personal-data removal,
first uninstall, inspect/back up the retained application folder, then delete
only that folder yourself. This intentionally requires a separate user decision.

## Managed deployment

Interactive installation is recommended for migration. For IT deployment under
the target user's account:

```powershell
.\JiraOverlay-2026.10.20.4-Setup.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOMIGRATE /NOLAUNCH
```

Optional flags: `/NOSTARTUP`, `/NOSHORTCUTS`, `/DIR="dedicated application path"`,
`/LEGACYDIR="old application folder"` (omit `/NOMIGRATE` to import it) and
`/LOG="installer log path"`. Avoid system-account deployment: this application
and its credentials are per-user. Silent uninstall uses the installed
`unins000.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART`.

Windows startup uses one overlay command; it starts the learner after credentials
are available. Enabling startup retires the old independently launched learner
command. A stale command for another folder/interpreter is not shown as enabled.
Offline workflow discovery retries automatically (up to five minutes between
attempts), and historical transport failures retain a resumable checkpoint.
Runtime problems are shown through the existing learner health display and report.
