use nanograph::query::ast::Param;
use nanograph::store::database::{CleanupOptions, CompactOptions, EmbedOptions, LoadMode};
use nanograph::{JsonParamMode, ParamMap, json_params_to_param_map};

pub struct ChangesOptions {
    pub from_graph_version_exclusive: u64,
    pub to_graph_version_inclusive: Option<u64>,
}

pub fn js_object_to_param_map(
    params: Option<&serde_json::Value>,
    query_params: &[Param],
) -> napi::Result<ParamMap> {
    json_params_to_param_map(params, query_params, JsonParamMode::JavaScript)
        .map_err(|err| napi::Error::from_reason(err.to_string()))
}

pub fn parse_load_mode(mode: &str) -> napi::Result<LoadMode> {
    match mode {
        "overwrite" => Ok(LoadMode::Overwrite),
        "append" => Ok(LoadMode::Append),
        "merge" => Ok(LoadMode::Merge),
        _ => Err(napi::Error::from_reason(format!(
            "invalid load mode '{}': expected 'overwrite', 'append', or 'merge'",
            mode
        ))),
    }
}

pub fn parse_compact_options(opts: Option<&serde_json::Value>) -> napi::Result<CompactOptions> {
    let mut result = CompactOptions::default();
    let obj = match opts {
        Some(serde_json::Value::Object(obj)) => obj,
        Some(serde_json::Value::Null) | None => return Ok(result),
        Some(_) => {
            return Err(napi::Error::from_reason(
                "compact options must be an object",
            ));
        }
    };
    for key in obj.keys() {
        match key.as_str() {
            "targetRowsPerFragment" | "materializeDeletions" | "materializeDeletionsThreshold" => {}
            _ => {
                return Err(napi::Error::from_reason(format!(
                    "unknown compact option '{}'",
                    key
                )));
            }
        }
    }
    if let Some(v) = obj.get("targetRowsPerFragment") {
        let parsed = v.as_u64().ok_or_else(|| {
            napi::Error::from_reason("targetRowsPerFragment must be a positive integer")
        })?;
        if parsed == 0 {
            return Err(napi::Error::from_reason(
                "targetRowsPerFragment must be a positive integer",
            ));
        }
        result.target_rows_per_fragment = usize::try_from(parsed).map_err(|_| {
            napi::Error::from_reason("targetRowsPerFragment is too large for this platform")
        })?;
    }
    if let Some(v) = obj.get("materializeDeletions") {
        result.materialize_deletions = v
            .as_bool()
            .ok_or_else(|| napi::Error::from_reason("materializeDeletions must be a boolean"))?;
    }
    if let Some(v) = obj.get("materializeDeletionsThreshold") {
        let threshold = v.as_f64().ok_or_else(|| {
            napi::Error::from_reason("materializeDeletionsThreshold must be a number")
        })?;
        if !(0.0..=1.0).contains(&threshold) {
            return Err(napi::Error::from_reason(
                "materializeDeletionsThreshold must be between 0.0 and 1.0",
            ));
        }
        result.materialize_deletions_threshold = threshold as f32;
    }
    Ok(result)
}

pub fn parse_cleanup_options(opts: Option<&serde_json::Value>) -> napi::Result<CleanupOptions> {
    let mut result = CleanupOptions::default();
    let obj = match opts {
        Some(serde_json::Value::Object(obj)) => obj,
        Some(serde_json::Value::Null) | None => return Ok(result),
        Some(_) => {
            return Err(napi::Error::from_reason(
                "cleanup options must be an object",
            ));
        }
    };
    for key in obj.keys() {
        match key.as_str() {
            "retainTxVersions" | "retainDatasetVersions" => {}
            _ => {
                return Err(napi::Error::from_reason(format!(
                    "unknown cleanup option '{}'",
                    key
                )));
            }
        }
    }
    if let Some(v) = obj.get("retainTxVersions") {
        let parsed = v.as_u64().ok_or_else(|| {
            napi::Error::from_reason("retainTxVersions must be a positive integer")
        })?;
        if parsed == 0 {
            return Err(napi::Error::from_reason(
                "retainTxVersions must be a positive integer",
            ));
        }
        result.retain_tx_versions = parsed;
    }
    if let Some(v) = obj.get("retainDatasetVersions") {
        let parsed = v.as_u64().ok_or_else(|| {
            napi::Error::from_reason("retainDatasetVersions must be a positive integer")
        })?;
        if parsed == 0 {
            return Err(napi::Error::from_reason(
                "retainDatasetVersions must be a positive integer",
            ));
        }
        result.retain_dataset_versions = usize::try_from(parsed).map_err(|_| {
            napi::Error::from_reason("retainDatasetVersions is too large for this platform")
        })?;
    }
    Ok(result)
}

pub fn parse_changes_options(opts: Option<&serde_json::Value>) -> napi::Result<ChangesOptions> {
    let obj = match opts {
        Some(serde_json::Value::Object(obj)) => obj,
        Some(serde_json::Value::Null) | None => {
            return Ok(ChangesOptions {
                from_graph_version_exclusive: 0,
                to_graph_version_inclusive: None,
            });
        }
        Some(_) => {
            return Err(napi::Error::from_reason(
                "changes options must be an object",
            ));
        }
    };

    for key in obj.keys() {
        match key.as_str() {
            "since" | "from" | "to" => {}
            _ => {
                return Err(napi::Error::from_reason(format!(
                    "unknown changes option '{}'",
                    key
                )));
            }
        }
    }

    if obj.contains_key("since") && obj.contains_key("from") {
        return Err(napi::Error::from_reason(
            "changes options must not include both since and from",
        ));
    }

    let from_graph_version_exclusive = if let Some(v) = obj.get("since") {
        v.as_u64()
            .ok_or_else(|| napi::Error::from_reason("since must be a non-negative integer"))?
    } else if let Some(v) = obj.get("from") {
        v.as_u64()
            .ok_or_else(|| napi::Error::from_reason("from must be a non-negative integer"))?
    } else {
        0
    };

    let to_graph_version_inclusive = obj
        .get("to")
        .map(|v| {
            v.as_u64()
                .ok_or_else(|| napi::Error::from_reason("to must be a non-negative integer"))
        })
        .transpose()?;

    Ok(ChangesOptions {
        from_graph_version_exclusive,
        to_graph_version_inclusive,
    })
}

pub fn parse_embed_options(opts: Option<&serde_json::Value>) -> napi::Result<EmbedOptions> {
    let mut result = EmbedOptions::default();
    let obj = match opts {
        Some(serde_json::Value::Object(obj)) => obj,
        Some(serde_json::Value::Null) | None => return Ok(result),
        Some(_) => return Err(napi::Error::from_reason("embed options must be an object")),
    };

    for key in obj.keys() {
        match key.as_str() {
            "typeName" | "property" | "onlyNull" | "limit" | "reindex" | "dryRun" => {}
            _ => {
                return Err(napi::Error::from_reason(format!(
                    "unknown embed option '{}'",
                    key
                )));
            }
        }
    }

    if let Some(v) = obj.get("typeName") {
        let type_name = v
            .as_str()
            .ok_or_else(|| napi::Error::from_reason("typeName must be a string"))?
            .trim();
        if type_name.is_empty() {
            return Err(napi::Error::from_reason("typeName must not be empty"));
        }
        result.type_name = Some(type_name.to_string());
    }

    if let Some(v) = obj.get("property") {
        let property = v
            .as_str()
            .ok_or_else(|| napi::Error::from_reason("property must be a string"))?
            .trim();
        if property.is_empty() {
            return Err(napi::Error::from_reason("property must not be empty"));
        }
        result.property = Some(property.to_string());
    }

    if let Some(v) = obj.get("onlyNull") {
        result.only_null = v
            .as_bool()
            .ok_or_else(|| napi::Error::from_reason("onlyNull must be a boolean"))?;
    }

    if let Some(v) = obj.get("limit") {
        let limit = v
            .as_u64()
            .ok_or_else(|| napi::Error::from_reason("limit must be a positive integer"))?;
        if limit == 0 {
            return Err(napi::Error::from_reason("limit must be a positive integer"));
        }
        result.limit = Some(
            usize::try_from(limit)
                .map_err(|_| napi::Error::from_reason("limit is too large for this platform"))?,
        );
    }

    if let Some(v) = obj.get("reindex") {
        result.reindex = v
            .as_bool()
            .ok_or_else(|| napi::Error::from_reason("reindex must be a boolean"))?;
    }

    if let Some(v) = obj.get("dryRun") {
        result.dry_run = v
            .as_bool()
            .ok_or_else(|| napi::Error::from_reason("dryRun must be a boolean"))?;
    }

    Ok(result)
}
