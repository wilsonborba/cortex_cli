#!/usr/bin/env bash
set -euo pipefail

REPOSITORY="wilsonborba/cortex_cli"
INSTALL_DIR="${CORTEX_INSTALL_DIR:-${HOME:?HOME must be set}/.local/bin}"
VERSION=""
DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: install.sh --version VERSION [--install-dir DIR] [--dry-run]

Downloads one versioned Cortex CLI release, verifies its SHA-256 checksum,
and installs the executable in ~/.local/bin by default. It does not change PATH.
Supported targets are Linux and macOS on x86_64 and arm64/aarch64.

Options:
  --version VERSION     Required release version, with or without a leading v
  --install-dir DIR     Installation directory (default: ~/.local/bin)
  --dry-run             Print planned URLs and destination without network or writes
  -h, --help            Show this help
EOF
}

while (($#)); do
  case "$1" in
    --version)
      (($# >= 2)) || { echo "--version needs a value." >&2; exit 2; }
      VERSION="$2"
      shift 2
      ;;
    --install-dir)
      (($# >= 2)) || { echo "--install-dir needs a value." >&2; exit 2; }
      INSTALL_DIR="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ ! "$VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  echo "Provide an explicit semantic version, for example 0.1.0." >&2
  exit 2
fi
VERSION="${VERSION#v}"

case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64) TARGET="x86_64-unknown-linux-gnu" ;;
  Linux:aarch64|Linux:arm64) TARGET="aarch64-unknown-linux-gnu" ;;
  Darwin:x86_64|Darwin:amd64) TARGET="x86_64-apple-darwin" ;;
  Darwin:arm64|Darwin:aarch64) TARGET="aarch64-apple-darwin" ;;
  *) echo "This release installer supports Linux and macOS on x86_64 and arm64." >&2; exit 2 ;;
esac

ASSET="cortex-v${VERSION}-${TARGET}.tar.gz"
BASE_URL="https://github.com/${REPOSITORY}/releases/download/v${VERSION}"
ARCHIVE_URL="${BASE_URL}/${ASSET}"
CHECKSUMS_URL="${BASE_URL}/SHA256SUMS"

if ((DRY_RUN)); then
  printf 'Version: v%s\nTarget: %s\nArchive: %s\nChecksums: %s\nInstall: %s/cortex\n' \
    "$VERSION" "$TARGET" "$ARCHIVE_URL" "$CHECKSUMS_URL" "$INSTALL_DIR"
  printf 'No network requests or filesystem changes were made.\n'
  exit 0
fi

if [[ "$INSTALL_DIR" != /* ]]; then
  echo "--install-dir must be an absolute path." >&2
  exit 2
fi
if command -v curl >/dev/null 2>&1; then
  download() { curl --fail --location --silent --show-error "$1" --output "$2"; }
elif command -v wget >/dev/null 2>&1; then
  download() { wget --quiet "$1" --output-document="$2"; }
else
  echo "curl or wget is required." >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  hash_file() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  hash_file() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  echo "sha256sum or shasum is required to verify the release." >&2
  exit 1
fi

TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT
download "$ARCHIVE_URL" "$TEMP_DIR/$ASSET"
download "$CHECKSUMS_URL" "$TEMP_DIR/SHA256SUMS"

EXPECTED="$(awk -v name="$ASSET" '$2 == name { print $1; count++ } END { if (count != 1) exit 1 }' "$TEMP_DIR/SHA256SUMS")" || {
  echo "The release checksum manifest has no unique entry for $ASSET." >&2
  exit 1
}
if [[ ! "$EXPECTED" =~ ^[[:xdigit:]]{64}$ ]] || [[ "$(hash_file "$TEMP_DIR/$ASSET")" != "$EXPECTED" ]]; then
  echo "The release archive failed SHA-256 verification." >&2
  exit 1
fi

MEMBERS="$(tar -tzf "$TEMP_DIR/$ASSET")"
if [[ "$MEMBERS" != cortex ]]; then
  echo "The release archive must contain only the cortex executable." >&2
  exit 1
fi
tar -xzf "$TEMP_DIR/$ASSET" -C "$TEMP_DIR"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$TEMP_DIR/cortex" "$INSTALL_DIR/cortex.new.$$"
mv -f "$INSTALL_DIR/cortex.new.$$" "$INSTALL_DIR/cortex"

echo "Installed Cortex v${VERSION} to ${INSTALL_DIR}/cortex."
echo "Add ${INSTALL_DIR} to PATH if needed; the installer does not edit shell configuration."
if [[ "$TARGET" == *linux* ]]; then
  echo "Linux may require the system D-Bus runtime library for Secret Service keyring access."
fi
