#!/usr/bin/env bash
#
# build-pkg.sh — build, sign, notarize, and package displayd + displayctl.
#
# Produces a universal (arm64 + x86_64), Developer ID-signed, notarized,
# stapled .pkg that installs the CLI + daemon and a per-user LaunchAgent.
#
# ── Environment (same names as Phosphor/macos, so one shell profile drives both)
#
#   Signing:
#     MACOS_SIGN_IDENTITY       "Developer ID Application: …" for the binaries.
#                               Overridden by --sign. Auto-detected if unset.
#     MACOS_INSTALLER_IDENTITY  "Developer ID Installer: …" for the .pkg.
#                               Overridden by --installer-sign. Auto-detected.
#
#   Notarization — prefer the keychain profile, fall back to the trio:
#     MACOS_NOTARY_PROFILE      `xcrun notarytool store-credentials` profile.
#     MACOS_TEAM_ID             Apple team id       ) used only when
#     MACOS_APPLE_ID            Apple ID email      ) NOTARY_PROFILE
#     MACOS_APP_PASSWORD        app-specific pass   ) is unset.
#
#   Optional:
#     BUNDLE_ID                 LaunchAgent label. Default io.github.displaymanager.daemon.
#     INSTALL_PREFIX            Binary install dir. Default /usr/local/bin.
#
# Unsigned dev build (no identity, no notarization) if nothing is set — useful
# for testing the pkg layout without credentials.
#
#   ./packaging/build-pkg.sh
#   ./packaging/build-pkg.sh --sign "Developer ID Application: Foo (TEAMID)"
#   MACOS_NOTARY_PROFILE=display ./packaging/build-pkg.sh --notarize

set -euo pipefail

# ── Locations ────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$PROJECT_DIR/target/pkg"
STAGE_DIR="$BUILD_DIR/root"

BUNDLE_ID="${BUNDLE_ID:-io.github.displaymanager.daemon}"
INSTALL_PREFIX="${INSTALL_PREFIX:-/usr/local/bin}"
BINARIES=(displayd displayctl)

VERSION="$(
    grep -m1 '^version' "$PROJECT_DIR/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/'
)"
[[ -n "$VERSION" ]] || { echo "error: could not read version from Cargo.toml"; exit 1; }

# ── Load .env (git-ignored) and route DEVELOPER_ID to the right identity ──────
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

# ── Args + env resolution ────────────────────────────────────────────────────
SIGN_IDENTITY="${MACOS_SIGN_IDENTITY:-}"
INSTALLER_IDENTITY="${MACOS_INSTALLER_IDENTITY:-}"
NOTARY_PROFILE="${MACOS_NOTARY_PROFILE:-}"
NOTARIZE=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign)            SIGN_IDENTITY="$2";      shift 2 ;;
        --installer-sign)  INSTALLER_IDENTITY="$2"; shift 2 ;;
        --notary-profile)  NOTARY_PROFILE="$2";     shift 2 ;;
        --notarize)        NOTARIZE=true;           shift ;;
        -h|--help)         sed -n '2,40p' "$0";     exit 0 ;;
        *) echo "unknown arg: $1"; exit 1 ;;
    esac
done

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*"; }

# ── Auto-detect signing identities when not provided ─────────────────────────
detect_identity() {
    # $1 = identity prefix to grep for in the login keychain.
    security find-identity -v -p codesigning 2>/dev/null \
        | grep "$1" | head -1 | sed -E 's/.*"([^"]+)".*/\1/'
}

if [[ -z "$SIGN_IDENTITY" ]]; then
    SIGN_IDENTITY="$(detect_identity 'Developer ID Application')" || true
    [[ -n "$SIGN_IDENTITY" ]] && log "auto-detected app identity: $SIGN_IDENTITY"
fi
if [[ -z "$INSTALLER_IDENTITY" ]]; then
    INSTALLER_IDENTITY="$(detect_identity 'Developer ID Installer')" || true
    [[ -n "$INSTALLER_IDENTITY" ]] && log "auto-detected installer identity: $INSTALLER_IDENTITY"
fi

if [[ -z "$SIGN_IDENTITY" ]]; then
    warn "no Developer ID Application identity — building UNSIGNED (dev only)."
fi
# Notarization is impossible without signing; refuse rather than fail cryptically.
if $NOTARIZE && [[ -z "$SIGN_IDENTITY" ]]; then
    echo "error: --notarize requires a signing identity (Gatekeeper rejects unsigned)."
    exit 1
fi

# ── Notarytool credential helper (mirrors Phosphor) ──────────────────────────
# Prefers a stored keychain profile; falls back to the three-var combo. The
# `:?` expansions fail loudly with the exact missing var rather than a generic
# notarytool auth error.
notarize_submit() {
    local path="$1"
    if [[ -n "$NOTARY_PROFILE" ]]; then
        xcrun notarytool submit "$path" --keychain-profile "$NOTARY_PROFILE" --wait
    else
        local team apple_id app_pass
        team="${MACOS_TEAM_ID:?Set MACOS_NOTARY_PROFILE or MACOS_TEAM_ID to notarize}"
        apple_id="${MACOS_APPLE_ID:?Set MACOS_NOTARY_PROFILE or MACOS_APPLE_ID to notarize}"
        app_pass="${MACOS_APP_PASSWORD:?Set MACOS_NOTARY_PROFILE or MACOS_APP_PASSWORD to notarize}"
        xcrun notarytool submit "$path" \
            --apple-id "$apple_id" --team-id "$team" --password "$app_pass" --wait
    fi
}

# ── Build universal binaries ─────────────────────────────────────────────────
log "building v$VERSION for arm64 + x86_64 …"
rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null 2>&1 || true
cargo build --release --target aarch64-apple-darwin -p display-daemon -p displayctl
cargo build --release --target x86_64-apple-darwin  -p display-daemon -p displayctl

rm -rf "$BUILD_DIR"
mkdir -p "$STAGE_DIR$INSTALL_PREFIX" "$BUILD_DIR/scripts"

for bin in "${BINARIES[@]}"; do
    log "lipo $bin …"
    lipo -create \
        "$PROJECT_DIR/target/aarch64-apple-darwin/release/$bin" \
        "$PROJECT_DIR/target/x86_64-apple-darwin/release/$bin" \
        -output "$STAGE_DIR$INSTALL_PREFIX/$bin"
    lipo -info "$STAGE_DIR$INSTALL_PREFIX/$bin"
done

# ── Sign the binaries (hardened runtime) ─────────────────────────────────────
if [[ -n "$SIGN_IDENTITY" ]]; then
    for bin in "${BINARIES[@]}"; do
        log "signing $bin …"
        codesign --force --timestamp --options runtime \
            --entitlements "$SCRIPT_DIR/DisplayStudio.entitlements" \
            --sign "$SIGN_IDENTITY" \
            "$STAGE_DIR$INSTALL_PREFIX/$bin"
        codesign --verify --strict --verbose=2 "$STAGE_DIR$INSTALL_PREFIX/$bin"
    done

    # Notarize the binaries themselves, zipped, before packaging. Stapling a
    # ticket onto a bare Mach-O is not possible, but notarizing them means
    # Gatekeeper recognises them once the stapled .pkg is verified on install.
    if $NOTARIZE; then
        log "notarizing binaries …"
        BIN_ZIP="$BUILD_DIR/binaries.zip"
        ( cd "$STAGE_DIR$INSTALL_PREFIX" && ditto -c -k --keepParent . "$BIN_ZIP" )
        notarize_submit "$BIN_ZIP"
    fi
fi

# ── LaunchAgent plist (installed per-user by the postinstall) ────────────────
LOG_DIR_TOKEN='$HOME/Library/Logs/DisplayStudio'
sed -e "s|@BUNDLE_ID@|$BUNDLE_ID|g" \
    -e "s|@INSTALL_PREFIX@|$INSTALL_PREFIX|g" \
    -e "s|@LOG_DIR@|$LOG_DIR_TOKEN|g" \
    "$SCRIPT_DIR/launchagent.plist.template" > "$BUILD_DIR/$BUNDLE_ID.plist"

# ── Stage the plist template inside the payload ──────────────────────────────
# Lands in a persistent share dir rather than /tmp, so a later "repair" or
# reinstall can rebuild the agent, and nothing lingers world-readable in /tmp.
SHARE_DIR="$STAGE_DIR/usr/local/share/displaystudio"
mkdir -p "$SHARE_DIR"
cp "$BUILD_DIR/$BUNDLE_ID.plist" "$SHARE_DIR/$BUNDLE_ID.plist"

# ── postinstall: install + load the LaunchAgent for the console user ─────────
# Runs as root, so it drops to the console user for the per-user agent. The pkg
# lays down binaries + the plist template under root; the live agent is user
# state, installed here.
cat > "$BUILD_DIR/scripts/postinstall" <<POSTINSTALL
#!/usr/bin/env bash
set -euo pipefail

CONSOLE_USER="\$(stat -f%Su /dev/console)"
[[ "\$CONSOLE_USER" == "root" || -z "\$CONSOLE_USER" ]] && exit 0
USER_HOME="\$(dscl . -read "/Users/\$CONSOLE_USER" NFSHomeDirectory | awk '{print \$2}')"
UID_NUM="\$(id -u "\$CONSOLE_USER")"

TEMPLATE="/usr/local/share/displaystudio/$BUNDLE_ID.plist"
AGENTS="\$USER_HOME/Library/LaunchAgents"
PLIST="\$AGENTS/$BUNDLE_ID.plist"
mkdir -p "\$AGENTS" "\$USER_HOME/Library/Logs/DisplayStudio"

# Expand \$HOME in the log paths for this specific user.
sed "s|\\\$HOME|\$USER_HOME|g" "\$TEMPLATE" > "\$PLIST"
chown "\$CONSOLE_USER" "\$PLIST"
chown "\$CONSOLE_USER" "\$USER_HOME/Library/Logs/DisplayStudio"

# Reload cleanly whether or not a previous version was running.
sudo -u "\$CONSOLE_USER" launchctl bootout "gui/\$UID_NUM/$BUNDLE_ID" 2>/dev/null || true
sudo -u "\$CONSOLE_USER" launchctl bootstrap "gui/\$UID_NUM" "\$PLIST" 2>/dev/null || \
    sudo -u "\$CONSOLE_USER" launchctl load "\$PLIST" 2>/dev/null || true
exit 0
POSTINSTALL
chmod +x "$BUILD_DIR/scripts/postinstall"

# ── Filesystem hygiene before packaging ─────────────────────────────────────
# Remove any real .DS_Store / AppleDouble files a cp may have left on disk.
#
# Note: `pkgutil --payload-files` will still LIST ._* entries — that is how the
# CPIO payload represents each node's metadata, and it appears even for files
# with no xattrs. Verified that the payload extracts to exactly the intended
# files with zero ._* on disk, so those listing entries are benign, not phantom
# installs. `com.apple.provenance` is a sticky system xattr that cannot be
# stripped, and does not need to be. COPYFILE_DISABLE keeps our own tooling from
# adding more.
export COPYFILE_DISABLE=1
find "$STAGE_DIR" -name '._*' -delete 2>/dev/null || true
find "$STAGE_DIR" -name '.DS_Store' -delete 2>/dev/null || true

# ── Build the component pkg ──────────────────────────────────────────────────
COMPONENT_PKG="$BUILD_DIR/DisplayStudio-component.pkg"
FINAL_PKG="$BUILD_DIR/DisplayStudio-$VERSION.pkg"

log "pkgbuild …"
pkgbuild \
    --root "$STAGE_DIR" \
    --identifier "io.github.displaymanager" \
    --version "$VERSION" \
    --scripts "$BUILD_DIR/scripts" \
    --install-location "/" \
    "$COMPONENT_PKG"

log "productbuild …"
if [[ -n "$INSTALLER_IDENTITY" ]]; then
    productbuild --package "$COMPONENT_PKG" --sign "$INSTALLER_IDENTITY" "$FINAL_PKG"
else
    warn "no Developer ID Installer identity — pkg will be UNSIGNED."
    productbuild --package "$COMPONENT_PKG" "$FINAL_PKG"
fi

# ── Notarize + staple the final pkg ──────────────────────────────────────────
if $NOTARIZE && [[ -n "$INSTALLER_IDENTITY" ]]; then
    log "notarizing pkg …"
    notarize_submit "$FINAL_PKG"
    log "stapling …"
    xcrun stapler staple "$FINAL_PKG"
    xcrun stapler validate "$FINAL_PKG"
elif $NOTARIZE; then
    warn "pkg not notarized: no Developer ID Installer identity to sign it first."
fi

log "done → $FINAL_PKG"
[[ -n "$SIGN_IDENTITY" ]] || warn "this is an UNSIGNED dev build."
