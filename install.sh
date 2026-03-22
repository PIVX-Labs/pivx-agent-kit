#!/bin/sh
# PIVX Agent Kit installer
# Usage: curl -sSf https://raw.githubusercontent.com/PIVX-Labs/pivx-agent-kit/master/install.sh | sh
set -e

REPO="PIVX-Labs/pivx-agent-kit"
INSTALL_DIR="/usr/local/bin"

# Detect platform
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)  PLATFORM="linux" ;;
    Darwin) PLATFORM="macos" ;;
    *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
    x86_64|amd64)  ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *)             echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

NAME="${PLATFORM}-${ARCH}"

# Get latest release tag
LATEST=$(curl -sSf "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)
if [ -z "$LATEST" ]; then
    echo "Failed to fetch latest release"
    exit 1
fi

ARCHIVE="pivx-agent-kit-${NAME}.tar.gz"
TMPDIR=$(mktemp -d)

echo "Installing pivx-agent-kit ${LATEST} (${NAME})..."

# Download archive and checksums
curl -sSfL "https://github.com/${REPO}/releases/download/${LATEST}/${ARCHIVE}" -o "${TMPDIR}/${ARCHIVE}"
curl -sSfL "https://github.com/${REPO}/releases/download/${LATEST}/checksums.txt" -o "${TMPDIR}/checksums.txt"

# Verify SHA256
EXPECTED=$(grep "${ARCHIVE}" "${TMPDIR}/checksums.txt" | awk '{print $1}')
ACTUAL=$(sha256sum "${TMPDIR}/${ARCHIVE}" 2>/dev/null || shasum -a 256 "${TMPDIR}/${ARCHIVE}" | awk '{print $1}')
ACTUAL=$(echo "$ACTUAL" | awk '{print $1}')

if [ -z "$EXPECTED" ]; then
    echo "Warning: no checksum found for ${ARCHIVE}"
elif [ "$ACTUAL" != "$EXPECTED" ]; then
    echo "Checksum verification FAILED — download may be corrupted or tampered."
    echo "  expected: $EXPECTED"
    echo "  got:      $ACTUAL"
    rm -rf "$TMPDIR"
    exit 1
else
    echo "Checksum verified."
fi

# Extract
tar xzf "${TMPDIR}/${ARCHIVE}" -C "$TMPDIR"

# Install
if [ -w "$INSTALL_DIR" ]; then
    mv "${TMPDIR}/pivx-agent-kit" "${INSTALL_DIR}/pivx-agent-kit"
else
    sudo mv "${TMPDIR}/pivx-agent-kit" "${INSTALL_DIR}/pivx-agent-kit"
fi

rm -rf "$TMPDIR"

echo "Installed pivx-agent-kit to ${INSTALL_DIR}/pivx-agent-kit"
pivx-agent-kit
