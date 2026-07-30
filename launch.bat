@echo off
rem Start the whole thing: overlay + triage learner.
cd /d "%~dp0"
start "" pythonw "%~dp0jira_overlay.pyw"
start "" pythonw "%~dp0triage_learner.pyw"
