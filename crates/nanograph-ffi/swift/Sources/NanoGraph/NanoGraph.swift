import CNanoGraph
import Foundation

public enum NanoGraphError: Error, LocalizedError {
    case message(String)

    public var errorDescription: String? {
        switch self {
        case .message(let message):
            return message
        }
    }
}

public func decodeArrow(_ data: Data) throws -> Any {
    guard !data.isEmpty else {
        throw NanoGraphError.message("Arrow IPC payload is empty")
    }

    let ptr = data.withUnsafeBytes { rawBuffer -> UnsafeMutablePointer<CChar>? in
        guard let base = rawBuffer.bindMemory(to: UInt8.self).baseAddress else {
            return nil
        }
        return nanograph_arrow_to_json(base, UInt(rawBuffer.count))
    }
    return try decodeOwnedJSONString(ptr)
}

public func decodeArrow<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
    let raw = try decodeArrow(data)
    return try decodeValue(type, from: raw)
}

public enum LoadMode: String {
    case overwrite
    case append
    case merge
}

public struct EmbedOptions {
    public var typeName: String?
    public var property: String?
    public var onlyNull: Bool
    public var limit: Int?
    public var reindex: Bool
    public var dryRun: Bool

    public init(
        typeName: String? = nil,
        property: String? = nil,
        onlyNull: Bool = false,
        limit: Int? = nil,
        reindex: Bool = false,
        dryRun: Bool = false
    ) {
        self.typeName = typeName
        self.property = property
        self.onlyNull = onlyNull
        self.limit = limit
        self.reindex = reindex
        self.dryRun = dryRun
    }

    fileprivate func jsonObject() -> [String: Any] {
        var object: [String: Any] = [
            "onlyNull": onlyNull,
            "reindex": reindex,
            "dryRun": dryRun,
        ]
        if let typeName {
            object["typeName"] = typeName
        }
        if let property {
            object["property"] = property
        }
        if let limit {
            object["limit"] = limit
        }
        return object
    }
}

public struct EmbedResult: Decodable {
    public let nodeTypesConsidered: Int
    public let propertiesSelected: Int
    public let rowsSelected: Int
    public let embeddingsGenerated: Int
    public let reindexedTypes: Int
    public let dryRun: Bool
}

public enum MediaRef {
    case file(String, mimeType: String? = nil)
    case base64(String, mimeType: String? = nil)
    case uri(String, mimeType: String? = nil)

    fileprivate var mimeType: String? {
        switch self {
        case .file(_, let mimeType), .base64(_, let mimeType), .uri(_, let mimeType):
            return mimeType
        }
    }

    fileprivate func encodedValue() -> String {
        switch self {
        case .file(let path, _):
            return "@file:\(absoluteFilePath(path))"
        case .base64(let data, _):
            return "@base64:\(data)"
        case .uri(let uri, _):
            return "@uri:\(uri)"
        }
    }
}

public enum LoadRow {
    case node(type: String, data: [String: Any])
    case edge(type: String, from: String, to: String, data: [String: Any] = [:])
}

private struct SchemaDescribeMetadata: Decodable {
    struct TypeDef: Decodable {
        let name: String
        let keyProperty: String?
        let endpointKeys: EndpointKeys?
        let srcType: String?
        let dstType: String?
        let properties: [Property]
    }

    struct EndpointKeys: Decodable {
        let src: String?
        let dst: String?
    }

    struct Property: Decodable {
        let name: String
        let type: String?
        let mediaMimeProp: String?
    }

    let nodeTypes: [TypeDef]
    let edgeTypes: [TypeDef]
}

public struct MutationResult: Decodable, Sendable {
    public let affectedNodes: Int
    public let affectedEdges: Int

    public init(affectedNodes: Int, affectedEdges: Int) {
        self.affectedNodes = affectedNodes
        self.affectedEdges = affectedEdges
    }
}

/// A single committed mutation in the CDC log.
///
/// Mirrors the Rust-side `VisibleChangeRow` in
/// `crates/nanograph/src/store/txlog.rs`. Field names use snake_case on the
/// wire; Swift surfaces them as camelCase via `CodingKeys`.
///
/// The cursor for resuming a stream is `graphVersion` — monotonic across a
/// database's lifetime, unique per commit.
public struct Change: Decodable, Sendable {
    public enum Kind: String, Decodable, Sendable {
        case insert, update, delete
    }

    public enum EntityKind: String, Decodable, Sendable {
        case node, edge
    }

    public let graphVersion: UInt64
    public let txId: String
    public let committedAt: String
    public let changeKind: Kind
    public let entityKind: EntityKind
    public let typeName: String
    public let tableId: String
    public let rowid: UInt64
    public let entityId: UInt64
    public let logicalKey: String
    public let row: JSONValue?
    public let previousGraphVersion: UInt64?

    private enum CodingKeys: String, CodingKey {
        case graphVersion = "graph_version"
        case txId = "tx_id"
        case committedAt = "committed_at"
        case changeKind = "change_kind"
        case entityKind = "entity_kind"
        case typeName = "type_name"
        case tableId = "table_id"
        case rowid
        case entityId = "entity_id"
        case logicalKey = "logical_key"
        case row
        case previousGraphVersion = "previous_graph_version"
    }
}

/// A type-erased JSON value, suitable for exposing opaque payloads (like the
/// `row` field on a `Change`) to callers without forcing a schema-specific
/// decoder. Supports `null`, bools, numbers, strings, arrays, and objects.
public enum JSONValue: Decodable, Sendable {
    case null
    case bool(Bool)
    case int(Int64)
    case double(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let bool = try? container.decode(Bool.self) {
            self = .bool(bool)
        } else if let int = try? container.decode(Int64.self) {
            self = .int(int)
        } else if let double = try? container.decode(Double.self) {
            self = .double(double)
        } else if let string = try? container.decode(String.self) {
            self = .string(string)
        } else if let array = try? container.decode([JSONValue].self) {
            self = .array(array)
        } else if let object = try? container.decode([String: JSONValue].self) {
            self = .object(object)
        } else {
            throw DecodingError.dataCorruptedError(
                in: container,
                debugDescription: "Unsupported JSON value"
            )
        }
    }
}

public final class Database {
    // Serializes handle lifecycle/use to avoid close-vs-operation races.
    private let lock = NSLock()
    private var handle: OpaquePointer?
    private var cachedSchema: SchemaDescribeMetadata?

    private init(handle: OpaquePointer) {
        self.handle = handle
    }

    deinit {
        lock.withLock {
            if let handle {
                nanograph_db_destroy(handle)
                self.handle = nil
            }
        }
    }

    public static func create(dbPath: String, schemaSource: String) throws -> Database {
        let handle = dbPath.withCString { dbPathPtr in
            schemaSource.withCString { schemaPtr in
                nanograph_db_init(dbPathPtr, schemaPtr)
            }
        }
        guard let handle else {
            throw NanoGraphError.message(Self.lastErrorMessage())
        }
        return Database(handle: handle)
    }

    public static func open(dbPath: String) throws -> Database {
        let handle = dbPath.withCString { dbPathPtr in
            nanograph_db_open(dbPathPtr)
        }
        guard let handle else {
            throw NanoGraphError.message(Self.lastErrorMessage())
        }
        return Database(handle: handle)
    }

    public static func openInMemory(schemaSource: String) throws -> Database {
        let handle = schemaSource.withCString { schemaPtr in
            nanograph_db_open_in_memory(schemaPtr)
        }
        guard let handle else {
            throw NanoGraphError.message(Self.lastErrorMessage())
        }
        return Database(handle: handle)
    }

    public func close() throws {
        try lock.withLock {
            guard let handle else {
                return
            }
            let status = nanograph_db_close(handle)
            if status != 0 {
                throw NanoGraphError.message(Self.lastErrorMessage())
            }
            nanograph_db_destroy(handle)
            self.handle = nil
            cachedSchema = nil
        }
    }

    /// Lazy, per-instance cache of the schema metadata used by mutation
    /// helpers. Populated on first use; invalidated by `close()`.
    fileprivate func cachedSchemaMetadata() throws -> SchemaDescribeMetadata {
        if let cached = lock.withLock({ self.cachedSchema }) {
            return cached
        }
        // Compute outside the lock — `describe` takes its own lock internally.
        let schema = try describe(SchemaDescribeMetadata.self)
        lock.withLock { self.cachedSchema = schema }
        return schema
    }

    public func load(dataSource: String, mode: LoadMode) throws {
        try lock.withLock {
            let handle = try requireHandleLocked()
            let status = dataSource.withCString { dataPtr in
                mode.rawValue.withCString { modePtr in
                    nanograph_db_load(handle, dataPtr, modePtr)
                }
            }
            if status != 0 {
                throw NanoGraphError.message(Self.lastErrorMessage())
            }
        }
    }

    public func loadFile(dataPath: String, mode: LoadMode) throws {
        try lock.withLock {
            let handle = try requireHandleLocked()
            let status = dataPath.withCString { dataPathPtr in
                mode.rawValue.withCString { modePtr in
                    nanograph_db_load_file(handle, dataPathPtr, modePtr)
                }
            }
            if status != 0 {
                throw NanoGraphError.message(Self.lastErrorMessage())
            }
        }
    }

    public func loadRows(_ rows: [LoadRow], mode: LoadMode) throws {
        let schema = try describe(SchemaDescribeMetadata.self)
        let jsonl = try encodeLoadRows(rows, schema: schema)
        try load(dataSource: jsonl, mode: mode)
    }

    public func run(
        querySource: String,
        queryName: String,
        params: Any? = nil
    ) throws -> Any {
        let paramsJSON = try encodeJSON(params)
        return try lock.withLock {
            let handle = try requireHandleLocked()
            let ptr = querySource.withCString { querySourcePtr in
                queryName.withCString { queryNamePtr in
                    if let paramsJSON {
                        return paramsJSON.withCString { paramsPtr in
                            nanograph_db_run(handle, querySourcePtr, queryNamePtr, paramsPtr)
                        }
                    }
                    return nanograph_db_run(handle, querySourcePtr, queryNamePtr, nil)
                }
            }
            return try decodeOwnedJSONString(ptr)
        }
    }

    public func check(querySource: String) throws -> Any {
        return try lock.withLock {
            let handle = try requireHandleLocked()
            let ptr = querySource.withCString { querySourcePtr in
                nanograph_db_check(handle, querySourcePtr)
            }
            return try decodeOwnedJSONString(ptr)
        }
    }

    public func runArrow(
        querySource: String,
        queryName: String,
        params: Any? = nil
    ) throws -> Data {
        let paramsJSON = try encodeJSON(params)
        return try lock.withLock {
            let handle = try requireHandleLocked()
            let bytes = querySource.withCString { querySourcePtr in
                queryName.withCString { queryNamePtr in
                    if let paramsJSON {
                        return paramsJSON.withCString { paramsPtr in
                            nanograph_db_run_arrow(handle, querySourcePtr, queryNamePtr, paramsPtr)
                        }
                    }
                    return nanograph_db_run_arrow(handle, querySourcePtr, queryNamePtr, nil)
                }
            }
            return try decodeOwnedBytes(bytes)
        }
    }

    public func describe() throws -> Any {
        try lock.withLock {
            let handle = try requireHandleLocked()
            let ptr = nanograph_db_describe(handle)
            return try decodeOwnedJSONString(ptr)
        }
    }

    public func embed(options: EmbedOptions? = nil) throws -> Any {
        let optionsJSON = try encodeJSON(options?.jsonObject())
        return try lock.withLock {
            let handle = try requireHandleLocked()
            let ptr = if let optionsJSON {
                optionsJSON.withCString { optionsPtr in
                    nanograph_db_embed(handle, optionsPtr)
                }
            } else {
                nanograph_db_embed(handle, nil)
            }
            return try decodeOwnedJSONString(ptr)
        }
    }

    public func compact(options: Any? = nil) throws -> Any {
        let optionsJSON = try encodeJSON(options)
        return try lock.withLock {
            let handle = try requireHandleLocked()
            let ptr = if let optionsJSON {
                optionsJSON.withCString { optionsPtr in
                    nanograph_db_compact(handle, optionsPtr)
                }
            } else {
                nanograph_db_compact(handle, nil)
            }
            return try decodeOwnedJSONString(ptr)
        }
    }

    public func cleanup(options: Any? = nil) throws -> Any {
        let optionsJSON = try encodeJSON(options)
        return try lock.withLock {
            let handle = try requireHandleLocked()
            let ptr = if let optionsJSON {
                optionsJSON.withCString { optionsPtr in
                    nanograph_db_cleanup(handle, optionsPtr)
                }
            } else {
                nanograph_db_cleanup(handle, nil)
            }
            return try decodeOwnedJSONString(ptr)
        }
    }

    public func doctor() throws -> Any {
        try lock.withLock {
            let handle = try requireHandleLocked()
            let ptr = nanograph_db_doctor(handle)
            return try decodeOwnedJSONString(ptr)
        }
    }

    public func changes(options: Any? = nil) throws -> Any {
        let optionsJSON = try encodeJSON(options)
        return try lock.withLock {
            let handle = try requireHandleLocked()
            let ptr = if let optionsJSON {
                optionsJSON.withCString { optionsPtr in
                    nanograph_db_changes(handle, optionsPtr)
                }
            } else {
                nanograph_db_changes(handle, nil)
            }
            return try decodeOwnedJSONString(ptr)
        }
    }

    public func isInMemory() throws -> Bool {
        try lock.withLock {
            let handle = try requireHandleLocked()
            let status = nanograph_db_is_in_memory(handle)
            if status == -1 {
                throw NanoGraphError.message(Self.lastErrorMessage())
            }
            return status != 0
        }
    }

    public func run<T: Decodable>(
        _ type: T.Type,
        querySource: String,
        queryName: String,
        params: Any? = nil
    ) throws -> T {
        let raw = try run(querySource: querySource, queryName: queryName, params: params)
        return try decodeValue(type, from: raw)
    }

    public func check<T: Decodable>(_ type: T.Type, querySource: String) throws -> T {
        let raw = try check(querySource: querySource)
        return try decodeValue(type, from: raw)
    }

    public func describe<T: Decodable>(_ type: T.Type) throws -> T {
        let raw = try describe()
        return try decodeValue(type, from: raw)
    }

    public func embed<T: Decodable>(_ type: T.Type, options: EmbedOptions? = nil) throws -> T {
        let raw = try embed(options: options)
        return try decodeValue(type, from: raw)
    }

    public func doctor<T: Decodable>(_ type: T.Type) throws -> T {
        let raw = try doctor()
        return try decodeValue(type, from: raw)
    }

    public func changes<T: Decodable>(_ type: T.Type, options: Any? = nil) throws -> T {
        let raw = try changes(options: options)
        return try decodeValue(type, from: raw)
    }

    /// Fetch the CDC log since a given `graph_version` cursor. Pass `nil` to
    /// fetch everything from the beginning.
    ///
    /// The returned array is ordered by `graphVersion` ascending (engine
    /// contract); callers can advance their cursor with `.max(by:)` on the
    /// returned rows.
    public func changes(since graphVersion: UInt64? = nil) throws -> [Change] {
        var options: [String: Any] = [:]
        if let graphVersion {
            options["since"] = graphVersion
        }
        return try changes([Change].self, options: options.isEmpty ? nil : options)
    }

    /// Returns the current (highest committed) `graph_version` — useful for
    /// seeding a CDC stream cursor after an initial hydrate so the first poll
    /// doesn't replay history.
    ///
    /// Implementation: reads the full change log once. Cheap on fresh DBs;
    /// consider caching this at the call site for high-traffic loops.
    public func currentGraphVersion() throws -> UInt64? {
        let allChanges = try changes(since: nil)
        return allChanges.map(\.graphVersion).max()
    }

    /// A polling AsyncSequence over the CDC log. Every `interval`, fetches
    /// any new `Change` rows since the last seen cursor and yields them as a
    /// batch. Quiet DBs produce empty polls that yield nothing (non-empty
    /// batches only are yielded).
    ///
    /// The sequence terminates when the Task consuming it is cancelled.
    ///
    /// `startAt`: seed cursor — typically the `graph_version` of the most
    /// recent state the consumer has already applied. Pass `nil` to replay
    /// from the beginning.
    public func changeStream(
        interval: Duration = .seconds(1.5),
        startAt cursor: UInt64? = nil
    ) -> AsyncThrowingStream<[Change], Error> {
        AsyncThrowingStream { continuation in
            let task = Task { [self] in
                var currentCursor = cursor
                while !Task.isCancelled {
                    do {
                        let batch = try self.changes(since: currentCursor)
                        if !batch.isEmpty {
                            continuation.yield(batch)
                            currentCursor = batch.map(\.graphVersion).max() ?? currentCursor
                        }
                    } catch {
                        continuation.finish(throwing: error)
                        return
                    }
                    do {
                        try await Task.sleep(for: interval)
                    } catch {
                        break
                    }
                }
                continuation.finish()
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    private func requireHandleLocked() throws -> OpaquePointer {
        guard let handle else {
            throw NanoGraphError.message("Database is closed")
        }
        return handle
    }

    private static func lastErrorMessage() -> String {
        guard let errPtr = nanograph_last_error_message() else {
            return "Unknown NanoGraph FFI error"
        }
        return String(cString: errPtr)
    }

    private func decodeOwnedJSONString(_ ptr: UnsafeMutablePointer<CChar>?) throws -> Any {
        guard let ptr else {
            throw NanoGraphError.message(Self.lastErrorMessage())
        }
        defer {
            nanograph_string_free(ptr)
        }
        let json = String(cString: ptr)
        return try decodeJSON(json)
    }

    private func decodeOwnedBytes(_ bytes: NanoGraphBytes) throws -> Data {
        guard let ptr = bytes.ptr else {
            throw NanoGraphError.message(Self.lastErrorMessage())
        }
        defer {
            nanograph_bytes_free(bytes)
        }
        return Data(bytes: ptr, count: Int(bytes.len))
    }
}

// MARK: - Tier 1 mutation helpers
//
// Each helper synthesizes a named .gq query, executes it through the existing
// `run(querySource:queryName:params:)` FFI entry point, and decodes the
// `{affectedNodes, affectedEdges}` payload that the engine returns for
// mutation queries (see `MutationResult::to_sdk_json` in the core crate).
//
// Zero-match deletes return `affectedNodes: 0` silently — the engine treats
// missing rows as a no-op, not an error. See `crates/nanograph/src/store/
// database/persist.rs:949-966`.

extension Database {
    /// Insert or fully replace a node keyed by its `@key` property.
    ///
    /// **This is a full-row replace**, not a partial merge. Per the core
    /// loader's keyed-merge semantics (see `crates/nanograph/src/store/loader/
    /// merge.rs`), the incoming row overwrites every column — unmentioned
    /// non-null columns that are required by the schema will fail validation.
    /// For partial edits that preserve existing columns, use `updateNode`.
    public func upsertNode(type: String, data: [String: Any]) throws {
        try loadRows([.node(type: type, data: data)], mode: .merge)
    }

    /// Insert or fully replace an edge keyed by `(from, to)`. Re-writing the
    /// same `(from, to)` pair with different property values (e.g. a new
    /// `position` for outline reordering) overwrites the existing row.
    ///
    /// Identity is `(from, to)` — this is **not a multigraph**. Two parallel
    /// edges between the same pair collapse to one.
    ///
    /// Edge properties written here cannot currently be projected back through
    /// `.gq` queries (the grammar lacks an edge-binding form), only through
    /// the CDC `changes()` stream or raw Lance reads.
    public func upsertEdge(
        type: String,
        from: String,
        to: String,
        data: [String: Any] = [:]
    ) throws {
        try loadRows([.edge(type: type, from: from, to: to, data: data)], mode: .merge)
    }

    /// Delete a single node by its `@key`. Cascades all incident edges across
    /// edge types and directions. Silent no-op on miss.
    @discardableResult
    public func deleteNode(type: String, key: Any) throws -> MutationResult {
        let (nodeDef, keyTypeAnn) = try nodeKeyMetadata(for: type)
        let keyProperty = try requireKeyProperty(for: nodeDef)
        let querySource = """
        query __sdk_delete_node($__key: \(keyTypeAnn)) {
          delete \(type) where \(keyProperty) = $__key
        }
        """
        return try runMutation(
            querySource: querySource,
            queryName: "__sdk_delete_node",
            params: ["__key": key]
        )
    }

    /// Delete every edge of `type` whose source endpoint matches `key`.
    ///
    /// Grammar limitation: `.gq` `where` clauses on mutations accept a **single**
    /// `ident op value` predicate (see `query.pest: mutation_predicate`), so
    /// there's no single-edge delete-by-(from, to) today. Callers that want
    /// to sever a specific parent-child link should pair this with a semantic
    /// model where one endpoint uniquely identifies the edge (e.g. an outline
    /// where a child has at most one structural parent — `deleteEdgesTo(type:
    /// "Contains", key: childKey)` cleanly unparents).
    @discardableResult
    public func deleteEdgesFrom(type: String, key: Any) throws -> MutationResult {
        let edgeDef = try edgeTypeDefinition(for: type)
        let annotations = try edgeEndpointKeyTypeAnnotations(for: edgeDef)
        let querySource = """
        query __sdk_delete_edges_from($__from: \(annotations.src)) {
          delete \(type) where from = $__from
        }
        """
        return try runMutation(
            querySource: querySource,
            queryName: "__sdk_delete_edges_from",
            params: ["__from": key]
        )
    }

    /// Delete every edge of `type` whose target endpoint matches `key`.
    ///
    /// See `deleteEdgesFrom(type:key:)` for the grammar-limit rationale.
    @discardableResult
    public func deleteEdgesTo(type: String, key: Any) throws -> MutationResult {
        let edgeDef = try edgeTypeDefinition(for: type)
        let annotations = try edgeEndpointKeyTypeAnnotations(for: edgeDef)
        let querySource = """
        query __sdk_delete_edges_to($__to: \(annotations.dst)) {
          delete \(type) where to = $__to
        }
        """
        return try runMutation(
            querySource: querySource,
            queryName: "__sdk_delete_edges_to",
            params: ["__to": key]
        )
    }

    /// Update properties on a single node identified by its `@key`. The key
    /// itself is immutable — attempting to include it in `set` is rejected
    /// client-side before hitting the engine.
    ///
    /// Unmentioned properties are preserved (the engine reads the full row,
    /// overlays `set`, writes back — see `persist.rs:758-764`).
    @discardableResult
    public func updateNode(
        type: String,
        key: Any,
        set: [String: Any]
    ) throws -> MutationResult {
        guard !set.isEmpty else {
            return MutationResult(affectedNodes: 0, affectedEdges: 0)
        }
        let (nodeDef, keyTypeAnn) = try nodeKeyMetadata(for: type)
        let keyProperty = try requireKeyProperty(for: nodeDef)
        if set.keys.contains(keyProperty) {
            throw NanoGraphError.message(
                "Cannot update the @key property '\(keyProperty)' on type '\(type)'. " +
                "Use deleteNode + upsertNode to rekey a node."
            )
        }

        let assignments = try synthesizeAssignments(set: set, nodeDef: nodeDef)
        var paramDecls = assignments.paramDecls
        paramDecls.append("$__key: \(keyTypeAnn)")
        let querySource = """
        query __sdk_update_node(\(paramDecls.joined(separator: ", "))) {
          update \(type) set { \(assignments.setClauses.joined(separator: ", ")) } where \(keyProperty) = $__key
        }
        """

        var params = assignments.params
        params["__key"] = key
        return try runMutation(
            querySource: querySource,
            queryName: "__sdk_update_node",
            params: params
        )
    }

    /// Update properties on every node matching a raw predicate. Escape hatch
    /// for bulk edits — prefer `updateNode(type:key:set:)` for single-row
    /// updates in interactive contexts (safer: can't silently rewrite more
    /// than one row on a typo).
    ///
    /// `wherePredicate` is raw `.gq` text inserted after `where`. Param names
    /// referenced inside it must be declared in `paramTypes`.
    @discardableResult
    public func updateNodes(
        type: String,
        wherePredicate: String,
        paramTypes: [String: String] = [:],
        params: [String: Any] = [:],
        set: [String: Any]
    ) throws -> MutationResult {
        guard !set.isEmpty else {
            return MutationResult(affectedNodes: 0, affectedEdges: 0)
        }
        let nodeDef = try nodeTypeDefinition(for: type)
        if let keyProperty = nodeDef.keyProperty, set.keys.contains(keyProperty) {
            throw NanoGraphError.message(
                "Cannot update the @key property '\(keyProperty)' on type '\(type)'."
            )
        }

        let assignments = try synthesizeAssignments(set: set, nodeDef: nodeDef)
        var paramDecls = assignments.paramDecls
        for (name, typeAnn) in paramTypes {
            paramDecls.append("$\(name): \(typeAnn)")
        }
        let querySource = """
        query __sdk_update_nodes(\(paramDecls.joined(separator: ", "))) {
          update \(type) set { \(assignments.setClauses.joined(separator: ", ")) } where \(wherePredicate)
        }
        """

        var allParams = assignments.params
        for (name, value) in params {
            allParams[name] = value
        }
        return try runMutation(
            querySource: querySource,
            queryName: "__sdk_update_nodes",
            params: allParams
        )
    }

    // MARK: - Private synthesis helpers

    private func runMutation(
        querySource: String,
        queryName: String,
        params: [String: Any]?
    ) throws -> MutationResult {
        let raw = try run(querySource: querySource, queryName: queryName, params: params)
        return try decodeValue(MutationResult.self, from: raw)
    }

    private struct Assignments {
        var paramDecls: [String]
        var setClauses: [String]
        var params: [String: Any]
    }

    private func synthesizeAssignments(
        set: [String: Any],
        nodeDef: SchemaDescribeMetadata.TypeDef
    ) throws -> Assignments {
        var paramDecls: [String] = []
        var setClauses: [String] = []
        var params: [String: Any] = [:]
        for propertyName in set.keys.sorted() {
            let value = set[propertyName]!
            let typeAnn = try propertyTypeAnnotation(for: nodeDef, propertyName: propertyName)
            let paramName = "__p_\(propertyName)"
            paramDecls.append("$\(paramName): \(typeAnn)")
            setClauses.append("\(propertyName): $\(paramName)")
            params[paramName] = value
        }
        return Assignments(paramDecls: paramDecls, setClauses: setClauses, params: params)
    }

    private func nodeKeyMetadata(
        for type: String
    ) throws -> (SchemaDescribeMetadata.TypeDef, String) {
        let nodeDef = try nodeTypeDefinition(for: type)
        let keyProperty = try requireKeyProperty(for: nodeDef)
        let keyType = try propertyTypeAnnotation(for: nodeDef, propertyName: keyProperty)
        return (nodeDef, keyType)
    }

    private func nodeTypeDefinition(
        for type: String
    ) throws -> SchemaDescribeMetadata.TypeDef {
        let schema = try cachedSchemaMetadata()
        guard let def = schema.nodeTypes.first(where: { $0.name == type }) else {
            throw NanoGraphError.message("Unknown node type '\(type)'")
        }
        return def
    }

    private func edgeTypeDefinition(
        for type: String
    ) throws -> SchemaDescribeMetadata.TypeDef {
        let schema = try cachedSchemaMetadata()
        guard let def = schema.edgeTypes.first(where: { $0.name == type }) else {
            throw NanoGraphError.message("Unknown edge type '\(type)'")
        }
        return def
    }

    private func requireKeyProperty(
        for nodeDef: SchemaDescribeMetadata.TypeDef
    ) throws -> String {
        guard let key = nodeDef.keyProperty, !key.isEmpty else {
            throw NanoGraphError.message(
                "Node type '\(nodeDef.name)' has no @key — cannot address rows by key"
            )
        }
        return key
    }

    private func propertyTypeAnnotation(
        for typeDef: SchemaDescribeMetadata.TypeDef,
        propertyName: String
    ) throws -> String {
        guard let prop = typeDef.properties.first(where: { $0.name == propertyName }) else {
            throw NanoGraphError.message(
                "Unknown property '\(propertyName)' on type '\(typeDef.name)'"
            )
        }
        guard let typeAnn = prop.type, !typeAnn.isEmpty else {
            throw NanoGraphError.message(
                "Missing type annotation for '\(typeDef.name).\(propertyName)'"
            )
        }
        return typeAnn
    }

    private func edgeEndpointKeyTypeAnnotations(
        for edgeDef: SchemaDescribeMetadata.TypeDef
    ) throws -> (src: String, dst: String) {
        guard let srcTypeName = edgeDef.srcType, let dstTypeName = edgeDef.dstType else {
            throw NanoGraphError.message(
                "Edge type '\(edgeDef.name)' is missing endpoint type information in describe()"
            )
        }
        let srcDef = try nodeTypeDefinition(for: srcTypeName)
        let dstDef = try nodeTypeDefinition(for: dstTypeName)
        let srcKeyProp = try requireKeyProperty(for: srcDef)
        let dstKeyProp = try requireKeyProperty(for: dstDef)
        let srcKeyType = try propertyTypeAnnotation(for: srcDef, propertyName: srcKeyProp)
        let dstKeyType = try propertyTypeAnnotation(for: dstDef, propertyName: dstKeyProp)
        return (srcKeyType, dstKeyType)
    }
}

private func encodeJSON(_ value: Any?) throws -> String? {
    guard let value else {
        return nil
    }
    guard JSONSerialization.isValidJSONObject(value) else {
        throw NanoGraphError.message("Value is not valid JSON")
    }
    let data = try JSONSerialization.data(withJSONObject: value, options: [])
    guard let string = String(data: data, encoding: .utf8) else {
        throw NanoGraphError.message("Failed to encode JSON string")
    }
    return string
}

private func decodeJSON(_ json: String) throws -> Any {
    let data = Data(json.utf8)
    return try JSONSerialization.jsonObject(with: data, options: [])
}

private func decodeOwnedJSONString(_ ptr: UnsafeMutablePointer<CChar>?) throws -> Any {
    guard let ptr else {
        throw NanoGraphError.message(lastErrorMessage())
    }
    defer {
        nanograph_string_free(ptr)
    }
    let json = String(cString: ptr)
    return try decodeJSON(json)
}

private func decodeValue<T: Decodable>(_ type: T.Type, from value: Any) throws -> T {
    let data = try JSONSerialization.data(withJSONObject: value, options: [])
    return try JSONDecoder().decode(type, from: data)
}

private func lastErrorMessage() -> String {
    guard let errPtr = nanograph_last_error_message() else {
        return "Unknown NanoGraph FFI error"
    }
    return String(cString: errPtr)
}

private func absoluteFilePath(_ path: String) -> String {
    let url = URL(fileURLWithPath: path, relativeTo: URL(fileURLWithPath: FileManager.default.currentDirectoryPath))
    return url.standardizedFileURL.path
}

private func buildMediaMimeMap(from schema: SchemaDescribeMetadata) -> [String: [String: String]] {
    var result: [String: [String: String]] = [:]
    for typeDef in schema.nodeTypes + schema.edgeTypes {
        let propMap = Dictionary(
            uniqueKeysWithValues: typeDef.properties.compactMap { prop in
                prop.mediaMimeProp.map { (prop.name, $0) }
            }
        )
        result[typeDef.name] = propMap
    }
    return result
}

private func encodeLoadRows(_ rows: [LoadRow], schema: SchemaDescribeMetadata) throws -> String {
    let mediaMimeMap = buildMediaMimeMap(from: schema)
    let encodedRows = try rows.map { row in
        let object = try encodeLoadRow(row, mediaMimeMap: mediaMimeMap)
        let data = try JSONSerialization.data(withJSONObject: object, options: [])
        guard let line = String(data: data, encoding: .utf8) else {
            throw NanoGraphError.message("Failed to encode load row JSON")
        }
        return line
    }
    return encodedRows.joined(separator: "\n")
}

private func encodeLoadRow(
    _ row: LoadRow,
    mediaMimeMap: [String: [String: String]]
) throws -> [String: Any] {
    switch row {
    case .node(let type, let data):
        return [
            "type": type,
            "data": try encodeLoadData(typeName: type, data: data, mediaMimeMap: mediaMimeMap),
        ]
    case .edge(let type, let from, let to, let data):
        var object: [String: Any] = [
            "edge": type,
            "from": from,
            "to": to,
        ]
        if !data.isEmpty {
            object["data"] = try encodeLoadData(typeName: type, data: data, mediaMimeMap: mediaMimeMap)
        }
        return object
    }
}

private func encodeLoadData(
    typeName: String,
    data: [String: Any],
    mediaMimeMap: [String: [String: String]]
) throws -> [String: Any] {
    var object: [String: Any] = [:]
    let typeMediaMap = mediaMimeMap[typeName] ?? [:]

    for (key, value) in data {
        if let mediaRef = value as? MediaRef {
            object[key] = mediaRef.encodedValue()
            if let mimeType = mediaRef.mimeType,
               let mimeProp = typeMediaMap[key]
            {
                if let existing = object[mimeProp] ?? data[mimeProp] {
                    guard let existingMime = existing as? String, existingMime == mimeType else {
                        throw NanoGraphError.message(
                            "media MIME mismatch for \(typeName).\(key): helper mimeType '\(mimeType)' conflicts with '\(existing)' in '\(mimeProp)'"
                        )
                    }
                } else {
                    object[mimeProp] = mimeType
                }
            }
            continue
        }

        object[key] = try encodeLoadValue(value)
    }

    return object
}

private func encodeLoadValue(_ value: Any) throws -> Any {
    if let mediaRef = value as? MediaRef {
        return mediaRef.encodedValue()
    }
    if value is NSNull || value is String || value is Bool || value is NSNumber {
        return value
    }
    if let value = value as? Int { return value }
    if let value = value as? Int32 { return Int(value) }
    if let value = value as? Int64 { return value }
    if let value = value as? UInt { return value }
    if let value = value as? UInt32 { return value }
    if let value = value as? UInt64 { return value }
    if let value = value as? Double { return value }
    if let value = value as? Float { return Double(value) }
    if let array = value as? [Any] {
        return try array.map(encodeLoadValue)
    }
    if let dict = value as? [String: Any] {
        var out: [String: Any] = [:]
        for (key, nested) in dict {
            out[key] = try encodeLoadValue(nested)
        }
        return out
    }
    throw NanoGraphError.message("Value is not valid JSON or MediaRef")
}
