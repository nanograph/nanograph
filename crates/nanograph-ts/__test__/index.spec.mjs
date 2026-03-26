import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { Database, decodeArrow, mediaFile, mediaUri } from "../index.js";

// ---------- fixtures ----------

const SCHEMA = `
node Person {
  name: String @key
  age: I32?
  score: F64?
  active: Bool?
  joined: Date?
  role: enum(engineer, manager, intern)?
}

node Company {
  name: String @key
}

edge WorksAt: Person -> Company {
  since: I32?
  tags: [String]?
}
`;

const DATA = [
  '{"type": "Person", "data": {"name": "Alice", "age": 30, "score": 9.5, "active": true, "joined": "2020-06-15", "role": "engineer"}}',
  '{"type": "Person", "data": {"name": "Bob", "age": 25, "score": 7.2, "active": true, "joined": "2022-01-10", "role": "intern"}}',
  '{"type": "Person", "data": {"name": "Carol", "age": 35, "active": false}}',
  '{"type": "Company", "data": {"name": "Acme"}}',
  '{"type": "Company", "data": {"name": "Globex"}}',
  '{"edge": "WorksAt", "from": "Alice", "to": "Acme", "data": {"since": 2020, "tags": ["core", "backend"]}}',
  '{"edge": "WorksAt", "from": "Bob", "to": "Acme", "data": {"since": 2022}}',
].join("\n");

const QUERIES = `
query allPeople() {
  match { $p: Person }
  return { $p.name, $p.age }
}

query personByName($name: String) {
  match { $p: Person { name: $name } }
  return { $p.name, $p.age, $p.score, $p.active, $p.joined, $p.role }
}

query olderThan($minAge: I32) {
  match {
    $p: Person
    $p.age > $minAge
  }
  return { $p.name, $p.age }
  order { $p.age asc }
}

query top2() {
  match { $p: Person }
  return { $p.name, $p.age }
  order { $p.age desc }
  limit 2
}

query countPeople() {
  match { $p: Person }
  return { count($p) as total }
}

query avgAge() {
  match { $p: Person }
  return { avg($p.age) as avg_age }
}

query coworkers() {
  match {
    $p: Person
    $p worksAt $c
    $c: Company
  }
  return { $p.name as person, $c.name as company }
}

query activeFilter($active: Bool) {
  match {
    $p: Person { active: $active }
  }
  return { $p.name }
}

query scoreAbove($minScore: F64) {
  match {
    $p: Person
    $p.score > $minScore
  }
  return { $p.name, $p.score }
}

query joinedAfter($since: Date) {
  match {
    $p: Person
    $p.joined > $since
  }
  return { $p.name, $p.joined }
}

query byRole($role: String) {
  match {
    $p: Person { role: $role }
  }
  return { $p.name, $p.role }
}

query insertPerson($name: String, $age: I32) {
  insert Person { name: $name, age: $age }
}

query updatePerson($name: String, $age: I32) {
  update Person set { age: $age } where name = $name
}

query deletePerson($name: String) {
  delete Person where name = $name
}

`;

const U64_QUERY = `query validateU64($v: U64) {
  match { $p: Person }
  return { $p.name }
  limit 1
}`;

const REPO_ROOT = fileURLToPath(new URL("../../..", import.meta.url));
const CLI_BINARY_NAME = process.platform === "win32" ? "nanograph.exe" : "nanograph";

let tmpDir;
let dbCounter = 0;

async function freshDb() {
  const dbPath = join(tmpDir, `test-${++dbCounter}.nano`);
  const db = await Database.init(dbPath, SCHEMA);
  await db.load(DATA, "overwrite");
  return { db, dbPath };
}

async function writeDataFile(name, contents) {
  const path = join(tmpDir, `${name}-${++dbCounter}.jsonl`);
  await writeFile(path, contents);
  return path;
}

async function writeJpegAsset(name) {
  const path = join(tmpDir, `${name}.jpg`);
  await writeFile(path, Buffer.from([0xff, 0xd8, 0xff, 0xd9]));
  return path;
}

function runNanograph(args) {
  const debugBinary = join(REPO_ROOT, "target", "debug", CLI_BINARY_NAME);
  const command = existsSync(debugBinary) ? debugBinary : "cargo";
  const commandArgs = existsSync(debugBinary)
    ? args
    : ["run", "--quiet", "-p", "nanograph-cli", "--", ...args];

  try {
    execFileSync(command, commandArgs, {
      cwd: REPO_ROOT,
      encoding: "utf8",
      env: process.env,
    });
  } catch (error) {
    const stdout = error?.stdout ?? "";
    const stderr = error?.stderr ?? "";
    throw new Error(
      `nanograph command failed: ${command} ${commandArgs.join(" ")}\nstdout:\n${stdout}\nstderr:\n${stderr}`,
    );
  }
}

// ---------- tests ----------

describe("Database", () => {
  before(async () => {
    tmpDir = await mkdtemp(join(tmpdir(), "nanograph-ts-test-"));
  });

  after(async () => {
    await rm(tmpDir, { recursive: true, force: true });
  });

  // ---- init + load ----

  describe("init + load", () => {
    it("should init and load data", async () => {
      const { db } = await freshDb();
      assert.equal(await db.isInMemory(), false);
      const rows = await db.run(QUERIES, "allPeople");
      assert.equal(rows.length, 3);
      const names = rows.map((r) => r.name).sort();
      assert.deepEqual(names, ["Alice", "Bob", "Carol"]);
      await db.close();
    });

    it("should open an in-memory database", async () => {
      const db = await Database.openInMemory(SCHEMA);
      assert.equal(await db.isInMemory(), true);
      await db.load(DATA, "overwrite");
      const rows = await db.run(QUERIES, "allPeople");
      assert.equal(rows.length, 3);
      const names = rows.map((r) => r.name).sort();
      assert.deepEqual(names, ["Alice", "Bob", "Carol"]);
      await db.close();
    });
  });

  // ---- load modes ----

  describe("load modes", () => {
    it("loadFile should match string load", async () => {
      const dbPath = join(tmpDir, `test-${++dbCounter}.nano`);
      const db = await Database.init(dbPath, SCHEMA);
      const dataPath = await writeDataFile("load-file", DATA);
      await db.loadFile(dataPath, "overwrite");
      const rows = await db.run(QUERIES, "allPeople");
      assert.equal(rows.length, 3);
      const names = rows.map((r) => r.name).sort();
      assert.deepEqual(names, ["Alice", "Bob", "Carol"]);
      await db.close();
    });

    it("loadFile should resolve forward-reference edges", async () => {
      const dbPath = join(tmpDir, `test-${++dbCounter}.nano`);
      const db = await Database.init(dbPath, SCHEMA);
      const dataPath = await writeDataFile(
        "forward-refs",
        [
          '{"edge": "WorksAt", "from": "Alice", "to": "Acme", "data": {"since": 2020}}',
          '{"type": "Person", "data": {"name": "Alice", "age": 30}}',
          '{"type": "Company", "data": {"name": "Acme"}}',
        ].join("\n"),
      );
      await db.loadFile(dataPath, "overwrite");
      const rows = await db.run(
        `
query coworkers() {
  match {
    $p: Person
    $p worksAt $c
  }
  return { $p.name, $c.name as company }
}
`,
        "coworkers",
      );
      assert.deepEqual(rows, [{ name: "Alice", company: "Acme" }]);
      await db.close();
    });

    it("append should add rows without replacing", async () => {
      const { db } = await freshDb();
      const extra = '{"type": "Person", "data": {"name": "Dave", "age": 40}}';
      await db.load(extra, "append");
      const rows = await db.run(QUERIES, "allPeople");
      assert.equal(rows.length, 4);
      const names = rows.map((r) => r.name).sort();
      assert.ok(names.includes("Dave"));
      assert.ok(names.includes("Alice"));
      await db.close();
    });

    it("merge should upsert keyed rows", async () => {
      const { db } = await freshDb();
      // Update Alice's age via merge
      const update = '{"type": "Person", "data": {"name": "Alice", "age": 99}}';
      await db.load(update, "merge");
      const rows = await db.run(QUERIES, "personByName", { name: "Alice" });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].age, 99);
      await db.close();
    });

    it("overwrite should replace all data", async () => {
      const { db } = await freshDb();
      const replacement = '{"type": "Person", "data": {"name": "Zara", "age": 22}}';
      await db.load(replacement, "overwrite");
      const rows = await db.run(QUERIES, "allPeople");
      assert.equal(rows.length, 1);
      assert.equal(rows[0].name, "Zara");
      await db.close();
    });
  });

  // ---- read queries ----

  describe("run — read queries", () => {
    it("should export decodeArrow helper", async () => {
      assert.equal(typeof decodeArrow, "function");
    });

    it("should execute a parameterized query", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "personByName", { name: "Alice" });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].name, "Alice");
      assert.equal(rows[0].age, 30);
      await db.close();
    });

    it("should filter with comparison params", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "olderThan", { minAge: 28 });
      assert.equal(rows.length, 2);
      assert.equal(rows[0].name, "Alice");
      assert.equal(rows[1].name, "Carol");
      await db.close();
    });

    it("should traverse edges", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "coworkers");
      assert.equal(rows.length, 2);
      for (const row of rows) {
        assert.equal(row.company, "Acme");
      }
      await db.close();
    });

    it("should return empty array for no matches", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "personByName", { name: "Nobody" });
      assert.equal(rows.length, 0);
      await db.close();
    });

    it("should support order + limit", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "top2");
      assert.equal(rows.length, 2);
      // desc order by age: Carol(35), Alice(30)
      assert.equal(rows[0].name, "Carol");
      assert.equal(rows[1].name, "Alice");
      await db.close();
    });

    it("should support count aggregation", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "countPeople");
      assert.equal(rows.length, 1);
      assert.equal(rows[0].total, 3);
      await db.close();
    });

    it("should support avg aggregation", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "avgAge");
      assert.equal(rows.length, 1);
      assert.equal(rows[0].avg_age, 30); // (30+25+35)/3 = 30
      await db.close();
    });

    it("should return Arrow IPC for read queries", async () => {
      const { db } = await freshDb();
      const arrow = await db.runArrow(QUERIES, "personByName", { name: "Alice" });
      const table = decodeArrow(arrow);
      const rows = table.toArray();
      assert.equal(Buffer.isBuffer(arrow), true);
      assert.ok(arrow.byteLength > 0);
      assert.equal(rows.length, 1);
      assert.equal(rows[0].name, "Alice");
      await db.close();
    });

    it("should return Arrow IPC for empty read results", async () => {
      const { db } = await freshDb();
      const arrow = await db.runArrow(QUERIES, "personByName", { name: "Nobody" });
      const table = decodeArrow(arrow);
      assert.equal(Buffer.isBuffer(arrow), true);
      assert.ok(arrow.byteLength > 0);
      assert.equal(table.toArray().length, 0);
      await db.close();
    });

    it("should reject runArrow for mutations", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.runArrow(QUERIES, "insertPerson", { name: "Frank", age: 41 }),
        (err) => {
          assert.ok(err.message.includes("runArrow only supports read queries"));
          return true;
        },
      );
      await db.close();
    });
  });

  // ---- nullable and typed results ----

  describe("result types", () => {
    it("should return null for missing nullable properties", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "personByName", { name: "Carol" });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].name, "Carol");
      assert.equal(rows[0].age, 35);
      assert.equal(rows[0].score, null);
      assert.equal(rows[0].joined, null);
      assert.equal(rows[0].role, null);
      await db.close();
    });

    it("should return correct types for all scalar fields", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "personByName", { name: "Alice" });
      const r = rows[0];
      assert.equal(typeof r.name, "string");
      assert.equal(typeof r.age, "number");
      assert.equal(typeof r.score, "number");
      assert.equal(typeof r.active, "boolean");
      assert.equal(typeof r.joined, "string"); // ISO date string
      assert.equal(typeof r.role, "string"); // enum as string
      assert.equal(r.score, 9.5);
      assert.equal(r.active, true);
      assert.equal(r.joined, "2020-06-15");
      assert.equal(r.role, "engineer");
      await db.close();
    });

    it("should verify list properties exist via describe", async () => {
      const { db } = await freshDb();
      const info = await db.describe();
      const worksAt = info.edgeTypes.find((t) => t.name === "WorksAt");
      const tagsProp = worksAt.properties.find((p) => p.name === "tags");
      assert.ok(tagsProp);
      assert.equal(tagsProp.list, true);
      assert.equal(tagsProp.type, "String");
      assert.equal(tagsProp.nullable, true);
      await db.close();
    });
  });

  // ---- typed parameter conversion ----

  describe("parameter types", () => {
    it("Bool param filters correctly", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "activeFilter", { active: true });
      const names = rows.map((r) => r.name).sort();
      assert.deepEqual(names, ["Alice", "Bob"]);
      await db.close();
    });

    it("F64 param filters correctly", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "scoreAbove", { minScore: 8.0 });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].name, "Alice");
      await db.close();
    });

    it("Date param filters correctly", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "joinedAfter", {
        since: "2021-01-01",
      });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].name, "Bob");
      await db.close();
    });

    it("enum param matches correctly", async () => {
      const { db } = await freshDb();
      const rows = await db.run(QUERIES, "byRole", { role: "engineer" });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].name, "Alice");
      await db.close();
    });

    it("should reject non-string value for String param", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.run(QUERIES, "byRole", { role: 42 }),
        (err) => {
          assert.ok(err.message.includes("expected string"));
          return true;
        },
      );
      await db.close();
    });

    it("should reject wrong type for Bool param", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.run(QUERIES, "activeFilter", { active: "yes" }),
        (err) => {
          assert.ok(err.message.includes("expected boolean"));
          return true;
        },
      );
      await db.close();
    });

    it("should reject wrong type for F64 param", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.run(QUERIES, "scoreAbove", { minScore: "high" }),
        (err) => {
          assert.ok(err.message.includes("expected float"));
          return true;
        },
      );
      await db.close();
    });

    it("should reject wrong type for Date param", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.run(QUERIES, "joinedAfter", { since: 12345 }),
        (err) => {
          assert.ok(err.message.includes("expected date string"));
          return true;
        },
      );
      await db.close();
    });

    it("should reject I32 params outside range", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.run(QUERIES, "olderThan", { minAge: 2147483648 }),
        (err) => {
          assert.ok(err.message.includes("exceeds I32 range"));
          return true;
        },
      );
      await assert.rejects(
        () => db.run(QUERIES, "olderThan", { minAge: -2147483649 }),
        (err) => {
          assert.ok(err.message.includes("exceeds I32 range"));
          return true;
        },
      );
      await db.close();
    });
  });

  // ---- mutations ----

  describe("run — mutations", () => {
    it("should insert a node", async () => {
      const { db } = await freshDb();
      const result = await db.run(QUERIES, "insertPerson", {
        name: "Dave",
        age: 40,
      });
      assert.equal(result.affectedNodes, 1);

      const rows = await db.run(QUERIES, "personByName", { name: "Dave" });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].age, 40);
      await db.close();
    });

    it("should update a node", async () => {
      const { db } = await freshDb();
      const result = await db.run(QUERIES, "updatePerson", {
        name: "Alice",
        age: 31,
      });
      assert.equal(result.affectedNodes, 1);

      const rows = await db.run(QUERIES, "personByName", { name: "Alice" });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].age, 31);
      await db.close();
    });

    it("should delete a node", async () => {
      const { db } = await freshDb();
      const result = await db.run(QUERIES, "deletePerson", { name: "Carol" });
      assert.equal(result.affectedNodes, 1);

      const rows = await db.run(QUERIES, "allPeople");
      assert.equal(rows.length, 2);
      const names = rows.map((r) => r.name).sort();
      assert.deepEqual(names, ["Alice", "Bob"]);
      await db.close();
    });

    it("insert then delete round-trip", async () => {
      const { db } = await freshDb();
      await db.run(QUERIES, "insertPerson", { name: "Temp", age: 1 });
      let rows = await db.run(QUERIES, "allPeople");
      assert.equal(rows.length, 4);

      await db.run(QUERIES, "deletePerson", { name: "Temp" });
      rows = await db.run(QUERIES, "allPeople");
      assert.equal(rows.length, 3);
      await db.close();
    });
  });

  // ---- check ----

  describe("check", () => {
    it("should typecheck all queries with correct kinds", async () => {
      const { db } = await freshDb();
      const checks = await db.check(QUERIES);
      assert.ok(Array.isArray(checks));

      for (const c of checks) {
        assert.equal(c.status, "ok", `query '${c.name}' failed: ${c.error}`);
        assert.ok(["read", "mutation"].includes(c.kind));
      }

      // Verify mutation kinds
      const insert = checks.find((c) => c.name === "insertPerson");
      assert.equal(insert.kind, "mutation");
      const update = checks.find((c) => c.name === "updatePerson");
      assert.equal(update.kind, "mutation");
      const del = checks.find((c) => c.name === "deletePerson");
      assert.equal(del.kind, "mutation");

      // Verify read kinds
      const allPeople = checks.find((c) => c.name === "allPeople");
      assert.equal(allPeople.kind, "read");
      await db.close();
    });

    it("should report errors for bad queries", async () => {
      const { db } = await freshDb();
      const badQuery = `query broken() {
  match { $x: NonExistentType }
  return { $x.name }
}`;
      const checks = await db.check(badQuery);
      assert.equal(checks.length, 1);
      assert.equal(checks[0].status, "error");
      assert.equal(checks[0].kind, "read");
      assert.ok(checks[0].error.length > 0);
      await db.close();
    });
  });

  // ---- describe ----

  describe("describe", () => {
    it("should return full schema introspection", async () => {
      const { db } = await freshDb();
      const info = await db.describe();

      // Node types
      assert.ok(Array.isArray(info.nodeTypes));
      const person = info.nodeTypes.find((t) => t.name === "Person");
      assert.ok(person);
      assert.equal(typeof person.typeId, "number");

      // propId present on all properties
      for (const prop of person.properties) {
        assert.equal(typeof prop.propId, "number", `${prop.name} missing propId`);
      }

      // Key property
      const nameProp = person.properties.find((p) => p.name === "name");
      assert.ok(nameProp);
      assert.equal(nameProp.key, true);
      assert.equal(nameProp.nullable, false);
      assert.equal(nameProp.type, "String");

      // Nullable property
      const ageProp = person.properties.find((p) => p.name === "age");
      assert.ok(ageProp);
      assert.equal(ageProp.nullable, true);

      // Enum property
      const roleProp = person.properties.find((p) => p.name === "role");
      assert.ok(roleProp);
      assert.ok(Array.isArray(roleProp.enumValues));
      assert.ok(roleProp.enumValues.includes("engineer"));
      assert.ok(roleProp.enumValues.includes("manager"));
      assert.ok(roleProp.enumValues.includes("intern"));

      // Edge types
      const worksAt = info.edgeTypes.find((t) => t.name === "WorksAt");
      assert.ok(worksAt);
      assert.equal(worksAt.srcType, "Person");
      assert.equal(worksAt.dstType, "Company");
      assert.equal(typeof worksAt.typeId, "number");

      // List property on edge
      const tagsProp = worksAt.properties.find((p) => p.name === "tags");
      assert.ok(tagsProp);
      assert.equal(tagsProp.list, true);
      assert.equal(tagsProp.nullable, true);

      await db.close();
    });
  });

  // ---- doctor ----

  describe("doctor", () => {
    it("should report healthy with all fields", async () => {
      const { db } = await freshDb();
      const report = await db.doctor();
      assert.equal(report.healthy, true);
      assert.deepEqual(report.issues, []);
      assert.ok(Array.isArray(report.warnings));
      assert.equal(typeof report.manifestDbVersion, "number");
      assert.ok(report.manifestDbVersion >= 1);
      assert.equal(typeof report.datasetsChecked, "number");
      assert.equal(typeof report.txRows, "number");
      assert.equal(typeof report.cdcRows, "number");
      await db.close();
    });
  });

  // ---- open ----

  describe("open", () => {
    it("should reopen and preserve data", async () => {
      const { db, dbPath } = await freshDb();
      await db.close();

      const db2 = await Database.open(dbPath);
      const rows = await db2.run(QUERIES, "allPeople");
      assert.equal(rows.length, 3);
      await db2.close();
    });

    it("mutations should persist across reopen", async () => {
      const { db, dbPath } = await freshDb();
      await db.run(QUERIES, "insertPerson", { name: "Eve", age: 28 });
      await db.close();

      const db2 = await Database.open(dbPath);
      const rows = await db2.run(QUERIES, "personByName", { name: "Eve" });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].age, 28);
      await db2.close();
    });

    it("should open a database created by the CLI", async () => {
      const cliDir = await mkdtemp(join(tmpDir, `cli-db-${++dbCounter}-`));
      const dbPath = join(cliDir, "interop.nano");
      const schemaPath = join(cliDir, "schema.pg");
      const dataPath = join(cliDir, "seed.jsonl");

      await writeFile(schemaPath, SCHEMA);
      await writeFile(dataPath, DATA);

      runNanograph(["init", "--db", dbPath, "--schema", schemaPath]);
      runNanograph(["load", "--db", dbPath, "--data", dataPath, "--mode", "overwrite"]);

      const db = await Database.open(dbPath);
      const rows = await db.run(QUERIES, "allPeople");
      assert.deepEqual(rows, [
        { name: "Alice", age: 30 },
        { name: "Bob", age: 25 },
        { name: "Carol", age: 35 },
      ]);
      await db.close();
    });
  });

  // ---- compact + cleanup ----

  describe("compact + cleanup", () => {
    it("should compact with default options", async () => {
      const { db } = await freshDb();
      const result = await db.compact();
      assert.equal(typeof result.datasetsConsidered, "number");
      assert.equal(typeof result.datasetsCompacted, "number");
      assert.equal(typeof result.fragmentsRemoved, "number");
      assert.equal(typeof result.fragmentsAdded, "number");
      assert.equal(typeof result.filesRemoved, "number");
      assert.equal(typeof result.filesAdded, "number");
      assert.equal(typeof result.manifestCommitted, "boolean");
      await db.close();
    });

    it("should compact with explicit options", async () => {
      const { db } = await freshDb();
      const result = await db.compact({
        targetRowsPerFragment: 512,
        materializeDeletions: true,
        materializeDeletionsThreshold: 0.5,
      });
      assert.equal(typeof result.datasetsConsidered, "number");
      await db.close();
    });

    it("should reject unknown compact options", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.compact({ targetRowsPerFrament: 512 }),
        (err) => {
          assert.ok(err.message.includes("unknown compact option"));
          return true;
        },
      );
      await db.close();
    });

    it("should cleanup with default options", async () => {
      const { db } = await freshDb();
      const result = await db.cleanup();
      assert.equal(typeof result.txRowsRemoved, "number");
      assert.equal(typeof result.txRowsKept, "number");
      assert.equal(typeof result.cdcRowsRemoved, "number");
      assert.equal(typeof result.cdcRowsKept, "number");
      assert.equal(typeof result.datasetsCleaned, "number");
      assert.equal(typeof result.datasetOldVersionsRemoved, "number");
      assert.equal(typeof result.datasetBytesRemoved, "number");
      await db.close();
    });

    it("should cleanup with explicit options", async () => {
      const { db } = await freshDb();
      const result = await db.cleanup({
        retainTxVersions: 10,
        retainDatasetVersions: 5,
      });
      assert.equal(typeof result.txRowsKept, "number");
      await db.close();
    });

    it("should reject unknown cleanup options", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.cleanup({ retainTxVersionz: 10 }),
        (err) => {
          assert.ok(err.message.includes("unknown cleanup option"));
          return true;
        },
      );
      await db.close();
    });
  });

  // ---- search ----

  describe("search", () => {
    const SEARCH_SCHEMA = `
node Signal {
  slug: String @key
  summary: String
  embedding: Vector(3) @index
}
`;

    const SEARCH_DATA = [
      '{"type":"Signal","data":{"slug":"sig-billing-delay","summary":"billing reconciliation delay due to missing invoice data","embedding":[1.0,0.0,0.0]}}',
      '{"type":"Signal","data":{"slug":"sig-referral-analytics","summary":"warm referral for analytics migration project","embedding":[0.0,1.0,0.0]}}',
      '{"type":"Signal","data":{"slug":"sig-procurement","summary":"enterprise procurement questionnaire backlog and mitigation owner tracking","embedding":[0.0,0.0,1.0]}}',
    ].join("\n");

    const SEARCH_QUERIES = `
query keyword($q: String) {
  match {
    $s: Signal
    search($s.summary, $q)
  }
  return { $s.slug as slug }
  order { $s.slug asc }
}

query lexical($q: String) {
  match {
    $s: Signal
    match_text($s.summary, $q)
  }
  return { $s.slug as slug }
}

query fuzzy_q($q: String) {
  match {
    $s: Signal
    fuzzy($s.summary, $q)
  }
  return { $s.slug as slug }
  order { $s.slug asc }
}

query bm25_q($q: String) {
  match { $s: Signal }
  return { $s.slug as slug, bm25($s.summary, $q) as score }
  order { bm25($s.summary, $q) desc }
  limit 3
}

query nearest_q($vq: Vector(3)) {
  match { $s: Signal }
  return { $s.slug as slug, nearest($s.embedding, $vq) as score }
  order { nearest($s.embedding, $vq) }
  limit 3
}

query hybrid_rrf($vq: Vector(3), $tq: String) {
  match { $s: Signal }
  return {
    $s.slug as slug,
    rrf(nearest($s.embedding, $vq), bm25($s.summary, $tq), 60) as score
  }
  order { rrf(nearest($s.embedding, $vq), bm25($s.summary, $tq), 60) desc }
  limit 3
}
`;

    async function freshSearchDb() {
      const dbPath = join(tmpDir, `search-${++dbCounter}.nano`);
      const db = await Database.init(dbPath, SEARCH_SCHEMA);
      await db.load(SEARCH_DATA, "overwrite");
      return db;
    }

    it("keyword search matches tokens", async () => {
      const db = await freshSearchDb();
      const rows = await db.run(SEARCH_QUERIES, "keyword", {
        q: "billing missing",
      });
      assert.ok(rows.length >= 1);
      const slugs = rows.map((r) => r.slug);
      assert.ok(slugs.includes("sig-billing-delay"));
      await db.close();
    });

    it("match_text finds contiguous phrase", async () => {
      const db = await freshSearchDb();
      const rows = await db.run(SEARCH_QUERIES, "lexical", {
        q: "missing invoice",
      });
      assert.ok(rows.length >= 1);
      assert.equal(rows[0].slug, "sig-billing-delay");
      await db.close();
    });

    it("fuzzy search tolerates typos", async () => {
      const db = await freshSearchDb();
      const rows = await db.run(SEARCH_QUERIES, "fuzzy_q", {
        q: "reconciliaton delay",
      });
      assert.ok(rows.length >= 1);
      assert.equal(rows[0].slug, "sig-billing-delay");
      await db.close();
    });

    it("bm25 ranking returns scored results", async () => {
      const db = await freshSearchDb();
      const rows = await db.run(SEARCH_QUERIES, "bm25_q", {
        q: "billing missing invoice",
      });
      assert.equal(rows.length, 3);
      assert.equal(rows[0].slug, "sig-billing-delay");
      assert.equal(typeof rows[0].score, "number");
      // Scores should be descending
      assert.ok(rows[0].score >= rows[1].score);
      await db.close();
    });

    it("nearest vector ranks identical vector first", async () => {
      const db = await freshSearchDb();
      const rows = await db.run(SEARCH_QUERIES, "nearest_q", {
        vq: [1.0, 0.0, 0.0],
      });
      assert.equal(rows.length, 3);
      assert.equal(rows[0].slug, "sig-billing-delay");
      assert.equal(typeof rows[0].score, "number");
      await db.close();
    });

    it("hybrid rrf fuses vector and text ranking", async () => {
      const db = await freshSearchDb();
      const rows = await db.run(SEARCH_QUERIES, "hybrid_rrf", {
        vq: [1.0, 0.0, 0.0],
        tq: "billing missing invoice",
      });
      assert.equal(rows.length, 3);
      assert.equal(rows[0].slug, "sig-billing-delay");
      assert.equal(typeof rows[0].score, "number");
      assert.ok(rows[0].score >= rows[1].score);
      await db.close();
    });
  });

  describe("media helpers", () => {
    const MEDIA_SCHEMA = `
node PhotoAsset {
  slug: String @key
  uri: String @media_uri(mime)
  mime: String
  embedding: Vector(16) @embed(uri) @index
}

node Product {
  slug: String @key
  name: String
}

edge HasPhoto: Product -> PhotoAsset
`;

    const MEDIA_QUERIES = `
query photo_by_slug($slug: String) {
  match { $img: PhotoAsset { slug: $slug } }
  return { $img.slug as slug, $img.uri as uri, $img.mime as mime }
}

query products_from_image_search($q: String) {
  match {
    $product: Product
    $product hasPhoto $img
  }
  return { $product.slug as product, $img.slug as image }
  order { nearest($img.embedding, $q) }
  limit 1
}
`;

    async function freshMediaDb() {
      const dbPath = join(tmpDir, `media-${++dbCounter}.nano`);
      const db = await Database.init(dbPath, MEDIA_SCHEMA);
      return db;
    }

    async function withMockEmbeddings(fn) {
      const prev = process.env.NANOGRAPH_EMBEDDINGS_MOCK;
      process.env.NANOGRAPH_EMBEDDINGS_MOCK = "1";
      try {
        await fn();
      } finally {
        if (prev === undefined) {
          delete process.env.NANOGRAPH_EMBEDDINGS_MOCK;
        } else {
          process.env.NANOGRAPH_EMBEDDINGS_MOCK = prev;
        }
      }
    }

    const placeholderEmbedding = [1, ...Array(15).fill(0)];

    it("describe exposes mediaMimeProp for media URI properties", async () => {
      const db = await freshMediaDb();
      const info = await db.describe();
      const photo = info.nodeTypes.find((t) => t.name === "PhotoAsset");
      const uriProp = photo.properties.find((p) => p.name === "uri");
      assert.equal(uriProp.mediaMimeProp, "mime");
      await db.close();
    });

    it("loadRows imports media files and fills the mime property", async () => {
      const db = await freshMediaDb();
      const heroPath = await writeJpegAsset("hero");

      await db.loadRows(
        [
          {
            type: "PhotoAsset",
            data: {
              slug: "hero",
              uri: mediaFile(heroPath, "image/jpeg"),
              embedding: placeholderEmbedding,
            },
          },
        ],
        "overwrite",
      );

      const rows = await db.run(MEDIA_QUERIES, "photo_by_slug", { slug: "hero" });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].mime, "image/jpeg");
      assert.match(rows[0].uri, /^file:\/\//);
      await db.close();
    });

    it("embed backfills media embeddings and supports text-to-image traversal", async () => {
      const db = await freshMediaDb();
      const spacePath = await writeJpegAsset("space");
      const beachPath = await writeJpegAsset("beach");

      await db.loadRows(
        [
          {
            type: "PhotoAsset",
            data: {
              slug: "space",
              uri: mediaUri(pathToFileURL(spacePath).toString(), "image/jpeg"),
              embedding: placeholderEmbedding,
            },
          },
          {
            type: "PhotoAsset",
            data: {
              slug: "beach",
              uri: mediaUri(pathToFileURL(beachPath).toString(), "image/jpeg"),
              embedding: placeholderEmbedding,
            },
          },
          { type: "Product", data: { slug: "rocket", name: "Rocket Poster" } },
          { type: "Product", data: { slug: "sand", name: "Beach Poster" } },
          { edge: "HasPhoto", from: "rocket", to: "space" },
          { edge: "HasPhoto", from: "sand", to: "beach" },
        ],
        "overwrite",
      );

      await withMockEmbeddings(async () => {
        const onlyNull = await db.embed({
          typeName: "PhotoAsset",
          property: "embedding",
          onlyNull: true,
        });
        assert.equal(onlyNull.rowsSelected, 0);

        const result = await db.embed({
          typeName: "PhotoAsset",
          property: "embedding",
          reindex: true,
        });
        assert.equal(result.propertiesSelected, 1);
        assert.equal(result.embeddingsGenerated, 2);

        const rows = await db.run(MEDIA_QUERIES, "products_from_image_search", {
          q: "space",
        });
        assert.equal(rows.length, 1);
        assert.equal(rows[0].product, "rocket");
        assert.equal(rows[0].image, "space");
      });

      await db.close();
    });
  });

  // ---- error cases ----

  describe("error cases", () => {
    it("should reject bad schema", async () => {
      const dbPath = join(tmpDir, `bad-schema-${++dbCounter}.nano`);
      await assert.rejects(
        () => Database.init(dbPath, "this is not valid schema"),
        (err) => {
          assert.ok(err.message.length > 0);
          return true;
        },
      );
    });

    it("should reject invalid load mode", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.load(DATA, "invalid"),
        (err) => {
          assert.ok(err.message.includes("invalid load mode"));
          return true;
        },
      );
      await db.close();
    });

    it("should reject missing query name", async () => {
      const { db } = await freshDb();
      const q = `query exists() { match { $p: Person } return { $p.name } }`;
      await assert.rejects(
        () => db.run(q, "nonExistentQuery"),
        (err) => {
          assert.ok(err.message.includes("not found"));
          return true;
        },
      );
      await db.close();
    });

    it("should reject invalid query syntax", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () => db.run("not a valid query", "whatever"),
        (err) => {
          assert.ok(err.message.length > 0);
          return true;
        },
      );
      await db.close();
    });

    it("should reject run on closed database", async () => {
      const { db } = await freshDb();
      await db.close();
      // run() parses query before lock, but mutation path hits closed check
      const q = `query ins($name: String) { insert Person { name: $name } }`;
      await assert.rejects(
        () => db.run(q, "ins", { name: "X" }),
        (err) => {
          assert.ok(err.message.includes("closed"));
          return true;
        },
      );
    });

    it("should reject load on closed database", async () => {
      const { db } = await freshDb();
      await db.close();
      await assert.rejects(
        () => db.load(DATA, "overwrite"),
        (err) => {
          assert.ok(err.message.includes("closed"));
          return true;
        },
      );
    });

    it("should reject describe on closed database", async () => {
      const { db } = await freshDb();
      await db.close();
      await assert.rejects(
        () => db.describe(),
        (err) => {
          assert.ok(err.message.includes("closed"));
          return true;
        },
      );
    });

    it("should reject doctor on closed database", async () => {
      const { db } = await freshDb();
      await db.close();
      await assert.rejects(
        () => db.doctor(),
        (err) => {
          assert.ok(err.message.includes("closed"));
          return true;
        },
      );
    });

    it("close on already-closed db is a no-op", async () => {
      const { db } = await freshDb();
      await db.close();
      // Second close should not throw
      await db.close();
    });

    it("should reject unsafe U64 number params", async () => {
      const { db } = await freshDb();
      await assert.rejects(
        () =>
          db.run(U64_QUERY, "validateU64", {
            v: Number.MAX_SAFE_INTEGER + 1,
          }),
        (err) => {
          assert.ok(err.message.includes("decimal string"));
          return true;
        },
      );
      await db.close();
    });

    it("should accept U64 decimal string params", async () => {
      const { db } = await freshDb();
      const rows = await db.run(U64_QUERY, "validateU64", {
        v: "9007199254740993",
      });
      assert.equal(rows.length, 1);
      await db.close();
    });

    it("should reject non-existent db path for open", async () => {
      await assert.rejects(
        () => Database.open(join(tmpDir, "does-not-exist.nano")),
        (err) => {
          assert.ok(err.message.length > 0);
          return true;
        },
      );
    });
  });
});
