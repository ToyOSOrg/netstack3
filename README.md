# netstack3 — ToyOS's packaging mirror of Fuchsia's Netstack3 core

Not a fork. `upstream/` holds Google's files byte for byte from the Fuchsia
commit `upstream.toml` names — each crate directory's `BUILD.gn` and `src/`,
plus Fuchsia's `LICENSE` (BSD-2-Clause) and `PATENTS` (Google's patent grant)
— and nothing under it is ever edited. What is ours sits beside it:

- `crates/`: a Cargo manifest per crate, each pointing its library at the
  upstream file, and `fidl_fuchsia_net_common`, an empty stand-in for the one
  FIDL-generated crate `net-declare` re-exports;
- `tools/mirror`: `cargo run -p netstack3-mirror -- check` holds `upstream/`
  to the pinned git blob ids offline, and `-- sync <fuchsia commit>` is the
  only writer of `upstream/` and `upstream.toml`;
- `.github/workflows/identity.yml`: the check, on every push.

ToyOS (`ToyOSOrg/ToyOS`) consumes the crates as git dependencies on branch
`toyos`, its lockfile pinning the commit, and records this repository in its
`forks.toml`. The code needs `never_type` and `-Zpolonius=next`, which ToyOS
passes to these crates alone.

Our files are MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`); Google's are
under `upstream/LICENSE` and `upstream/PATENTS`.
