use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::io::Cursor;
use std::mem;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::slice;
use std::sync::RwLock;

use arrow_ipc::reader::StreamReader;
use tokio::runtime::Runtime;

use nanograph::error::NanoError;
use nanograph::json_output::record_batches_to_rust_json_rows;
use nanograph::query::parser::parse_query;
use nanograph::query::typecheck::{CheckedQuery, typecheck_query_decl};
use nanograph::store::database::{
    CleanupOptions, CompactOptions, Database, EmbedOptions, LoadMode,
};
use nanograph::{JsonParamMode, RunInputError, find_named_query, json_params_to_param_map};

type FfiResult<T> = std::result::Result<T, String>;

const STATUS_OK: c_int = 0;
const STATUS_ERR: c_int = -1;

#[repr(C)]
pub struct NanoGraphBytes {
    pub ptr: *mut u8,
    pub len: usize,
}

impl NanoGraphBytes {
    const fn empty() -> Self {
        Self {
            ptr: ptr::null_mut(),
            len: 0,
        }
    }
}

thread_local! {
    // FFI callers fetch the last error after a null/-1 return, so the slot must
    // stay isolated per thread to avoid concurrent calls clobbering diagnostics.
    static LAST_ERROR_CSTR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(message: impl Into<String>) {
    let next = message.into().replace('\0', "\\0");
    let cstr = CString::new(next).unwrap_or_else(|_| {
        CString::new("nanograph-ffi: error contained interior null byte")
            .expect("static string must be valid CString")
    });
    LAST_ERROR_CSTR.with(|slot| {
        *slot.borrow_mut() = Some(cstr);
    });
}

fn clear_last_error() {
    LAST_ERROR_CSTR.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

fn to_status(result: FfiResult<()>) -> c_int {
    match result {
        Ok(()) => {
            clear_last_error();
            STATUS_OK
        }
        Err(err) => {
            set_last_error(err);
            STATUS_ERR
        }
    }
}

fn json_result_to_ptr(result: FfiResult<serde_json::Value>) -> *mut c_char {
    match result {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(serialized) => match CString::new(serialized) {
                Ok(s) => {
                    clear_last_error();
                    s.into_raw()
                }
                Err(_) => {
                    set_last_error("failed to build CString for JSON response");
                    ptr::null_mut()
                }
            },
            Err(e) => {
                set_last_error(format!("failed to serialize JSON response: {}", e));
                ptr::null_mut()
            }
        },
        Err(err) => {
            set_last_error(err);
            ptr::null_mut()
        }
    }
}

fn bytes_result_to_buffer(result: FfiResult<Vec<u8>>) -> NanoGraphBytes {
    match result {
        Ok(bytes) => {
            let mut boxed = bytes.into_boxed_slice();
            let len = boxed.len();
            let ptr = boxed.as_mut_ptr();
            mem::forget(boxed);
            clear_last_error();
            NanoGraphBytes { ptr, len }
        }
        Err(err) => {
            set_last_error(err);
            NanoGraphBytes::empty()
        }
    }
}

fn parse_required_str(arg_name: &str, value: *const c_char) -> FfiResult<String> {
    if value.is_null() {
        return Err(format!("{} must not be null", arg_name));
    }
    // SAFETY: `value` is verified non-null and points to a caller-provided C string.
    let c_str = unsafe { CStr::from_ptr(value) };
    c_str
        .to_str()
        .map(|s| s.to_string())
        .map_err(|e| format!("{} must be valid UTF-8: {}", arg_name, e))
}

fn parse_optional_json(value: *const c_char) -> FfiResult<Option<serde_json::Value>> {
    if value.is_null() {
        return Ok(None);
    }
    let s = parse_required_str("json", value)?;
    if s.trim().is_empty() {
        return Ok(None);
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&s).map_err(|e| format!("invalid JSON payload: {}", e))?;
    Ok(Some(parsed))
}

fn to_ffi_err(err: NanoError) -> String {
    err.to_string()
}

fn to_ffi_input_err(err: RunInputError) -> String {
    err.to_string()
}

fn parse_load_mode(mode: &str) -> FfiResult<LoadMode> {
    match mode {
        "overwrite" => Ok(LoadMode::Overwrite),
        "append" => Ok(LoadMode::Append),
        "merge" => Ok(LoadMode::Merge),
        _ => Err(format!(
            "invalid load mode '{}': expected 'overwrite', 'append', or 'merge'",
            mode
        )),
    }
}

fn parse_compact_options(opts: Option<&serde_json::Value>) -> FfiResult<CompactOptions> {
    let mut result = CompactOptions::default();
    let obj = match opts {
        Some(serde_json::Value::Object(obj)) => obj,
        Some(serde_json::Value::Null) | None => return Ok(result),
        Some(_) => return Err("compact options must be a JSON object".to_string()),
    };
    for key in obj.keys() {
        match key.as_str() {
            "targetRowsPerFragment" | "materializeDeletions" | "materializeDeletionsThreshold" => {}
            _ => return Err(format!("unknown compact option '{}'", key)),
        }
    }
    if let Some(v) = obj.get("targetRowsPerFragment") {
        let parsed = v
            .as_u64()
            .ok_or_else(|| "targetRowsPerFragment must be a positive integer".to_string())?;
        if parsed == 0 {
            return Err("targetRowsPerFragment must be a positive integer".to_string());
        }
        result.target_rows_per_fragment = usize::try_from(parsed)
            .map_err(|_| "targetRowsPerFragment is too large for this platform".to_string())?;
    }
    if let Some(v) = obj.get("materializeDeletions") {
        result.materialize_deletions = v
            .as_bool()
            .ok_or_else(|| "materializeDeletions must be a boolean".to_string())?;
    }
    if let Some(v) = obj.get("materializeDeletionsThreshold") {
        let threshold = v
            .as_f64()
            .ok_or_else(|| "materializeDeletionsThreshold must be a number".to_string())?;
        if !(0.0..=1.0).contains(&threshold) {
            return Err("materializeDeletionsThreshold must be between 0.0 and 1.0".to_string());
        }
        result.materialize_deletions_threshold = threshold as f32;
    }
    Ok(result)
}

fn parse_cleanup_options(opts: Option<&serde_json::Value>) -> FfiResult<CleanupOptions> {
    let mut result = CleanupOptions::default();
    let obj = match opts {
        Some(serde_json::Value::Object(obj)) => obj,
        Some(serde_json::Value::Null) | None => return Ok(result),
        Some(_) => return Err("cleanup options must be a JSON object".to_string()),
    };
    for key in obj.keys() {
        match key.as_str() {
            "retainTxVersions" | "retainDatasetVersions" => {}
            _ => return Err(format!("unknown cleanup option '{}'", key)),
        }
    }
    if let Some(v) = obj.get("retainTxVersions") {
        let parsed = v
            .as_u64()
            .ok_or_else(|| "retainTxVersions must be a positive integer".to_string())?;
        if parsed == 0 {
            return Err("retainTxVersions must be a positive integer".to_string());
        }
        result.retain_tx_versions = parsed;
    }
    if let Some(v) = obj.get("retainDatasetVersions") {
        let parsed = v
            .as_u64()
            .ok_or_else(|| "retainDatasetVersions must be a positive integer".to_string())?;
        if parsed == 0 {
            return Err("retainDatasetVersions must be a positive integer".to_string());
        }
        result.retain_dataset_versions = usize::try_from(parsed)
            .map_err(|_| "retainDatasetVersions is too large for this platform".to_string())?;
    }
    Ok(result)
}

fn parse_embed_options(opts: Option<&serde_json::Value>) -> FfiResult<EmbedOptions> {
    let mut result = EmbedOptions::default();
    let obj = match opts {
        Some(serde_json::Value::Object(obj)) => obj,
        Some(serde_json::Value::Null) | None => return Ok(result),
        Some(_) => return Err("embed options must be a JSON object".to_string()),
    };
    for key in obj.keys() {
        match key.as_str() {
            "typeName" | "property" | "onlyNull" | "limit" | "reindex" | "dryRun" => {}
            _ => return Err(format!("unknown embed option '{}'", key)),
        }
    }
    if let Some(v) = obj.get("typeName") {
        let type_name = v
            .as_str()
            .ok_or_else(|| "typeName must be a string".to_string())?
            .trim();
        if type_name.is_empty() {
            return Err("typeName must not be empty".to_string());
        }
        result.type_name = Some(type_name.to_string());
    }
    if let Some(v) = obj.get("property") {
        let property = v
            .as_str()
            .ok_or_else(|| "property must be a string".to_string())?
            .trim();
        if property.is_empty() {
            return Err("property must not be empty".to_string());
        }
        result.property = Some(property.to_string());
    }
    if let Some(v) = obj.get("onlyNull") {
        result.only_null = v
            .as_bool()
            .ok_or_else(|| "onlyNull must be a boolean".to_string())?;
    }
    if let Some(v) = obj.get("limit") {
        let limit = v
            .as_u64()
            .ok_or_else(|| "limit must be a positive integer".to_string())?;
        if limit == 0 {
            return Err("limit must be a positive integer".to_string());
        }
        result.limit = Some(
            usize::try_from(limit)
                .map_err(|_| "limit is too large for this platform".to_string())?,
        );
    }
    if let Some(v) = obj.get("reindex") {
        result.reindex = v
            .as_bool()
            .ok_or_else(|| "reindex must be a boolean".to_string())?;
    }
    if let Some(v) = obj.get("dryRun") {
        result.dry_run = v
            .as_bool()
            .ok_or_else(|| "dryRun must be a boolean".to_string())?;
    }
    Ok(result)
}

fn prop_def_to_json(prop: &nanograph::schema_ir::PropDef) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "name": prop.name,
        "propId": prop.prop_id,
        "type": prop.scalar_type,
        "nullable": prop.nullable,
    });
    if prop.list {
        obj["list"] = serde_json::Value::Bool(true);
    }
    if prop.key {
        obj["key"] = serde_json::Value::Bool(true);
    }
    if prop.unique {
        obj["unique"] = serde_json::Value::Bool(true);
    }
    if prop.index {
        obj["index"] = serde_json::Value::Bool(true);
    }
    if !prop.enum_values.is_empty() {
        obj["enumValues"] = serde_json::json!(prop.enum_values);
    }
    if let Some(ref src) = prop.embed_source {
        obj["embedSource"] = serde_json::Value::String(src.clone());
    }
    if let Some(ref mime_prop) = prop.media_mime_prop {
        obj["mediaMimeProp"] = serde_json::Value::String(mime_prop.clone());
    }
    if let Some(ref description) = prop.description {
        obj["description"] = serde_json::Value::String(description.clone());
    }
    obj
}

pub struct NanoGraphHandle {
    runtime: Runtime,
    db: RwLock<Option<Database>>,
}

impl NanoGraphHandle {
    fn with_runtime(runtime: Runtime, db: Database) -> Self {
        Self {
            runtime,
            db: RwLock::new(Some(db)),
        }
    }

    fn db(&self) -> FfiResult<Database> {
        let guard = self
            .db
            .read()
            .map_err(|_| "database rwlock is poisoned".to_string())?;
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| "database is closed".to_string())
    }
}

fn with_handle<T>(
    handle: *mut NanoGraphHandle,
    f: impl FnOnce(&NanoGraphHandle) -> FfiResult<T>,
) -> FfiResult<T> {
    if handle.is_null() {
        return Err("database handle is null".to_string());
    }
    // SAFETY: pointer is checked for null above and expected to come from this library.
    let handle = unsafe { &*handle };
    f(handle)
}

fn run_query_json(
    handle: &NanoGraphHandle,
    query_source: &str,
    query_name: &str,
    params: Option<serde_json::Value>,
) -> FfiResult<serde_json::Value> {
    let query = find_named_query(query_source, query_name).map_err(to_ffi_input_err)?;
    let param_map =
        json_params_to_param_map(params.as_ref(), &query.params, JsonParamMode::Standard)
            .map_err(to_ffi_input_err)?;
    let db = handle.db()?;

    if query.mutation.is_some() {
        let result = handle
            .runtime
            .block_on(db.run_query(&query, &param_map))
            .map_err(to_ffi_err)?;
        Ok(result.to_sdk_json())
    } else {
        let prepared = db.prepare_read_query(&query).map_err(to_ffi_err)?;
        let results = handle
            .runtime
            .block_on(prepared.execute(&param_map))
            .map_err(to_ffi_err)?;
        Ok(results.to_sdk_json())
    }
}

fn run_query_arrow(
    handle: &NanoGraphHandle,
    query_source: &str,
    query_name: &str,
    params: Option<serde_json::Value>,
) -> FfiResult<Vec<u8>> {
    let query = find_named_query(query_source, query_name).map_err(to_ffi_input_err)?;
    if query.mutation.is_some() {
        return Err("runArrow only supports read queries; use run() for mutations".to_string());
    }

    let param_map =
        json_params_to_param_map(params.as_ref(), &query.params, JsonParamMode::Standard)
            .map_err(to_ffi_input_err)?;
    let db = handle.db()?;
    let prepared = db.prepare_read_query(&query).map_err(to_ffi_err)?;
    let results = handle
        .runtime
        .block_on(prepared.execute(&param_map))
        .map_err(to_ffi_err)?;
    results.to_arrow_ipc().map_err(to_ffi_err)
}

fn arrow_bytes_to_json(data: *const u8, len: usize) -> FfiResult<serde_json::Value> {
    if data.is_null() {
        return Err("arrow data must not be null".to_string());
    }
    if len == 0 {
        return Err("arrow data must not be empty".to_string());
    }

    // SAFETY: caller provides a non-null pointer and explicit length for the Arrow IPC buffer.
    let bytes = unsafe { slice::from_raw_parts(data, len) };
    let reader = StreamReader::try_new(Cursor::new(bytes), None)
        .map_err(|err| format!("invalid Arrow IPC stream: {}", err))?;
    let batches = reader
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|err| format!("failed to decode Arrow IPC stream: {}", err))?;
    Ok(serde_json::Value::Array(record_batches_to_rust_json_rows(
        &batches,
    )))
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_last_error_message() -> *const c_char {
    LAST_ERROR_CSTR.with(|slot| {
        let slot = slot.borrow();
        slot.as_ref().map_or(ptr::null(), |s| s.as_ptr())
    })
}

#[unsafe(no_mangle)]
/// Frees a string previously returned by this library.
///
/// # Safety
/// `value` must be null or a pointer returned by `CString::into_raw` from this library.
pub unsafe extern "C" fn nanograph_string_free(value: *mut c_char) {
    if value.is_null() {
        return;
    }
    // SAFETY: pointer must originate from CString::into_raw in this library.
    unsafe {
        let _ = CString::from_raw(value);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_bytes_free(value: NanoGraphBytes) {
    if value.ptr.is_null() {
        return;
    }
    // SAFETY: pointer and length must originate from Box<[u8]>::into_raw parts in this library.
    unsafe {
        let slice = ptr::slice_from_raw_parts_mut(value.ptr, value.len);
        drop(Box::from_raw(slice));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_init(
    db_path: *const c_char,
    schema_source: *const c_char,
) -> *mut NanoGraphHandle {
    let result: FfiResult<*mut NanoGraphHandle> = (|| {
        let db_path = parse_required_str("db_path", db_path)?;
        let schema_source = parse_required_str("schema_source", schema_source)?;
        let runtime = Runtime::new().map_err(|e| format!("failed to create runtime: {}", e))?;
        let db = runtime
            .block_on(Database::init(db_path.as_ref(), &schema_source))
            .map_err(to_ffi_err)?;
        let handle = NanoGraphHandle::with_runtime(runtime, db);
        Ok(Box::into_raw(Box::new(handle)))
    })();

    match result {
        Ok(handle) => {
            clear_last_error();
            handle
        }
        Err(err) => {
            set_last_error(err);
            ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_open(db_path: *const c_char) -> *mut NanoGraphHandle {
    let result: FfiResult<*mut NanoGraphHandle> = (|| {
        let db_path = parse_required_str("db_path", db_path)?;
        let runtime = Runtime::new().map_err(|e| format!("failed to create runtime: {}", e))?;
        let db = runtime
            .block_on(Database::open(db_path.as_ref()))
            .map_err(to_ffi_err)?;
        let handle = NanoGraphHandle::with_runtime(runtime, db);
        Ok(Box::into_raw(Box::new(handle)))
    })();

    match result {
        Ok(handle) => {
            clear_last_error();
            handle
        }
        Err(err) => {
            set_last_error(err);
            ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_open_in_memory(
    schema_source: *const c_char,
) -> *mut NanoGraphHandle {
    let result: FfiResult<*mut NanoGraphHandle> = (|| {
        let schema_source = parse_required_str("schema_source", schema_source)?;
        let runtime = Runtime::new().map_err(|e| format!("failed to create runtime: {}", e))?;
        let db = runtime
            .block_on(Database::open_in_memory(&schema_source))
            .map_err(to_ffi_err)?;
        let handle = NanoGraphHandle::with_runtime(runtime, db);
        Ok(Box::into_raw(Box::new(handle)))
    })();

    match result {
        Ok(handle) => {
            clear_last_error();
            handle
        }
        Err(err) => {
            set_last_error(err);
            ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_close(handle: *mut NanoGraphHandle) -> c_int {
    to_status(with_handle(handle, |handle| {
        let mut guard = handle
            .db
            .write()
            .map_err(|_| "database rwlock is poisoned".to_string())?;
        *guard = None;
        Ok(())
    }))
}

#[unsafe(no_mangle)]
/// Destroys a database handle previously returned by this library.
///
/// # Safety
/// `handle` must be null or a pointer returned by `Box::into_raw` from this library.
pub unsafe extern "C" fn nanograph_db_destroy(handle: *mut NanoGraphHandle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: pointer must originate from Box::into_raw in this library.
    unsafe {
        drop(Box::from_raw(handle));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_load(
    handle: *mut NanoGraphHandle,
    data_source: *const c_char,
    mode: *const c_char,
) -> c_int {
    let data_source = match parse_required_str("data_source", data_source) {
        Ok(v) => v,
        Err(err) => {
            set_last_error(err);
            return STATUS_ERR;
        }
    };
    let mode = match parse_required_str("mode", mode) {
        Ok(v) => v,
        Err(err) => {
            set_last_error(err);
            return STATUS_ERR;
        }
    };

    to_status(with_handle(handle, |handle| {
        let load_mode = parse_load_mode(&mode)?;
        let db = handle.db()?;
        handle
            .runtime
            .block_on(db.load_with_mode(&data_source, load_mode))
            .map_err(to_ffi_err)?;
        Ok(())
    }))
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_load_file(
    handle: *mut NanoGraphHandle,
    data_path: *const c_char,
    mode: *const c_char,
) -> c_int {
    let data_path = match parse_required_str("data_path", data_path) {
        Ok(v) => v,
        Err(err) => {
            set_last_error(err);
            return STATUS_ERR;
        }
    };
    let mode = match parse_required_str("mode", mode) {
        Ok(v) => v,
        Err(err) => {
            set_last_error(err);
            return STATUS_ERR;
        }
    };

    to_status(with_handle(handle, |handle| {
        let load_mode = parse_load_mode(&mode)?;
        let db = handle.db()?;
        handle
            .runtime
            .block_on(db.load_file_with_mode(std::path::Path::new(&data_path), load_mode))
            .map_err(to_ffi_err)?;
        Ok(())
    }))
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_run(
    handle: *mut NanoGraphHandle,
    query_source: *const c_char,
    query_name: *const c_char,
    params_json: *const c_char,
) -> *mut c_char {
    let result = (|| {
        let query_source = parse_required_str("query_source", query_source)?;
        let query_name = parse_required_str("query_name", query_name)?;
        let params = parse_optional_json(params_json)?;
        with_handle(handle, |handle| {
            run_query_json(handle, &query_source, &query_name, params)
        })
    })();
    json_result_to_ptr(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_run_arrow(
    handle: *mut NanoGraphHandle,
    query_source: *const c_char,
    query_name: *const c_char,
    params_json: *const c_char,
) -> NanoGraphBytes {
    let result = (|| {
        let query_source = parse_required_str("query_source", query_source)?;
        let query_name = parse_required_str("query_name", query_name)?;
        let params = parse_optional_json(params_json)?;
        with_handle(handle, |handle| {
            run_query_arrow(handle, &query_source, &query_name, params)
        })
    })();
    bytes_result_to_buffer(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_arrow_to_json(data: *const u8, len: usize) -> *mut c_char {
    json_result_to_ptr(arrow_bytes_to_json(data, len))
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_check(
    handle: *mut NanoGraphHandle,
    query_source: *const c_char,
) -> *mut c_char {
    let result = (|| {
        let query_source = parse_required_str("query_source", query_source)?;
        with_handle(handle, |handle| {
            let queries = parse_query(&query_source).map_err(to_ffi_err)?;
            let db = handle.db()?;
            let catalog = db.catalog().clone();

            let mut checks = Vec::with_capacity(queries.queries.len());
            for q in &queries.queries {
                match typecheck_query_decl(&catalog, q) {
                    Ok(CheckedQuery::Read(_)) => checks.push(serde_json::json!({
                        "name": q.name,
                        "kind": "read",
                        "status": "ok",
                    })),
                    Ok(CheckedQuery::Mutation(_)) => checks.push(serde_json::json!({
                        "name": q.name,
                        "kind": "mutation",
                        "status": "ok",
                    })),
                    Err(e) => checks.push(serde_json::json!({
                        "name": q.name,
                        "kind": if q.mutation.is_some() { "mutation" } else { "read" },
                        "status": "error",
                        "error": e.to_string(),
                    })),
                }
            }
            Ok(serde_json::Value::Array(checks))
        })
    })();
    json_result_to_ptr(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_describe(handle: *mut NanoGraphHandle) -> *mut c_char {
    let result = with_handle(handle, |handle| {
        let db = handle.db()?;
        let ir = &db.schema_ir;

        let node_types: Vec<serde_json::Value> = ir
            .node_types()
            .map(|nt| {
                serde_json::json!({
                    "name": nt.name,
                    "typeId": nt.type_id,
                    "description": nt.description,
                    "instruction": nt.instruction,
                    "keyProperty": nt.key_property_name(),
                    "uniqueProperties": nt.unique_properties().map(|prop| prop.name.clone()).collect::<Vec<_>>(),
                    "outgoingEdges": ir.edge_types().filter(|edge| edge.src_type_name == nt.name).map(|edge| serde_json::json!({"name": edge.name, "toType": edge.dst_type_name})).collect::<Vec<_>>(),
                    "incomingEdges": ir.edge_types().filter(|edge| edge.dst_type_name == nt.name).map(|edge| serde_json::json!({"name": edge.name, "fromType": edge.src_type_name})).collect::<Vec<_>>(),
                    "properties": nt.properties.iter().map(prop_def_to_json).collect::<Vec<_>>(),
                })
            })
            .collect();

        let edge_types: Vec<serde_json::Value> = ir
            .edge_types()
            .map(|et| {
                serde_json::json!({
                    "name": et.name,
                    "srcType": et.src_type_name,
                    "dstType": et.dst_type_name,
                    "typeId": et.type_id,
                    "description": et.description,
                    "instruction": et.instruction,
                    "endpointKeys": {
                        "src": ir.node_key_property_name(&et.src_type_name),
                        "dst": ir.node_key_property_name(&et.dst_type_name),
                    },
                    "properties": et.properties.iter().map(prop_def_to_json).collect::<Vec<_>>(),
                })
            })
            .collect();

        Ok(serde_json::json!({
            "nodeTypes": node_types,
            "edgeTypes": edge_types,
        }))
    });
    json_result_to_ptr(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_compact(
    handle: *mut NanoGraphHandle,
    options_json: *const c_char,
) -> *mut c_char {
    let result = (|| {
        let options = parse_optional_json(options_json)?;
        with_handle(handle, |handle| {
            let opts = parse_compact_options(options.as_ref())?;
            let db = handle.db()?;
            let result = handle
                .runtime
                .block_on(db.compact(opts))
                .map_err(to_ffi_err)?;
            Ok(serde_json::json!({
                "datasetsConsidered": result.datasets_considered,
                "datasetsCompacted": result.datasets_compacted,
                "fragmentsRemoved": result.fragments_removed,
                "fragmentsAdded": result.fragments_added,
                "filesRemoved": result.files_removed,
                "filesAdded": result.files_added,
                "manifestCommitted": result.manifest_committed,
            }))
        })
    })();
    json_result_to_ptr(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_cleanup(
    handle: *mut NanoGraphHandle,
    options_json: *const c_char,
) -> *mut c_char {
    let result = (|| {
        let options = parse_optional_json(options_json)?;
        with_handle(handle, |handle| {
            let opts = parse_cleanup_options(options.as_ref())?;
            let db = handle.db()?;
            let result = handle
                .runtime
                .block_on(db.cleanup(opts))
                .map_err(to_ffi_err)?;
            Ok(serde_json::json!({
                "txRowsRemoved": result.tx_rows_removed,
                "txRowsKept": result.tx_rows_kept,
                "cdcRowsRemoved": result.cdc_rows_removed,
                "cdcRowsKept": result.cdc_rows_kept,
                "datasetsCleaned": result.datasets_cleaned,
                "datasetOldVersionsRemoved": result.dataset_old_versions_removed,
                "datasetBytesRemoved": result.dataset_bytes_removed,
            }))
        })
    })();
    json_result_to_ptr(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_embed(
    handle: *mut NanoGraphHandle,
    options_json: *const c_char,
) -> *mut c_char {
    let result = (|| {
        let options = parse_optional_json(options_json)?;
        with_handle(handle, |handle| {
            let opts = parse_embed_options(options.as_ref())?;
            let db = handle.db()?;
            let result = handle
                .runtime
                .block_on(db.embed(opts))
                .map_err(to_ffi_err)?;
            Ok(serde_json::json!({
                "nodeTypesConsidered": result.node_types_considered,
                "propertiesSelected": result.properties_selected,
                "rowsSelected": result.rows_selected,
                "embeddingsGenerated": result.embeddings_generated,
                "reindexedTypes": result.reindexed_types,
                "dryRun": result.dry_run,
            }))
        })
    })();
    json_result_to_ptr(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_doctor(handle: *mut NanoGraphHandle) -> *mut c_char {
    let result = with_handle(handle, |handle| {
        let db = handle.db()?;
        let report = handle.runtime.block_on(db.doctor()).map_err(to_ffi_err)?;
        Ok(serde_json::json!({
            "healthy": report.healthy,
            "issues": report.issues,
            "warnings": report.warnings,
            "datasetsChecked": report.datasets_checked,
            "txRows": report.tx_rows,
            "cdcRows": report.cdc_rows,
        }))
    });
    json_result_to_ptr(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn nanograph_db_is_in_memory(handle: *mut NanoGraphHandle) -> c_int {
    match with_handle(handle, |handle| {
        let db = handle.db()?;
        Ok(db.is_in_memory())
    }) {
        Ok(true) => {
            clear_last_error();
            1
        }
        Ok(false) => {
            clear_last_error();
            0
        }
        Err(err) => {
            set_last_error(err);
            STATUS_ERR
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{CStr, CString};
    use std::ptr;
    use std::thread;

    use super::{
        clear_last_error, nanograph_db_close, nanograph_db_destroy, nanograph_db_open,
        nanograph_db_open_in_memory, nanograph_last_error_message,
    };

    #[test]
    fn last_error_is_visible_on_the_same_thread() {
        clear_last_error();

        let _ = nanograph_db_open(ptr::null());
        let ptr = nanograph_last_error_message();
        assert!(!ptr.is_null(), "expected error pointer");
        // SAFETY: pointer originates from `nanograph_last_error_message`.
        let msg = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };

        assert!(
            msg.contains("db_path must not be null"),
            "unexpected error message: {}",
            msg
        );
    }

    #[test]
    fn last_error_is_isolated_per_thread() {
        clear_last_error();

        let _ = nanograph_db_open(ptr::null());

        thread::spawn(|| {
            let schema = CString::new("node Person { name: String @key }")
                .expect("schema must be a valid CString");
            let handle = nanograph_db_open_in_memory(schema.as_ptr());
            assert!(!handle.is_null(), "expected in-memory database handle");
            assert!(
                nanograph_last_error_message().is_null(),
                "successful call on another thread should not inherit caller error"
            );
            assert_eq!(nanograph_db_close(handle), 0, "expected close to succeed");
            // SAFETY: pointer originates from `nanograph_db_open_in_memory`.
            unsafe {
                nanograph_db_destroy(handle);
            }
        })
        .join()
        .expect("isolation thread panicked");

        let ptr = nanograph_last_error_message();
        assert!(
            !ptr.is_null(),
            "error from originating thread should remain visible"
        );
        // SAFETY: pointer originates from `nanograph_last_error_message`.
        let msg = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        assert!(
            msg.contains("db_path must not be null"),
            "unexpected error message: {}",
            msg
        );
    }
}
