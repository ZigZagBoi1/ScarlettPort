# Launches a title on the ScarlettPort-based runtime from your own dump (you need your own legally
# obtained copy; nothing is included in this repository).
# Usage: .\scripts\play-scarlett.ps1 -Game "D:\path\to\dump" [-AutoInput] [-Console series-s|series-x]
#          [-NoOwnUpscaler] [-KeepWatchdog] [-FastBoot] [-RayTracing] [-SaveSlot name] [-Precompile] [-SkipPrecompile]
#
# Environment it sets (you can also set these by hand before running the exe):
#   SCARLETT_GAME_DIR       the dump folder
#   SCARLETT_OWN_UPSCALER=1 ask for the title's own upscaler on the Series S profile (recommended)
#   SCARLETT_NO_WATCHDOG=1  disarm the title's stall watchdog so it can wait for slow first-run
#                          shader builds instead of ending itself
#   SCARLETT_SYNC_PRECISE=1 a submission waits only for the submissions it depends on
#   SCARLETT_READ_SETTLE_PRECISE=1  read-backs settle on the exact submissions they need
#   SCARLETT_READFIX, SCARLETT_COPY_BACK_ONLY_READBACK, SCARLETT_NARROW_ROOT / _VIEWS / _BVH,
#                          SCARLETT_PREP_BARRIERS / _TABLES / _REUSE = 1: the other verified speed settings
#   SCARLETT_RELAY=2        submission relay, form 2 (needed by the two settings above)
#   SCARLETT_PIPELINE_WAIT_MS=0  do not hold a draw back waiting for a pipeline build
#   SCARLETT_FAST_BOOT=1    with -FastBoot: press through logos and menus (testing only)
#   SCARLETT_RAYTRACING=1   with -RayTracing: the title's ray-traced lighting, for that launch only
#   SCARLETT_SAVE_SLOT      with -SaveSlot: pick a save slot by name
#   SCARLETT_AUTO_INPUT=1   with -AutoInput: press A on a timer so menus advance without a pad
#   SCARLETT_PRESS_FILE     press.txt in the repo root: write button names ("A", "DOWN DOWN A") to press them
param(
    [Parameter(Mandatory)] [string]$Game,
    # The console profile the title sees. series-s is the recommended default.
    [ValidateSet('series-s', 'series-x')] [string]$Console = 'series-s',
    # Press A on a timer so the title screens advance without a controller.
    [switch]$AutoInput,
    # Opt-in picture and shader translation fixes (SCARLETT_LOOK, SCARLETT_SHADER_FIXES); not yet measured on the RTX 3060.
    [switch]$Look,
    # Do not set SCARLETT_OWN_UPSCALER=1.
    [switch]$NoOwnUpscaler,
    # Do not set SCARLETT_NO_WATCHDOG=1.
    [switch]$KeepWatchdog,
    # Testing only: SCARLETT_FAST_BOOT=1.
    [switch]$FastBoot,
    # Run the title's ray-traced lighting passes with its software BVH traversal.
    [switch]$RayTracing,
    # Save slot name (SCARLETT_SAVE_SLOT).
    [string]$SaveSlot = '',
    # Run `scarlett shaders` even if it already ran once.
    [switch]$Precompile,
    # Do not run `scarlett shaders` (shaders are then translated while you play).
    [switch]$SkipPrecompile
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$vic  = Join-Path $root 'engine\target\release\scarlett.exe'


if (-not (Test-Path $vic)) { throw 'Not built yet. Run .\scripts\setup-scarlett.ps1 first.' }
$exe = Join-Path $Game 'game_scarlett_master.exe'
if (-not (Test-Path $exe)) {
    throw "No game_scarlett_master.exe in '$Game'. Point -Game at your own dump."
}

$env:SCARLETT_GAME_DIR = $Game
if (-not $NoOwnUpscaler) { $env:SCARLETT_OWN_UPSCALER = '1' }
if (-not $KeepWatchdog) { $env:SCARLETT_NO_WATCHDOG = '1' }
# The verified speed settings (see docs/RUNTIMES.md). The experimental memory switches
# (msfix, mem2) stay off.
$env:SCARLETT_RELAY = '2'
$env:SCARLETT_SYNC_PRECISE = '1'
$env:SCARLETT_READ_SETTLE_PRECISE = '1'
# The rest of the verified speed settings.
foreach ($k in 'SCARLETT_READFIX','SCARLETT_COPY_BACK_ONLY_READBACK','SCARLETT_NARROW_ROOT','SCARLETT_NARROW_VIEWS','SCARLETT_NARROW_BVH','SCARLETT_PREP_BARRIERS','SCARLETT_PREP_TABLES','SCARLETT_PREP_REUSE') { Set-Item "env:$k" '1' }
# fx-cand4: sub-page memory, copies without the extra query, 4 GB card buffers. SCARLETT_REMAP_LAZY is left
# out: a moving A/B showed it now slows the world down.
$env:SCARLETT_MEM_SUBPAGE = '1'; $env:SCARLETT_COPY_NOQUERY = '1'; $env:SCARLETT_CARD_BUFFERS_MB = '4096'
# Leaves processor time to the desktop while the title runs.
$env:SCARLETT_FRIENDLY = '1'
$env:SCARLETT_PIPELINE_WAIT_MS = '0'
if ($FastBoot) { $env:SCARLETT_FAST_BOOT = '1' }
if ($Look) { $env:SCARLETT_LOOK = '1'; $env:SCARLETT_SHADER_FIXES = '1' }
if ($SaveSlot) { $env:SCARLETT_SAVE_SLOT = $SaveSlot }
# Write button names here ("A", "DOWN DOWN A") to press them while the game runs.
$env:SCARLETT_PRESS_FILE = Join-Path $root 'press.txt'
Set-Content -Path $env:SCARLETT_PRESS_FILE -Value '' -NoNewline
if ($AutoInput) { $env:SCARLETT_AUTO_INPUT = '1' }

# Translate every shader in the dump into the shader cache, so the first run does not stop to do
# it (a few minutes, done once and marked by a stamp file). The engine since 0.4.6 has no
# `precompile-graphics`; its `warm` only makes pipelines that runs with SCARLETT_PIPELINE_BOOK=1
# recorded, so this script does not run it.
$stamp = Join-Path $root 'logs\shaders.done'
if (-not $SkipPrecompile -and ($Precompile -or -not (Test-Path $stamp))) {
    Write-Host 'Translating the title''s shaders into the cache (once)...' -ForegroundColor Cyan
    $ErrorActionPreference = 'Continue'
    & $vic shaders $Game
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "scarlett shaders failed (exit $code)." }
    New-Item -ItemType Directory -Force (Split-Path $stamp) | Out-Null
    Set-Content -Path $stamp -Value (Get-Date -Format s)
}

$logs = Join-Path $root 'logs'
New-Item -ItemType Directory -Force $logs | Out-Null
$log = Join-Path $logs ("scarlett-{0:yyyyMMdd-HHmmss}.log" -f (Get-Date))
Write-Host "Starting on the $Console profile... log: $log" -ForegroundColor Cyan
# Windows PowerShell 5.1 turns a native program's stderr into a terminating error under 'Stop'
# when it is redirected, which would kill the run on the first log line.
$ErrorActionPreference = 'Continue'
# For this launch only: PowerShell's environment belongs to the whole session, so a plain launch
# after one with -RayTracing in the same window would still run with ray tracing. A value set by
# hand before running the script is kept when the switch is left out.
$rayTracingBefore = $env:SCARLETT_RAYTRACING
if ($RayTracing) { $env:SCARLETT_RAYTRACING = '1' }
try {
    & $vic run $exe --execute --console $Console *> $log
} finally {
    $env:SCARLETT_RAYTRACING = $rayTracingBefore
}
exit $LASTEXITCODE
