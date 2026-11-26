# Roadmap: 30 fps on a mid-range NVIDIA GPU

Current state is in [STATUS.md](STATUS.md): the lit world at 5.4 fps while walking and turning on an RTX 3060 (Series S profile), with steady pacing (95th-percentile frame 197 ms), hair and lighting correct in the opening cell, shadowed areas too dark, the intro cutscene very slow unless skipped, not yet verified playable. The target is 30 fps in the open world on a card such as an RTX 3060 with a correct image. Items are ordered roughly by what blocks the next one; none has a date.

## 1. Light in shadow

Verified in the opening cell on the Series S profile and a Radeon RX 7900 XTX. Ray-traced bounce light brightens the wall and the papers by the bunk. The table casts a shadow under the fluorescent lamps. The launcher's ray-tracing option and `-RayTracing` in `scripts/play-scarlett.ps1` enable the builder and traversal. [Measurements and hardware](../engine/docs/fork-ray-tracing.md)

The back of the inmate shirt can still render black. A capture traced that color to the material buffer before lighting. Other interiors and GPU models still need coverage.

## 2. Frame time

5.4 fps means a median frame of 184 ms; 30 fps is not reached. Where the frame goes, measured on the thread that gates each present: about 187 ms per frame, of which about 55 ms is preparing draws, about 31 ms is refresh copies, about 76 ms is waiting on the card after the title writes next to memory that shaders wrote, and about 19 ms is waiting for title reads. An infinitely fast card would still leave about 99 ms per frame; free copy-back would leave about 180 ms; a quarter of the resolution saves only about 13 ms of GPU time. So the card is not the limit: the waits and the processor work around it are. Tested with no gain: the shader optimizer mask, the pipelining switch, a lock split, thin LTO and removing the settle. The work that follows from this is on the waits and copies around the card, not on the card's own time.

The GPU profiler (`SCARLETT_GPU_PROFILE`) measures about 93-111 ms of GPU time per frame, spread over about 1,100 pipelines, so the gain has to come from many passes at once: the shader optimizer's safe mask removes 51% of instructions offline, and its in-game test is pending. Done in v0.4.0: the yard stutter (lazy remap plus the copy fix) and memory copies cut from 286 to 84 MB per frame. Remaining candidates: title-memory copies, submission waits (the opt-in relay), shader efficiency, and the 64-lane programs that run as two 32-lane halves on NVIDIA.

## 3. Intro cutscene

The intro is very slow unless it is skipped. Find out why before judging overall speed.

## 4. Correctness of the world

Some cards lose the first room's walls and roof with certain switches; shader clean-up passes are off by default for that reason. Find the cause so they can be turned on.

## 5. Ahead-of-time shader and pipeline build

First-run shader compiles and pipeline creation stall the world (pipeline compile freezes when moving on first runs). v0.4.0 adds a shader prewarm. XCL has a pipeline journal and an offline build; the ScarlettPort runtime caches translated shaders on disk. The aim is a warm-up that finishes before the first frame.

## 6. Series S profile and memory

Run the title the way the smaller console does (its memory budget, resolution and feature set), which also lowers memory use, and keep streaming within the VRAM of a 12 GB card.

## 7. Verify playable

Play a session end to end, with saves, and record what breaks.

## Changelog

### v0.4.4

Runtime: picture fixes (`SCARLETT_LOOK`), shader translation fixes (`SCARLETT_SHADER_FIXES`), the XMA audio fix (always on), memory options (`SCARLETT_OOM_BACKOFF`, `SCARLETT_COMMIT_GATE`, `SCARLETT_PREFETCH_CAP`), `SCARLETT_WAVE32_LOWER`, `SCARLETT_MOVIE_PLANE`, `SCARLETT_THIN`; all new switches off by default. Launcher 0.4.4: opt-in Look preset. Not yet measured on the RTX 3060; frame rate unchanged at about 5 fps.

### v0.4.3

Runtime synced to fx-cand4 (8d32f87); launcher 0.4.3 (version only). Measured on an RTX 3060 12 GB with a Ryzen 5 5600X (Series S profile), walking and turning in the world: 5.4 fps, median frame 184 ms, 95th percentile 197 ms, 99th percentile 207 ms (3.9-4.5 fps before). 30 fps is not reached.

- **Faster:** the texture upload queue is fair (`SCARLETT_FAIR_DUE=2`, default; `SCARLETT_TEX_SPAN_PCT`, 25 by default) and now drains: about 110 textures waiting at the end of a run, against 21,000 or more before.
- **Fixed:** hair and lighting render correctly in the opening cell.
- **New:** a "texture queue" log line every five seconds.
- **Tested with no gain:** the shader optimizer mask, the pipelining switch, a lock split, thin LTO, removing the settle.
- **Known issues:** ray tracing is off, shadowed areas are too dark, and the speed is far from 30 fps.

### v0.4.2

Runtime synced to fx-cand4 (b3882b9); launcher 0.4.2. Measured on an RTX 3060 (Series S profile): about 4 fps while walking and turning in the world, with steady pacing (95th-percentile frame about 270 ms).

- **Fixed:** the screen lighting passes (their workers are on by default).
- **Fixed:** the texture reserve (the room kept for card copies is capped, `SCARLETT_CARD_ROOM_KEPT_MB`, 512 by default).
- **Faster:** load time from an internal SSD: the world is reached in about 212 s, against 300 s or more before.
- **New:** in-session A/B tooling for comparing switches live.
- **New:** GPU profiler (`SCARLETT_GPU_PROFILE`): about 93-111 ms of GPU time per frame, spread over about 1,100 pipelines.
- **In testing:** the shader optimizer's safe mask removes 51% of instructions offline; the in-game test is pending.
- **Launcher:** the Stable preset adds `SCARLETT_FRIENDLY=1`.
- **Known issues:** hair renders too bright, ray tracing is off, and the speed is far from 30 fps.

### v0.4.0

Runtime synced to fx-cand4 (efcc7b9); launcher 0.4.0. Measured on an RTX 3060 (Series S profile): about 5 fps in the cell scene and about 4 fps while moving, with stable frame pacing.

- **Fixed:** the yard stutter (lazy remap plus the copy fix).
- **Faster:** memory copies cut from 286 to 84 MB per frame.
- **Fixed:** fast boot (still optional, off in the presets).
- **New:** in-session A/B tooling (live switches cycled within one session).
- **New:** shader prewarm.
- **Launcher:** the Stable preset uses the fx-cand4 settings (precise syncs and settles, precompiled graphics, read fix, read-back-only copies, narrow root/views/BVH, prepared barriers/tables/reuse, lazy remap, sub-page memory, no-query copies, 4 GB card buffers).
- **Known issues:** lighting and hair look wrong (ray tracing is off); pipeline compile freezes when moving on first runs.

### Still in progress

- **In testing:** further streaming work (narrower declared write ranges, fills and copies served on the card).
- **In testing:** missing-geometry fixes: character skin shaders, mesh shaders linked under the 32 KiB output limit, view heap size on NVIDIA.
- **In testing:** CPU recording cuts, with a per-part preparing line for measuring them.
- **In testing:** save-safety fix.
- **In testing:** one-command benchmark queue for A/B runs.
- **Planned:** FSR upscaler (AMD PC FidelityFX) and frame generation.

### v0.2.1

- Runtime synced to the verified default build; launcher with Discord status, updates from Releases and hardware report.
