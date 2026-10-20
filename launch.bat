@echo off
rem Start the whole thing: overlay + triage learner.
cd /d "%~dp0"
rem The overlay starts the learner after first-run credentials are ready.
if exist "%~dp0runtime\pythonw.exe" (
    start "" "%~dp0runtime\pythonw.exe" "%~dp0launch_installed.pyw"
) else (
    start "" pythonw "%~dp0launch_installed.pyw"
)
