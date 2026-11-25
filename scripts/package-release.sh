#!/usr/bin/env bash
set -euo pipefail

VERSION=""
TARGET=""
BINARY=""
OUTPUT_DIR=""
DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: package-release.sh --version VERSION --target TARGET --binary PATH --output-dir DIR [--dry-run]

Packages one native Cortex executable as a versioned tar.gz and adds its
SHA-256 entry to SHA256SUMS. Run once per Linux/macOS target in the same
staging directory. This script never creates a tag or publishes a release.

Supported TARGET values:
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu
  x86_64-apple-darwin
  aarch64-apple-darwin

--dry-run prints the archive and manifest names without reading the binary
or changing the output directory.
EOF
}

while (($#)); do
  case "$1" in
    --version) (($# >= 2)) || { echo "--version needs a value." >&2; exit 2; }; VERSION="$2"; shift 2 ;;
    --target) (($# >= 2)) || { echo "--target needs a value." >&2; exit 2; }; TARGET="$2"; shift 2 ;;
    --binary) (($# >= 2)) || { echo "--binary needs a value." >&2; exit 2; }; BINARY="$2"; shift 2 ;;
    --output-dir) (($# >= 2)) || { echo "--output-dir needs a value." >&2; exit 2; }; OUTPUT_DIR="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ ! "$VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  echo "Provide an explicit semantic version, for example 0.1.0." >&2
  exit 2
fi
VERSION="${VERSION#v}"
case "$TARGET" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
  *) echo "Unsupported release target: $TARGET" >&2; exit 2 ;;
esac
if [[ -z "$BINARY" || -z "$OUTPUT_DIR" ]]; then
  echo "--binary and --output-dir are required." >&2
  exit 2
fi

ASSET="cortex-v${VERSION}-${TARGET}.tar.gz"
if ((DRY_RUN)); then
  printf 'Version: v%s\nTarget: %s\nInput: %s\nArchive: %s/%s\nManifest: %s/SHA256SUMS\n' \
    "$VERSION" "$TARGET" "$BINARY" "$OUTPUT_DIR" "$ASSET" "$OUTPUT_DIR"
  printf 'No binary reads or filesystem changes were made.\n'
  exit 0
fi
if [[ "$OUTPUT_DIR" != /* ]]; then
  echo "--output-dir must be an absolute path." >&2
  exit 2
fi
case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64) HOST_TARGET="x86_64-unknown-linux-gnu" ;;
  Linux:aarch64|Linux:arm64) HOST_TARGET="aarch64-unknown-linux-gnu" ;;
  Darwin:x86_64|Darwin:amd64) HOST_TARGET="x86_64-apple-darwin" ;;
  Darwin:arm64|Darwin:aarch64) HOST_TARGET="aarch64-apple-darwin" ;;
  *) echo "Run packaging on a supported native target." >&2; exit 2 ;;
esac
if [[ "$TARGET" != "$HOST_TARGET" ]]; then
  echo "Build and package this target on a matching native system." >&2
  exit 2
fi
if [[ ! -f "$BINARY" || ! -x "$BINARY" ]]; then
  echo "--binary must name an executable file." >&2
  exit 1
fi
if [[ -e "$OUTPUT_DIR/$ASSET" ]]; then
  echo "The versioned archive already exists: $ASSET" >&2
  exit 1
fi
mkdir -p "$OUTPUT_DIR"
STAGE="$(mktemp -d "$OUTPUT_DIR/.cortex-package.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
cp "$BINARY" "$STAGE/cortex"
tar -czf "$STAGE/$ASSET" -C "$STAGE" cortex

if command -v sha256sum >/dev/null 2>&1; then
  DIGEST="$(sha256sum "$STAGE/$ASSET" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  DIGEST="$(shasum -a 256 "$STAGE/$ASSET" | awk '{print $1}')"
else
  echo "sha256sum or shasum is required to create the release manifest." >&2
  exit 1
fi
if [[ -f "$OUTPUT_DIR/SHA256SUMS" ]] && awk -v name="$ASSET" '$2 == name { found = 1 } END { exit !found }' "$OUTPUT_DIR/SHA256SUMS"; then
  echo "The checksum manifest already contains $ASSET." >&2
  exit 1
fi
if [[ -f "$OUTPUT_DIR/SHA256SUMS" ]]; then
  cp "$OUTPUT_DIR/SHA256SUMS" "$STAGE/SHA256SUMS"
else
  : > "$STAGE/SHA256SUMS"
fi
printf '%s  %s\n' "$DIGEST" "$ASSET" >> "$STAGE/SHA256SUMS"
mv "$STAGE/$ASSET" "$OUTPUT_DIR/$ASSET"
mv "$STAGE/SHA256SUMS" "$OUTPUT_DIR/SHA256SUMS"
echo "Packaged $ASSET and updated $OUTPUT_DIR/SHA256SUMS."
