#!/usr/bin/env bash
# Build, sign and (optionally) notarize the macOS release artifacts:
#
#   $DIST/filmcraft-<version>-<build>-macos-<arch>.dmg  drag-to-Applications DMG
#   $DIST/filmcraft-<version>-<build>-macos-<arch>.pkg  Installer package
#   $DIST/filmcraft-cli-<version>-<build>-macos-<arch>.zip  headless CLI
#
# Usage: packaging/macos/package.sh [--arch universal|aarch64|x86_64] [--skip-build]
#
# Signing (env):
#   MACOS_SIGN_IDENTITY   codesign identity (name or SHA-1). Default "-" = ad-hoc (local testing;
#                         Gatekeeper will reject the result on other Macs). In CI, import-cert.sh sets it.
#   MACOS_INSTALLER_IDENTITY  Developer ID Installer identity for the PKG (optional)
#   MACOS_KEYCHAIN        keychain holding the identity (optional)
# Notarization (env; all three needed, and a real identity):
#   APPLE_ID, APPLE_PASSWORD (app-specific password), APPLE_TEAM_ID
#   Or MACOS_NOTARY_PROFILE (notarytool Keychain profile), MACOS_NOTARY_KEYCHAIN (optional)
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/macos"

ARCH=universal
SKIP_BUILD=0
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) ARCH="$2"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h | --help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$ARCH" in
  universal) TARGETS=(aarch64-apple-darwin x86_64-apple-darwin) ;;
  aarch64) TARGETS=(aarch64-apple-darwin) ;;
  x86_64) TARGETS=(x86_64-apple-darwin) ;;
  *) echo "unknown --arch $ARCH" >&2; exit 2 ;;
esac

# Keep in sync with LSMinimumSystemVersion in Info.plist.in.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
# Native single-architecture builds share Cargo's ordinary release output (including proc macros).
NATIVE=0
HOST="$(rustc -vV | sed -n 's/^host: //p')"
if [ "${#TARGETS[@]}" = 1 ] && [ "${TARGETS[0]}" = "$HOST" ]; then NATIVE=1; fi
IDENTITY="${MACOS_SIGN_IDENTITY:--}"
SHORT_VERSION="${VERSION%%-*}"
# File Provider/iCloud can add FinderInfo after signing .app directories in Documents.
# Keep the signing tree outside synchronized folders; final archive files still go to DIST.
WORK="${MACOS_PACKAGE_WORK:-$HOME/Library/Caches/FilmCraftPackaging/$FILMCRAFT_BUILD_NUMBER-$ARCH}"
APP="$WORK/FilmCraft.app"
DMG="$DIST/filmcraft-$VERSION-$FILMCRAFT_BUILD_NUMBER-macos-$ARCH.dmg"
CLI_ZIP="$DIST/filmcraft-cli-$VERSION-$FILMCRAFT_BUILD_NUMBER-macos-$ARCH.zip"
PKG="$DIST/filmcraft-$VERSION-$FILMCRAFT_BUILD_NUMBER-macos-$ARCH.pkg"

NOTARIZE=0
NOTARY_AUTH=()
if [ "$IDENTITY" = "-" ]; then
  warn "macOS: ad-hoc signing (no MACOS_SIGN_IDENTITY); artifacts are not notarized"
elif [ -n "${MACOS_NOTARY_PROFILE:-}" ]; then
  NOTARY_AUTH=(--keychain-profile "$MACOS_NOTARY_PROFILE")
  if [ -n "${MACOS_NOTARY_KEYCHAIN:-}" ]; then NOTARY_AUTH+=(--keychain "$MACOS_NOTARY_KEYCHAIN"); fi
  NOTARIZE=1
elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
  NOTARY_AUTH=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
  NOTARIZE=1
else
  warn "macOS: APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID incomplete; signed but not notarized"
fi

if [ "${MACOS_REQUIRE_NOTARIZATION:-0}" = 1 ] && { [ "$NOTARIZE" != 1 ] || [ -z "${MACOS_INSTALLER_IDENTITY:-}" ]; }; then
  echo "error: public release requires Developer ID Application/Installer identities and notarization credentials" >&2
  exit 1
fi

if [ "$NOTARIZE" = 1 ]; then
  # Fail before rebuilding or replacing any artifact if the profile is absent/invalid.
  xcrun notarytool history "${NOTARY_AUTH[@]}" --output-format json >/dev/null
fi

echo "==> FilmCraft $VERSION for macOS ($ARCH), identity: $IDENTITY, notarize: $NOTARIZE"

# ---- build -------------------------------------------------------------------------------------
if [ "$SKIP_BUILD" = 0 ]; then
  args=()
  if [ "$NATIVE" = 0 ]; then
    for t in "${TARGETS[@]}"; do args+=(--target "$t"); done
  fi
  (cd "$ROOT" && cargo build --release --locked -j1 -p filmcraft -p filmcraft-cli --features whisper,macos-voices ${args[@]+"${args[@]}"})
fi

rm -rf "$WORK"
mkdir -p "$WORK/bin"
for bin in filmcraft filmcraft-cli; do
  inputs=()
  if [ "$NATIVE" = 1 ]; then
    inputs+=("$CARGO_TARGET_DIR/release/$bin")
  else
    for t in "${TARGETS[@]}"; do inputs+=("$CARGO_TARGET_DIR/$t/release/$bin"); done
  fi
  lipo -create -output "$WORK/bin/$bin" "${inputs[@]}"
  lipo -info "$WORK/bin/$bin"
done
# Refuse to relabel a stale/debug executable when using --skip-build.
EXPECTED="FilmCraft $SHORT_VERSION · release · Build $FILMCRAFT_BUILD_NUMBER ($FILMCRAFT_BUILD_DATE, $FILMCRAFT_BUILD_SHA)"
ACTUAL="$("$WORK/bin/filmcraft" --version)"
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "error: binary identity mismatch: $ACTUAL (expected $EXPECTED)" >&2
  exit 1
fi

# ---- signing helpers ---------------------------------------------------------------------------
sign() {
  # Hardened runtime + secure timestamp for a real identity; ad-hoc can't be timestamped.
  local ts=(--timestamp)
  [ "$IDENTITY" = "-" ] && ts=(--timestamp=none)
  local kc=()
  [ -n "${MACOS_KEYCHAIN:-}" ] && kc=(--keychain "$MACOS_KEYCHAIN")
  codesign --force --sign "$IDENTITY" ${kc[@]+"${kc[@]}"} "${ts[@]}" "$@"
}

notarize() {
  local file="$1" out id status
  echo "==> notarizing $(basename "$file") (this can take a few minutes)"
  out="$(xcrun notarytool submit "$file" "${NOTARY_AUTH[@]}" --wait --timeout 1h --output-format json)" || true
  echo "$out"
  id="$(printf '%s' "$out" | plutil -extract id raw -o - - 2>/dev/null || true)"
  status="$(printf '%s' "$out" | plutil -extract status raw -o - - 2>/dev/null || true)"
  if [ "$status" != "Accepted" ]; then
    if [ -n "$id" ]; then
      xcrun notarytool log "$id" "${NOTARY_AUTH[@]}" || true
    fi
    echo "error: notarization of $(basename "$file") failed (status: ${status:-unknown})" >&2
    exit 1
  fi
}

# ---- FilmCraft.app ----------------------------------------------------------------------------
echo "==> assembling $APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# Executable and icon carry the display name (CFBundleExecutable / CFBundleIconFile).
cp "$WORK/bin/filmcraft" "$APP/Contents/MacOS/FilmCraft"
cp "$ROOT/assets/app-icon/filmcraft.icns" "$APP/Contents/Resources/FilmCraft.icns"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@SHORT_VERSION@/$SHORT_VERSION/g" \
  -e "s/@BUILD_NUMBER@/$FILMCRAFT_BUILD_NUMBER/g" \
  -e "s/@BUILD_DATE@/$FILMCRAFT_BUILD_DATE/g" \
  -e "s/@MIN_OS@/$MACOSX_DEPLOYMENT_TARGET/g" \
  -e "s/@BUILD_SHA@/${FILMCRAFT_BUILD_SHA:-unknown}/g" \
  "$HERE/Info.plist.in" >"$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"
printf 'APPL????' >"$APP/Contents/PkgInfo"
# licences of the embedded craft-fonts fonts (builds with CRAFT_FONTS_DIR)
copy_font_licences "$APP/Contents/Resources"
copy_docs "$APP/Contents/Resources"
cp "$ROOT/ATTRIBUTION.md" "$APP/Contents/Resources/"
cp "$ROOT/assets/app-icon/LICENSE.txt" "$APP/Contents/Resources/LICENSE-app-icon.txt"
cp "$ROOT"/assets/fonts/OFL-*.txt "$APP/Contents/Resources/"
sed -e 's/asset:        filmcraft.icns/asset:        FilmCraft.icns/' \
  -e 's|license-file: assets/app-icon/LICENSE.txt|license-file: LICENSE-app-icon.txt|' \
  "$ROOT/assets/app-icon/filmcraft.icns.attribution" >"$APP/Contents/Resources/FilmCraft.icns.attribution"

# Sign inside-out: nested code first, then the bundle itself (no --deep on the final signature).
# Today the only nested code is the main executable; frameworks/helpers would be signed here too.
sign --options runtime --entitlements "$HERE/entitlements.plist" "$APP/Contents/MacOS/FilmCraft"
sign --options runtime --entitlements "$HERE/entitlements.plist" "$APP"
codesign --verify --strict --deep --verbose=2 "$APP"

if [ "$NOTARIZE" = 1 ]; then
  ditto -c -k --keepParent "$APP" "$WORK/FilmCraft-notarize.zip"
  notarize "$WORK/FilmCraft-notarize.zip"
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  spctl --assess --type execute -vvv "$APP"
fi

# ---- DMG ---------------------------------------------------------------------------------------
echo "==> building $DMG"
STAGE="$WORK/dmg"
mkdir -p "$STAGE"
ditto "$APP" "$STAGE/FilmCraft.app"
ln -s /Applications "$STAGE/Applications"
# Finder window layout: background, icon size and positions (packaging/macos/dmg/README.md).
mkdir -p "$STAGE/.background"
cp "$HERE/dmg/background.tiff" "$STAGE/.background/background.tiff"
sed 's|license-file: LICENSE-MIT, LICENSE-APACHE|license-file: ../.licenses/LICENSE-MIT, ../.licenses/LICENSE-APACHE|' \
  "$HERE/dmg/background.tiff.attribution" >"$STAGE/.background/background.tiff.attribution"
mkdir -p "$STAGE/.licenses"
copy_docs "$STAGE/.licenses"
cp "$ROOT/ATTRIBUTION.md" "$STAGE/.licenses/"
cp "$HERE/dmg/dmg-layout.DS_Store" "$STAGE/.DS_Store"
rm -f "$DMG"
# makehybrid's HFS catalog synthesizes FinderInfo on .app directories, invalidating their
# strict signatures even when the source bundle verifies. APFS preserves the signed bundle.
# Keep the volume name: .DS_Store resolves its background alias through it.
hdiutil create -fs APFS -volname "FilmCraft" -srcfolder "$STAGE" -format UDZO -ov "$DMG"
MOUNT="$WORK/dmg-check"
mkdir -p "$MOUNT"
hdiutil attach -readonly -nobrowse -mountpoint "$MOUNT" "$DMG"
if ! codesign --verify --strict --deep "$MOUNT/FilmCraft.app"; then
  hdiutil detach "$MOUNT" || true
  echo "error: DMG payload signature is invalid" >&2
  exit 1
fi
hdiutil detach "$MOUNT"
sign "$DMG"
codesign --verify --strict --verbose=2 "$DMG"
if [ "$NOTARIZE" = 1 ]; then
  notarize "$DMG"
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"
  spctl --assess --type open --context context:primary-signature -vvv "$DMG"
fi

# ---- Installer package -------------------------------------------------------------------------
# A component plist prevents Installer from relocating an existing QA/home-directory copy.
PKG_ROOT="$WORK/pkg-root"
mkdir -p "$PKG_ROOT/Applications"
ditto "$APP" "$PKG_ROOT/Applications/FilmCraft.app"
pkgbuild --analyze --root "$PKG_ROOT" "$WORK/components.plist"
if /usr/libexec/PlistBuddy -c 'Print :0:BundleIsRelocatable' "$WORK/components.plist" >/dev/null 2>&1; then
  /usr/libexec/PlistBuddy -c 'Set :0:BundleIsRelocatable false' "$WORK/components.plist"
else
  /usr/libexec/PlistBuddy -c 'Add :0:BundleIsRelocatable bool false' "$WORK/components.plist"
fi
# Explicit installer also replaces legacy local bundles with date-based build numbers/identifiers.
/usr/libexec/PlistBuddy -c 'Set :0:BundleIsVersionChecked false' "$WORK/components.plist"
/usr/libexec/PlistBuddy -c 'Set :0:BundleHasStrictIdentifier false' "$WORK/components.plist"
/usr/libexec/PlistBuddy -c 'Set :0:BundleOverwriteAction upgrade' "$WORK/components.plist"
mkdir -p "$WORK/installer-scripts"
cp "$HERE/installer-scripts/preinstall" "$WORK/installer-scripts/preinstall"
chmod 755 "$WORK/installer-scripts/preinstall"
pkgbuild --root "$PKG_ROOT" --component-plist "$WORK/components.plist" \
  --scripts "$WORK/installer-scripts" --identifier ai.storyteller.filmcraft \
  --version "$SHORT_VERSION.$FILMCRAFT_BUILD_NUMBER" --install-location / "$PKG"
if [ -n "${MACOS_INSTALLER_IDENTITY:-}" ]; then
  productsign --sign "$MACOS_INSTALLER_IDENTITY" "$PKG" "$WORK/signed.pkg"
  mv "$WORK/signed.pkg" "$PKG"
  if [ "$NOTARIZE" = 1 ]; then
    notarize "$PKG"
    xcrun stapler staple "$PKG"
    xcrun stapler validate "$PKG"
    spctl --assess --type install -vvv "$PKG"
  fi
else
  warn "macOS: PKG has no Developer ID Installer signature; external distribution is not trusted by Gatekeeper"
fi

# ---- CLI ---------------------------------------------------------------------------------------
echo "==> building $CLI_ZIP"
CLI_DIR="$WORK/filmcraft-cli-$VERSION-macos-$ARCH"
mkdir -p "$CLI_DIR"
cp "$WORK/bin/filmcraft-cli" "$CLI_DIR/"
copy_docs "$CLI_DIR"
sign --options runtime "$CLI_DIR/filmcraft-cli"
codesign --verify --strict --verbose=2 "$CLI_DIR/filmcraft-cli"
rm -f "$CLI_ZIP"
ditto -c -k --keepParent "$CLI_DIR" "$CLI_ZIP"
# A bare Mach-O can't carry a stapled ticket; Gatekeeper looks the notarization up online.
if [ "$NOTARIZE" = 1 ]; then notarize "$CLI_ZIP"; fi

"$WORK/bin/filmcraft-cli" --version
echo "==> done"
ls -lh "$DMG" "$PKG" "$CLI_ZIP"
