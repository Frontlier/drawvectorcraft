@echo off
setlocal
set "APP=%~dp0target\debug\vectorcraft.exe"
if not exist "%APP%" (
  echo Compile o aplicativo antes de abrir: cargo build -p vectorcraft -p vectorcraft-cli
  pause
  exit /b 1
)
if not exist "%~dp0target\debug\cdr\cdr2svg.exe" (
  powershell.exe -NoProfile -File "%~dp0packaging\windows\Install-CdrRuntime.ps1"
  if errorlevel 1 exit /b 1
)
if "%~1"=="" (
  start "" "%APP%"
) else (
  start "" "%APP%" "%~1"
)
