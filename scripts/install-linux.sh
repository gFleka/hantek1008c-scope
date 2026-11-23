#!/bin/sh
# Build and install LR Scope 1008C for the current user (no sudo): binary ~/.local/bin/lrscope (debug info
# stripped), icon, and a desktop entry so "LR Scope 1008C" shows up in the app menu and can be pinned to the dock /
# quick launch bar. Run again after updating to install the new build. Settings stay in ~/.config/hantek1008c.
set -e
cd "$(dirname "$0")/.."
PATH="$HOME/.cargo/bin:$PATH" cargo build --release
mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
install -s -m 755 target/release/hantek1008c "$HOME/.local/bin/lrscope"
# icon in every size for the theme; the launcher uses the 256 px file by full path, which GNOME always finds
# (a newly created ~/.local/share/icons/hicolor is often only picked up after the next login -> generic gear icon)
for s in 48 64 128 256 512; do
    mkdir -p "$HOME/.local/share/icons/hicolor/${s}x${s}/apps"
    install -m 644 "assets/icons/lrscope_$s.png" "$HOME/.local/share/icons/hicolor/${s}x${s}/apps/lrscope.png"
done
ICON="$HOME/.local/share/icons/hicolor/256x256/apps/lrscope.png"
cat > "$HOME/.local/share/applications/lrscope.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=LR Scope 1008C
GenericName=Oscilloscope
Comment=Automotive oscilloscope software for the Hantek 1008C, by Light Racing
Exec=$HOME/.local/bin/lrscope
Icon=$ICON
Terminal=false
Categories=Utility;Electronics;Engineering;
Keywords=scope;oscilloscope;hantek;1008c;automotive;light racing;
StartupWMClass=lrscope
Actions=simulator;

[Desktop Action simulator]
Name=Start with the simulator (no scope needed)
Exec=$HOME/.local/bin/lrscope --sim
DESKTOP
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
gtk-update-icon-cache -q "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "Installed $HOME/.local/bin/lrscope and the 'LR Scope 1008C' launcher."
echo "Pin it: open the app menu, right-click 'LR Scope 1008C' -> Pin to dash / Add to favorites."
