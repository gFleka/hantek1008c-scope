# Third-party licenses

LR Scope 1008C is built from Rust crates listed in `Cargo.lock` (full list with versions: `cargo tree`). They are
published under permissive licenses (MIT, Apache-2.0, BSD, Zlib and similar); their license texts are in each crate's
source on <https://crates.io>.

## libusb (LGPL-2.1-or-later) — linked statically

The `rusb` crate is used with its `vendored` feature, which compiles **libusb 1.0.27** (from the crate
`libusb1-sys 0.7.0`) into the program. libusb is licensed under the GNU Lesser General Public License, version 2.1 or
later — full text in [licenses/libusb-COPYING](licenses/libusb-COPYING).

- libusb source: <https://github.com/libusb/libusb/releases/tag/v1.0.27> (also inside the `libusb1-sys` crate).
- As required by the LGPL, you may modify libusb and relink: the complete source of this program is in this
  repository; build it against your modified libusb (e.g. a system libusb, by turning off the `vendored` feature in
  `Cargo.toml`) with `cargo build --release`.

## hantek1008py (Apache-2.0)

The USB protocol implementation started as a port of <https://github.com/mfg92/hantek1008py> — see [NOTICE](NOTICE).
