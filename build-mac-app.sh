#!/usr/bin/env bash
set -euo pipefail

APP_NAME="BitEngine"
BUNDLE_ID="com.yourname.bitengine"
VERSION="1.0"

# Set this to your actual Cargo binary name if different
BIN_NAME="bitcoin_node_manager"

ICON_FILE="app-icon.icns"
APP_DIR="${APP_NAME}.app"

cargo build --release

mkdir -p "${APP_DIR}/Contents/MacOS"
mkdir -p "${APP_DIR}/Contents/Resources"

cp "target/release/${BIN_NAME}" "${APP_DIR}/Contents/MacOS/${APP_NAME}"
chmod +x "${APP_DIR}/Contents/MacOS/${APP_NAME}"

cp "${ICON_FILE}" "${APP_DIR}/Contents/Resources/${ICON_FILE}"

cat > "${APP_DIR}/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>

    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>

    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>

    <key>CFBundleVersion</key>
    <string>${VERSION}</string>

    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>

    <key>CFBundleExecutable</key>
    <string>${APP_NAME}</string>

    <key>CFBundleIconFile</key>
    <string>${ICON_FILE}</string>

    <key>CFBundlePackageType</key>
    <string>APPL</string>

    <key>LSMinimumSystemVersion</key>
    <string>10.13</string>
</dict>
</plist>
EOF

codesign --deep --force --verify --sign - "${APP_DIR}"

echo "Built ${APP_DIR}"