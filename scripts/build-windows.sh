#!/bin/sh
# Windows build (cross-compiled on Linux): dist/LRScope1008C-windows-<version>.zip with the stripped .exe and a readme.
# Needs: rustup target x86_64-pc-windows-gnu, and the MinGW toolchain (sudo apt install mingw-w64).
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
T=x86_64-pc-windows-gnu
command -v x86_64-w64-mingw32-gcc >/dev/null || { echo "MinGW missing: sudo apt install mingw-w64"; exit 1; }
rustup target list --installed | grep -q "$T" || rustup target add "$T"
cargo build --release --target "$T"
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT=dist/LRScope1008C-windows-$VER
rm -rf "$OUT" "$OUT.zip"
mkdir -p "$OUT"
x86_64-w64-mingw32-strip -o "$OUT/LRScope1008C.exe" "target/$T/release/hantek1008c.exe"
cat > "$OUT/README-Windows.txt" <<'EOF'
LR Scope 1008C - automotive scope software for the Hantek 1008C, by Light Racing (Windows build, cross-compiled; not yet tested on Windows)
Hantek and 1008C are trademarks of Qingdao Hantek Electronic Co., Ltd. This project is independent and not affiliated with Hantek.

1. One-time USB driver: plug in the scope, run Zadig (https://zadig.akeo.ie), Options > List All Devices,
   pick the Hantek 1008C (USB ID 0783:5725), driver "WinUSB", Install Driver.
   (The Hantek software uses its own driver; after this it no longer sees the scope until the driver is switched back.)
2. Start LRScope1008C.exe. Without a scope: LRScope1008C.exe --sim (simulator).
3. Settings: %APPDATA%\hantek1008c   Recordings / exports / references: Documents\hantek1008c\recordings
4. Command line (from cmd or PowerShell): LRScope1008C.exe check   (hardware self-test, CH1 on the 1 kHz generator)
EOF
python3 -I -c "import shutil,sys; shutil.make_archive(sys.argv[1], 'zip', 'dist', sys.argv[2])" "$OUT" "$(basename "$OUT")"
ls -la "$OUT.zip"
