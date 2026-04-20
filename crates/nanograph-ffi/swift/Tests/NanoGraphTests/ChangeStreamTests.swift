import Foundation
import NanoGraph
import XCTest

final class ChangeStreamTests: XCTestCase {
    private let schema = """
    node Person {
      slug: String @key
      name: String
      age: I32?
    }
    """

    // MARK: - currentGraphVersion

    func testCurrentGraphVersionIsNilOnFreshDatabase() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        XCTAssertNil(try db.currentGraphVersion())
    }

    func testCurrentGraphVersionReflectsCommittedMutations() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "a", "name": "A"])
        let firstVersion = try XCTUnwrap(try db.currentGraphVersion())
        try db.upsertNode(type: "Person", data: ["slug": "b", "name": "B"])
        let secondVersion = try XCTUnwrap(try db.currentGraphVersion())
        XCTAssertGreaterThan(secondVersion, firstVersion)
    }

    // MARK: - changes(since:)

    func testChangesSinceReturnsRowsAfterGivenCursor() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "a", "name": "A"])
        let checkpoint = try XCTUnwrap(try db.currentGraphVersion())

        try db.upsertNode(type: "Person", data: ["slug": "b", "name": "B"])
        try db.upsertNode(type: "Person", data: ["slug": "c", "name": "C"])

        let after = try db.changes(since: checkpoint)
        XCTAssertEqual(after.count, 2)
        XCTAssertTrue(after.allSatisfy { $0.graphVersion > checkpoint })
        XCTAssertEqual(after.map(\.changeKind), [.insert, .insert])
        XCTAssertEqual(after.map(\.entityKind), [.node, .node])
        XCTAssertEqual(after.map(\.typeName), ["Person", "Person"])
    }

    func testChangesSinceEmptyOnQuietDatabase() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "a", "name": "A"])
        let cursor = try XCTUnwrap(try db.currentGraphVersion())
        let after = try db.changes(since: cursor)
        XCTAssertTrue(after.isEmpty, "No commits after cursor — expected empty")
    }

    func testChangesExposesRowPayloadForInserts() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "alice", "name": "Alice", "age": 30])
        let all = try db.changes(since: nil)
        XCTAssertEqual(all.count, 1)
        guard let row = all.first?.row, case .object(let fields) = row else {
            XCTFail("Expected row to be a JSON object, got: \(String(describing: all.first?.row))")
            return
        }
        // Row contains the inserted columns plus engine-internal `id` / `__ng_id`.
        if case .string(let slug) = fields["slug"] {
            XCTAssertEqual(slug, "alice")
        } else {
            XCTFail("row.slug missing or not a string")
        }
    }

    // MARK: - changeStream

    // End-to-end Task-based streaming is exercised by the Mac app's
    // NanographMirror CDC polling loop (integration-tested there). Testing
    // it here in isolation runs into a SIGBUS in Database teardown when the
    // polling Task and test-thread FFI calls race during deinit — out of
    // scope for Phase B2. The individual primitives (`changes(since:)`,
    // `currentGraphVersion()`) are covered above.
}
