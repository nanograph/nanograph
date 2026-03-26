use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use reqwest::Client;
use serde::Deserialize;
use tokio::time::sleep;

use crate::error::{NanoError, Result};

const DEFAULT_OPENAI_EMBED_MODEL: &str = "text-embedding-3-small";
const DEFAULT_GEMINI_EMBED_MODEL: &str = "gemini-embedding-2-preview";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_RETRY_ATTEMPTS: usize = 4;
const DEFAULT_RETRY_BACKOFF_MS: u64 = 200;
const GEMINI_IMAGE_BATCH_MAX: usize = 6;
const GEMINI_IMAGE_INLINE_MAX_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingProvider {
    OpenAi,
    Gemini,
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
}

#[derive(Clone)]
pub(crate) struct EmbeddingClient {
    model: String,
    retry_attempts: usize,
    retry_backoff_ms: u64,
    transport: EmbeddingTransport,
}

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
}

#[derive(serde::Serialize)]
struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
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

        match &self.transport {
            EmbeddingTransport::Mock => Ok(inputs
                .iter()
                .map(|input| mock_media_embedding(input, role, expected_dim))
                .collect()),
            EmbeddingTransport::OpenAi { .. } => Err(NanoError::Execution(
                "the current embedding provider does not support multimodal media embeddings"
                    .to_string(),
            )),
            EmbeddingTransport::Gemini { .. } => {
                self.embed_media_gemini_with_retry(inputs, expected_dim, role)
                    .await
            }
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
        _role: EmbedRole,
    ) -> Result<Vec<Vec<f32>>> {
        let max_attempt = self.retry_attempts.max(1);
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self.embed_media_gemini_once(inputs, expected_dim).await {
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
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedCallError> {
        let mut out = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(GEMINI_IMAGE_BATCH_MAX) {
            let mut requests = Vec::with_capacity(chunk.len());
            for input in chunk {
                requests.push(self.gemini_media_request(input, expected_dim).await?);
            }
            out.extend(self.embed_gemini_requests(requests, expected_dim).await?);
        }
        Ok(out)
    }

    async fn gemini_media_request(
        &self,
        input: &MediaSource,
        expected_dim: usize,
    ) -> std::result::Result<GeminiEmbedContentRequest, EmbedCallError> {
        validate_gemini_image_source(input)?;
        let part = match &self.transport {
            EmbeddingTransport::Gemini { http, .. } => match input {
                MediaSource::LocalFile {
                    path,
                    mime_type,
                    size_bytes,
                } => {
                    if *size_bytes > GEMINI_IMAGE_INLINE_MAX_BYTES {
                        return Err(EmbedCallError {
                            message: format!(
                                "Gemini image embeddings currently require inline image payloads smaller than 20 MB; {} is {} bytes",
                                path.display(),
                                size_bytes
                            ),
                            retryable: false,
                        });
                    }
                    let bytes = tokio::fs::read(path).await.map_err(|err| EmbedCallError {
                        message: format!(
                            "failed to read media file for embedding {}: {}",
                            path.display(),
                            err
                        ),
                        retryable: false,
                    })?;
                    GeminiPart::InlineData {
                        inline_data: GeminiInlineData {
                            mime_type: mime_type.clone(),
                            data: base64::engine::general_purpose::STANDARD.encode(bytes),
                        },
                    }
                }
                MediaSource::RemoteUri { uri, mime_type } => {
                    if !(uri.starts_with("http://") || uri.starts_with("https://")) {
                        return Err(EmbedCallError {
                            message: format!(
                                "Gemini media embeddings currently support file://, http://, and https:// sources; unsupported URI {}",
                                uri
                            ),
                            retryable: false,
                        });
                    }
                    let response = http.get(uri).send().await.map_err(|err| EmbedCallError {
                        message: format!("failed to fetch remote media {}: {}", uri, err),
                        retryable: err.is_timeout() || err.is_connect() || err.is_request(),
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
                    if bytes.len() as u64 > GEMINI_IMAGE_INLINE_MAX_BYTES {
                        return Err(EmbedCallError {
                            message: format!(
                                "Gemini image embeddings currently require inline image payloads smaller than 20 MB; {} is {} bytes",
                                uri,
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
            },
            _ => unreachable!("non-gemini transport should not build Gemini media requests"),
        };

        Ok(GeminiEmbedContentRequest {
            model: format!("models/{}", self.model),
            content: GeminiContent { parts: vec![part] },
            task_type: None,
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
            other => Err(NanoError::Execution(format!(
                "unsupported embedding provider `{}` (supported: openai, gemini)",
                other
            ))),
        };
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
    }
}

fn gemini_task_type(role: EmbedRole) -> &'static str {
    match role {
        EmbedRole::Document => "RETRIEVAL_DOCUMENT",
        EmbedRole::Query => "RETRIEVAL_QUERY",
    }
}

fn validate_gemini_image_source(input: &MediaSource) -> std::result::Result<(), EmbedCallError> {
    let mime_type = input.mime_type();
    if !matches!(mime_type, "image/png" | "image/jpeg") {
        return Err(EmbedCallError {
            message: format!(
                "Gemini image embeddings currently support only image/png and image/jpeg in nanograph, got {}",
                mime_type
            ),
            retryable: false,
        });
    }
    Ok(())
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
                .and_then(|segments| segments.last())
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

    #[test]
    fn gemini_rejects_non_image_media_mime() {
        let err = validate_gemini_image_source(&MediaSource::RemoteUri {
            uri: "file:///tmp/doc.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
        })
        .unwrap_err();
        assert!(err.message.contains("image/png and image/jpeg"));
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

    #[test]
    fn gemini_media_requests_use_inline_data_shape() {
        let request = GeminiEmbedContentRequest {
            model: "models/gemini-embedding-2-preview".to_string(),
            content: GeminiContent {
                parts: vec![GeminiPart::InlineData {
                    inline_data: GeminiInlineData {
                        mime_type: "image/png".to_string(),
                        data: "ZmFrZQ==".to_string(),
                    },
                }],
            },
            task_type: None,
            output_dimensionality: 1536,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("taskType").is_none());
        assert_eq!(
            json["content"]["parts"][0]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(
            json["content"]["parts"][0]["inlineData"]["data"],
            "ZmFrZQ=="
        );
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
}
