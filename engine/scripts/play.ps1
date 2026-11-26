# Starts the title with the settings this fork is played with (README.md, "How to run it").
#   scripts\play.ps1 -Exe <path of your own copy's executable>
# -Safe         without the newer, faster ways (the first thing to try when something is wrong)
# -RayTracing   the title's own ray-traced indirect light: lit interiors, about a third slower
# -Build        target\diag (the default, the build all measurements are of) or target\release
# -More         other settings: -More @{SCARLETT_PANEL='1'}
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$Safe,
    [switch]$RayTracing,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [hashtable]$More = @{}
)
$settings = @{
    SCARLETT_TEXTURE_NO_WAIT = '1'; SCARLETT_RELAY = '2'; SCARLETT_WIDE_BACK = '1'; SCARLETT_CARD_REHASH_RUNS = '1'
    SCARLETT_JOIN_COPIES = '1'; SCARLETT_INDIRECT_WINDOW = '1'; SCARLETT_RELAY_COPY = '1'; SCARLETT_CARD_STEPS = '2'
    SCARLETT_INDIRECT_BATCH = '2'; SCARLETT_MAP_THREADS = '1'; SCARLETT_TITLE_PRIORITIES = '0'; SCARLETT_CARD_AWAKE = '1'
    SCARLETT_SHOW_AFTER = '1'; SCARLETT_AMBIENT = '0.8'; SCARLETT_D3D12_DRED = 'light'; SCARLETT_SKIP_PIPELINES = 'none'
}
if ($Safe) {
    $settings.SCARLETT_UNDER_WIDE = '0'; $settings.SCARLETT_LAND_ECHO = '0'
} else {
    $settings.SCARLETT_THIN = '1'; $settings.SCARLETT_JOIN_PARTS = '8'; $settings.SCARLETT_UPLOAD_AHEAD = '1'
}
if ($RayTracing) { $settings.SCARLETT_BVH_BUILDER = '1' }
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }

$scarlett = Join-Path $PSScriptRoot "..\target\$Build\scarlett.exe"
if (-not (Test-Path $scarlett)) { Write-Host "No $scarlett. Build it first (README.md)."; return }
if (-not (Test-Path $Exe)) { Write-Host "No $Exe."; return }
# (Other settings left over in this window go; where your files are stays.)
$paths = 'SCARLETT_CONFIG', 'SCARLETT_GAME_DIR', 'SCARLETT_CACHE_DIR', 'SCARLETT_DATA_DIR'
Get-ChildItem Env:SCARLETT_* | Where-Object { $_.Name -notin $paths } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }
$Exe = (Resolve-Path $Exe).Path
Push-Location (Join-Path $PSScriptRoot '..')   # scarlett.toml is looked for here
try { & $scarlett run $Exe --execute } finally { Pop-Location }
