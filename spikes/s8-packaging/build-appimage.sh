#!/usr/bin/env bash
# Assembles AppDir/ from the host-built hello_gpui release binary and runs
# appimagetool over it. Run build-hello-gpui.sh first.
#
# No linuxdeploy in this environment (checked: not installed, only
# appimagetool via the appimagetool-bin AUR package), so this assembles the
# AppDir by hand and does NOT bundle shared libraries. The AppImage relies on
# the host providing libxcb/libxkbcommon/libwayland-client -- see S-8.md for
# why that's a reasonable bar (those are low-level X11/Wayland client libs
# present on virtually any Linux desktop) versus GTK/Qt/KDE (which gpui does
# not link against at all -- verified with ldd + strings).
set -euo pipefail
cd "$(dirname "$0")"

BIN=hello-gpui/target/release/hello_gpui
if [[ ! -x "$BIN" ]]; then
  echo "error: $BIN not found; run build-hello-gpui.sh first" >&2
  exit 1
fi

rm -rf AppDir
mkdir -p AppDir/usr/bin AppDir/usr/share/applications AppDir/usr/share/icons/hicolor/256x256/apps

cp "$BIN" AppDir/usr/bin/hello_gpui
cp org.duet.HelloGpui.desktop AppDir/usr/share/applications/org.duet.HelloGpui.desktop
cp org.duet.HelloGpui.desktop AppDir/org.duet.HelloGpui.desktop
cp org.duet.HelloGpui.png AppDir/usr/share/icons/hicolor/256x256/apps/org.duet.HelloGpui.png
cp org.duet.HelloGpui.png AppDir/org.duet.HelloGpui.png
ln -sf org.duet.HelloGpui.png AppDir/.DirIcon

cat > AppDir/AppRun <<'EOF'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "${0}")")"
exec "${HERE}/usr/bin/hello_gpui" "$@"
EOF
chmod +x AppDir/AppRun

ARCH=x86_64 appimagetool AppDir HelloGpui-x86_64.AppImage
ls -la HelloGpui-x86_64.AppImage
