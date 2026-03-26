/// <reference types="node" />

import type { Table } from "apache-arrow"

export type LoadMode = "overwrite" | "append" | "merge"

export type JsonPrimitive = string | number | boolean | null
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue }
export type QueryParams = Record<string, JsonValue>
export type QueryRow = Record<string, JsonValue>
export type MediaFileRef = { file: string; mimeType?: string }
export type MediaBase64Ref = { base64: string; mimeType?: string }
export type MediaUriRef = { uri: string; mimeType?: string }
export type MediaRef = MediaFileRef | MediaBase64Ref | MediaUriRef
export type LoadValue = JsonValue | MediaRef
export interface NodeLoadRow {
  type: string
  data: Record<string, LoadValue>
}
export interface EdgeLoadRow {
  edge: string
  from: string
  to: string
  data?: Record<string, LoadValue>
}
export type LoadRow = NodeLoadRow | EdgeLoadRow

export interface MutationResult {
  affectedNodes: number
  affectedEdges: number
}

export interface QueryCheckResult {
  name: string
  kind: "read" | "mutation"
  status: "ok" | "error"
  error?: string
}

export interface PropDescription {
  name: string
  propId: number
  type: string
  nullable: boolean
  description?: string
  list?: true
  key?: true
  unique?: true
  index?: true
  enumValues?: string[]
  embedSource?: string
  mediaMimeProp?: string
}

export interface EdgeEndpointSummary {
  name: string
  toType?: string
  fromType?: string
}

export interface NodeTypeDescription {
  name: string
  typeId: number
  description?: string
  instruction?: string
  keyProperty?: string
  uniqueProperties: string[]
  outgoingEdges: EdgeEndpointSummary[]
  incomingEdges: EdgeEndpointSummary[]
  properties: PropDescription[]
}

export interface EdgeTypeDescription {
  name: string
  srcType: string
  dstType: string
  typeId: number
  description?: string
  instruction?: string
  endpointKeys: {
    src?: string
    dst?: string
  }
  properties: PropDescription[]
}

export interface SchemaDescription {
  nodeTypes: NodeTypeDescription[]
  edgeTypes: EdgeTypeDescription[]
}

export interface CompactOptions {
  targetRowsPerFragment?: number
  materializeDeletions?: boolean
  materializeDeletionsThreshold?: number
}

export interface CleanupOptions {
  retainTxVersions?: number
  retainDatasetVersions?: number
}

export interface CompactResult {
  datasetsConsidered: number
  datasetsCompacted: number
  fragmentsRemoved: number
  fragmentsAdded: number
  filesRemoved: number
  filesAdded: number
  manifestCommitted: boolean
}

export interface CleanupResult {
  txRowsRemoved: number
  txRowsKept: number
  cdcRowsRemoved: number
  cdcRowsKept: number
  datasetsCleaned: number
  datasetOldVersionsRemoved: number
  datasetBytesRemoved: number
}

export interface EmbedOptions {
  typeName?: string
  property?: string
  onlyNull?: boolean
  limit?: number
  reindex?: boolean
  dryRun?: boolean
}

export interface EmbedResult {
  nodeTypesConsidered: number
  propertiesSelected: number
  rowsSelected: number
  embeddingsGenerated: number
  reindexedTypes: number
  dryRun: boolean
}

export interface DoctorReport {
  healthy: boolean
  issues: string[]
  warnings: string[]
  manifestDbVersion: number
  datasetsChecked: number
  txRows: number
  cdcRows: number
}

export declare class Database {
  static init(dbPath: string, schemaSource: string): Promise<Database>
  static open(dbPath: string): Promise<Database>
  static openInMemory(schemaSource: string): Promise<Database>

  load(dataSource: string, mode: LoadMode): Promise<void>
  loadFile(dataPath: string, mode: LoadMode): Promise<void>
  loadRows(rows: LoadRow[], mode: LoadMode): Promise<void>

  run<T extends QueryRow = QueryRow>(
    querySource: string,
    queryName: string,
    params?: QueryParams | null,
  ): Promise<T[] | MutationResult>
  runArrow(querySource: string, queryName: string, params?: QueryParams | null): Promise<Buffer>
  check(querySource: string): Promise<QueryCheckResult[]>
  describe(): Promise<SchemaDescription>
  embed(options?: EmbedOptions | null): Promise<EmbedResult>
  compact(options?: CompactOptions | null): Promise<CompactResult>
  cleanup(options?: CleanupOptions | null): Promise<CleanupResult>
  doctor(): Promise<DoctorReport>
  isInMemory(): Promise<boolean>
  close(): Promise<void>
}

export declare function decodeArrow(buffer: Buffer | Uint8Array): Table<any>
export declare function mediaFile(file: string, mimeType?: string): MediaFileRef
export declare function mediaBase64(base64: string, mimeType?: string): MediaBase64Ref
export declare function mediaUri(uri: string, mimeType?: string): MediaUriRef
