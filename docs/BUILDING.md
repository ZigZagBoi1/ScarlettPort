# Building

Windows 10 22H2 or Windows 11 x64 only. Neither runtime builds on macOS or Linux.

## Prerequisites

- Rust via [rustup](https://rustup.rs), MSVC target (`stable-x86_64-pc-windows-msvc`). `runtime/victory` pins its own toolchain in `rust-toolchain.toml`; rustup installs it on first use.
- Visual Studio Build Tools with the "Desktop development with C++" workload and a Windows 10/11 SDK.
- PowerShell 5.1 or later for `scripts/*.ps1`.
- A GPU and driver with Shader Model 6.6 and mesh shader support to run (not needed to build).

Commands below are run from the repository root.

## XCL

```powershell
cd runtime\xcl
cargo build --release -p xcl --locked
cargo test --release -p xcl --lib --locked -- --test-threads=4
```

GPU tests print `SKIPPED <test>: <reason>` and pass when the adapter cannot run them. Or use `.\scripts\setup.ps1`, which also fetches the Agility SDK. More notes: `runtime/xcl/AGENTS.md`.

## VIctory stack

```powershell
cd runtime\victory
cargo rustc --release -p victory --bin victory --features diagnostics --locked -- -C link-arg=/EXPORT:D3D12SDKVersion,DATA -C link-arg=/EXPORT:D3D12SDKPath,DATA
```

The two `/EXPORT` flags are required so Windows loads the Agility SDK placed next to the executable. `.\scripts\setup-victory.ps1` runs this command and does the Agility SDK step.

Tests that do not need a GPU: `cargo test --release --locked --lib -p victory-core -p victory-formats -p victory-pe -p victory-loader -p victory-shader -p victory-pipeline`. Tests in `victory-gpu` and `victory-hle` need a Shader Model 6.6 device.

## Agility SDK

Windows 10 needs the D3D12 Agility SDK (1.614.1) next to the executable, in a `D3D12` folder containing `D3D12Core.dll` and `d3d12SDKLayers.dll`. `setup-victory.ps1` downloads it from NuGet (`Microsoft.Direct3D.D3D12`) into `runtime\victory\target\release\D3D12`. To do it by hand, download the package, unzip it, and copy both DLLs from `build\native\bin\x64`. Do not commit these DLLs.

## Launcher

The launcher embeds the XCL runtime (`xcl.exe`, Agility SDK DLLs, `dxcompiler.dll`, `dxil.dll`), so build XCL first.

```powershell
cd launcher
cargo build --release --locked
cargo test --release --locked
```

`build.rs` copies the runtime files into `launcher\runtime` from `XCL_SRC` (a folder holding `xcl.exe` and `D3D12\`) and `DXC_SRC` (a folder holding `dxcompiler.dll` and `dxil.dll`, for example the Windows SDK `bin\<version>\x64`). Files already in `launcher\runtime` are used as is.

To build and test the launcher without any runtime files (CI does this), set `SCARLETT_NO_EMBED=1`. It embeds empty placeholders; the result builds but cannot run a game.

## Where things live

| What | Where |
|---|---|
| Build output | `runtime\xcl\target`, `runtime\victory\target`, `launcher\target` |
| Launcher unpacked runtime | `%LOCALAPPDATA%\ScarlettPort\runtime` |
| Launcher settings and default cache | `%LOCALAPPDATA%\ScarlettPort`, cache on the fixed drive with the most free space (`<drive>\ScarlettCache`) |
| DXIL shader cache | `<cache>\dxil` |
| XCL title data, pipeline journal | `%XCL_DATA_ROOT%` or `%LOCALAPPDATA%\XCL`, then the package name; `cache\pipelines.bin` inside it |

Caches are safe to delete; they are rebuilt on the next run (slowly: first-run shader compiles stall the world).
