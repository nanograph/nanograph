use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use reqwest::Client;
use serde::Deserialize;
use tokio::time::sleep;

#[cfg(feature = "local-embed")]
use std::sync::Arc;

use crate::error::{NanoError, Result};
#[cfg(feature = "local-embed")]
use crate::embedding_local::LocalEmbeddingModel;

const DEFAULT_OPENAI_EMBED_MODEL: &str = "text-embedding-3-small";
const DEFAULT_GEMINI_EMBED_MODEL: &str = "gemini-embedding-2-preview";
#[cfg(feature = "local-embed")]
const DEFAULT_LOCAL_EMBED_MODEL: &str = "hf:sentence-transformers/all-MiniLM-L6-v2";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_RETRY_ATTEMPTS: usize = 4;
const DEFAULT_RETRY_BACKOFF_MS: u64 = 200;
const GEMINI_MEDIA_BATCH_MAX: usize = 6;
const GEMINI_TEXT_TOKEN_LIMIT: usize = 8192;
const GEMINI_VIDEO_MAX_SECONDS: f64 = 120.0;
const GEMINI_PDF_MAX_PAGES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingProvider {
    OpenAi,
    Gemini,
    #[cfg(feature = "local-embed")]
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiMediaKind {
    Image,
    Video,
    Audio,
    Document,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EmbedRole {
    Document,
    Query,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum MediaSource {
    LocalFile {
        path: PathBuf,
        mime_type: String,
        size_bytes: u64,
    },
    RemoteUri {
        uri: String,
        mime_type: String,
    },
}

impl MediaSource {
    pub(crate) fn mime_type(&self) -> &str {
        match self {
            Self::LocalFile { mime_type, .. } | Self::RemoteUri { mime_type, .. } => mime_type,
        }
    }

    fn mock_identity(&self) -> String {
        match self {
            Self::LocalFile {
                path,
                mime_type,
                size_bytes,
            } => format!("local:{}:{}:{}", path.display(), mime_type, size_bytes),
            Self::RemoteUri { uri, mime_type } => format!("remote:{}:{}", uri, mime_type),
        }
    }
}

impl EmbeddingTransport {
    fn provider_name(&self) -> &'static str {
        match self {
            Self::Mock => "Mock",
            Self::OpenAi { .. } => "OpenAI",
            Self::Gemini { .. } => "Gemini",
            #[cfg(feature = "local-embed")]
            Self::Local { .. } => "Local",
        }
    }

    fn supports_media_embeddings(&self) -> bool {
        matches!(self, Self::Mock | Self::Gemini { .. })
    }

    fn media_embedding_error(&self) -> NanoError {
        match self {
            Self::Mock | Self::Gemini { .. } => {
                unreachable!("transport supports media embeddings")
            }
            Self::OpenAi { .. } => NanoError::Execution(format!(
                "{} embeddings are text-only; media embeddings are not supported",
                self.provider_name()
            )),
            #[cfg(feature = "local-embed")]
            Self::Local { .. } => NanoError::Execution(
                "local ONNX embeddings are text-only; media embeddings are not supported"
                    .to_string(),
            ),
        }
    }
}

#[derive(Clone)]
enum EmbeddingTransport {
    Mock,
    OpenAi {
        api_key: String,
        base_url: String,
        http: Client,
    },
    Gemini {
        api_key: String,
        base_url: String,
        http: Client,
    },
    #[cfg(feature = "local-embed")]
    Local {
        model: Arc<LocalEmbeddingModel>,
    },
}

#[derive(Clone)]
pub(crate) struct EmbeddingClient {
    model: String,
    retry_attempts: usize,
    retry_backoff_ms: u64,
    transport: EmbeddingTransport,
}

#[derive(Debug)]
struct EmbedCallError {
    message: String,
    retryable: bool,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingDatum>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingDatum {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorEnvelope {
    error: OpenAiErrorBody,
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorBody {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GeminiBatchEmbeddingResponse {
    embeddings: Vec<GeminiContentEmbedding>,
}

#[derive(Debug, Deserialize)]
struct GeminiContentEmbedding {
    values: Vec<f32>,
}

#[derive(Debug, Deserialize)]
struct GeminiErrorEnvelope {
    error: GeminiErrorBody,
}

#[derive(Debug, Deserialize)]
struct GeminiErrorBody {
    message: String,
}

#[derive(serde::Serialize)]
struct GeminiBatchEmbedContentsRequest {
    requests: Vec<GeminiEmbedContentRequest>,
}

#[derive(serde::Serialize)]
struct GeminiEmbedContentRequest {
    model: String,
    content: GeminiContent,
    #[serde(rename = "taskType", skip_serializing_if = "Option::is_none")]
    task_type: Option<String>,
    #[serde(rename = "outputDimensionality")]
    output_dimensionality: usize,
}

#[derive(serde::Serialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
    },
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: GeminiInlineData,
    },
    FileData {
        #[serde(rename = "fileData")]
        file_data: GeminiFileData,
    },
}

#[derive(serde::Serialize)]
struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(serde::Serialize)]
struct GeminiFileData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    #[serde(rename = "fileUri")]
    file_uri: String,
}

impl EmbeddingClient {
    pub(crate) fn from_env() -> Result<Self> {
        let retry_attempts =
            parse_env_usize("NANOGRAPH_EMBED_RETRY_ATTEMPTS", DEFAULT_RETRY_ATTEMPTS);
        let retry_backoff_ms =
            parse_env_u64("NANOGRAPH_EMBED_RETRY_BACKOFF_MS", DEFAULT_RETRY_BACKOFF_MS);

        if env_flag("NANOGRAPH_EMBEDDINGS_MOCK") {
            return Ok(Self {
                model: DEFAULT_OPENAI_EMBED_MODEL.to_string(),
                retry_attempts,
                retry_backoff_ms,
                transport: EmbeddingTransport::Mock,
            });
        }

        let configured_model = std::env::var("NANOGRAPH_EMBED_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let provider = infer_embedding_provider(configured_model.as_deref())?;

        #[cfg(feature = "local-embed")]
        if provider == EmbeddingProvider::Local {
            let repo_id = configured_model
                .as_deref()
                .and_then(crate::embedding_local::parse_hf_model_id)
                .ok_or_else(|| {
                    NanoError::Execution(
                        "local embedding provider requires NANOGRAPH_EMBED_MODEL=hf:<org>/<repo>"
                            .to_string(),
                    )
                })?;
            let model_arc = LocalEmbeddingModel::load(repo_id)?;
            let model_name = model_arc.model_name().to_string();
            return Ok(Self {
                model: model_name,
                retry_attempts,
                retry_backoff_ms,
                transport: EmbeddingTransport::Local { model: model_arc },
            });
        }

        let model =
            configured_model.unwrap_or_else(|| default_model_for_provider(provider).to_string());

        let timeout_ms = parse_env_u64("NANOGRAPH_EMBED_TIMEOUT_MS", DEFAULT_TIMEOUT_MS);
        let http = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .map_err(|e| {
                NanoError::Execution(format!("failed to initialize HTTP client: {}", e))
            })?;

        let transport = match provider {
            EmbeddingProvider::OpenAi => {
                let api_key = require_api_key("OPENAI_API_KEY", "OpenAI")?;
                let base_url = std::env::var("OPENAI_BASE_URL")
                    .ok()
                    .map(|v| v.trim_end_matches('/').to_string())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
                EmbeddingTransport::OpenAi {
                    api_key,
                    base_url,
                    http,
                }
            }
            EmbeddingProvider::Gemini => {
                let api_key = require_api_key("GEMINI_API_KEY", "Gemini")?;
                let base_url = std::env::var("GEMINI_BASE_URL")
                    .ok()
                    .map(|v| v.trim_end_matches('/').to_string())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| DEFAULT_GEMINI_BASE_URL.to_string());
                EmbeddingTransport::Gemini {
                    api_key,
                    base_url,
                    http,
                }
            }
            #[cfg(feature = "local-embed")]
            EmbeddingProvider::Local => unreachable!("handled above"),
        };

        Ok(Self {
            model,
            retry_attempts,
            retry_backoff_ms,
            transport,
        })
    }

    #[cfg(test)]
    pub(crate) fn mock_for_tests() -> Self {
        Self {
            model: DEFAULT_OPENAI_EMBED_MODEL.to_string(),
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_backoff_ms: DEFAULT_RETRY_BACKOFF_MS,
            transport: EmbeddingTransport::Mock,
        }
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) async fn embed_text(&self, input: &str, expected_dim: usize) -> Result<Vec<f32>> {
        let mut vectors = self
            .embed_texts_with_role(&[input.to_string()], expected_dim, EmbedRole::Document)
            .await?;
        vectors.pop().ok_or_else(|| {
            NanoError::Execution("embedding provider returned no vector".to_string())
        })
    }

    pub(crate) async fn embed_texts(
        &self,
        inputs: &[String],
        expected_dim: usize,
    ) -> Result<Vec<Vec<f32>>> {
        self.embed_texts_with_role(inputs, expected_dim, EmbedRole::Document)
            .await
    }

    pub(crate) async fn embed_texts_with_role(
        &self,
        inputs: &[String],
        expected_dim: usize,
        role: EmbedRole,
    ) -> Result<Vec<Vec<f32>>> {
        if expected_dim == 0 {
            return Err(NanoError::Execution(
                "embedding dimension must be greater than zero".to_string(),
            ));
        }
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        match &self.transport {
            EmbeddingTransport::Mock => Ok(inputs
                .iter()
                .map(|input| mock_text_embedding(input, role, expected_dim))
                .collect()),
            EmbeddingTransport::OpenAi { .. } => {
                self.embed_texts_openai_with_retry(inputs, expected_dim)
                    .await
            }
            EmbeddingTransport::Gemini { .. } => {
                self.embed_texts_gemini_with_retry(inputs, expected_dim, role)
                    .await
            }
            #[cfg(feature = "local-embed")]
            EmbeddingTransport::Local { model } => {
                let model_dim = model.dim();
                if model_dim != expected_dim {
                    return Err(NanoError::Execution(format!(
                        "local embedding model produces {}-d vectors but schema declares Vector({}); \
                         use a model whose output dimension matches or change the schema",
                        model_dim, expected_dim
                    )));
                }
                let model = model.clone();
                let inputs = inputs.to_vec();
                tokio::task::spawn_blocking(move || model.embed_texts(&inputs))
                    .await
                    .map_err(|e| {
                        NanoError::Execution(format!("local embedding task failed: {}", e))
                    })?
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn embed_media(
        &self,
        input: &MediaSource,
        expected_dim: usize,
        role: EmbedRole,
    ) -> Result<Vec<f32>> {
        let mut vectors = self
            .embed_media_many(std::slice::from_ref(input), expected_dim, role)
            .await?;
        vectors.pop().ok_or_else(|| {
            NanoError::Execution("embedding provider returned no vector".to_string())
        })
    }

    pub(crate) async fn embed_media_many(
        &self,
        inputs: &[MediaSource],
        expected_dim: usize,
        role: EmbedRole,
    ) -> Result<Vec<Vec<f32>>> {
        if expected_dim == 0 {
            return Err(NanoError::Execution(
                "embedding dimension must be greater than zero".to_string(),
            ));
        }
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        if !self.transport.supports_media_embeddings() {
            return Err(self.transport.media_embedding_error());
        }

        match &self.transport {
            EmbeddingTransport::Mock => Ok(inputs
                .iter()
                .map(|input| mock_media_embedding(input, role, expected_dim))
                .collect()),
            EmbeddingTransport::OpenAi { .. } => unreachable!("openai transport is rejected"),
            EmbeddingTransport::Gemini { .. } => {
                self.embed_media_gemini_with_retry(inputs, expected_dim, role)
                    .await
            }
            #[cfg(feature = "local-embed")]
            EmbeddingTransport::Local { .. } => unreachable!("local transport is rejected"),
        }
    }

    async fn embed_texts_openai_with_retry(
        &self,
        inputs: &[String],
        expected_dim: usize,
    ) -> Result<Vec<Vec<f32>>> {
        let max_attempt = self.retry_attempts.max(1);
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self.embed_texts_openai_once(inputs, expected_dim).await {
                Ok(vectors) => return Ok(vectors),
                Err(err) => {
                    if !err.retryable || attempt >= max_attempt {
                        return Err(NanoError::Execution(err.message));
                    }
                    let shift = (attempt - 1).min(10) as u32;
                    let delay = self.retry_backoff_ms.saturating_mul(1u64 << shift);
                    sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }

    async fn embed_texts_openai_once(
        &self,
        inputs: &[String],
        expected_dim: usize,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedCallError> {
        let (api_key, base_url, http) = match &self.transport {
            EmbeddingTransport::OpenAi {
                api_key,
                base_url,
                http,
            } => (api_key, base_url, http),
            EmbeddingTransport::Mock => unreachable!("mock transport should not call OpenAI"),
            EmbeddingTransport::Gemini { .. } => {
                unreachable!("gemini transport should not call OpenAI")
            }
            #[cfg(feature = "local-embed")]
            EmbeddingTransport::Local { .. } => {
                unreachable!("local transport should not call OpenAI")
            }
        };

        let request = serde_json::json!({
            "model": self.model,
            "input": inputs,
            "dimensions": expected_dim,
        });
        let url = format!("{}/embeddings", base_url);
        let response = http
            .post(&url)
            .bearer_auth(api_key)
            .json(&request)
            .send()
            .await;

        let response = match response {
            Ok(resp) => resp,
            Err(err) => {
                let retryable = err.is_timeout() || err.is_connect() || err.is_request();
                return Err(EmbedCallError {
                    message: format!("embedding request failed: {}", err),
                    retryable,
                });
            }
        };

        let status = response.status();
        let body = match response.text().await {
            Ok(body) => body,
            Err(err) => {
                return Err(EmbedCallError {
                    message: format!(
                        "embedding response read failed (status {}): {}",
                        status, err
                    ),
                    retryable: status.is_server_error() || status.as_u16() == 429,
                });
            }
        };

        if !status.is_success() {
            let message = parse_openai_error_message(&body).unwrap_or_else(|| body.clone());
            return Err(EmbedCallError {
                message: format!(
                    "embedding request failed with status {}: {}",
                    status, message
                ),
                retryable: status.is_server_error() || status.as_u16() == 429,
            });
        }

        let mut parsed: OpenAiEmbeddingResponse =
            serde_json::from_str(&body).map_err(|err| EmbedCallError {
                message: format!("embedding response decode failed: {}", err),
                retryable: false,
            })?;

        if parsed.data.len() != inputs.len() {
            return Err(EmbedCallError {
                message: format!(
                    "embedding response size mismatch: expected {}, got {}",
                    inputs.len(),
                    parsed.data.len()
                ),
                retryable: false,
            });
        }

        parsed.data.sort_by_key(|item| item.index);
        let mut vectors = Vec::with_capacity(parsed.data.len());
        for (idx, item) in parsed.data.into_iter().enumerate() {
            if item.index != idx {
                return Err(EmbedCallError {
                    message: format!(
                        "embedding response index mismatch at position {}: got {}",
                        idx, item.index
                    ),
                    retryable: false,
                });
            }
            if item.embedding.len() != expected_dim {
                return Err(EmbedCallError {
                    message: format!(
                        "embedding dimension mismatch: expected {}, got {}",
                        expected_dim,
                        item.embedding.len()
                    ),
                    retryable: false,
                });
            }
            vectors.push(item.embedding);
        }
        Ok(vectors)
    }

    async fn embed_texts_gemini_with_retry(
        &self,
        inputs: &[String],
        expected_dim: usize,
        role: EmbedRole,
    ) -> Result<Vec<Vec<f32>>> {
        let max_attempt = self.retry_attempts.max(1);
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self
                .embed_texts_gemini_once(inputs, expected_dim, role)
                .await
            {
                Ok(vectors) => return Ok(vectors),
                Err(err) => {
                    if !err.retryable || attempt >= max_attempt {
                        return Err(NanoError::Execution(err.message));
                    }
                    let shift = (attempt - 1).min(10) as u32;
                    let delay = self.retry_backoff_ms.saturating_mul(1u64 << shift);
                    sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }

    async fn embed_texts_gemini_once(
        &self,
        inputs: &[String],
        expected_dim: usize,
        role: EmbedRole,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedCallError> {
        for input in inputs {
            validate_gemini_text_input(input)?;
        }
        let requests = inputs
            .iter()
            .map(|input| GeminiEmbedContentRequest {
                model: format!("models/{}", self.model),
                content: GeminiContent {
                    parts: vec![GeminiPart::Text {
                        text: input.to_string(),
                    }],
                },
                task_type: Some(gemini_task_type(role).to_string()),
                output_dimensionality: expected_dim,
            })
            .collect();
        self.embed_gemini_requests(requests, expected_dim).await
    }

    async fn embed_media_gemini_with_retry(
        &self,
        inputs: &[MediaSource],
        expected_dim: usize,
        role: EmbedRole,
    ) -> Result<Vec<Vec<f32>>> {
        let max_attempt = self.retry_attempts.max(1);
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self
                .embed_media_gemini_once(inputs, expected_dim, role)
                .await
            {
                Ok(vectors) => return Ok(vectors),
                Err(err) => {
                    if !err.retryable || attempt >= max_attempt {
                        return Err(NanoError::Execution(err.message));
                    }
                    let shift = (attempt - 1).min(10) as u32;
                    let delay = self.retry_backoff_ms.saturating_mul(1u64 << shift);
                    sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }

    async fn embed_media_gemini_once(
        &self,
        inputs: &[MediaSource],
        expected_dim: usize,
        role: EmbedRole,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedCallError> {
        let mut out = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(GEMINI_MEDIA_BATCH_MAX) {
            let mut requests = Vec::with_capacity(chunk.len());
            for input in chunk {
                requests.push(self.gemini_media_request(input, expected_dim, role).await?);
            }
            out.extend(self.embed_gemini_requests(requests, expected_dim).await?);
        }
        Ok(out)
    }

    async fn gemini_media_request(
        &self,
        input: &MediaSource,
        expected_dim: usize,
        role: EmbedRole,
    ) -> std::result::Result<GeminiEmbedContentRequest, EmbedCallError> {
        let media_kind = classify_gemini_media_kind(input.mime_type())?;
        let part = match &self.transport {
            EmbeddingTransport::Gemini { http, .. } => match input {
                MediaSource::LocalFile {
                    path,
                    mime_type,
                    size_bytes,
                } => {
                    let bytes = tokio::fs::read(path).await.map_err(|err| EmbedCallError {
                        message: format!(
                            "failed to read media file for embedding {}: {}",
                            path.display(),
                            err
                        ),
                        retryable: false,
                    })?;
                    validate_gemini_media_bytes(
                        media_kind,
                        mime_type,
                        &bytes,
                        &path.display().to_string(),
                    )?;
                    if bytes.len() as u64 != *size_bytes {
                        return Err(EmbedCallError {
                            message: format!(
                                "media file size changed while reading {}: expected {} bytes, got {}",
                                path.display(),
                                size_bytes,
                                bytes.len()
                            ),
                            retryable: false,
                        });
                    }
                    GeminiPart::InlineData {
                        inline_data: GeminiInlineData {
                            mime_type: mime_type.clone(),
                            data: base64::engine::general_purpose::STANDARD.encode(bytes),
                        },
                    }
                }
                MediaSource::RemoteUri { uri, mime_type } => {
                    let parsed = reqwest::Url::parse(uri).map_err(|err| EmbedCallError {
                        message: format!("invalid media URI {}: {}", uri, err),
                        retryable: false,
                    })?;
                    match parsed.scheme() {
                        "http" | "https" => {
                            let response =
                                http.get(uri).send().await.map_err(|err| EmbedCallError {
                                    message: format!(
                                        "failed to fetch remote media {}: {}",
                                        uri, err
                                    ),
                                    retryable: err.is_timeout()
                                        || err.is_connect()
                                        || err.is_request(),
                                })?;
                            let status = response.status();
                            if !status.is_success() {
                                return Err(EmbedCallError {
                                    message: format!(
                                        "failed to fetch remote media {}: status {}",
                                        uri, status
                                    ),
                                    retryable: status.is_server_error() || status.as_u16() == 429,
                                });
                            }
                            let bytes = response.bytes().await.map_err(|err| EmbedCallError {
                                message: format!("failed to read remote media {}: {}", uri, err),
                                retryable: false,
                            })?;
                            validate_gemini_media_bytes(media_kind, mime_type, &bytes, uri)?;
                            GeminiPart::InlineData {
                                inline_data: GeminiInlineData {
                                    mime_type: mime_type.clone(),
                                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                                },
                            }
                        }
                        _ => match media_kind {
                            GeminiMediaKind::Image | GeminiMediaKind::Audio => {
                                GeminiPart::FileData {
                                    file_data: GeminiFileData {
                                        mime_type: mime_type.clone(),
                                        file_uri: uri.clone(),
                                    },
                                }
                            }
                            GeminiMediaKind::Video => {
                                return Err(EmbedCallError {
                                    message: format!(
                                        "Gemini video embeddings require local files or http(s) URLs so NanoGraph can enforce the 120-second limit; unsupported URI {}",
                                        uri
                                    ),
                                    retryable: false,
                                });
                            }
                            GeminiMediaKind::Document => {
                                return Err(EmbedCallError {
                                    message: format!(
                                        "Gemini PDF embeddings require local files or http(s) URLs so NanoGraph can enforce the 6-page limit; unsupported URI {}",
                                        uri
                                    ),
                                    retryable: false,
                                });
                            }
                        },
                    }
                }
            },
            _ => unreachable!("non-gemini transport should not build Gemini media requests"),
        };

        Ok(GeminiEmbedContentRequest {
            model: format!("models/{}", self.model),
            content: GeminiContent { parts: vec![part] },
            task_type: Some(gemini_task_type(role).to_string()),
            output_dimensionality: expected_dim,
        })
    }

    async fn embed_gemini_requests(
        &self,
        requests: Vec<GeminiEmbedContentRequest>,
        expected_dim: usize,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedCallError> {
        let (api_key, base_url, http) = match &self.transport {
            EmbeddingTransport::Gemini {
                api_key,
                base_url,
                http,
            } => (api_key, base_url, http),
            _ => unreachable!("non-gemini transport should not call Gemini"),
        };
        let expected_count = requests.len();
        let url = format!("{}/models/{}:batchEmbedContents", base_url, self.model);
        let response = http
            .post(&url)
            .header("x-goog-api-key", api_key)
            .json(&GeminiBatchEmbedContentsRequest { requests })
            .send()
            .await;

        let response = match response {
            Ok(resp) => resp,
            Err(err) => {
                let retryable = err.is_timeout() || err.is_connect() || err.is_request();
                return Err(EmbedCallError {
                    message: format!("embedding request failed: {}", err),
                    retryable,
                });
            }
        };

        let status = response.status();
        let body = match response.text().await {
            Ok(body) => body,
            Err(err) => {
                return Err(EmbedCallError {
                    message: format!(
                        "embedding response read failed (status {}): {}",
                        status, err
                    ),
                    retryable: status.is_server_error() || status.as_u16() == 429,
                });
            }
        };

        if !status.is_success() {
            let message = parse_gemini_error_message(&body).unwrap_or_else(|| body.clone());
            return Err(EmbedCallError {
                message: format!(
                    "embedding request failed with status {}: {}",
                    status, message
                ),
                retryable: status.is_server_error() || status.as_u16() == 429,
            });
        }

        let parsed: GeminiBatchEmbeddingResponse =
            serde_json::from_str(&body).map_err(|err| EmbedCallError {
                message: format!("embedding response decode failed: {}", err),
                retryable: false,
            })?;
        if parsed.embeddings.len() != expected_count {
            return Err(EmbedCallError {
                message: format!(
                    "embedding response size mismatch: expected {}, got {}",
                    expected_count,
                    parsed.embeddings.len()
                ),
                retryable: false,
            });
        }
        let mut vectors = Vec::with_capacity(parsed.embeddings.len());
        for embedding in parsed.embeddings {
            if embedding.values.len() != expected_dim {
                return Err(EmbedCallError {
                    message: format!(
                        "embedding dimension mismatch: expected {}, got {}",
                        expected_dim,
                        embedding.values.len()
                    ),
                    retryable: false,
                });
            }
            vectors.push(normalize_embedding(embedding.values));
        }
        Ok(vectors)
    }
}

fn parse_openai_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<OpenAiErrorEnvelope>(body)
        .ok()
        .map(|e| e.error.message)
        .filter(|msg| !msg.trim().is_empty())
}

fn parse_gemini_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<GeminiErrorEnvelope>(body)
        .ok()
        .map(|e| e.error.message)
        .filter(|msg| !msg.trim().is_empty())
}

fn require_api_key(var_name: &str, provider_name: &str) -> Result<String> {
    std::env::var(var_name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            NanoError::Execution(format!(
                "{} API key env {} is required when an embedding call is needed",
                provider_name, var_name
            ))
        })
}

fn infer_embedding_provider(configured_model: Option<&str>) -> Result<EmbeddingProvider> {
    if let Some(provider) = std::env::var("NANOGRAPH_EMBED_PROVIDER")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
    {
        return match provider.as_str() {
            "openai" => Ok(EmbeddingProvider::OpenAi),
            "gemini" => Ok(EmbeddingProvider::Gemini),
            #[cfg(feature = "local-embed")]
            "local" => Ok(EmbeddingProvider::Local),
            #[cfg(not(feature = "local-embed"))]
            "local" => Err(NanoError::Execution(
                "embedding provider `local` requires the `local-embed` feature flag \
                 (rebuild with `cargo build --features local-embed`)"
                    .to_string(),
            )),
            other => Err(NanoError::Execution(format!(
                "unsupported embedding provider `{}` (supported: openai, gemini, local)",
                other
            ))),
        };
    }

    #[cfg(feature = "local-embed")]
    if configured_model.is_some_and(|m| crate::embedding_local::parse_hf_model_id(m).is_some()) {
        return Ok(EmbeddingProvider::Local);
    }

    if configured_model.is_some_and(|model| model.starts_with("gemini-")) {
        return Ok(EmbeddingProvider::Gemini);
    }
    if std::env::var_os("GEMINI_API_KEY").is_some() && std::env::var_os("OPENAI_API_KEY").is_none()
    {
        return Ok(EmbeddingProvider::Gemini);
    }
    Ok(EmbeddingProvider::OpenAi)
}

fn default_model_for_provider(provider: EmbeddingProvider) -> &'static str {
    match provider {
        EmbeddingProvider::OpenAi => DEFAULT_OPENAI_EMBED_MODEL,
        EmbeddingProvider::Gemini => DEFAULT_GEMINI_EMBED_MODEL,
        #[cfg(feature = "local-embed")]
        EmbeddingProvider::Local => DEFAULT_LOCAL_EMBED_MODEL,
    }
}

fn gemini_task_type(role: EmbedRole) -> &'static str {
    match role {
        EmbedRole::Document => "RETRIEVAL_DOCUMENT",
        EmbedRole::Query => "RETRIEVAL_QUERY",
    }
}

fn validate_gemini_text_input(input: &str) -> std::result::Result<(), EmbedCallError> {
    let estimated_tokens = estimate_gemini_text_tokens(input);
    if estimated_tokens > GEMINI_TEXT_TOKEN_LIMIT {
        return Err(EmbedCallError {
            message: format!(
                "Gemini text embeddings support up to {} input tokens; NanoGraph's conservative estimate for this input is {} tokens",
                GEMINI_TEXT_TOKEN_LIMIT, estimated_tokens
            ),
            retryable: false,
        });
    }
    Ok(())
}

fn estimate_gemini_text_tokens(input: &str) -> usize {
    let word_based = input.split_whitespace().count().saturating_mul(2);
    let byte_based = input.len().div_ceil(3);
    word_based.max(byte_based)
}

fn classify_gemini_media_kind(
    mime_type: &str,
) -> std::result::Result<GeminiMediaKind, EmbedCallError> {
    match mime_type {
        "image/png" | "image/jpeg" => Ok(GeminiMediaKind::Image),
        "video/mp4" | "video/quicktime" => Ok(GeminiMediaKind::Video),
        "application/pdf" => Ok(GeminiMediaKind::Document),
        mime if mime.starts_with("audio/") => Ok(GeminiMediaKind::Audio),
        other => Err(EmbedCallError {
            message: format!(
                "Gemini multimodal embeddings support only image/png, image/jpeg, application/pdf, video/mp4, video/quicktime, and audio/* inputs in NanoGraph; got {}",
                other
            ),
            retryable: false,
        }),
    }
}

fn validate_gemini_media_bytes(
    media_kind: GeminiMediaKind,
    mime_type: &str,
    bytes: &[u8],
    label: &str,
) -> std::result::Result<(), EmbedCallError> {
    match media_kind {
        GeminiMediaKind::Image | GeminiMediaKind::Audio => Ok(()),
        GeminiMediaKind::Document => {
            let page_count = count_pdf_pages(bytes).ok_or_else(|| EmbedCallError {
                message: format!(
                    "failed to determine PDF page count for {}; Gemini PDF embeddings require a PDF up to {} pages",
                    label, GEMINI_PDF_MAX_PAGES
                ),
                retryable: false,
            })?;
            if page_count > GEMINI_PDF_MAX_PAGES {
                return Err(EmbedCallError {
                    message: format!(
                        "Gemini PDF embeddings support up to {} pages; {} has {} pages",
                        GEMINI_PDF_MAX_PAGES, label, page_count
                    ),
                    retryable: false,
                });
            }
            Ok(())
        }
        GeminiMediaKind::Video => {
            let duration_seconds =
                parse_mp4_like_duration_seconds(bytes).ok_or_else(|| EmbedCallError {
                    message: format!(
                        "failed to determine video duration for {}; Gemini video embeddings require MP4 or MOV inputs up to {:.0} seconds",
                        label, GEMINI_VIDEO_MAX_SECONDS
                    ),
                    retryable: false,
                })?;
            if duration_seconds > GEMINI_VIDEO_MAX_SECONDS {
                return Err(EmbedCallError {
                    message: format!(
                        "Gemini video embeddings support up to {:.0} seconds; {} is {:.2} seconds",
                        GEMINI_VIDEO_MAX_SECONDS, label, duration_seconds
                    ),
                    retryable: false,
                });
            }
            match mime_type {
                "video/mp4" | "video/quicktime" => Ok(()),
                other => Err(EmbedCallError {
                    message: format!(
                        "Gemini video embeddings support only MP4 and MOV inputs in NanoGraph; got {}",
                        other
                    ),
                    retryable: false,
                }),
            }
        }
    }
}

fn count_pdf_pages(bytes: &[u8]) -> Option<usize> {
    let needle = b"/Type /Page";
    let mut count = 0usize;
    let mut offset = 0usize;
    while offset + needle.len() <= bytes.len() {
        if &bytes[offset..offset + needle.len()] == needle {
            let next = bytes.get(offset + needle.len()).copied();
            if !matches!(next, Some(b'a'..=b'z' | b'A'..=b'Z')) {
                count += 1;
            }
            offset += needle.len();
        } else {
            offset += 1;
        }
    }
    if count > 0 {
        return Some(count);
    }

    let fallback_text = String::from_utf8_lossy(bytes);
    fallback_text
        .split("endobj")
        .filter(|object| object.contains("/Type /Pages") || object.contains("/Type/Pages"))
        .flat_map(extract_pdf_count_values)
        .max()
}

fn extract_pdf_count_values(object: &str) -> Vec<usize> {
    let mut counts = Vec::new();
    let mut rest = object;
    while let Some(idx) = rest.find("/Count") {
        let after = &rest[idx + "/Count".len()..];
        let digits: String = after
            .chars()
            .skip_while(|ch| ch.is_ascii_whitespace())
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if let Ok(value) = digits.parse::<usize>() {
            counts.push(value);
        }
        rest = after;
    }
    counts
}

fn parse_mp4_like_duration_seconds(bytes: &[u8]) -> Option<f64> {
    let mut offset = 0usize;
    while let Some((box_type, box_payload, next_offset)) = next_mp4_box(bytes, offset) {
        if box_type == *b"moov" {
            let mut inner_offset = 0usize;
            while let Some((inner_type, inner_payload, next_inner_offset)) =
                next_mp4_box(box_payload, inner_offset)
            {
                if inner_type == *b"mvhd" {
                    return parse_mvhd_duration_seconds(inner_payload);
                }
                inner_offset = next_inner_offset;
            }
        }
        offset = next_offset;
    }
    None
}

fn next_mp4_box(bytes: &[u8], offset: usize) -> Option<([u8; 4], &[u8], usize)> {
    if offset.checked_add(8)? > bytes.len() {
        return None;
    }
    let size32 = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?);
    let box_type: [u8; 4] = bytes[offset + 4..offset + 8].try_into().ok()?;
    let (box_size, header_size) = match size32 {
        0 => (bytes.len().checked_sub(offset)?, 8usize),
        1 => {
            if offset.checked_add(16)? > bytes.len() {
                return None;
            }
            let size64 = u64::from_be_bytes(bytes[offset + 8..offset + 16].try_into().ok()?);
            let size = usize::try_from(size64).ok()?;
            (size, 16usize)
        }
        n => (usize::try_from(n).ok()?, 8usize),
    };
    if box_size < header_size {
        return None;
    }
    let end = offset.checked_add(box_size)?;
    if end > bytes.len() {
        return None;
    }
    Some((box_type, &bytes[offset + header_size..end], end))
}

fn parse_mvhd_duration_seconds(payload: &[u8]) -> Option<f64> {
    let version = *payload.first()?;
    match version {
        0 => {
            if payload.len() < 20 {
                return None;
            }
            let timescale = u32::from_be_bytes(payload[12..16].try_into().ok()?);
            let duration = u32::from_be_bytes(payload[16..20].try_into().ok()?);
            (timescale > 0).then_some(duration as f64 / timescale as f64)
        }
        1 => {
            if payload.len() < 32 {
                return None;
            }
            let timescale = u32::from_be_bytes(payload[20..24].try_into().ok()?);
            let duration = u64::from_be_bytes(payload[24..32].try_into().ok()?);
            (timescale > 0).then_some(duration as f64 / timescale as f64)
        }
        _ => None,
    }
}

fn parse_env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn parse_env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            s == "1" || s == "true" || s == "yes" || s == "on"
        })
        .unwrap_or(false)
}

fn normalize_embedding(mut input: Vec<f32>) -> Vec<f32> {
    let norm = input
        .iter()
        .map(|value| (*value as f64) * (*value as f64))
        .sum::<f64>()
        .sqrt() as f32;
    if norm > f32::EPSILON {
        for value in &mut input {
            *value /= norm;
        }
    }
    input
}

fn mock_text_embedding(input: &str, role: EmbedRole, dim: usize) -> Vec<f32> {
    let semantic_key = normalize_mock_text_key(input);
    mock_semantic_embedding(&semantic_key, role, "text", dim)
}

fn mock_media_embedding(input: &MediaSource, role: EmbedRole, dim: usize) -> Vec<f32> {
    let semantic_key = normalize_mock_media_key(input);
    mock_semantic_embedding(&semantic_key, role, "media", dim)
}

fn mock_semantic_embedding(
    semantic_key: &str,
    role: EmbedRole,
    modality: &str,
    dim: usize,
) -> Vec<f32> {
    let mut base = mock_embedding_from_seed(&format!("semantic:{}", semantic_key), dim);
    let role_vec = mock_embedding_from_seed(
        &format!(
            "role:{}",
            match role {
                EmbedRole::Document => "document",
                EmbedRole::Query => "query",
            }
        ),
        dim,
    );
    let modality_vec = mock_embedding_from_seed(&format!("modality:{}", modality), dim);
    for idx in 0..dim {
        base[idx] = (base[idx] * 0.92) + (role_vec[idx] * 0.02) + (modality_vec[idx] * 0.06);
    }
    normalize_embedding(base)
}

fn normalize_mock_text_key(input: &str) -> String {
    input.trim().to_ascii_lowercase()
}

fn normalize_mock_media_key(input: &MediaSource) -> String {
    let candidate = match input {
        MediaSource::LocalFile { path, .. } => path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string),
        MediaSource::RemoteUri { uri, .. } => {
            let parsed = reqwest::Url::parse(uri).ok();
            parsed
                .as_ref()
                .and_then(|url| url.path_segments())
                .and_then(|mut segments| segments.next_back())
                .and_then(|last| last.split('.').next())
                .map(str::to_string)
                .or_else(|| {
                    uri.rsplit('/')
                        .next()
                        .and_then(|last| last.split('.').next())
                        .map(str::to_string)
                })
        }
    };
    candidate
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| input.mock_identity())
}

fn mock_embedding_from_seed(input: &str, dim: usize) -> Vec<f32> {
    let mut seed = fnv1a64(input.as_bytes());
    let mut out = Vec::with_capacity(dim);
    for _ in 0..dim {
        seed = xorshift64(seed);
        let ratio = (seed as f64 / u64::MAX as f64) as f32;
        out.push((ratio * 2.0) - 1.0);
    }

    let norm = out
        .iter()
        .map(|v| (*v as f64) * (*v as f64))
        .sum::<f64>()
        .sqrt() as f32;
    if norm > f32::EPSILON {
        for value in &mut out {
            *value /= norm;
        }
    }
    out
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 14695981039346656037u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211u64);
    }
    hash
}

fn xorshift64(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    #[tokio::test]
    async fn mock_embeddings_are_deterministic() {
        let client = EmbeddingClient::mock_for_tests();
        let a = client.embed_text("alpha", 8).await.unwrap();
        let b = client.embed_text("alpha", 8).await.unwrap();
        let c = client.embed_text("beta", 8).await.unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 8);
    }

    #[tokio::test]
    async fn mock_media_embeddings_are_deterministic() {
        let client = EmbeddingClient::mock_for_tests();
        let source = MediaSource::RemoteUri {
            uri: "s3://bucket/photo.png".to_string(),
            mime_type: "image/png".to_string(),
        };
        let a = client
            .embed_media(&source, 8, EmbedRole::Document)
            .await
            .unwrap();
        let b = client
            .embed_media(&source, 8, EmbedRole::Document)
            .await
            .unwrap();
        let c = client
            .embed_media(&source, 8, EmbedRole::Query)
            .await
            .unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 8);
    }

    #[tokio::test]
    async fn mock_embeddings_share_cross_modal_semantics_by_asset_name() {
        let client = EmbeddingClient::mock_for_tests();
        let query = client
            .embed_texts_with_role(&["space".to_string()], 16, EmbedRole::Query)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let matching = client
            .embed_media(
                &MediaSource::RemoteUri {
                    uri: "file:///tmp/space.jpg".to_string(),
                    mime_type: "image/jpeg".to_string(),
                },
                16,
                EmbedRole::Document,
            )
            .await
            .unwrap();
        let non_matching = client
            .embed_media(
                &MediaSource::RemoteUri {
                    uri: "file:///tmp/beach.jpg".to_string(),
                    mime_type: "image/jpeg".to_string(),
                },
                16,
                EmbedRole::Document,
            )
            .await
            .unwrap();

        assert!(cosine_similarity(&query, &matching) > cosine_similarity(&query, &non_matching));
    }

    #[tokio::test]
    async fn openai_rejects_media_embeddings() {
        let client = EmbeddingClient {
            model: DEFAULT_OPENAI_EMBED_MODEL.to_string(),
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_backoff_ms: DEFAULT_RETRY_BACKOFF_MS,
            transport: EmbeddingTransport::OpenAi {
                api_key: "openai-key".to_string(),
                base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
                http: Client::builder().build().unwrap(),
            },
        };

        let err = client
            .embed_media(
                &MediaSource::RemoteUri {
                    uri: "https://example.com/hero.jpg".to_string(),
                    mime_type: "image/jpeg".to_string(),
                },
                16,
                EmbedRole::Document,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("OpenAI embeddings are text-only"));
    }

    #[tokio::test]
    async fn gemini_local_media_requests_use_inline_data_shape() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("clip.mp4");
        let bytes = fake_mp4_with_duration(60, 1);
        tokio::fs::write(&path, &bytes).await.unwrap();
        let client = EmbeddingClient {
            model: DEFAULT_GEMINI_EMBED_MODEL.to_string(),
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_backoff_ms: DEFAULT_RETRY_BACKOFF_MS,
            transport: EmbeddingTransport::Gemini {
                api_key: "gemini-key".to_string(),
                base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
                http: Client::builder().build().unwrap(),
            },
        };

        let request = client
            .gemini_media_request(
                &MediaSource::LocalFile {
                    path,
                    mime_type: "video/mp4".to_string(),
                    size_bytes: bytes.len() as u64,
                },
                16,
                EmbedRole::Document,
            )
            .await
            .unwrap();
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["taskType"], "RETRIEVAL_DOCUMENT");
        assert_eq!(
            json["content"]["parts"][0]["inlineData"]["mimeType"],
            "video/mp4"
        );
        assert!(
            !json["content"]["parts"][0]["inlineData"]["data"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn gemini_remote_image_requests_use_file_data_passthrough_for_non_http_uris() {
        let client = EmbeddingClient {
            model: DEFAULT_GEMINI_EMBED_MODEL.to_string(),
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_backoff_ms: DEFAULT_RETRY_BACKOFF_MS,
            transport: EmbeddingTransport::Gemini {
                api_key: "gemini-key".to_string(),
                base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
                http: Client::builder().build().unwrap(),
            },
        };

        let request = client
            .gemini_media_request(
                &MediaSource::RemoteUri {
                    uri: "gs://bucket/photo.png".to_string(),
                    mime_type: "image/png".to_string(),
                },
                16,
                EmbedRole::Document,
            )
            .await
            .unwrap();
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["taskType"], "RETRIEVAL_DOCUMENT");
        assert_eq!(
            json["content"]["parts"][0]["fileData"]["mimeType"],
            "image/png"
        );
        assert_eq!(
            json["content"]["parts"][0]["fileData"]["fileUri"],
            "gs://bucket/photo.png"
        );
    }

    #[tokio::test]
    async fn gemini_rejects_remote_pdf_passthrough_without_byte_access() {
        let client = EmbeddingClient {
            model: DEFAULT_GEMINI_EMBED_MODEL.to_string(),
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_backoff_ms: DEFAULT_RETRY_BACKOFF_MS,
            transport: EmbeddingTransport::Gemini {
                api_key: "gemini-key".to_string(),
                base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
                http: Client::builder().build().unwrap(),
            },
        };

        let err = match client
            .gemini_media_request(
                &MediaSource::RemoteUri {
                    uri: "gs://bucket/manual.pdf".to_string(),
                    mime_type: "application/pdf".to_string(),
                },
                16,
                EmbedRole::Document,
            )
            .await
        {
            Ok(_) => panic!("expected remote PDF passthrough to be rejected"),
            Err(err) => err,
        };
        assert!(
            err.message
                .contains("Gemini PDF embeddings require local files or http(s) URLs")
        );
    }

    #[test]
    fn gemini_rejects_text_inputs_over_conservative_token_limit() {
        let input = "a".repeat((GEMINI_TEXT_TOKEN_LIMIT * 3) + 1);
        let err = validate_gemini_text_input(&input).unwrap_err();
        assert!(err.message.contains("up to 8192 input tokens"));
    }

    #[test]
    fn gemini_rejects_unsupported_image_mime() {
        let err = classify_gemini_media_kind("image/webp").unwrap_err();
        assert!(err.message.contains("image/png, image/jpeg"));
    }

    #[test]
    fn gemini_rejects_pdfs_over_page_limit() {
        let pdf = b"%PDF-1.7
1 0 obj << /Type /Catalog >> endobj
2 0 obj << /Type /Page >> endobj
3 0 obj << /Type /Page >> endobj
4 0 obj << /Type /Page >> endobj
5 0 obj << /Type /Page >> endobj
6 0 obj << /Type /Page >> endobj
7 0 obj << /Type /Page >> endobj
8 0 obj << /Type /Page >> endobj
%%EOF";
        let err = validate_gemini_media_bytes(
            GeminiMediaKind::Document,
            "application/pdf",
            pdf,
            "manual.pdf",
        )
        .unwrap_err();
        assert!(err.message.contains("up to 6 pages"));
    }

    #[test]
    fn count_pdf_pages_prefers_pages_tree_count_over_outline_counts() {
        let pdf = br#"%PDF-1.7
1 0 obj << /Type /Catalog /Outlines 2 0 R /Pages 3 0 R >> endobj
2 0 obj << /Count 28 /First 10 0 R /Last 20 0 R >> endobj
3 0 obj << /Count 5 /Kids [ 4 0 R 5 0 R 6 0 R 7 0 R 8 0 R ] /Type /Pages >> endobj
%%EOF"#;
        assert_eq!(count_pdf_pages(pdf), Some(5));
    }

    #[test]
    fn count_pdf_pages_uses_largest_pages_tree_count_for_nested_page_trees() {
        let pdf = br#"%PDF-1.7
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj
2 0 obj << /Count 15 /Kids [ 3 0 R 4 0 R 5 0 R ] /Type /Pages >> endobj
3 0 obj << /Count 6 /Kids [ 10 0 R 11 0 R ] /Type /Pages >> endobj
4 0 obj << /Count 6 /Kids [ 12 0 R 13 0 R ] /Type /Pages >> endobj
5 0 obj << /Count 3 /Kids [ 14 0 R ] /Type /Pages >> endobj
%%EOF"#;
        assert_eq!(count_pdf_pages(pdf), Some(15));
    }

    #[test]
    fn gemini_rejects_videos_over_duration_limit() {
        let video = fake_mp4_with_duration(121, 1);
        let err =
            validate_gemini_media_bytes(GeminiMediaKind::Video, "video/mp4", &video, "clip.mp4")
                .unwrap_err();
        assert!(err.message.contains("up to 120 seconds"));
    }

    #[tokio::test]
    async fn infer_provider_prefers_explicit_gemini_env() {
        let _guard = ENV_LOCK.lock().await;
        let prev_provider = std::env::var_os("NANOGRAPH_EMBED_PROVIDER");
        let prev_model = std::env::var_os("NANOGRAPH_EMBED_MODEL");
        let prev_openai = std::env::var_os("OPENAI_API_KEY");
        let prev_gemini = std::env::var_os("GEMINI_API_KEY");
        unsafe {
            std::env::set_var("NANOGRAPH_EMBED_PROVIDER", "gemini");
            std::env::remove_var("NANOGRAPH_EMBED_MODEL");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::set_var("GEMINI_API_KEY", "gem-key");
        }

        let provider = infer_embedding_provider(None).unwrap();
        assert_eq!(provider, EmbeddingProvider::Gemini);

        restore_env_var("NANOGRAPH_EMBED_PROVIDER", prev_provider);
        restore_env_var("NANOGRAPH_EMBED_MODEL", prev_model);
        restore_env_var("OPENAI_API_KEY", prev_openai);
        restore_env_var("GEMINI_API_KEY", prev_gemini);
    }

    #[test]
    fn gemini_text_requests_use_retrieval_roles() {
        let request = GeminiEmbedContentRequest {
            model: "models/gemini-embedding-2-preview".to_string(),
            content: GeminiContent {
                parts: vec![GeminiPart::Text {
                    text: "hello".to_string(),
                }],
            },
            task_type: Some(gemini_task_type(EmbedRole::Query).to_string()),
            output_dimensionality: 768,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["taskType"], "RETRIEVAL_QUERY");
        assert_eq!(json["outputDimensionality"], 768);
        assert_eq!(json["content"]["parts"][0]["text"], "hello");
    }

    fn restore_env_var(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
        left.iter()
            .zip(right.iter())
            .map(|(a, b)| a * b)
            .sum::<f32>()
    }

    fn fake_mp4_with_duration(duration: u32, timescale: u32) -> Vec<u8> {
        let mut mvhd_payload = Vec::new();
        mvhd_payload.push(0);
        mvhd_payload.extend_from_slice(&[0, 0, 0]);
        mvhd_payload.extend_from_slice(&0u32.to_be_bytes());
        mvhd_payload.extend_from_slice(&0u32.to_be_bytes());
        mvhd_payload.extend_from_slice(&timescale.to_be_bytes());
        mvhd_payload.extend_from_slice(&duration.to_be_bytes());
        let mvhd = mp4_box(*b"mvhd", &mvhd_payload);
        mp4_box(*b"moov", &mvhd)
    }

    fn mp4_box(box_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(size as usize);
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(&box_type);
        out.extend_from_slice(payload);
        out
    }
}
