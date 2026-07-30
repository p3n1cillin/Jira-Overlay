@echo off
rem ---------------------------------------------------------------------
rem  Jira Overlay - setup
rem  Double-click this. It installs Python if the machine hasn't got it,
rem  then the packages, then starts the app.
rem ---------------------------------------------------------------------
title Jira Overlay - Setup
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup.ps1"
if errorlevel 1 (
    echo.
    echo Setup did not finish. See the messages above.
    pause
)
