@echo off
setlocal EnableExtensions DisableDelayedExpansion
if not "%~1"=="" if /i not "%~1"=="pareto-residency" (
  echo Usage: Play-ScarlettPort.cmd pareto-residency 1>&2
  exit /b 2
)
if not defined SCARLETT_GAME_DIR set "SCARLETT_GAME_DIR=%~dp0..\GTA6.Xbox.v1.0.2.1"
if not exist "%SCARLETT_GAME_DIR%\game_scarlett_master.exe" (
  echo Set SCARLETT_GAME_DIR to your own dump folder containing game_scarlett_master.exe. 1>&2
  exit /b 2
)
set "runner=%~dp0scarlett-3d-rtx-pareto-residency.exe"
if not exist "%runner%" set "runner=%~dp0target\release\scarlett.exe"
if not exist "%runner%" (
  echo Extract the Windows release or run cargo build --release --locked -p scarlett. 1>&2
  exit /b 2
)
if not defined SCARLETT_AMBIENT_AIR set "SCARLETT_AMBIENT_AIR=1.5"
if not defined SCARLETT_CACHE_DIR set "SCARLETT_CACHE_DIR=%LOCALAPPDATA%\ScarlettPort\cache-pareto"
if not defined SCARLETT_TEXTURE_NO_WAIT set "SCARLETT_TEXTURE_NO_WAIT=1"
if not defined SCARLETT_RELAY set "SCARLETT_RELAY=2"
if not defined SCARLETT_WIDE_BACK set "SCARLETT_WIDE_BACK=1"
if not defined SCARLETT_CARD_REHASH_RUNS set "SCARLETT_CARD_REHASH_RUNS=1"
if not defined SCARLETT_JOIN_COPIES set "SCARLETT_JOIN_COPIES=1"
if not defined SCARLETT_INDIRECT_WINDOW set "SCARLETT_INDIRECT_WINDOW=1"
if not defined SCARLETT_RELAY_COPY set "SCARLETT_RELAY_COPY=1"
if not defined SCARLETT_CARD_STEPS set "SCARLETT_CARD_STEPS=2"
if not defined SCARLETT_INDIRECT_BATCH set "SCARLETT_INDIRECT_BATCH=2"
if not defined SCARLETT_MAP_THREADS set "SCARLETT_MAP_THREADS=1"
if not defined SCARLETT_CARD_AWAKE set "SCARLETT_CARD_AWAKE=1"
if not defined SCARLETT_SHOW_AFTER set "SCARLETT_SHOW_AFTER=1"
if not defined SCARLETT_AMBIENT set "SCARLETT_AMBIENT=0.8"
if not defined SCARLETT_THIN set "SCARLETT_THIN=1"
if not defined SCARLETT_JOIN_PARTS set "SCARLETT_JOIN_PARTS=8"
if not defined SCARLETT_UPLOAD_AHEAD set "SCARLETT_UPLOAD_AHEAD=1"
if not defined SCARLETT_LEVELS_ON_DEMAND set "SCARLETT_LEVELS_ON_DEMAND=1"
if not defined SCARLETT_CARD_BUFFERS_MIN_KB set "SCARLETT_CARD_BUFFERS_MIN_KB=64"
if not defined SCARLETT_LOG if not defined RUST_LOG set "SCARLETT_LOG=warn"
"%runner%" run "%SCARLETT_GAME_DIR%\game_scarlett_master.exe" --console series-s --execute
exit /b %errorlevel%
