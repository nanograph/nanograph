import Foundation
import NanoGraph
import XCTest

final class DatabaseTests: XCTestCase {
    private struct CheckRow: Decodable {
        let name: String
        let kind: String
        let status: String
        let error: String?
    }

    private struct DescribeResult: Decodable {
        struct TypeDef: Decodable {
            let name: String
        }

        let nodeTypes: [TypeDef]
        let edgeTypes: [TypeDef]
    }

    private let schema = """
    node Person {
      name: String @key
      age: I32?
      role: enum(engineer, manager)?
      active: Bool?
    }
    """

    private let data = [
        #"{"type":"Person","data":{"name":"Alice","age":30,"role":"engineer","active":true}}"#,
        #"{"type":"Person","data":{"name":"Bob","age":25,"role":"manager","active":false}}"#,
    ].joined(separator: "\n")

    private let queries = """
    query allPeople() {
      match { $p: Person }
      return { $p.name as name, $p.age as age }
      order { $p.name asc }
    }

    query byRole($role: String) {
      match { $p: Person { role: $role } }
      return { $p.name as name }
    }

    query addPerson($name: String, $age: I32) {
      insert Person { name: $name, age: $age }
    }
    """

    private let searchSchema = """
    node Signal {
      slug: String @key
      summary: String
      embedding: Vector(3) @index
    }
    """

    private let searchData = [
        #"{"type":"Signal","data":{"slug":"sig-billing-delay","summary":"billing reconciliation delay due to missing invoice data","embedding":[1.0,0.0,0.0]}}"#,
        #"{"type":"Signal","data":{"slug":"sig-referral-analytics","summary":"warm referral for analytics migration project","embedding":[0.0,1.0,0.0]}}"#,
        #"{"type":"Signal","data":{"slug":"sig-procurement","summary":"enterprise procurement questionnaire backlog and mitigation owner tracking","embedding":[0.0,0.0,1.0]}}"#,
    ].joined(separator: "\n")

    private let searchQueries = """
    query keyword($q: String) {
      match {
        $s: Signal
        search($s.summary, $q)
      }
      return { $s.slug as slug }
      order { $s.slug asc }
    }

    query nearest_q($vq: Vector(3)) {
      match { $s: Signal }
      return { $s.slug as slug, nearest($s.embedding, $vq) as score }
      order { nearest($s.embedding, $vq) }
      limit 3
    }
    """

    private let mediaSchema = """
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
    """

    private let mediaQueries = """
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
    """

    private let placeholderEmbedding = [1.0] + Array(repeating: 0.0, count: 15)

    func testInitLoadRunAndTypedDecode() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        XCTAssertFalse(try db.isInMemory())

        let raw = try db.run(querySource: queries, queryName: "allPeople")
        let rows = try castRows(raw)
        XCTAssertEqual(rows.count, 2)
        XCTAssertEqual(rows.compactMap { $0["name"] as? String }, ["Alice", "Bob"])

        struct PersonRow: Decodable {
            let name: String
            let age: Int?
        }
        let typedRows = try db.run([PersonRow].self, querySource: queries, queryName: "allPeople")
        XCTAssertEqual(typedRows.map(\.name), ["Alice", "Bob"])
        XCTAssertEqual(typedRows.map(\.age), [30, 25])
    }

    func testOpenInMemoryLoadsAndQueries() throws {
        let db = try Database.openInMemory(schemaSource: schema)
        defer { try? db.close() }

        XCTAssertTrue(try db.isInMemory())
        try db.load(dataSource: data, mode: .overwrite)

        let raw = try db.run(querySource: queries, queryName: "allPeople")
        let rows = try castRows(raw)
        XCTAssertEqual(rows.count, 2)
        XCTAssertEqual(rows.compactMap { $0["name"] as? String }, ["Alice", "Bob"])
    }

    func testErrorDescriptionExposesMessage() {
        let error = NanoGraphError.message("boom")
        XCTAssertEqual(error.errorDescription, "boom")
    }

    func testCloseIsIdempotentAndBlocksFurtherCalls() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }

        try db.close()
        XCTAssertNoThrow(try db.close())
        XCTAssertThrowsError(try db.describe()) { error in
            XCTAssertTrue(errorMessage(error).contains("Database is closed"))
        }
    }

    func testRunRejectsNonStringForStringParam() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        XCTAssertThrowsError(
            try db.run(querySource: queries, queryName: "byRole", params: ["role": 42])
        ) { error in
            XCTAssertTrue(errorMessage(error).contains("expected string"))
        }
    }

    func testCompactRejectsUnknownOptions() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        XCTAssertThrowsError(try db.compact(options: ["targetRowsPerFrament": 512])) { error in
            XCTAssertTrue(errorMessage(error).contains("unknown compact option"))
        }
    }

    func testCleanupRejectsUnknownOptions() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        XCTAssertThrowsError(try db.cleanup(options: ["retainTxVersionz": 10])) { error in
            XCTAssertTrue(errorMessage(error).contains("unknown cleanup option"))
        }
    }

    func testCreateRejectsInvalidSchema() {
        let dbPath = tempPath(prefix: "nanograph-swift-invalid-schema")
        defer { cleanup(dbPath: dbPath) }

        XCTAssertThrowsError(
            try Database.create(
                dbPath: dbPath,
                schemaSource: "node Person { name: }"
            )
        ) { error in
            XCTAssertFalse(errorMessage(error).isEmpty)
        }
    }

    func testOpenRejectsMissingDatabase() {
        let missingPath = tempPath(prefix: "nanograph-swift-missing")
        XCTAssertThrowsError(try Database.open(dbPath: missingPath)) { error in
            XCTAssertFalse(errorMessage(error).isEmpty)
        }
    }

    func testLoadRejectsInvalidDataSource() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        XCTAssertThrowsError(try db.load(dataSource: "not-jsonl", mode: .append)) { error in
            XCTAssertFalse(errorMessage(error).isEmpty)
        }
    }

    func testLoadFileMatchesStringLoad() throws {
        let dbPath = tempPath(prefix: "nanograph-swift-load-file")
        let db = try Database.create(dbPath: dbPath, schemaSource: schema)
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let dataPath = tempPath(prefix: "nanograph-swift-data")
        defer { cleanup(dbPath: dataPath) }
        try data.write(toFile: dataPath, atomically: true, encoding: .utf8)

        try db.loadFile(dataPath: dataPath, mode: .overwrite)
        let raw = try db.run(querySource: queries, queryName: "allPeople")
        let rows = try castRows(raw)
        XCTAssertEqual(rows.count, 2)
        XCTAssertEqual(rows.compactMap { $0["name"] as? String }, ["Alice", "Bob"])
    }

    func testRunRejectsNonJSONParams() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        XCTAssertThrowsError(
            try db.run(querySource: queries, queryName: "byRole", params: ["role": Date()])
        ) { error in
            XCTAssertTrue(errorMessage(error).contains("Value is not valid JSON"))
        }
    }

    func testTypedCheckAndDescribeDecode() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let checks = try db.check([CheckRow].self, querySource: queries)
        XCTAssertEqual(checks.count, 3)
        XCTAssertTrue(checks.allSatisfy { $0.status == "ok" })

        let describe = try db.describe(DescribeResult.self)
        XCTAssertEqual(describe.nodeTypes.first?.name, "Person")
        XCTAssertEqual(describe.edgeTypes.count, 0)
    }

    func testDeinitWithoutClose() throws {
        let dbPath = tempPath(prefix: "nanograph-swift-deinit")
        do {
            let db = try Database.create(dbPath: dbPath, schemaSource: schema)
            try db.load(dataSource: data, mode: .overwrite)
        }
        cleanup(dbPath: dbPath)
    }

    func testCheckReturnsStatuses() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let raw = try db.check(querySource: queries)
        let checks = try castRows(raw)
        XCTAssertEqual(checks.count, 3)
        XCTAssertTrue(checks.allSatisfy { ($0["status"] as? String) == "ok" })
        XCTAssertEqual((checks[0]["kind"] as? String), "read")
    }

    func testDescribeReturnsSchema() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let raw = try db.describe()
        let describe = try castObject(raw)
        let nodeTypes = try XCTUnwrap(describe["nodeTypes"] as? [[String: Any]])
        let person = try XCTUnwrap(nodeTypes.first { ($0["name"] as? String) == "Person" })
        let properties = try XCTUnwrap(person["properties"] as? [[String: Any]])
        XCTAssertTrue(properties.contains { ($0["name"] as? String) == "name" })
    }

    func testDoctorReturnsHealthReport() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let raw = try db.doctor()
        let report = try castObject(raw)
        XCTAssertEqual(report["healthy"] as? Bool, true)
        XCTAssertNotNil(report["datasetsChecked"] as? Int)
        XCTAssertNotNil(report["txRows"] as? Int)
        XCTAssertNotNil(report["cdcRows"] as? Int)
    }

    func testRunArrowReturnsDataForReadQueries() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let data = try db.runArrow(querySource: queries, queryName: "allPeople")
        XCTAssertFalse(data.isEmpty)

        let raw = try decodeArrow(data)
        let rows = try castRows(raw)
        XCTAssertEqual(rows.count, 2)
        XCTAssertEqual(rows.compactMap { $0["name"] as? String }, ["Alice", "Bob"])
    }

    func testDecodeArrowSupportsTypedDecode() throws {
        struct PersonRow: Decodable {
            let name: String
            let age: Int?
        }

        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let data = try db.runArrow(querySource: queries, queryName: "allPeople")
        let rows = try decodeArrow([PersonRow].self, from: data)
        XCTAssertEqual(rows.map(\.name), ["Alice", "Bob"])
        XCTAssertEqual(rows.map(\.age), [30, 25])
    }

    func testRunArrowRejectsMutations() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        XCTAssertThrowsError(
            try db.runArrow(
                querySource: queries,
                queryName: "addPerson",
                params: ["name": "Carol", "age": 28]
            )
        ) { error in
            XCTAssertTrue(errorMessage(error).contains("runArrow only supports read queries"))
        }
    }

    func testMutationsPersistAcrossReopen() throws {
        let (db, dbPath) = try freshDatabase()
        _ = try db.run(
            querySource: queries,
            queryName: "addPerson",
            params: ["name": "Carol", "age": 28]
        )
        try db.close()

        let reopened = try Database.open(dbPath: dbPath)
        defer {
            try? reopened.close()
            cleanup(dbPath: dbPath)
        }

        let raw = try reopened.run(querySource: queries, queryName: "allPeople")
        let rows = try castRows(raw)
        XCTAssertEqual(rows.count, 3)
        XCTAssertEqual(rows.compactMap { $0["name"] as? String }, ["Alice", "Bob", "Carol"])
    }

    func testKeywordSearchMatchesTokens() throws {
        let (db, dbPath) = try freshSearchDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let raw = try db.run(querySource: searchQueries, queryName: "keyword", params: ["q": "billing missing"])
        let rows = try castRows(raw)
        XCTAssertFalse(rows.isEmpty)
        XCTAssertTrue(rows.contains { ($0["slug"] as? String) == "sig-billing-delay" })
    }

    func testNearestVectorRanksIdenticalVectorFirst() throws {
        let (db, dbPath) = try freshSearchDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let raw = try db.run(
            querySource: searchQueries,
            queryName: "nearest_q",
            params: ["vq": [1.0, 0.0, 0.0]]
        )
        let rows = try castRows(raw)
        XCTAssertEqual(rows.count, 3)
        XCTAssertEqual(rows[0]["slug"] as? String, "sig-billing-delay")
        XCTAssertNotNil(rows[0]["score"] as? Double)
    }

    func testDescribeIncludesMediaMimeProp() throws {
        let dbPath = tempPath(prefix: "nanograph-swift-media-describe")
        let db = try Database.create(dbPath: dbPath, schemaSource: mediaSchema)
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let raw = try db.describe()
        let describe = try castObject(raw)
        let nodeTypes = try XCTUnwrap(describe["nodeTypes"] as? [[String: Any]])
        let photo = try XCTUnwrap(nodeTypes.first { ($0["name"] as? String) == "PhotoAsset" })
        let properties = try XCTUnwrap(photo["properties"] as? [[String: Any]])
        let uri = try XCTUnwrap(properties.first { ($0["name"] as? String) == "uri" })
        XCTAssertEqual(uri["mediaMimeProp"] as? String, "mime")
    }

    func testLoadRowsImportsMediaFilesAndFillsMime() throws {
        let (db, dbPath) = try freshMediaDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let assetsDir = tempPath(prefix: "nanograph-swift-media-assets")
        let heroPath = URL(fileURLWithPath: assetsDir).appendingPathComponent("hero.jpg").path
        defer { cleanup(dbPath: assetsDir) }
        try writeJpegAsset(heroPath)

        try db.loadRows([
            .node(type: "PhotoAsset", data: [
                "slug": "hero",
                "uri": MediaRef.file(heroPath, mimeType: "image/jpeg"),
                "embedding": placeholderEmbedding,
            ])
        ], mode: .overwrite)

        let raw = try db.run(
            querySource: mediaQueries,
            queryName: "photo_by_slug",
            params: ["slug": "hero"]
        )
        let rows = try castRows(raw)
        XCTAssertEqual(rows.count, 1)
        XCTAssertEqual(rows[0]["mime"] as? String, "image/jpeg")
        XCTAssertTrue((rows[0]["uri"] as? String)?.hasPrefix("file://") == true)
    }

    func testEmbedBackfillsMediaEmbeddingsAndTraversesFromImages() throws {
        let (db, dbPath) = try freshMediaDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let assetsDir = tempPath(prefix: "nanograph-swift-media-assets")
        let assetsURL = URL(fileURLWithPath: assetsDir)
        let spacePath = assetsURL.appendingPathComponent("space.jpg").path
        let beachPath = assetsURL.appendingPathComponent("beach.jpg").path
        defer { cleanup(dbPath: assetsDir) }
        try writeJpegAsset(spacePath)
        try writeJpegAsset(beachPath)

        try db.loadRows([
            .node(type: "PhotoAsset", data: [
                "slug": "space",
                "uri": MediaRef.uri(URL(fileURLWithPath: spacePath).absoluteString, mimeType: "image/jpeg"),
                "embedding": placeholderEmbedding,
            ]),
            .node(type: "PhotoAsset", data: [
                "slug": "beach",
                "uri": MediaRef.uri(URL(fileURLWithPath: beachPath).absoluteString, mimeType: "image/jpeg"),
                "embedding": placeholderEmbedding,
            ]),
            .node(type: "Product", data: ["slug": "rocket", "name": "Rocket Poster"]),
            .node(type: "Product", data: ["slug": "sand", "name": "Beach Poster"]),
            .edge(type: "HasPhoto", from: "rocket", to: "space", data: [:]),
            .edge(type: "HasPhoto", from: "sand", to: "beach", data: [:]),
        ], mode: .overwrite)

        try withMockEmbeddings {
            let onlyNull = try db.embed(EmbedResult.self, options: EmbedOptions(
                typeName: "PhotoAsset",
                property: "embedding",
                onlyNull: true
            ))
            XCTAssertEqual(onlyNull.rowsSelected, 0)

            let result = try db.embed(EmbedResult.self, options: EmbedOptions(
                typeName: "PhotoAsset",
                property: "embedding",
                reindex: true
            ))
            XCTAssertEqual(result.propertiesSelected, 1)
            XCTAssertEqual(result.embeddingsGenerated, 2)

            let raw = try db.run(
                querySource: mediaQueries,
                queryName: "products_from_image_search",
                params: ["q": "space"]
            )
            let rows = try castRows(raw)
            XCTAssertEqual(rows.count, 1)
            XCTAssertEqual(rows[0]["product"] as? String, "rocket")
            XCTAssertEqual(rows[0]["image"] as? String, "space")
        }
    }

    func testOpenPreservesData() throws {
        let (db, dbPath) = try freshDatabase()
        try db.close()

        let reopened = try Database.open(dbPath: dbPath)
        defer {
            try? reopened.close()
            cleanup(dbPath: dbPath)
        }

        let raw = try reopened.run(querySource: queries, queryName: "allPeople")
        let rows = try castRows(raw)
        XCTAssertEqual(rows.count, 2)
    }

    func testCompactAndCleanupDefaultOptions() throws {
        let (db, dbPath) = try freshDatabase()
        defer { cleanup(dbPath: dbPath) }
        defer { try? db.close() }

        let compactRaw = try db.compact()
        let compact = try castObject(compactRaw)
        XCTAssertNotNil(compact["datasetsConsidered"] as? Int)
        XCTAssertNotNil(compact["datasetsCompacted"] as? Int)
        XCTAssertNotNil(compact["manifestCommitted"] as? Bool)

        let cleanupRaw = try db.cleanup()
        let cleanup = try castObject(cleanupRaw)
        XCTAssertNotNil(cleanup["txRowsRemoved"] as? Int)
        XCTAssertNotNil(cleanup["datasetsCleaned"] as? Int)
        XCTAssertNotNil(cleanup["datasetBytesRemoved"] as? Int)
    }

    private func freshDatabase() throws -> (Database, String) {
        let dbPath = tempPath(prefix: "nanograph-swift")
        let db = try Database.create(dbPath: dbPath, schemaSource: schema)
        try db.load(dataSource: data, mode: .overwrite)
        return (db, dbPath)
    }

    private func freshSearchDatabase() throws -> (Database, String) {
        let dbPath = tempPath(prefix: "nanograph-swift-search")
        let db = try Database.create(dbPath: dbPath, schemaSource: searchSchema)
        try db.load(dataSource: searchData, mode: .overwrite)
        return (db, dbPath)
    }

    private func freshMediaDatabase() throws -> (Database, String) {
        let dbPath = tempPath(prefix: "nanograph-swift-media")
        let db = try Database.create(dbPath: dbPath, schemaSource: mediaSchema)
        return (db, dbPath)
    }

    private func tempPath(prefix: String) -> String {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("\(prefix)-\(UUID().uuidString)")
            .path
    }

    private func castRows(_ value: Any) throws -> [[String: Any]] {
        try XCTUnwrap(value as? [[String: Any]], "Expected [[String: Any]], got \(type(of: value))")
    }

    private func castObject(_ value: Any) throws -> [String: Any] {
        try XCTUnwrap(value as? [String: Any], "Expected [String: Any], got \(type(of: value))")
    }

    private func errorMessage(_ error: Error) -> String {
        if let nanoError = error as? NanoGraphError,
           case .message(let message) = nanoError
        {
            return message
        }
        return String(describing: error)
    }

    private func cleanup(dbPath: String) {
        try? FileManager.default.removeItem(atPath: dbPath)
    }

    private func writeJpegAsset(_ path: String) throws {
        let parent = URL(fileURLWithPath: path).deletingLastPathComponent()
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        let bytes = Data([0xFF, 0xD8, 0xFF, 0xD9])
        try bytes.write(to: URL(fileURLWithPath: path))
    }

    private func withMockEmbeddings<T>(_ body: () throws -> T) throws -> T {
        let name = "NANOGRAPH_EMBEDDINGS_MOCK"
        let previous = getenv(name).map { String(cString: $0) }
        setenv(name, "1", 1)
        defer {
            if let previous {
                setenv(name, previous, 1)
            } else {
                unsetenv(name)
            }
        }
        return try body()
    }
}
