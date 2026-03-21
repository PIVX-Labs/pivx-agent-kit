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

URL="https://github.com/${REPO}/releases/download/${LATEST}/pivx-agent-kit-${NAME}.tar.gz"

echo "Installing pivx-agent-kit ${LATEST} (${NAME})..."

# Download and extract
TMPDIR=$(mktemp -d)
curl -sSfL "$URL" -o "${TMPDIR}/pivx-agent-kit.tar.gz"
tar xzf "${TMPDIR}/pivx-agent-kit.tar.gz" -C "$TMPDIR"

# Install
if [ -w "$INSTALL_DIR" ]; then
    mv "${TMPDIR}/pivx-agent-kit" "${INSTALL_DIR}/pivx-agent-kit"
else
    sudo mv "${TMPDIR}/pivx-agent-kit" "${INSTALL_DIR}/pivx-agent-kit"
fi

rm -rf "$TMPDIR"

echo "Installed pivx-agent-kit to ${INSTALL_DIR}/pivx-agent-kit"
pivx-agent-kit
