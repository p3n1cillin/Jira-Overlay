@echo off
rem Launch the Triage Shadow Learner (read-only — never actions tickets)
cd /d "%~dp0"
start "" pythonw triage_learner.pyw
