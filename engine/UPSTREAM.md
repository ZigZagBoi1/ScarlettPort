# Upstream

Started as a fork of VIctory (GPL-3.0) by Marvisak and contributors (https://github.com/Marvisak/VIctory), taken through fxceless's fork (https://github.com/fxceless/victory-fork).

- **Forked from:** fxceless's fork, `main` at `4e17478b9c27bb530643e5b0e16a767cf4419e08` (2026-11-24),
  whose most recent commit from the original project is `ffe310eb2098e52169f4c44db5ffdabf43a72622`.
- **Licence:** GPL-3.0-or-later, unchanged. `LICENSE` in this folder is the upstream licence text;
  ScarlettPort's changes are distributed under the same licence. Third-party notices are in `THIRD_PARTY_LICENSES/`.
- **Credits:** the full lineage, the forks and pull requests merged in, and their authors are in
  [`../CREDITS.md`](../CREDITS.md) and [`CREDITS.md`](CREDITS.md).

## How upstream changes are merged

This folder is the source of truth; it is not a patch stack on top of another tree. Changes from the
original project, fxceless's fork or other forks are taken by hand: each is reviewed, cherry-picked or
ported into `engine/` through a pull request, keeping the original author on the commit where it applies
as is and naming the source commit or pull request in the message and in `CREDITS.md`. Changes that
conflict with work here are left out and listed there as not taken. Upstream names are rewritten to
ScarlettPort's on the way in (crates `scarlett-*`, switches `SCARLETT_*`, executable `scarlett.exe`).
