#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  render_package.sh --output <dir> --version <version> (--artifact-url <url> --checksum <sha256> | --artifact-path <path>)

Examples:
  render_package.sh \
    --output /tmp/nanograph-swift \
    --version 1.2.2 \
    --artifact-url https://example.com/NanoGraphFFI.xcframework.zip \
    --checksum abcdef123456

  render_package.sh \
    --output /tmp/nanograph-swift \
    --version 1.2.2 \
    --artifact-path /tmp/NanoGraphFFI.xcframework
EOF
}

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "${SCRIPT_DIR}/../.." && pwd)

OUTPUT_DIR=""
VERSION=""
ARTIFACT_URL=""
CHECKSUM=""
ARTIFACT_PATH=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      OUTPUT_DIR="$2"
      shift 2
      ;;
    --version)
      VERSION="$2"
      shift 2
      ;;
    --artifact-url)
      ARTIFACT_URL="$2"
      shift 2
      ;;
    --checksum)
      CHECKSUM="$2"
      shift 2
      ;;
    --artifact-path)
      ARTIFACT_PATH="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "${OUTPUT_DIR}" || -z "${VERSION}" ]]; then
  usage >&2
  exit 1
fi

if [[ -n "${ARTIFACT_PATH}" && ( -n "${ARTIFACT_URL}" || -n "${CHECKSUM}" ) ]]; then
  echo "artifact-path mode and artifact-url/checksum mode are mutually exclusive" >&2
  exit 1
fi

if [[ -n "${ARTIFACT_URL}" && -z "${CHECKSUM}" ]]; then
  echo "artifact-url mode requires --checksum" >&2
  exit 1
fi

if [[ -z "${ARTIFACT_PATH}" && -z "${ARTIFACT_URL}" ]]; then
  echo "must provide either --artifact-path or --artifact-url/--checksum" >&2
  exit 1
fi

if [[ -n "${ARTIFACT_PATH}" && ! -e "${ARTIFACT_PATH}" ]]; then
  echo "artifact path does not exist: ${ARTIFACT_PATH}" >&2
  exit 1
fi

PACKAGE_SWIFT="${OUTPUT_DIR}/Package.swift"
CN_HEADER_DIR="${OUTPUT_DIR}/Sources/CNanoGraph/include"
SWIFT_SRC_DIR="${OUTPUT_DIR}/Sources/NanoGraph"
TEST_DIR="${OUTPUT_DIR}/Tests/NanoGraphTests"
ARTIFACTS_DIR="${OUTPUT_DIR}/Artifacts"

rm -rf "${OUTPUT_DIR}"
mkdir -p "${CN_HEADER_DIR}" "${SWIFT_SRC_DIR}" "${TEST_DIR}" "${ARTIFACTS_DIR}"

cp "${REPO_ROOT}/crates/nanograph-ffi/include/nanograph_ffi.h" "${CN_HEADER_DIR}/nanograph_ffi.h"
cp "${REPO_ROOT}/crates/nanograph-ffi/swift/Sources/NanoGraph/NanoGraph.swift" "${SWIFT_SRC_DIR}/NanoGraph.swift"
cp "${REPO_ROOT}/crates/nanograph-ffi/swift/Tests/NanoGraphTests/DatabaseTests.swift" "${TEST_DIR}/DatabaseTests.swift"

if [[ -n "${ARTIFACT_PATH}" ]]; then
  LOCAL_ARTIFACT_NAME="$(basename "${ARTIFACT_PATH}")"
  cp -R "${ARTIFACT_PATH}" "${ARTIFACTS_DIR}/${LOCAL_ARTIFACT_NAME}"
  BINARY_TARGET_DECL=$(cat <<EOF
        .binaryTarget(
            name: "NanoGraphFFI",
            path: "Artifacts/${LOCAL_ARTIFACT_NAME}"
        ),
EOF
)
else
  BINARY_TARGET_DECL=$(cat <<EOF
        .binaryTarget(
            name: "NanoGraphFFI",
            url: "${ARTIFACT_URL}",
            checksum: "${CHECKSUM}"
        ),
EOF
)
fi

cat > "${PACKAGE_SWIFT}" <<EOF
// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "NanoGraph",
    platforms: [
        .macOS(.v13),
    ],
    products: [
        .library(
            name: "NanoGraph",
            targets: ["NanoGraph"]
        ),
    ],
    targets: [
${BINARY_TARGET_DECL}
        .target(
            name: "CNanoGraph",
            path: "Sources/CNanoGraph",
            publicHeadersPath: "include"
        ),
        .target(
            name: "NanoGraph",
            dependencies: ["CNanoGraph", "NanoGraphFFI"],
            path: "Sources/NanoGraph"
        ),
        .testTarget(
            name: "NanoGraphTests",
            dependencies: ["NanoGraph"],
            path: "Tests/NanoGraphTests"
        ),
    ]
)
EOF

cat > "${OUTPUT_DIR}/README.md" <<EOF
# NanoGraph Swift Package

Generated from the nanograph monorepo.

- Version: ${VERSION}
- Source of truth:
  - Rust FFI: crates/nanograph-ffi/
  - Swift wrapper: crates/nanograph-ffi/swift/
EOF

echo "Rendered Swift package to ${OUTPUT_DIR}"
