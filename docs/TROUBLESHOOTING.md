# Troubleshooting

When something goes wrong, check here first, then open an issue with a log attached (see [Logs](#logs)). For general questions see [FAQ.md](FAQ.md).

## Antivirus warnings

**Why:** the launcher and runtime are new, unsigned executables that few people have run, so reputation checks (SmartScreen and similar) have nothing to go on. The runtime also loads a large program into memory, watches memory pages and compiles code at run time, all things heuristic scanners associate with malware. These are false positives.

**Check your download.** Releases are built by GitHub Actions from this repo, and each release lists SHA-256 checksums in its notes. Compare before you run anything:

```powershell
Get-FileHash .\ScarlettPort-<version>-win64.zip -Algorithm SHA256
```

If the hash matches, the file is the one CI built. If it does not, delete it and download again from [Releases](https://github.com/ZigZagBoi1/ScarlettPort/releases) only; never use copies from other sites. If you allow the file in your antivirus, allow only that file or folder, not a whole drive.

## The first boot stalls or stutters

The title ships tens of thousands of GPU programs. Each is translated for your card and then compiled by your graphics driver into a pipeline. Anything not built ahead of time is built the first time it is needed, which freezes the picture for a moment.

- Press **Build shaders** in the launcher before **Play**. It resumes if stopped; pressing it again skips what is done.
- With the Classic (XCL) engine the count can sit at 83-85% for a long time; that is the slow tail, not a hang. The default World engine does not have it.
- Stalls the first time a new area or effect appears are expected for now. **Prewarm**, a warm-up that fills the cache before the first frame, is on the roadmap and not released yet.
- After an update that changes the shader translator, the shaders are rebuilt once (about 3 minutes).

## The "save is broken" prompt

**Don't panic, and don't delete your save folder.** The runtime writes saves atomically (a full new copy, then one rename), so killing the game does not leave a half-written save. Where this prompt was checked, the save files on disk were intact; the cause is still being investigated.

**Where saves live:**

- World engine (default, ScarlettPort runtime): `%LOCALAPPDATA%\ScarlettPort\data\<title id>\saves\`.
- Classic engine (XCL): `%LOCALAPPDATA%\XCL\<package name>\`, in the `saves` and `pls` folders (or under `%XCL_DATA_ROOT%` if you set it).

The runtime CLI only migrates a missing default `cache` or `data` store when it opens that store. Help and inspection do not move legacy files. Other folders in the old data folder under `%LOCALAPPDATA%` stay in place (the launcher moves those when it starts); if a store has to be copied instead of moved, its original stays in place too.

**Back up:**

- The launcher's **Saves** page backs up both folders to `<cache>\save-backups\<date>` and keeps the newest 10.
- By hand: exit the game, then copy the whole save folder above somewhere safe.

**If the prompt appears:** close the game, make a backup as above, and open an issue with the log. Answering the prompt in game may replace the save, so back up first.

## Known visual bugs

| What you see | Why | Status |
|---|---|---|
| Shadowed areas far too dark | the title's ray-traced lighting and shadows do not run yet | being worked on |
| Characters with missing faces, skin or hair | some skin, hair and clothes pixel shaders were refused, so those draws are dropped | fix in testing, not released |
| Missing objects or walls on NVIDIA | oversized mesh shaders and a too-small view heap drop draws | fixes in testing, not released |
| A grid of triangles or tiles over the picture | old builds decoded rectangle-list pipelines wrong | fixed in v0.2.1; update (if you build yourself, use the current source) |
| Black picture with certain switches | the opt-in `SCARLETT_COPY_BACK_ONLY_READBACK` mode left stale data in memory | fix in testing; leave that switch off |
| Some HUD text cut off | not known yet | open |

If you changed environment switches (see [RUNTIMES.md](RUNTIMES.md)) and the picture breaks, go back to the defaults (the launcher's default settings, or `play-scarlett.ps1` with no extra switches) before reporting.

## Controller not working

- Xbox pads: connect before starting the game; they are read through XInput.
- PlayStation pads (DualSense, DualSense Edge, DualShock 4) are read directly and the log says `PlayStation pad connected`. If it does not appear, close Steam Input or DS4Windows, which can hold the pad, and reconnect it; `SCARLETT_PAD_PS=0` turns direct reading off if a mapper should handle the pad instead.
- No pad: the keyboard acts as one (Enter = A, Backspace = B, WASD and IJKL = sticks). Click the window first so it has focus.

## GPU problems

- **Requirements:** Shader Model 6.6 and mesh shaders: NVIDIA RTX 20-series or newer, AMD RX 6000 or newer. Older cards (GTX 10-series, RX 5000 and earlier) cannot run it.
- **Update your graphics driver** first; old drivers are a common cause of failed pipeline builds.
- **NVIDIA:** the tested vendor (RTX 3060).
- **AMD:** expected to work but not measured by us yet. Please send a hardware report.
- **Windows 10:** the runtime needs the D3D12 Agility SDK 1.614 in a `D3D12` folder next to the runtime executable (`D3D12Core.dll` and `d3d12SDKLayers.dll`). The release ZIP includes it, so keep the `runtime` folder intact when you extract. If you build yourself, run `scripts\setup-scarlett.ps1`.

## Logs

- **Launcher:** the **Logs** page shows the run output. **Send report** opens a prefilled GitHub hardware-report issue with your user name and folders removed from the log.
- **Scripts:** `play.ps1` and `play-scarlett.ps1` write to the `logs\` folder of the repo.
- **Crash reports (Classic engine):** `%LOCALAPPDATA%\xcl\crash-reports`.

**To attach a log to an issue:** open an issue with the [bug report template](https://github.com/ZigZagBoi1/ScarlettPort/issues/new/choose), then drag the log file into the text box (zip it first if it is large). Include your GPU, driver version, CPU, RAM, Windows version, launcher version and any switches you changed. Before attaching, check the log for anything private such as user names in paths. Never attach game files.
