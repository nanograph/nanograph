#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "${SCRIPT_DIR}/../.." && pwd)
PACKAGE_DIR="${REPO_ROOT}/crates/nanograph-ts"

# Detect host platform to pick the matching per-platform package to pack
detect_platform() {
  local kernel arch
  kernel=$(uname -s)
  arch=$(uname -m)
  case "${kernel}-${arch}" in
    Darwin-arm64)        echo "darwin-arm64" ;;
    Darwin-x86_64)       echo "darwin-x64" ;;
    Linux-x86_64)        echo "linux-x64-gnu" ;;
    Linux-aarch64)       echo "linux-arm64-gnu" ;;
    MINGW*-x86_64 | MSYS*-x86_64) echo "win32-x64-msvc" ;;
    *) echo "unsupported platform: ${kernel}-${arch}" >&2; exit 1 ;;
  esac
}
PLATFORM=$(detect_platform)

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/nanograph-ts-consumer.XXXXXX")
PACKAGE_WORK_DIR="${WORK_DIR}/pkg"
CONSUMER_DIR="${WORK_DIR}/consumer"
DATA_DIR="${CONSUMER_DIR}/tmp"
NPM_CACHE_DIR="${WORK_DIR}/npm-cache"
trap 'rm -rf "${WORK_DIR}"' EXIT

mkdir -p "${PACKAGE_WORK_DIR}" "${CONSUMER_DIR}" "${DATA_DIR}" "${NPM_CACHE_DIR}"

echo "Building .node for ${PLATFORM}"
(
  cd "${PACKAGE_DIR}"
  NPM_CONFIG_CACHE="${NPM_CACHE_DIR}" npm run build >/dev/null
)

echo "Moving binary into per-platform package dir"
(
  cd "${PACKAGE_DIR}"
  NPM_CONFIG_CACHE="${NPM_CACHE_DIR}" npx napi artifacts --output-dir . --npm-dir ./npm >/dev/null
)

echo "Packing nanograph-db (meta) + nanograph-db-${PLATFORM}"
(
  cd "${PACKAGE_DIR}"
  NPM_CONFIG_CACHE="${NPM_CACHE_DIR}" npm pack --pack-destination "${PACKAGE_WORK_DIR}" >/dev/null
)
(
  cd "${PACKAGE_DIR}/npm/${PLATFORM}"
  NPM_CONFIG_CACHE="${NPM_CACHE_DIR}" npm pack --pack-destination "${PACKAGE_WORK_DIR}" >/dev/null
)

MAIN_TARBALL=$(find "${PACKAGE_WORK_DIR}" -maxdepth 1 -name 'nanograph-db-[0-9]*.tgz' -print | head -n 1)
PLATFORM_TARBALL=$(find "${PACKAGE_WORK_DIR}" -maxdepth 1 -name "nanograph-db-${PLATFORM}-*.tgz" -print | head -n 1)
if [[ -z "${MAIN_TARBALL}" || -z "${PLATFORM_TARBALL}" ]]; then
  echo "failed to locate one of the packed tarballs (main=${MAIN_TARBALL}, platform=${PLATFORM_TARBALL})" >&2
  exit 1
fi

cat > "${CONSUMER_DIR}/package.json" <<'EOF'
{
  "name": "nanograph-ts-consumer-smoke",
  "private": true
}
EOF

cat > "${CONSUMER_DIR}/smoke.cjs" <<'EOF'
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { Database, decodeArrow } = require("nanograph-db");
const cliDbPath = process.argv[2];

const schema = `
node Person {
  name: String @key
  age: I32?
}
`;

const queries = `
query allPeople() {
  match { $p: Person }
  return { $p.name as name, $p.age as age }
  order { $p.name asc }
}
`;

const cliQueries = `
query allPeople() {
  match { $p: Person }
  return { $p.name as name, $p.age as age }
  order { $p.name asc }
}
`;

async function main() {
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "nanograph-ts-smoke-"));
  const dataPath = path.join(tempDir, "data.jsonl");
  fs.writeFileSync(
    dataPath,
    [
      '{"type":"Person","data":{"name":"Alice","age":30}}',
      '{"type":"Person","data":{"name":"Bob","age":25}}'
    ].join("\n"),
    "utf8"
  );

  const db = await Database.openInMemory(schema);
  try {
    assert.equal(await db.isInMemory(), true);
    await db.loadFile(dataPath, "overwrite");

    const rows = await db.run(queries, "allPeople");
    assert.deepEqual(rows, [
      { name: "Alice", age: 30 },
      { name: "Bob", age: 25 }
    ]);

    const arrow = await db.runArrow(queries, "allPeople");
    const table = decodeArrow(arrow);
    assert.equal(table.toArray().length, 2);
  } finally {
    await db.close();
    fs.rmSync(tempDir, { recursive: true, force: true });
  }

  const cliDb = await Database.open(cliDbPath);
  try {
    const rows = await cliDb.run(cliQueries, "allPeople");
    assert.deepEqual(rows, [
      { name: "Alice", age: 30 },
      { name: "Bob", age: 25 }
    ]);
  } finally {
    await cliDb.close();
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
EOF

echo "Installing nanograph-db + nanograph-db-${PLATFORM} into temp consumer"
(
  cd "${CONSUMER_DIR}"
  NPM_CONFIG_CACHE="${NPM_CACHE_DIR}" npm install \
    --fetch-retries=1 \
    --fetch-retry-mintimeout=1000 \
    --fetch-retry-maxtimeout=2000 \
    "${MAIN_TARBALL}" "${PLATFORM_TARBALL}" >/dev/null
)

CLI_DB_DIR="${DATA_DIR}/cli-db"
CLI_DB_PATH="${CLI_DB_DIR}/test.nano"
mkdir -p "${CLI_DB_DIR}"

cat > "${CLI_DB_DIR}/schema.pg" <<'EOF'
node Person {
  name: String @key
  age: I32?
}
EOF

cat > "${CLI_DB_DIR}/seed.jsonl" <<'EOF'
{"type":"Person","data":{"name":"Alice","age":30}}
{"type":"Person","data":{"name":"Bob","age":25}}
EOF

echo "Creating CLI database for SDK interoperability check"
cargo run --quiet -p nanograph-cli -- init --db "${CLI_DB_PATH}" --schema "${CLI_DB_DIR}/schema.pg" >/dev/null
cargo run --quiet -p nanograph-cli -- load --db "${CLI_DB_PATH}" --data "${CLI_DB_DIR}/seed.jsonl" --mode overwrite >/dev/null

echo "Running consumer smoke test"
(
  cd "${CONSUMER_DIR}"
  node smoke.cjs "${CLI_DB_PATH}"
)

echo "TS consumer smoke test passed"
