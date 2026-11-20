#!/usr/bin/env bash
# Build distribution packages from an already compiled release binary.
#
# Usage: packaging/build.sh [--binary PATH] [--out DIR] [--formats LIST]
#   --formats accepts a comma-separated subset of: tarball,deb,rpm,appimage
#
# Requires: dpkg-deb for .deb, rpmbuild for .rpm, appimagetool on PATH or
# APPIMAGETOOL for the AppImage. The tarball needs only tar.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
[ -n "$VERSION" ] || { echo "cannot read version from Cargo.toml" >&2; exit 1; }

BINARY="$ROOT/target/release/pw-matrix-router"
OUT="$ROOT/dist"
FORMATS="tarball,deb,rpm,appimage"
HOST_ARCH=$(uname -m)

while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BINARY=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --formats) FORMATS=$2; shift 2 ;;
    -h|--help) sed -n '2,9p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -x "$BINARY" ] || { echo "missing release binary: $BINARY" >&2; exit 1; }

ICON="$ROOT/packaging/icons"
DESKTOP="$ROOT/packaging/io.github.masfana.PwMatrixRouter.desktop"
METAINFO="$ROOT/packaging/io.github.masfana.PwMatrixRouter.metainfo.xml"
MAINTAINER="MasFana <MasFana@users.noreply.github.com>"
HOMEPAGE="https://github.com/MasFana/pw-matrix-router"

mkdir -p "$OUT"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

wants() { case ",$FORMATS," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

install_data() {
  # $1 = target prefix
  local prefix=$1 size
  install -Dm755 "$BINARY" "$prefix/usr/bin/pw-matrix-router"
  install -Dm644 "$DESKTOP" "$prefix/usr/share/applications/io.github.masfana.PwMatrixRouter.desktop"
  install -Dm644 "$METAINFO" "$prefix/usr/share/metainfo/io.github.masfana.PwMatrixRouter.metainfo.xml"
  for size in 64 128 256 512; do
    install -Dm644 "$ICON/pw-matrix-router-$size.png" \
      "$prefix/usr/share/icons/hicolor/${size}x${size}/apps/pw-matrix-router.png"
  done
}

package_tarball() {
  local name="pw-matrix-router-${VERSION}-${HOST_ARCH}-linux"
  local stage="$WORK/$name"
  mkdir -p "$stage"
  cp "$BINARY" "$stage/"
  cp "$ROOT/README.md" "$ROOT/LICENSE" "$ROOT/CHANGELOG.md" "$stage/"
  cp -r "$ROOT/docs" "$stage/"
  install -Dm644 "$DESKTOP" "$stage/share/applications/io.github.masfana.PwMatrixRouter.desktop"
  tar -C "$WORK" -czf "$OUT/$name.tar.gz" "$name"
  echo "built $OUT/$name.tar.gz"
}

package_deb() {
  command -v dpkg-deb >/dev/null || { echo "dpkg-deb not found" >&2; exit 1; }
  local name="pw-matrix-router_${VERSION}_amd64.deb"
  local stage="$WORK/deb"
  install_data "$stage"
  mkdir -p "$stage/DEBIAN"
  cat > "$stage/DEBIAN/control" <<EOF
Package: pw-matrix-router
Version: ${VERSION}
Section: sound
Priority: optional
Architecture: amd64
Maintainer: ${MAINTAINER}
Homepage: ${HOMEPAGE}
Depends: libc6, libgcc-s1, libxkbcommon0, libxkbcommon-x11-0, libxcb1, libxcb-xkb1, libxau6, libxdmcp6
Recommends: pipewire, pipewire-pulse, pulseaudio-utils, libvulkan1
Description: PipeWire multi-output audio router
 Captures one 48 kHz stereo virtual sink and renders it to every enabled
 output at that output's own sample rate. Each output has independent
 volume, mute, and extra delay. Includes a native GPUI window with an
 output table, stereo meters, a spectrum, and a balance bar.
EOF
  dpkg-deb --root-owner-group --build "$stage" "$OUT/$name" >/dev/null
  echo "built $OUT/$name"
}

package_rpm() {
  command -v rpmbuild >/dev/null || { echo "rpmbuild not found" >&2; exit 1; }
  local top="$WORK/rpm"
  local name="pw-matrix-router-${VERSION}-1.${HOST_ARCH}.rpm"
  mkdir -p "$top"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
  install_data "$top/BUILD/root"
  cat > "$top/SPECS/pw-matrix-router.spec" <<EOF
Name: pw-matrix-router
Version: ${VERSION}
Release: 1
Summary: PipeWire multi-output audio router
License: MIT
URL: ${HOMEPAGE}
BuildArch: ${HOST_ARCH}
Requires: libxkbcommon, libxkbcommon-x11, libxcb, libXau, libXdmcp
Recommends: pipewire, pipewire-pulse, pulseaudio-utils

%description
Captures one 48 kHz stereo virtual sink and renders it to every enabled
output at that output's own sample rate. Each output has independent
volume, mute, and extra delay.

%install
mkdir -p %{buildroot}
cp -a ${top}/BUILD/root/. %{buildroot}/

%files
/usr/bin/pw-matrix-router
/usr/share/applications/io.github.masfana.PwMatrixRouter.desktop
/usr/share/metainfo/io.github.masfana.PwMatrixRouter.metainfo.xml
/usr/share/icons/hicolor/64x64/apps/pw-matrix-router.png
/usr/share/icons/hicolor/128x128/apps/pw-matrix-router.png
/usr/share/icons/hicolor/256x256/apps/pw-matrix-router.png
/usr/share/icons/hicolor/512x512/apps/pw-matrix-router.png
EOF
  rpmbuild -bb --quiet \
    --define "_topdir $top" \
    --define "_build_id_links none" \
    "$top/SPECS/pw-matrix-router.spec" >/dev/null
  cp "$top"/RPMS/*/pw-matrix-router-*.rpm "$OUT/$name"
  echo "built $OUT/$name"
}

package_appimage() {
  local tool=${APPIMAGETOOL:-$(command -v appimagetool || true)}
  [ -n "$tool" ] || { echo "appimagetool not found; set APPIMAGETOOL" >&2; exit 1; }
  local appdir="$WORK/AppDir"
  install_data "$appdir"
  cp "$DESKTOP" "$appdir/io.github.masfana.PwMatrixRouter.desktop"
  cp "$ICON/pw-matrix-router-512.png" "$appdir/pw-matrix-router.png"
  cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
HERE=$(dirname "$(readlink -f "$0")")
exec "$HERE/usr/bin/pw-matrix-router" "$@"
EOF
  chmod +x "$appdir/AppRun"
  local name="pw-matrix-router-${VERSION}-${HOST_ARCH}.AppImage"
  ARCH="$HOST_ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$tool" "$appdir" "$OUT/$name" >/dev/null
  chmod +x "$OUT/$name"
  echo "built $OUT/$name"
}

wants tarball && package_tarball
wants deb && package_deb
wants rpm && package_rpm
wants appimage && package_appimage

shopt -s nullglob
artifacts=("$OUT"/*.deb "$OUT"/*.rpm "$OUT"/*.AppImage "$OUT"/*.tar.gz)
if [ ${#artifacts[@]} -gt 0 ]; then
  (cd "$OUT" && sha256sum "${artifacts[@]##*/}" > sha256sums.txt)
  echo "checksums written to $OUT/sha256sums.txt"
fi