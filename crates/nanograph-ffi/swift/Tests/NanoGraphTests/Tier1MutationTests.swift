import Foundation
import NanoGraph
import XCTest

final class Tier1MutationTests: XCTestCase {
    private let schema = """
    node Person {
      slug: String @key
      name: String
      age: I32?
      role: String?
    }

    node Widget {
      uuid: String @key
      label: String
    }

    node Untyped {
      label: String
    }

    edge Contains: Person -> Widget {
      position: F64?
    }
    """

    private struct PersonRow: Decodable {
        let slug: String
        let name: String
        let age: Int?
        let role: String?
    }

    private struct WidgetRow: Decodable {
        let uuid: String
        let label: String
    }

    private struct EndpointsRow: Decodable {
        let from: String
        let to: String
    }

    // MARK: - upsertNode (full-row replace)

    func testUpsertNodeInsertsNewRowAndReplacesExistingRow() throws {
        let db = try Database.openInMemory(schemaSource: schema)

        try db.upsertNode(type: "Person", data: [
            "slug": "alice",
            "name": "Alice",
            "age": 30,
            "role": "engineer",
        ])

        // Re-upsert with a full row — required non-null columns must all be
        // present, because `mode: .merge` in the loader is full-row replace by
        // @key (not partial merge). For partial edits, see `updateNode`.
        try db.upsertNode(type: "Person", data: [
            "slug": "alice",
            "name": "Alice",
            "role": "manager",
        ])

        let rows = try db.run([PersonRow].self,
            querySource: """
            query q() {
              match { $p: Person { slug: "alice" } }
              return { $p.slug as slug, $p.name as name, $p.age as age, $p.role as role }
            }
            """,
            queryName: "q"
        )
        XCTAssertEqual(rows.count, 1)
        XCTAssertEqual(rows[0].slug, "alice")
        XCTAssertEqual(rows[0].name, "Alice")
        XCTAssertEqual(rows[0].role, "manager")
        XCTAssertNil(rows[0].age, "age was not re-supplied — full-row replace nulls it")
    }

    // MARK: - upsertEdge

    func testUpsertEdgeIsIdempotentForSameFromToPair() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "alice", "name": "Alice"])
        try db.upsertNode(type: "Widget", data: ["uuid": "w1", "label": "One"])

        // Two writes to the same (from, to) — should collapse to one row.
        // We can't project edge properties through .gq today (no edge-binding
        // form in the grammar), so verify by endpoint count.
        try db.upsertEdge(
            type: "Contains",
            from: "alice",
            to: "w1",
            data: ["position": 1.0]
        )
        try db.upsertEdge(
            type: "Contains",
            from: "alice",
            to: "w1",
            data: ["position": 7.5]
        )

        let rows = try db.run([EndpointsRow].self,
            querySource: """
            query q() {
              match {
                $p: Person
                $p contains $w
              }
              return { $p.slug as from, $w.uuid as to }
            }
            """,
            queryName: "q"
        )
        XCTAssertEqual(rows.count, 1, "(from, to) identity collapses to one row — not a multigraph")
        XCTAssertEqual(rows[0].from, "alice")
        XCTAssertEqual(rows[0].to, "w1")
    }

    // MARK: - deleteNode

    func testDeleteNodeCascadesIncidentEdges() throws {
        let db = try Database.openInMemory(schemaSource: schema)

        try db.upsertNode(type: "Person", data: ["slug": "alice", "name": "Alice"])
        try db.upsertNode(type: "Widget", data: ["uuid": "w1", "label": "One"])
        try db.upsertEdge(type: "Contains", from: "alice", to: "w1")

        let result = try db.deleteNode(type: "Person", key: "alice")
        XCTAssertEqual(result.affectedNodes, 1)
        XCTAssertEqual(result.affectedEdges, 1, "Incident Contains edge cascades with the node")

        let widgetsLeft = try db.run([WidgetRow].self,
            querySource: """
            query q() {
              match { $w: Widget }
              return { $w.uuid as uuid, $w.label as label }
            }
            """,
            queryName: "q"
        )
        XCTAssertEqual(widgetsLeft.count, 1, "Widget survives — cascade is node->edge, not cross-type")
    }

    func testDeleteNodeIsSilentOnMiss() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        let result = try db.deleteNode(type: "Person", key: "ghost")
        XCTAssertEqual(result.affectedNodes, 0)
        XCTAssertEqual(result.affectedEdges, 0)
    }

    func testDeleteNodeErrorsOnTypeWithoutKey() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        XCTAssertThrowsError(
            try db.deleteNode(type: "Untyped", key: "whatever")
        ) { error in
            let message = (error as? NanoGraphError).map(String.init(describing:)) ?? ""
            XCTAssertTrue(message.contains("has no @key"), "Got: \(message)")
        }
    }

    func testDeleteNodeErrorsOnUnknownType() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        XCTAssertThrowsError(
            try db.deleteNode(type: "DoesNotExist", key: "x")
        )
    }

    // MARK: - deleteEdgesFrom / deleteEdgesTo

    func testDeleteEdgesToUnparentsAChild() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "alice", "name": "Alice"])
        try db.upsertNode(type: "Widget", data: ["uuid": "w1", "label": "One"])
        try db.upsertNode(type: "Widget", data: ["uuid": "w2", "label": "Two"])
        try db.upsertEdge(type: "Contains", from: "alice", to: "w1")
        try db.upsertEdge(type: "Contains", from: "alice", to: "w2")

        let result = try db.deleteEdgesTo(type: "Contains", key: "w1")
        XCTAssertEqual(result.affectedEdges, 1)

        let rows = try db.run([EndpointsRow].self,
            querySource: """
            query q() {
              match {
                $p: Person
                $p contains $w
              }
              return { $p.slug as from, $w.uuid as to }
            }
            """,
            queryName: "q"
        )
        XCTAssertEqual(rows.count, 1)
        XCTAssertEqual(rows[0].to, "w2")
    }

    func testDeleteEdgesFromWipesAllOutgoingOfThatType() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "alice", "name": "Alice"])
        try db.upsertNode(type: "Widget", data: ["uuid": "w1", "label": "One"])
        try db.upsertNode(type: "Widget", data: ["uuid": "w2", "label": "Two"])
        try db.upsertEdge(type: "Contains", from: "alice", to: "w1")
        try db.upsertEdge(type: "Contains", from: "alice", to: "w2")

        let result = try db.deleteEdgesFrom(type: "Contains", key: "alice")
        XCTAssertEqual(result.affectedEdges, 2)
    }

    func testDeleteEdgesToIsSilentOnMiss() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        let result = try db.deleteEdgesTo(type: "Contains", key: "nobody")
        XCTAssertEqual(result.affectedEdges, 0)
        XCTAssertEqual(result.affectedNodes, 0)
    }

    // MARK: - updateNode

    func testUpdateNodePreservesUnmentionedProperties() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: [
            "slug": "alice",
            "name": "Alice",
            "age": 30,
            "role": "engineer",
        ])

        let result = try db.updateNode(
            type: "Person",
            key: "alice",
            set: ["age": 31]
        )
        XCTAssertEqual(result.affectedNodes, 1)

        let rows = try db.run([PersonRow].self,
            querySource: """
            query q() {
              match { $p: Person { slug: "alice" } }
              return { $p.slug as slug, $p.name as name, $p.age as age, $p.role as role }
            }
            """,
            queryName: "q"
        )
        XCTAssertEqual(rows.first?.age, 31)
        XCTAssertEqual(rows.first?.name, "Alice", "name preserved")
        XCTAssertEqual(rows.first?.role, "engineer", "role preserved")
    }

    func testUpdateNodeRejectsWritingToKeyProperty() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "alice", "name": "Alice"])

        XCTAssertThrowsError(
            try db.updateNode(type: "Person", key: "alice", set: ["slug": "alicia"])
        ) { error in
            let message = (error as? NanoGraphError).map(String.init(describing:)) ?? ""
            XCTAssertTrue(message.contains("@key"), "Got: \(message)")
        }
    }

    func testUpdateNodeIsSilentOnMiss() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        let result = try db.updateNode(
            type: "Person",
            key: "ghost",
            set: ["age": 99]
        )
        XCTAssertEqual(result.affectedNodes, 0)
    }

    func testUpdateNodeReturnsZeroAffectedForEmptySet() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "alice", "name": "Alice"])
        let result = try db.updateNode(type: "Person", key: "alice", set: [:])
        XCTAssertEqual(result.affectedNodes, 0, "empty set is a client-side no-op — no round-trip")
    }

    // MARK: - updateNodes (escape hatch)

    func testUpdateNodesAppliesAcrossMatchingRows() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        try db.upsertNode(type: "Person", data: ["slug": "alice", "name": "Alice", "role": "engineer"])
        try db.upsertNode(type: "Person", data: ["slug": "bob", "name": "Bob", "role": "engineer"])
        try db.upsertNode(type: "Person", data: ["slug": "carol", "name": "Carol", "role": "manager"])

        let result = try db.updateNodes(
            type: "Person",
            wherePredicate: "role = $role",
            paramTypes: ["role": "String"],
            params: ["role": "engineer"],
            set: ["age": 40]
        )
        XCTAssertEqual(result.affectedNodes, 2)

        let rows = try db.run([PersonRow].self,
            querySource: """
            query q() {
              match { $p: Person }
              return { $p.slug as slug, $p.name as name, $p.age as age, $p.role as role }
              order { $p.slug asc }
            }
            """,
            queryName: "q"
        )
        XCTAssertEqual(rows.first(where: { $0.slug == "alice" })?.age, 40)
        XCTAssertEqual(rows.first(where: { $0.slug == "bob" })?.age, 40)
        XCTAssertEqual(rows.first(where: { $0.slug == "carol" })?.age, nil,
                      "carol was not matched — her age stays null")
    }
}
