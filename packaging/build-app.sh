#!/usr/bin/env bash
#
# build-app.sh — assemble, sign, and notarize DisplayStudio.app.
#
# The app bundles all three binaries and is self-contained: the menu bar GUI is
# the main executable and starts the embedded daemon itself, so there is no
# separate install step for displayd.
#
#   DisplayStudio.app/Contents/
#     Info.plist
#     MacOS/display-gui        <- main executable (menu bar app)
#     Helpers/displayd         <- daemon, spawned by the GUI on demand
#     Helpers/displayctl       <- CLI
#
# Environment: the same MACOS_* variables as build-pkg.sh (shared with
# ~/Projects/Phosphor/macos). See that script's header for the full list.
#
#   ./packaging/build-app.sh                 # signed if a Developer ID is present
#   ./packaging/build-app.sh --notarize      # signed + notarized + stapled
#   ./packaging/build-app.sh --dmg           # also produce a drag-install .dmg

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$PROJECT_DIR/target/app"
APP="$BUILD_DIR/DisplayStudio.app"

VERSION="$(grep -m1 '^version' "$PROJECT_DIR/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
[[ -n "$VERSION" ]] || { echo "error: no version in Cargo.toml"; exit 1; }

# Load signing / notarization credentials from .env (git-ignored). Routes the
# generic DEVELOPER_ID to whichever identity variable it actually is, so a .env
# holding either a Developer ID Application or Installer cert works.
if [[ -f "$PROJECT_DIR/.env" ]]; then
    set -a; source "$PROJECT_DIR/.env"; set +a
fi
if [[ -n "${DEVELOPER_ID:-}" ]]; then
    if [[ "$DEVELOPER_ID" == *Installer* && -z "${MACOS_INSTALLER_IDENTITY:-}" ]]; then
        export MACOS_INSTALLER_IDENTITY="$DEVELOPER_ID"
    elif [[ "$DEVELOPER_ID" == *Application* && -z "${MACOS_SIGN_IDENTITY:-}" ]]; then
        export MACOS_SIGN_IDENTITY="$DEVELOPER_ID"
    fi
fi

SIGN_IDENTITY="${MACOS_SIGN_IDENTITY:-}"
NOTARY_PROFILE="${MACOS_NOTARY_PROFILE:-}"
NOTARIZE=false
MAKE_DMG=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign)           SIGN_IDENTITY="$2"; shift 2 ;;
        --notary-profile) NOTARY_PROFILE="$2"; shift 2 ;;
        --notarize)       NOTARIZE=true; shift ;;
        --dmg)            MAKE_DMG=true; shift ;;
        -h|--help)        sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1"; exit 1 ;;
    esac
done

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*"; }

if [[ -z "$SIGN_IDENTITY" ]]; then
    SIGN_IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
        | grep 'Developer ID Application' | head -1 | sed -E 's/.*"([^"]+)".*/\1/')" || true
    [[ -n "$SIGN_IDENTITY" ]] && log "auto-detected: $SIGN_IDENTITY"
fi
[[ -z "$SIGN_IDENTITY" ]] && warn "no Developer ID Application identity — UNSIGNED build."
if $NOTARIZE && [[ -z "$SIGN_IDENTITY" ]]; then
    echo "error: --notarize needs a signing identity."; exit 1
fi

notarize_submit() {
    local path="$1"
    if [[ -n "$NOTARY_PROFILE" ]]; then
        xcrun notarytool submit "$path" --keychain-profile "$NOTARY_PROFILE" --wait
    else
        xcrun notarytool submit "$path" \
            --apple-id "${MACOS_APPLE_ID:?Set MACOS_NOTARY_PROFILE or MACOS_APPLE_ID}" \
            --team-id "${MACOS_TEAM_ID:?Set MACOS_NOTARY_PROFILE or MACOS_TEAM_ID}" \
            --password "${MACOS_APP_PASSWORD:?Set MACOS_NOTARY_PROFILE or MACOS_APP_PASSWORD}" \
            --wait
    fi
}

# ── Build universal binaries ─────────────────────────────────────────────────
log "building v$VERSION (arm64 + x86_64) …"
rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null 2>&1 || true
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --target "$target" \
        -p display-gui -p display-daemon -p displayctl
done

# ── Assemble the bundle ──────────────────────────────────────────────────────
log "assembling $APP …"
rm -rf "$BUILD_DIR"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Helpers" "$APP/Contents/Resources"

lipo_into() {
    # $1 = binary name, $2 = destination dir
    lipo -create \
        "$PROJECT_DIR/target/aarch64-apple-darwin/release/$1" \
        "$PROJECT_DIR/target/x86_64-apple-darwin/release/$1" \
        -output "$2/$1"
}
lipo_into display-gui "$APP/Contents/MacOS"
lipo_into displayd    "$APP/Contents/Helpers"
lipo_into displayctl  "$APP/Contents/Helpers"

sed "s/@VERSION@/$VERSION/g" "$SCRIPT_DIR/Info.plist.template" > "$APP/Contents/Info.plist"

# App icon: generate an .icns from the 1024px master in assets/.
ICON_MASTER="$PROJECT_DIR/assets/icon.png"
if [[ -f "$ICON_MASTER" ]]; then
    log "generating AppIcon.icns …"
    ICONSET="$BUILD_DIR/AppIcon.iconset"
    rm -rf "$ICONSET"; mkdir -p "$ICONSET"
    for sz in 16 32 128 256 512; do
        sips -z "$sz" "$sz" "$ICON_MASTER" \
            --out "$ICONSET/icon_${sz}x${sz}.png" >/dev/null
        sips -z "$((sz * 2))" "$((sz * 2))" "$ICON_MASTER" \
            --out "$ICONSET/icon_${sz}x${sz}@2x.png" >/dev/null
    done
    iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
    rm -rf "$ICONSET"
else
    warn "no assets/icon — bundle will have no app icon."
fi

# ── Sign inside-out (helpers, then main exe, then bundle) ─────────────────────
if [[ -n "$SIGN_IDENTITY" ]]; then
    ENT="$SCRIPT_DIR/DisplayStudio.entitlements"
    sign() {
        codesign --force --timestamp --options runtime \
            --entitlements "$ENT" --sign "$SIGN_IDENTITY" "$1"
    }
    log "signing (inside-out) …"
    sign "$APP/Contents/Helpers/displayd"
    sign "$APP/Contents/Helpers/displayctl"
    sign "$APP/Contents/MacOS/display-gui"
    sign "$APP"
    codesign --verify --deep --strict --verbose=2 "$APP"

    if $NOTARIZE; then
        log "notarizing …"
        ZIP="$BUILD_DIR/DisplayStudio.zip"
        ditto -c -k --keepParent "$APP" "$ZIP"
        notarize_submit "$ZIP"
        # Staple the ticket onto the bundle that ships, not just the zip.
        xcrun stapler staple "$APP"
        xcrun stapler validate "$APP"
        rm -f "$ZIP"
    fi
fi

# ── Optional drag-install DMG ────────────────────────────────────────────────
if $MAKE_DMG; then
    log "building DMG …"
    DMG="$BUILD_DIR/DisplayStudio-$VERSION.dmg"
    STAGE="$BUILD_DIR/dmg"
    rm -rf "$STAGE"; mkdir -p "$STAGE"
    cp -R "$APP" "$STAGE/"
    ln -s /Applications "$STAGE/Applications"
    hdiutil create -volname "Display Studio" -srcfolder "$STAGE" \
        -ov -format UDZO "$DMG" >/dev/null
    [[ -n "$SIGN_IDENTITY" ]] && codesign --force --sign "$SIGN_IDENTITY" "$DMG"
    $NOTARIZE && { notarize_submit "$DMG"; xcrun stapler staple "$DMG"; }
    log "DMG → $DMG"
fi

log "done → $APP"
[[ -n "$SIGN_IDENTITY" ]] || warn "UNSIGNED dev build."
