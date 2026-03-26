use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::{NanoError, Result};

const MEDIA_ROOT_ENV: &str = "NANOGRAPH_MEDIA_ROOT";

#[derive(Debug, Clone)]
pub(crate) struct ResolvedMediaValue {
    pub(crate) uri: String,
    pub(crate) mime_type: String,
}

pub(crate) fn resolve_media_value(
    db_path: &Path,
    source_base: Option<&Path>,
    type_name: &str,
    prop_name: &str,
    raw_value: &str,
    mime_hint: Option<&str>,
) -> Result<ResolvedMediaValue> {
    if let Some(path) = raw_value.strip_prefix("@file:") {
        let source_path = resolve_source_file_path(source_base, path)?;
        let bytes = fs::read(&source_path).map_err(|err| {
            NanoError::Storage(format!(
                "failed to read media source {}.{} from {}: {}",
                type_name,
                prop_name,
                source_path.display(),
                err
            ))
        })?;
        let mime_type = choose_mime_type(
            mime_hint,
            detect_mime_from_bytes(&bytes),
            source_path.extension().and_then(|ext| ext.to_str()),
        )?;
        let uri = import_media_bytes(db_path, type_name, &bytes, &mime_type)?;
        return Ok(ResolvedMediaValue { uri, mime_type });
    }

    if let Some(data) = raw_value.strip_prefix("@base64:") {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|err| {
                NanoError::Storage(format!(
                    "failed to decode base64 media for {}.{}: {}",
                    type_name, prop_name, err
                ))
            })?;
        let mime_type = choose_mime_type(mime_hint, detect_mime_from_bytes(&bytes), None)?;
        let uri = import_media_bytes(db_path, type_name, &bytes, &mime_type)?;
        return Ok(ResolvedMediaValue { uri, mime_type });
    }

    if let Some(uri) = raw_value.strip_prefix("@uri:") {
        let uri = uri.trim();
        if uri.is_empty() {
            return Err(NanoError::Storage(format!(
                "media URI for {}.{} cannot be empty",
                type_name, prop_name
            )));
        }
        let mime_type = resolve_external_uri_mime(uri, mime_hint).map_err(|err| {
            NanoError::Storage(format!(
                "failed to determine mime type for {}.{} from URI {}: {}",
                type_name, prop_name, uri, err
            ))
        })?;
        return Ok(ResolvedMediaValue {
            uri: uri.to_string(),
            mime_type,
        });
    }

    Err(NanoError::Storage(format!(
        "media property {}.{} must use @file:, @base64:, or @uri:",
        type_name, prop_name
    )))
}

fn resolve_source_file_path(source_base: Option<&Path>, raw_path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw_path);
    if path.is_absolute() {
        return Ok(path);
    }
    let Some(base) = source_base else {
        return Err(NanoError::Storage(format!(
            "relative @file: path '{}' requires a source file location",
            raw_path
        )));
    };
    Ok(base.join(path))
}

fn resolve_external_uri_mime(uri: &str, mime_hint: Option<&str>) -> Result<String> {
    if let Some(hint) = sanitize_mime_hint(mime_hint) {
        return Ok(hint);
    }

    if let Ok(url) = Url::parse(uri) {
        if url.scheme() == "file" {
            let path = url
                .to_file_path()
                .map_err(|_| NanoError::Storage(format!("invalid file URI '{}'", uri)))?;
            let bytes = fs::read(&path).map_err(|err| {
                NanoError::Storage(format!("failed to read {}: {}", path.display(), err))
            })?;
            return choose_mime_type(
                None,
                detect_mime_from_bytes(&bytes),
                path.extension().and_then(|ext| ext.to_str()),
            );
        }
        if let Some(ext) = Path::new(url.path())
            .extension()
            .and_then(|ext| ext.to_str())
        {
            return choose_mime_type(None, None, Some(ext));
        }
    }

    Err(NanoError::Storage(
        "explicit mime_type is required for non-file external media URIs without a recognized extension"
            .to_string(),
    ))
}

fn configured_media_root(db_path: &Path) -> PathBuf {
    if let Ok(root) = std::env::var(MEDIA_ROOT_ENV) {
        let root_path = PathBuf::from(root);
        if root_path.is_absolute() {
            return root_path;
        }
        return db_path.parent().unwrap_or(db_path).join(root_path);
    }
    db_path.parent().unwrap_or(db_path).join("media")
}

fn import_media_bytes(
    db_path: &Path,
    type_name: &str,
    bytes: &[u8],
    mime_type: &str,
) -> Result<String> {
    let root = configured_media_root(db_path);
    let type_dir = root.join(type_name.to_ascii_lowercase());
    fs::create_dir_all(&type_dir).map_err(|err| {
        NanoError::Storage(format!(
            "failed to create media directory {}: {}",
            type_dir.display(),
            err
        ))
    })?;

    let hash = Sha256::digest(bytes);
    let hash_hex = hex_lower(&hash);
    let ext = mime_to_extension(mime_type).unwrap_or("bin");
    let dest = type_dir.join(format!("{}.{}", hash_hex, ext));
    if !dest.exists() {
        fs::write(&dest, bytes).map_err(|err| {
            NanoError::Storage(format!(
                "failed to write imported media {}: {}",
                dest.display(),
                err
            ))
        })?;
    }

    let canonical = dest.canonicalize().map_err(|err| {
        NanoError::Storage(format!(
            "failed to canonicalize imported media {}: {}",
            dest.display(),
            err
        ))
    })?;
    let uri = Url::from_file_path(&canonical).map_err(|_| {
        NanoError::Storage(format!(
            "failed to build file URI for imported media {}",
            canonical.display()
        ))
    })?;
    Ok(uri.to_string())
}

fn choose_mime_type(
    mime_hint: Option<&str>,
    detected: Option<&'static str>,
    ext: Option<&str>,
) -> Result<String> {
    let hint = sanitize_mime_hint(mime_hint);
    let ext_mime = ext.and_then(extension_to_mime);
    match (hint.as_deref(), detected, ext_mime) {
        (Some(hint), Some(detected), _) if hint != detected => Err(NanoError::Storage(format!(
            "explicit mime_type '{}' does not match detected mime '{}'",
            hint, detected
        ))),
        (Some(hint), _, _) => Ok(hint.to_string()),
        (None, Some(detected), _) => Ok(detected.to_string()),
        (None, None, Some(ext_mime)) => Ok(ext_mime.to_string()),
        (None, None, None) => Err(NanoError::Storage(
            "unable to determine mime type; provide mime explicitly or use a recognized file type"
                .to_string(),
        )),
    }
}

fn sanitize_mime_hint(mime_hint: Option<&str>) -> Option<String> {
    mime_hint
        .map(str::trim)
        .filter(|mime| !mime.is_empty())
        .map(str::to_string)
}

fn detect_mime_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"%PDF-") {
        return Some("application/pdf");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return Some("audio/wav");
    }
    if bytes.starts_with(b"ID3")
        || bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0
    {
        return Some("audio/mpeg");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some("video/mp4");
    }
    None
}

fn extension_to_mime(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "pdf" => Some("application/pdf"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "mp4" | "m4v" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        _ => None,
    }
}

fn mime_to_extension(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "application/pdf" => Some("pdf"),
        "audio/mpeg" => Some("mp3"),
        "audio/wav" => Some("wav"),
        "video/mp4" => Some("mp4"),
        "video/quicktime" => Some("mov"),
        _ => None,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}
