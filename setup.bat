@echo off
rem ---------------------------------------------------------------------
rem  Jira Overlay - setup
rem  Double-click this. It installs Python if the machine hasn't got it,
rem  then the packages, then starts the app.
rem ---------------------------------------------------------------------
title Jira Overlay - Setup
cd /d "%~dp0"
rem Fully-qualified: a bare "powershell" is resolved via PATH, which a
rem writable earlier PATH entry (e.g. %LOCALAPPDATA%\Microsoft\WindowsApps)
rem could shadow with something else entirely. %SystemRoot% is fixed on
rem every supported Windows version.
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup.ps1"
if errorlevel 1 (
    echo.
    echo Setup did not finish. See the messages above.
    pause
)
