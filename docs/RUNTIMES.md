# The two runtimes

Both runtimes run a title from your own dump. Each currently gets further at something different.

| | ScarlettPort runtime (`engine`) | XCL (`runtime/xcl`) |
|---|---|---|
| Shader path | RDNA2 to DXIL directly | RDNA2 to HLSL to DXC to DXIL |
| Menus and loading | 30 fps | 30 fps |
| Open world | world reached and gameplay starts on an RTX 3060, Series S profile | world reached, survives 7+ minutes |
| Speed in the world | about 4 fps in the prologue (RTX 3060, Series S profile), GPU-bound | not rendering the final image |
| Final image | lit; shadowed areas too dark; the tile-grid overlay is fixed | black |
| Extras | frame capture, save slots, press file, fast boot (testing), launcher (default engine) | launcher (Classic engine), pipeline journal and offline build, profiles, achievements, crash reports |
| Shader build | `scarlett shaders <dump>` (a few minutes) then `scarlett warm` (the pipelines earlier runs recorded) | `xcl shaders --precompile`: 45 min to hours (slow tail from 83%) |
| Scripts | `setup-scarlett.ps1`, `play-scarlett.ps1`, launcher | `setup.ps1`, `play.ps1`, launcher |

Neither is verified playable.

## ScarlettPort runtime

The source lives in `engine/`. Its fork point is in `engine/UPSTREAM.md`; upstream forks, community pull requests, our speed branches and our changes merged in. It includes the `image_bvh_intersect_ray` / `image_bvh64_intersect_ray` emulation (`crates/scarlett-shader/src/emit/intersect.rs`), so ray-tracing compute programs translate. Credits and the full lineage: [`CREDITS.md`](../CREDITS.md).

```powershell
.\scripts\setup-scarlett.ps1
.\scripts\play-scarlett.ps1 -Game "D:\path\to\your\dump"
```

Manual equivalent: build with `cargo rustc --release -p scarlett --bin scarlett --features diagnostics --locked -- -C link-arg=/EXPORT:D3D12SDKVersion,DATA -C link-arg=/EXPORT:D3D12SDKPath,DATA`, put the Agility SDK 1.614 DLLs in a `D3D12` folder next to `scarlett.exe`, then `scarlett.exe run "<dump>\game_scarlett_master.exe" --execute --console series-s`.

`--console series-s` (the profile the measurements here were taken on) is the recommended default; `series-x` is the runtime's own default.

Every switch is a `SCARLETT_*` environment variable. The full list is in the source and in `engine/README.md`; the ones the scripts use or that matter most:

| Variable | Effect |
|---|---|
| `SCARLETT_GAME_DIR` | the dump folder (required) |
| `SCARLETT_UPSCALER=<6\|title\|7\|fsr>` | the upscaler the title uses. Default: the title's own (6), also on the Series S profile, where the title would pick FSR; FSR asked for without `amd_fidelityfx_upscaler*` in the game folder falls back to the title's own. `SCARLETT_OWN_UPSCALER=0` leaves the title's choice alone (`crates/scarlett-loader/src/resolution.rs`) |
| `SCARLETT_NO_WATCHDOG=1` | set by `play-scarlett.ps1` and the launcher; `scarlett.exe` does not read it (the vendored runtime's `XCL_NO_WATCHDOG` is its counterpart) |
| `SCARLETT_FAST_BOOT=1` | testing only: press through logos and menus, stop once in the world |
| `SCARLETT_FAST_BOOT_ESC=0` | with fast boot: press only A (Enter), not START (Esc), which can open the pause menu once in the world so the world is never seen |
| `SCARLETT_FAST_BOOT_SUSTAIN=30` | with fast boot: world frames needed before fast boot stops (default 3); more keeps a slow loading screen from passing for the world |
| `SCARLETT_FAST_BOOT_GAP=10` | with fast boot: light frames in a row that start the count of world frames again (default 1); the world's frames can come with light ones between |
| `SCARLETT_SAVE_SLOT=<name>` | pick a save slot |
| `SCARLETT_AUTO_INPUT=1` | press A on a timer |
| `SCARLETT_KBM=0` | turn off keyboard and mouse as a pad (on by default; keys and settings in `engine/docs/input.md`) |
| `SCARLETT_PRESS_FILE=<file>` | live input: button and stick tokens written to the file are pressed |
| `SCARLETT_CAPTURE_EVERY=N` | save the frame every N frames (diagnostics build) |
| `SCARLETT_RELAY=1\|2` | submission relay (off in the runtime by default; `play-scarlett.ps1` sets `2`) |
| `SCARLETT_SYNC_PRECISE=1` | a submission waits only for the submissions it depends on. Set by `play-scarlett.ps1` |
| `SCARLETT_READ_SETTLE_PRECISE=1` | read-backs settle on the exact submissions they need (needs the relay). Set by `play-scarlett.ps1` |
| `SCARLETT_PRECOMPILED_GRAPHICS=1` | make graphics pipelines from the title's precompiled cache, engines before 0.4.6 only (their `precompile-graphics` command is gone); `play-scarlett.ps1` and `setup-scarlett.ps1 -Game <dump>` run `scarlett shaders` instead |
| `SCARLETT_PIPELINE_WAIT_MS=<ms>` | how long a frame may wait for pipelines still being made (default 0: such a draw is left out of that frame) |
| `SCARLETT_SHADER_OPT=on` | opt-in shader clean-up passes (off by default; they can drop walls and roofs on some cards) |
| `SCARLETT_FAIR_DUE=0\|1\|2` | order of the texture upload queue. `2` (default): textures showing less than the title holds first, then the longest due; `1`: plain turns; `0`: the older order |
| `SCARLETT_TEX_SPAN_PCT=<n>` | share of a long frame the texture pass may take (25 by default; `0`: off) |
| `SCARLETT_SHADER_LOOP_LIMIT=<n>` | cap on translated shader loop iterations (can smear hair; unset by default) |
| `SCARLETT_SKIP_BINK=1\|all` | skip the movies that play once (logos, intro); `all` also the looping ones, which then stay dark. Off by default |
| `SCARLETT_THREAD_TICKET=0` | the old thread numbering for shaders with console lanes in groups of several host waves (on by default: waves take their threads from a group counter) |
| `SCARLETT_RAW_VIEW_CLAMP=1` | keep raw buffer views inside their buffer: a range past the end is shortened, an unaligned or past-the-end start is a null view (off by default; the meter line counts the cuts) |

### Verified speed settings (set by `play-scarlett.ps1` and the launcher; each is off in the runtime itself)

Measured together on the reference machine (RTX 3060, Series S profile): the per-frame read waits drop from about 156 to about 26 ms.

| Variable | Effect |
|---|---|
| `SCARLETT_READFIX=1` | refreshes of a host copy leave the bytes shaders wrote alone |
| `SCARLETT_COPY_BACK_ONLY_READBACK=1` | copy shader writes back only for the title's read-back buffers |
| `SCARLETT_NARROW_ROOT=1`, `SCARLETT_NARROW_VIEWS=1`, `SCARLETT_NARROW_BVH=1` | copy and walk only the root-signature bytes, view tables and BVH addresses a program uses |
| `SCARLETT_PREP_BARRIERS=1`, `SCARLETT_PREP_TABLES=1`, `SCARLETT_PREP_REUSE=1` | prepare barriers and descriptor tables ahead, and keep a command list's storage when it is reset |

### Speed switches (all opt-in, off unless noted)

None of these has been measured on the reference machine in this build. Try them one at a time and back out if you see artifacts or hangs (back-out order: `SCARLETT_COPY_BACK_ONLY_READBACK`, `SCARLETT_KNOWN_WRITES`, `SCARLETT_BARRIER_LITERAL`, `SCARLETT_BARRIER_HAZARD`, `SCARLETT_SYNC_PRECISE`).

| Variable | Effect |
|---|---|
| `SCARLETT_INDIRECT_WINDOW=1` | clamp a run of indirect records in one pass (`SCARLETT_INDIRECT_WINDOW_MOST`, default 4096, is the window size) |
| `SCARLETT_INDIRECT_MERGE=1` | join consecutive indirect calls on one window (needs `SCARLETT_INDIRECT_WINDOW=1`) |
| `SCARLETT_CLAMP_SCOPED=1` | an indirect clamp waits only on its argument and count buffers |
| `SCARLETT_CLAMP_LIGHT_RESTORE` | light state restore after a clamp; on by default, `=0` turns it off |
| `SCARLETT_BARRIER_MERGE=1` | barriers on all resources only where unordered writes are pending; one call's barriers merged |
| `SCARLETT_MEM2_GPU_ONLY_NOSNAP=1`, `SCARLETT_MEM2_GPU_ONLY_NOVIEW=1`, `SCARLETT_MEM2_PAGE_REHASH=1` | the second round of memory work (`engine/crates/scarlett-core/src/mem2.rs`): off by default and left off by `play-scarlett.ps1`, like the pipeline-cache fix branch (`fx-msfix`) |
| `SCARLETT_BARRIER_HAZARD=1` | hold waits for earlier shader work until a direct dispatch conflicts with them |
| `SCARLETT_BARRIER_LITERAL=1` | read barrier groups exactly as D3D12 lays them out |
| `SCARLETT_FENCE_PRECISE=1` | a blocking fence wait waits only for that fence's own signal |
| `SCARLETT_KNOWN_WRITES=1` | skip copies and hashes of memory whose writes are already known |
| `SCARLETT_FAST_HASH=1` | faster lane hash for content checks |
| `SCARLETT_TEXTURE_HASH_EVERY=N` | re-hash a texture every N frames (default 8, 0 = never) |
| `SCARLETT_TEXTURE_OVERLAP=1` | texture overlap tracking |
| `SCARLETT_COPY_BACK_ONLY_READBACK=1` | copy shader writes back only for the title's read-back buffers |
| `SCARLETT_REBAR=1\|2` | keep host copies in CPU-visible card memory (needs GPU upload heaps, i.e. ReBAR); 2 also places title buffers there. `SCARLETT_REBAR_MB` is the budget (default 1/4 of VRAM, at most 4 GiB) |
| `SCARLETT_PRECOMPILED_PSO` | precompiled pipelines from the title's cache (`SCARLETT_PRECOMPILED_DUMP=<dir>` dumps them; diagnostics) |
| `SCARLETT_LINK_PAIRS=<file>` | linked mesh/pixel pairs file for translation |
| `SCARLETT_F3_VIEW_FIX` | host-side view fix; leave off |
| `SCARLETT_HELPER_PRIORITY=1` | the threads the submitting thread waits for (wide pool, landing pools, heap records, the heap pass) run above normal, at its priority |

### Shader build (launcher)

The launcher's **Build shaders** runs, from the `runtime` folder beside it:

1. `scarlett warm "<dump>" --jobs N` with `SCARLETT_GAME_DIR=<dump>` and `SCARLETT_CACHE_DIR=<cache>\scarlett`: translates every shader into the cache and makes the compute pipelines. About 3 min (171 s for 25,889 shaders on a 12-thread CPU). It prints a line every 2,000 shaders; between lines the launcher shows the current range and its elapsed time. `N` is the logical core count minus one, capped by free RAM (512 MB per job).
2. `scarlett precompile-graphics "<dump>\shaders.rpf" --skip S --limit 1000` in chunks of 1,000 records, each saved when it ends (491 s for 25,200 pipelines in one run). **On**: the rectangle-list fix is in this copy, so play runs set `SCARLETT_PRECOMPILED_GRAPHICS=1` and the launcher runs this phase (`engine::PRECOMPILED_GRAPHICS` controls both).

Resuming: finished steps and chunks are recorded in `<cache>\build-world.json`, keyed by the SHA-256 of `scarlett.exe`; translated shaders are read back from the cache, so a restarted build skips them. A different `scarlett.exe` starts the record over; the translation cache is keyed by the translator's source, so only a translator change makes every shader translate again. Play runs use the same `SCARLETT_CACHE_DIR`.

With **Skip slow shaders** (Settings, Options; on, 60 s by default) a pipeline chunk or the Classic precompile that prints nothing for that long is stopped and its shaders compile on demand in game. `scarlett warm` is never stopped this way.

Classic engine: `xcl shaders shaders.rpf --precompile --pipelines --only cs` with `XCL_PIPELINE_THREADS` set from the CPU count and free RAM (1.5 GB per driver compile, at most 8). It gets to about 22,400 of 26,503 (83–85%) quickly; the rest are huge compute shaders the driver compiles in 6–190 s each, during which the count does not move. It resumes from its DXIL and driver caches.

## XCL

Our modified XCL tree (ivall/xcl): console OS layer, D3D12.X, RDNA2 to HLSL to DXIL recompiler. Run it through `play.ps1`, or through the launcher's Classic engine when `xcl.exe` is in its `runtime` folder. Our changes against upstream are in `patches/xcl-stack.patch`.

| Variable | Effect |
|---|---|
| `XCL_DXC_THREADS=N` | shader compile threads (`play.ps1` uses 4) |
| `XCL_PIPELINE_THREADS=N` | pipeline build threads (`setup.ps1` uses 3) |
| `XCL_NO_WATCHDOG=1` | disable the title's 30 s stall watchdog |
| `XCL_PRESS_FILE=<file>` | live input from a file |
| `XCL_PRESS_AT=20:A,23:DOWN` | scripted presses (second:button) |
| `XCL_LAZY_PSO=0` | compile every pipeline up front |
| `XCL_FLAT=1` | tiny flat stand-in for the big lighting shaders |
| `XCL_TWO_PASS_INDIRECT=1` | draw two-pass mesh calls (off by default; can reset the GPU) |

Offline path: the runtime journals every pipeline the title creates; `xcl pipelines <game>` builds them ahead of time, and `setup.ps1 -Game <dump>` precompiles the whole shader pack.

## Patches

`patches/` holds our changes to XCL as a plain diff. The ScarlettPort runtime is not a patch stack: `engine/` is its source of truth (fork point and merge policy in [`engine/UPSTREAM.md`](../engine/UPSTREAM.md)).

| Patch | Base |
|---|---|
| `xcl-stack.patch` | upstream XCL `main` |
