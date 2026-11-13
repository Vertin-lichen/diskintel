@echo off
rem DiskIntel engine-bridge uninstaller.
rem Double-click: one UAC prompt, then removes the two scheduled tasks
rem ("DiskIntel gozd" / "DiskIntel gozd stop") and stops gozd.exe.
rem Source code, snapshots and the release package are NOT touched.
setlocal
set SCRIPT=%~dp0uninstall.ps1
where pwsh >nul 2>nul
if errorlevel 1 (
  echo PowerShell 7 ^(pwsh^) not found. Install PowerShell 7 or run uninstall.ps1
  echo from an elevated Windows PowerShell:
  echo   powershell -ExecutionPolicy Bypass -File "%SCRIPT%"
  pause
  exit /b 1
)
powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','%SCRIPT%'"
if errorlevel 1 (
  echo Elevation cancelled or failed.
  pause
)
endlocal