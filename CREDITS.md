# Credits

Started as a fork of VIctory (GPL-3.0): https://github.com/Marvisak/VIctory, through https://github.com/fxceless/victory-fork, with https://github.com/myokex/victory-amd and https://github.com/LooperSalty/victory.

This is the full list of the work it comes from and the work merged into it. The runtime is licensed GPL-3.0-or-later; `engine/LICENSE` is unchanged. Fork point and merge policy: [`engine/UPSTREAM.md`](engine/UPSTREAM.md).

The Classic engine in `runtime/xcl` is XCL by ivall and contributors (https://github.com/ivall/xcl, GPL-2.0-only); its credits stay in that folder.

Paths below are relative to `engine/`.

## Lineage

1. **The original project** by Marvisak and contributors (first link above), GPL-3.0-or-later. The loader, the platform services, the Direct3D 12 back end and the shader recompiler start here.
2. **fxceless's fork** of it (second link above), GPL-3.0. Our base is its `main` at `4e17478`.
3. **VIcky** by Xeltax (https://github.com/Xeltax/VIcky), branch `fx-heapmap-4e`: the heap map for NVIDIA's view-heap cap and the per-program wave64 switch, plus commits by its contributors (including an integer-blend fix and a window-close fix). See `VICKY.md`.
4. **AnyXboxX** (LooperSalty, last link above): the stable stack and notes that our earlier base and this work drew on; its `lot5` branch was checked for overlap and licence (GPL-3.0).
5. **myokex's AMD fork** (third link above, a fork of fxceless's, GPL-3.0), merged at `8e6d29e`: AMD fixes (texture views told apart by the memory their mips take, views of placed textures put back, dispatches over 65,535 groups split, DirectStorage page arrays read when run, compute pipelines waiting separately, view indices clamped on AMD, programs that stop the AMD compiler left out). Its README is kept as `README.md`. Commits by its other contributors (uiliannn, sagouindupm, lukka, DoubleD17, AscilCH) are in the range below.
6. **Community pull requests** picked on top, keeping their authors where the commit was applied as is. Numbers are those of the repository they came from (the original project for #1215; otherwise fxceless's fork):
   - #1215 (upstream): direct writes into title memory, `VirtualQuery` only after a failure
   - #156: ray-tracing work (acceleration-structure reads by slot)
   - #173: ray-traversal fix (patch applied by hand; its author is Bennet0k)
   - #191, #141, #187: submission relay and in-flight waits (#187's relay left opt-in, `SCARLETT_RELAY=1|2`)
   - #147: shader cache keyed by the translator's own source
   - #95, #92, #177: texture and memory-lock reworks, banded detiling
   - #47, #45, #42, #48: further fixes from the same group
   - Not taken: upstream #1529 and 6d021fe, pipeline pre-build commits c47b529 / 9c58a1c / 819f622, #175, #186 (conflicting designs).
7. **ScarlettPort changes** on top: Windows 10 Agility SDK opt-in, automatic input (`SCARLETT_AUTO_INPUT`), a live press file with stick tokens, capture-every and frame dumps, fast boot (`SCARLETT_FAST_BOOT`, testing only) and save slots (`SCARLETT_SAVE_SLOT`), a host descriptor-words table (descriptors read as numbers), exposure, ambient and shadow-mask stand-ins (off by default), `SCARLETT_SHADER_OPT` and the relay left off by default, a `SCARLETT_RAYTRACING=off` answer, and test switches (`SCARLETT_TEST_NO_GPU`). Since the earlier snapshot: keyboard and mouse played like a PC game (`SCARLETT_KBM`, `docs/input.md`), a splash inside the title's window, pipelined DirectStorage reads, precompiled graphics and compute pipelines (`SCARLETT_PRECOMPILED_PSO`), linked mesh/pixel pairs, a geometry-stage path for merged ES-GS programs, ReBAR host copies (`SCARLETT_REBAR`), and the opt-in speed switches for indirect clamps, barriers, synchronisation and memory copies (`SCARLETT_INDIRECT_*`, `SCARLETT_BARRIER_*`, `SCARLETT_SYNC_PRECISE`, `SCARLETT_KNOWN_WRITES`, `SCARLETT_FAST_HASH`, `SCARLETT_COPY_BACK_ONLY_READBACK`, ...; see `docs/RUNTIMES.md`). The ray-tracing node test (`image_bvh_intersect_ray`, `image_bvh64_intersect_ray`) is emulated as inline DXIL in `crates/scarlett-shader/src/emit/intersect.rs`.

Authors whose commits are in the range `4e17478..fx-cand4` (kept in the history of our working repository, not in this snapshot): Xeltax, fever308, iurysf, fxceless, UNYTIX, sedzz, ruvenesh, marquezini, gdawid94, Matheus, Looper_Salty, Last Thylacine, Christian Blomert, Bennet0k, Adrito, myokex, sagouindupm, uiliannn, lukka, DoubleD17, AscilCH, AlanQiang0418, God489, iNaxxs, Rivaldo Freitas de Carvalho, Ziggy.

Also credited: XCL by ivall and contributors (https://github.com/ivall/xcl, GPL-2.0-only), used as a reference for how a title behaves under another layer.

## This snapshot

- Source: branch `fx-cand4` at `8d32f87` of our fork's working repository, with the pull requests merged in this repository since the previous snapshot kept on top. Every new switch is off by default in the runtime; `scripts/play-scarlett.ps1` and the launcher turn on the verified ones (see `docs/RUNTIMES.md`).
- Since `8d32f87`: picture fixes (`SCARLETT_LOOK`), shader translation fixes (`SCARLETT_SHADER_FIXES` and its `SCARLETT_SHADER_*` parts), the XMA audio fix (always on), `SCARLETT_WAVE32_LOWER`, `SCARLETT_MOVIE_PLANE`, memory options (`SCARLETT_OOM_BACKOFF`, `SCARLETT_COMMIT_GATE`, `SCARLETT_PREFETCH_CAP`) and `SCARLETT_THIN`, all off by default. Much of it is adapted from codec-d/cool_fox (GPL-3.0); authors are kept in our working repository's history: Ziggy, codec_d, fxceless, Noname0202, jaschinski97-ops, mgunes1311-lab, and commits recorded as `local` that credit their sources in their messages.
- perf/nvidia-1127 (by Ziggy, ported by hand, GPL-3.0): Belobug/VIctory-Belobug-AMD #332 (`038c507c`), #294 (`70a2d5d8`), #326 (`61b11340`), #351 (`044eda92`); avatarmanz/VIctory-7900XTX renderer port 6 (`cc761bf`, `SCARLETT_PORT_GUARD`, off by default). Not taken: avatarmanz renderer port 7 and `PORT_CONSTANTS_ORDER`, Belobug #325 and #354, and DexNom/VIctory `cd94d353` + `940d32f6` (they build on code this tree does not have).
- port/victory-nvidia (ported by hand by Ziggy, GPL-3.0) from trustno1-coder/VIctory-NVIDIA (https://github.com/trustno1-coder/VIctory-NVIDIA): `854f57ff` + `2dc3df1c` by regiumlepidi (group thread counter for console lanes, a port of Vice-Tay `c125c0c`, fxceless #1243; `SCARLETT_THREAD_TICKET=0` restores the old numbering), `826ba150` by Ahyildiz (violet probe garbage dropped in the ambient stand-in), `47747d62` by wike1701 (dynamic descriptor slots go back only to the host heap that gave them), `80c70b71` by Indy2l (`SCARLETT_HELPER_PRIORITY`, off by default), `aec85efb` by CuteBooty after lichenlang89's fxceless #1266 (`SCARLETT_RAW_VIEW_CLAMP`, off by default here), `e31f4740` by CuteBooty after C-GDev's fxceless #613 (`SCARLETT_SKIP_BINK`, off by default). Not taken: `86a39760` (this tree has no settings menu or mouse capture), the window and input work of #17, FSR 3 / DLSS frame generation (`29d8dbf7`) and the F1 settings menu and F3 stats panel (they build on code this tree does not have).
- Since `b3882b9` (one commit, by Ziggy): the texture upload queue is fair (`SCARLETT_FAIR_DUE=2` by default, `SCARLETT_TEX_SPAN_PCT`), with a "texture queue" log line every five seconds.
- Merged in `efcc7b9..b3882b9` (`fx-cand4` at `efcc7b9`, which this repository took in 11be94c):
  - screen passes with workers of their own, on by default (`SCARLETT_SCREEN_WORKERS=0`: off): the lighting passes fix
  - room kept for card copies capped (`SCARLETT_CARD_ROOM_KEPT_MB`, 512 by default): the texture reserve fix
  - `SCARLETT_FRIENDLY` (background pools and priorities that leave processor time to the desktop)
  - DirectStorage: a load cache (`SCARLETT_LOAD_CACHE`), zlib-rs inflate (`SCARLETT_FAST_INFLATE`), several handles per file (`SCARLETT_DS_HANDLES`); all off by default
  - shader optimizer passes (`SCARLETT_SHADER_OPT` bounds and uniform-index bits) and a cost survey; off by default
  - native GPU memory work (`SCARLETT_NATIVE_GPUMEM`, `SCARLETT_NATIVE_BUFFERS`, `SCARLETT_NATIVE_PROBE`), off by default
  - `crates/scarlett-native`: a patcher that can replace a title function with a native one (absolute jump, trampoline, guard check). This snapshot carries the patcher only: the module of the working repository that named a title function and its entry bytes is left out, so `SCARLETT_NATIVE` does nothing here, and one unit test uses synthetic code instead.
  - indirect-record, bind-cache and card-copy switches ported or adapted from other forks (myokex's AMD fork by uiliannn; fxceless), off by default
- Authors in `efcc7b9..b3882b9`: Ziggy, fxceless, uiliannn, Diogo Sousa, Exopus, and commits recorded as `local` that adapt work credited in their messages to romainhedouin (Belobug) and ZJOU.
- Merged in the snapshot before (`fx-cand` at `bce072d` to `efcc7b9`):
  - lazy remaps of title memory (`SCARLETT_REMAP_LAZY`, `fx-remap3`) and sub-page refreshes (`SCARLETT_MEM_SUBPAGE`): the yard stutter fix
  - copies without the extra memory query (`SCARLETT_COPY_NOQUERY`); copies cut from 286 to 84 MB per frame with the Stable settings
  - fast boot fixed (`SCARLETT_FAST_BOOT`, still optional)
  - in-session A/B: live switches and `SCARLETT_AB` windows
  - shader prewarm (`fx-prewarm`), wave64 and mesh-link work, skin shaders (`fx-pedtex`), view-heap sizing (`fx-vram`), preparing cuts (`fx-prep3`), save fix (`fx-savefix`)
  - upscaler and pacing work (`fx-upscale`, `fx-play2`, `SCARLETT_LOW_LATENCY`, `SCARLETT_FPS_CAP`), GPU and preparing profilers, a zero-copy test switch; all off by default
  - the runtime now reads the launcher's `[display]`, `[game]`, `[pace]` and `[upscale]` sections itself (the acceptance test from #14 is kept)
- Authors new in `bce072d..efcc7b9`: CRT, Chipsrain, Demon12388, Filip722, KaiserBloo, Lilith133T, Pilote Production, SpaghettiMaster545, alessiodeia, andrewpozdnakov7-jpg, avt0m4t48, beloooo, denkencar-sketch, desp, fxceless, gomeaw, larelgit, mgunes1311-lab, naixue2233, powplusplus, sagouindupm, traidy2222, yaajme, Ziggy.
- Merged two snapshots ago (`fx-final` at `26f8afd`; before it `fx-harvest` at `38e8e7c`, which this repository took in 48ba23a), by branch:
  - boot fixes
  - saves fix (`fx-saves`, `fx-saves-h2`)
  - splash and account picker (`fx-splash`)
  - keyboard and mouse (`fx-kbm`)
  - pacing (`fx-pace`)
  - readback precise settles (`SCARLETT_READ_SETTLE_PRECISE`, off by default in the runtime; the play script turns it on)
  - indirect batching (`fx-batch`)
  - dispatch hazards (`fx-dispatch`)
  - memory-copy cuts (`fx-mem-copy`)
  - loading pipeline (`fx-load`)
  - msfix pipeline cache (25,200 of 25,471 pipelines prebuilt by `scarlett precompile-graphics`; `fx-msfix`, off by default)
  - mem2, the second round of memory work (`fx-mem2`, off by default)
  - cutscene diagnostics (`fx-cutscene`)
  - audio (`fx-audio`)
  - overlay (`fx-overlay`)
  - stable watchdog (`fx-stable`)
  - cachekey opt-in (`fx-cachekey`)
  - f3 view fix (`fx-f3view`, default off)
- The diff of this tree against fxceless's fork (`main` at `4e17478`) was published as a patch stack in earlier snapshots.
- The base already took the title's name out of its tree; this snapshot keeps it that way. Two doc comments (`readback.rs`, `descriptor/slot_map.rs`), one line of `VICKY.md` (a former repository name) and one line of `README.md` (a note on how the fork was written) differ from the working repository for that reason.

Build and run: see `docs/BUILDING.md`, `scripts/setup-scarlett.ps1` and `scripts/play-scarlett.ps1`.

- ScarlettPort naming (this repository): every crate, the executable (`scarlett.exe`), the data folder (`%LOCALAPPDATA%\ScarlettPort`) and every switch (`SCARLETT_*`) carry the ScarlettPort name since 0.4.7; `apps/scarlett/build.rs` stamps a version resource (ProductName and FileDescription "ScarlettPort"), and top-level window captions start with "ScarlettPort" (`crates/scarlett-hle/src/modules/lifecycle.rs`). The release ships the binary as `runtime\ScarlettPort-Runtime.exe`.
