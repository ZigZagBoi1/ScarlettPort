# Ray tracing in this fork: what the title does and what has been built

The opening cell on an RX 7900 XTX exercises the title's structure builder,
screen probes, irradiance cache and distant-shadow tracer. All three tracers
hit scene triangles. A frame capture showed that the packed screen-probe
output reached its sampled texture with the same bits. The bunk, shelves
and papers in the cell became visible when the runtime waited for the six
draws that populate the lighting tables. Ray tracing is opt-in.

## What the title does

In the opening cell, the Series S profile traces screen probes and distant
shadows. For local lights it dispatches the checkerboard and hair shadow-mask
programs. A shadow on a wall can come from either lighting path.

The title does not use a ray-tracing API:

- Traversal is ordinary compute-shader code: a per-thread stack in shared
  memory, and loads through raw 40-bit addresses of title memory.
- The static world's acceleration structures ship pre-built in the title's
  data, in the console GPU's native node format (64-byte nodes: box nodes with
  four children and 16-bit float bounds, 128-byte box nodes with 32-bit float
  bounds, triangle nodes, instance nodes). They are unpacked on the GPU.
- A set of builder programs (from the title's `XboxBvhBuilder.dll`, 52 compute
  programs) builds and refits the dynamic structures on the GPU every frame.
- Only the per-node test is hardware: the instructions
  `image_bvh64_intersect_ray` and `image_bvh_intersect_ray`. One call tests a
  ray against a box node's four children (returning the hit children, nearest
  first) or against a triangle node (returning the intersection's numerator,
  denominator and barycentric numerators).

So mapping this onto the host's own ray tracing (DXR) is not practical: the
layer would have to know the geometry behind structures the title never
describes to it, and the traversal loops live in 64 of the title's own
programs. This fork runs the title's builder and traversal as translated
shaders and emulates the one instruction.

## What this fork adds

1. **All 52 builder programs translate.** The opening scene created 53 builder
   pipelines. They pass buffer addresses in a spilled parameter table
   rather than in descriptors. Added: the spilled user-data table, root
   unordered-access views (translator and host binding), byte and 16-bit buffer
   accesses, and several control-flow shapes.
2. **Title memory by raw address from shaders.** A directory with one entry per
   64 KiB of title address space leads a shader to the layer's host copy of
   that memory; loads, stores, atomic add/sub and float min/max go through it.
   Each host view heap allocates descriptors as the title streams new regions.
   Submission also maps the destinations of the title's late-filled
   `InitHeaders` records before the GPU runs the builder.
3. **The ray test, emulated in DXIL.** Four slab tests and a sorting network
   for box nodes; a watertight ray/triangle test for triangle nodes; both forms
   of the instruction. A plain Rust reference lives in
   `crates/victory-shader/src/bvh.rs`; tests run the emitted DXIL on the GPU
   over 311 ray/node cases and compare with the reference bit for bit.
4. **The trace and shading programs translate** (59 of 64 trace programs, and
   the irradiance-cache shading program). This needed lane masks treated as
   plain numbers on some paths, and exact forms for programs written for
   64-lane waves running on a GPU with 32-lane waves.
5. **The host runs the builder as a unit.** Each registered stage must have
   a host pipeline. The first root UAV that writes a structure header maps
   its GPU buffer into the address directory. Later rays read the same
   buffer even when the title has another view of that page. The title
   receives compacted sizes and counters on the CPU.
6. **Recorded passes keep their acceleration-structure address.** Each
   descriptor-table snapshot stores the title's address alongside the host
   descriptor. The screen probes and shadow passes use it to reach their
   scene root. Before this change, they read the header at byte 48.

## Opening-cell comparison

On a Radeon RX 7900 XTX (driver 32.0.32029.2010), Ryzen 7 7800X3D and
Windows 11 build 26300, the Series S profile reached the cell with ray
tracing on and the shader wait set to zero. The game draws the environment
BRDF, area-light and scattering tables once at world entry. Previously,
the runtime could skip these draws while their pipelines compiled.

With those six draws prioritized, mean brightness on the wall left of the
window rose from 11 to 50 on a scale of 0 to 255. The papers beside the
bunk rose from 15 to 47. The same patches in a [reference image](https://www.gtabase.com/images/gta-6/missions/gta-vi-prologue.jpg)
measured 103 and 65. Measurements use 640 by 360 images and luma weights of
0.2126 red, 0.7152 green and 0.0722 blue. The camera faced the window in
each image. The character's pose differed.

With the builder's root mapped at first use, a release build measured 65
on the wall and 61 on the papers (medians over 16 frames).

After merging upstream through `be7717f`, we started the game with gamepad
input and held the camera still while switching ray traversal off and on.
The screen-probe irradiance fell to zero on both patches with traversal
disabled and returned when traversal resumed.

| Patch | Rays on | Rays off | Rays restored |
| --- | ---: | ---: | ---: |
| Wall left of the window | 64 | 51 | 63 |
| Papers beside the bunk | 60 | 45 | 60 |

On another cold start, the final release build measured 50 on the wall
and 45 on the papers (medians over 16 frames). Brightness varies between
starts, which is why the traversal comparison used one run.

A second reference measured 60 on the wall and 36 on the papers after cropping
the black borders. Framing and exposure differ between the two references.
[Reference image](https://gta6data.com/gta-6-map-reference/)

At the blue table under the fluorescent lamps, a fixed-camera capture
showed a shadow on the floor behind the table base. With the two local
shadow masks forced to full visibility, that patch brightened from 46 to
59. Restoring the game's masks brought it back to 43. An exposed patch
on the same floor stayed between 51 and 53. The character also cast a
head-shaped shadow on the wall opposite the cell window.

## Known gaps

- The window wall is darker than the brighter reference.
- In some runs the back of the inmate shirt is black. A pixel trace at
  (480, 592) in the 1280 by 720 base-color buffer showed
  `PS_DeferredPedTexturedNC_b1a27fc2_Wrapped` writing RGBA (0, 0, 0, 5).
  In a run where the shirt appeared orange, the same area had RGB values
  around (209, 90, 49). Toggling the rays left the material values unchanged.
  The normal and material targets differ at the same pixels too, so every map
  of the shirt is wrong, not only its color. In one of ten later runs the
  shirt showed the diamond pattern of the window's fence instead of black:
  the shirt's textures, which it picks by index, showed another texture.
  Its table textures were the same as in good runs. Two causes were ruled
  out. With every decoration bake dropped, the shirt stayed orange and the
  heads went invisible. Refreshing placed textures uploaded before their
  streamed bytes arrived (`VICTORY_PAGE_WRITES=1`) did not stop it; those
  were the same three or four small textures in every run.
- Gameplay measurements cover the opening cell on one GPU.
- An access to a title page that has no host copy is lost (a read gives zero, a
  store is dropped).
- Not publicly documented, and assumed: the instruction's rounding, the order
  of two boxes entered at the same distance, and the result for a ray exactly
  through an edge.
- Cost on the GPU is unmeasured.

## Where to look

- `crates/victory-shader/src/bvh.rs`: the reference for the node test.
- `crates/victory-shader/src/emit/lower.rs`: the lowering of the two
  instructions and of memory by address (`Lower::memory`).
- `crates/victory-shader/src/resource.rs`: address tracking for descriptors
  and raw addresses.
- `crates/victory-gpu/src/pipeline.rs`, `draw.rs`: the builder rule and the
  host side of root views and title memory.
