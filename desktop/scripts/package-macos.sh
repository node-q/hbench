#!/bin/bash
# Signs the app bundle, wraps it in a .dmg, and notarizes that. Uses only tools that ship with macOS and
# Xcode, so the Apple credentials never meet npm or cargo code.
#
#   package-macos.sh <App.app> <out.dmg> [<out.app.tar.gz>]
#
# With a third argument it also writes the signed app as the in-app updater's macOS payload: a .tar.gz with
# the .app at its root. The release signs that file with the updater key in a later job.
#
# Credentials come from the environment; each level is skipped (with a warning) when its are unset:
#   APPLE_CERTIFICATE            base64 of a "Developer ID Application" .p12
#   APPLE_CERTIFICATE_PASSWORD   that .p12's password
#   APPLE_API_KEY                an App Store Connect API key (.p8 contents), for notarization
#   APPLE_API_KEY_ID             its key ID
#   APPLE_API_ISSUER             its issuer ID
# Without a certificate the app is ad-hoc signed: it runs, but Gatekeeper blocks a downloaded copy.
# PACKAGE_UNSIGNED=1 says that is intended (a CI or local test build), and silences the warnings.
set -euo pipefail
app="${1:?usage: package-macos.sh <App.app> <out.dmg>}"
dmg="${2:?usage: package-macos.sh <App.app> <out.dmg> [<out.app.tar.gz>]}"
update="${3:-}"
[ -d "$app" ] || { echo "error: $app is not an app bundle" >&2; exit 1; }
work="$(mktemp -d)"
keychain="$work/signing.keychain-db"
cleanup() {
  security delete-keychain "$keychain" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
warn() {
  if [ -n "${PACKAGE_UNSIGNED:-}" ]; then echo "note: $*" >&2; else echo "${GITHUB_ACTIONS:+::warning::}$*" >&2; fi
}

identity=""
if [ -n "${APPLE_CERTIFICATE:-}" ]; then
  password="$(uuidgen)"
  security create-keychain -p "$password" "$keychain"
  security set-keychain-settings -lut 3600 "$keychain"
  security unlock-keychain -p "$password" "$keychain"
  printf '%s' "$APPLE_CERTIFICATE" | base64 --decode > "$work/cert.p12"
  security import "$work/cert.p12" -k "$keychain" -P "${APPLE_CERTIFICATE_PASSWORD:-}" -T /usr/bin/codesign >/dev/null
  security set-key-partition-list -S apple-tool:,apple: -s -k "$password" "$keychain" >/dev/null
  # codesign looks identities up in the user's keychain search list.
  # shellcheck disable=SC2046
  security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
  identity="$(security find-identity -v -p codesigning "$keychain" | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)"
  [ -n "$identity" ] || { echo "error: APPLE_CERTIFICATE holds no \"Developer ID Application\" identity" >&2; exit 1; }
  echo "signing as: $identity"
  # Inside out: the bundled hbench, then the app (its main executable and seal). Hardened runtime and a
  # secure timestamp are what notarization requires.
  codesign --force --options runtime --timestamp --sign "$identity" "$app/Contents/MacOS/hbench"
  codesign --force --options runtime --timestamp --sign "$identity" "$app"
else
  warn "APPLE_CERTIFICATE is not set: $(basename "$dmg") is ad-hoc signed, and Gatekeeper will block it when downloaded"
  codesign --force --deep --sign - "$app"
fi
codesign --verify --deep --strict "$app"

# The usual drag-to-Applications image, built without Finder scripting.
stage="$work/stage"
mkdir "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
mkdir -p "$(dirname "$dmg")"
hdiutil create -volname "$(basename "$app" .app)" -srcfolder "$stage" -ov -format UDZO "$dmg" >/dev/null

if [ -n "$identity" ]; then
  codesign --force --timestamp --sign "$identity" "$dmg"
  if [ -n "${APPLE_API_KEY:-}" ]; then
    printf '%s' "$APPLE_API_KEY" > "$work/AuthKey.p8"
    xcrun notarytool submit "$dmg" --key "$work/AuthKey.p8" --key-id "${APPLE_API_KEY_ID:?}" --issuer "${APPLE_API_ISSUER:?}" --wait
    xcrun stapler staple "$dmg"
    spctl --assess --type open --context context:primary-signature -v "$dmg"
    # The dmg's ticket covers the app inside it too; stapling the app lets the update payload pass Gatekeeper
    # offline. Not fatal: Gatekeeper looks the ticket up online otherwise.
    if [ -n "$update" ]; then
      xcrun stapler staple "$app" || warn "couldn't staple the ticket to $(basename "$app"); its update payload relies on the online check"
    fi
  else
    warn "APPLE_API_KEY is not set: $(basename "$dmg") is signed but not notarized, and Gatekeeper will still warn"
  fi
fi
echo "packaged: $dmg"

if [ -n "$update" ]; then
  mkdir -p "$(dirname "$update")"
  # No AppleDouble (._*) files: the updater unpacks every entry into the bundle.
  COPYFILE_DISABLE=1 tar -C "$(dirname "$app")" -czf "$update" "$(basename "$app")"
  echo "update payload: $update"
fi
