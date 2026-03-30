//! Local ONNX embedding runtime.
//!
//! Runs sentence-transformer models locally via `tract-onnx` + HuggingFace
//! `tokenizers`. No API key required — models are auto-downloaded from
//! HuggingFace Hub on first use and cached in the standard HuggingFace
//! cache directory (`~/.cache/huggingface/hub`).
//!
//! Activated by setting `NANOGRAPH_EMBED_MODEL=hf:<org>/<repo>`, e.g.
//! `hf:sentence-transformers/all-MiniLM-L6-v2`.
//!
//! Gated behind the `local-embed` Cargo feature.

use std::sync::Arc;

use tokenizers::Tokenizer;
use tracing::{debug, info, warn};
use tract_onnx::prelude::tract_ndarray as ndarray;
use tract_onnx::prelude::{
    DatumType, Framework, InferenceFact, InferenceModelExt, Tensor, TypedModel, TypedRunnableModel,
    tvec,
};

use crate::error::{NanoError, Result};

const DEFAULT_MAX_LENGTH: usize = 256;

/// A locally-loaded ONNX sentence-transformer model.
pub(crate) struct LocalEmbeddingModel {
    model: TypedRunnableModel<TypedModel>,
    tokenizer: Tokenizer,
    max_length: usize,
    /// Output dimension discovered from the model.
    dim: usize,
    /// Human-readable model name (the `hf:<org>/<repo>` string).
    model_name: String,
}

// TypedRunnableModel and Tokenizer are Send but not marked Sync by default.
// tract's `run` takes `&self` with no interior mutability; Tokenizer::encode
// also takes `&self` and is safe for concurrent reads.
unsafe impl Sync for LocalEmbeddingModel {}

impl LocalEmbeddingModel {
    /// Load the model for the given HuggingFace repo id (e.g.
    /// `"sentence-transformers/all-MiniLM-L6-v2"`).
    ///
    /// Downloads `onnx/model.onnx` and `tokenizer.json` from the repo on
    /// first call; subsequent calls use the HuggingFace Hub disk cache.
    pub(crate) fn load(repo_id: &str) -> Result<Arc<Self>> {
        let max_length = DEFAULT_MAX_LENGTH;

        info!(repo_id, max_length, "loading local embedding model");

        let api = hf_hub::api::sync::Api::new().map_err(|e| {
            NanoError::Execution(format!("failed to initialize HuggingFace Hub client: {}", e))
        })?;
        let repo = api.model(repo_id.to_string());

        let model_path = repo.get("onnx/model.onnx").map_err(|e| {
            NanoError::Execution(format!(
                "failed to fetch onnx/model.onnx from {}: {}",
                repo_id, e
            ))
        })?;
        let tokenizer_path = repo.get("tokenizer.json").map_err(|e| {
            NanoError::Execution(format!(
                "failed to fetch tokenizer.json from {}: {}",
                repo_id, e
            ))
        })?;

        debug!(
            model = model_path.display().to_string(),
            tokenizer = tokenizer_path.display().to_string(),
            "local embedding assets ready"
        );

        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            NanoError::Execution(format!("local embedding tokenizer load failed: {}", e))
        })?;

        let model = tract_onnx::onnx()
            .model_for_path(&model_path)
            .map_err(|e| {
                NanoError::Execution(format!("local embedding ONNX load failed: {}", e))
            })?
            .with_input_fact(
                0,
                InferenceFact::dt_shape(DatumType::I64, tvec!(1, max_length as i64)),
            )
            .map_err(|e| {
                NanoError::Execution(format!(
                    "local embedding ONNX input_ids shape failed: {}",
                    e
                ))
            })?
            .with_input_fact(
                1,
                InferenceFact::dt_shape(DatumType::I64, tvec!(1, max_length as i64)),
            )
            .map_err(|e| {
                NanoError::Execution(format!(
                    "local embedding ONNX attention_mask shape failed: {}",
                    e
                ))
            })?
            .with_input_fact(
                2,
                InferenceFact::dt_shape(DatumType::I64, tvec!(1, max_length as i64)),
            )
            .map_err(|e| {
                NanoError::Execution(format!(
                    "local embedding ONNX token_type_ids shape failed: {}",
                    e
                ))
            })?
            .into_optimized()
            .map_err(|e| {
                NanoError::Execution(format!("local embedding ONNX optimize failed: {}", e))
            })?
            .into_runnable()
            .map_err(|e| {
                NanoError::Execution(format!("local embedding ONNX runnable failed: {}", e))
            })?;

        let dim = probe_output_dim(&model, &tokenizer, max_length)?;
        let model_name = format!("hf:{}", repo_id);

        info!(dim, model_name, pooling = "mean", "local embedding model loaded");

        Ok(Arc::new(Self {
            model,
            tokenizer,
            max_length,
            dim,
            model_name,
        }))
    }

    /// The output embedding dimension of this model.
    pub(crate) fn dim(&self) -> usize {
        self.dim
    }

    /// Human-readable model name (e.g. `"hf:sentence-transformers/all-MiniLM-L6-v2"`).
    pub(crate) fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Embed a single text and return its vector.
    pub(crate) fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| NanoError::Execution(format!("local embedding tokenize failed: {}", e)))?;

        let mut ids: Vec<i64> = encoding.get_ids().iter().map(|v| *v as i64).collect();
        let mut mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|v| *v as i64)
            .collect();
        let mut type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|v| *v as i64).collect();

        // Warn if input exceeds the model's token window.
        let token_count = ids.len();
        if token_count > self.max_length {
            warn!(
                token_count,
                max_length = self.max_length,
                truncated = token_count - self.max_length,
                "input text exceeds local model max token length; truncating \
                 - consider setting NANOGRAPH_EMBED_CHUNK_CHARS to split long texts"
            );
        }

        // Truncate or pad to max_length.
        truncate_or_pad(&mut ids, self.max_length);
        truncate_or_pad(&mut mask, self.max_length);
        truncate_or_pad(&mut type_ids, self.max_length);

        let input_ids_arr =
            ndarray::Array2::from_shape_vec((1, self.max_length), ids).map_err(|e| {
                NanoError::Execution(format!("local embedding input_ids shape failed: {}", e))
            })?;
        let attention_mask_arr =
            ndarray::Array2::from_shape_vec((1, self.max_length), mask).map_err(|e| {
                NanoError::Execution(format!(
                    "local embedding attention_mask shape failed: {}",
                    e
                ))
            })?;
        let token_type_ids_arr =
            ndarray::Array2::from_shape_vec((1, self.max_length), type_ids).map_err(|e| {
                NanoError::Execution(format!(
                    "local embedding token_type_ids shape failed: {}",
                    e
                ))
            })?;

        let input_ids: Tensor = Tensor::from(input_ids_arr);
        let attention_mask: Tensor = Tensor::from(attention_mask_arr.clone());
        let token_type_ids: Tensor = Tensor::from(token_type_ids_arr);

        let outputs = self
            .model
            .run(tvec![
                input_ids.into(),
                attention_mask.clone().into(),
                token_type_ids.into()
            ])
            .map_err(|e| {
                NanoError::Execution(format!("local embedding ONNX run failed: {}", e))
            })?;

        let output = outputs[0].to_array_view::<f32>().map_err(|e| {
            NanoError::Execution(format!("local embedding ONNX output type failed: {}", e))
        })?;
        let output = output.into_dimensionality::<ndarray::Ix3>().map_err(|e| {
            NanoError::Execution(format!("local embedding ONNX output dims failed: {}", e))
        })?;

        // Mean pooling over non-padding tokens.
        let mask_view = attention_mask_arr.view();
        let hidden = output.shape()[2];
        let mut pooled = vec![0f32; hidden];
        let mut count = 0f32;
        for i in 0..self.max_length {
            if mask_view[[0, i]] == 0 {
                continue;
            }
            for h in 0..hidden {
                pooled[h] += output[[0, i, h]];
            }
            count += 1.0;
        }
        if count > 0.0 {
            for v in &mut pooled {
                *v /= count;
            }
        }
        normalize_l2(&mut pooled);
        Ok(pooled)
    }

    /// Embed multiple texts sequentially.
    pub(crate) fn embed_texts(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        inputs.iter().map(|text| self.embed_text(text)).collect()
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Strip the `hf:` prefix from a model string, returning the bare repo id.
/// Returns `None` if the string doesn't start with `hf:`.
pub(crate) fn parse_hf_model_id(model: &str) -> Option<&str> {
    model.strip_prefix("hf:")
}

/// Run a single dummy inference to discover the model's hidden dimension.
fn probe_output_dim(
    model: &TypedRunnableModel<TypedModel>,
    tokenizer: &Tokenizer,
    max_length: usize,
) -> Result<usize> {
    let encoding = tokenizer.encode("probe", true).map_err(|e| {
        NanoError::Execution(format!(
            "local embedding failed to probe output dimension: {}",
            e
        ))
    })?;
    let mut ids: Vec<i64> = encoding.get_ids().iter().map(|v| *v as i64).collect();
    let mut mask: Vec<i64> = encoding
        .get_attention_mask()
        .iter()
        .map(|v| *v as i64)
        .collect();
    let mut type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|v| *v as i64).collect();
    ids.resize(max_length, 0);
    mask.resize(max_length, 0);
    type_ids.resize(max_length, 0);

    let input_ids =
        Tensor::from(ndarray::Array2::from_shape_vec((1, max_length), ids).map_err(|e| {
            NanoError::Execution(format!("local embedding probe shape failed: {}", e))
        })?);
    let attention_mask =
        Tensor::from(ndarray::Array2::from_shape_vec((1, max_length), mask).map_err(|e| {
            NanoError::Execution(format!("local embedding probe shape failed: {}", e))
        })?);
    let token_type_ids = Tensor::from(
        ndarray::Array2::from_shape_vec((1, max_length), type_ids).map_err(|e| {
            NanoError::Execution(format!("local embedding probe shape failed: {}", e))
        })?,
    );

    let outputs = model
        .run(tvec![
            input_ids.into(),
            attention_mask.into(),
            token_type_ids.into()
        ])
        .map_err(|e| {
            NanoError::Execution(format!(
                "local embedding failed to probe output dimension: {}",
                e
            ))
        })?;
    let output = outputs[0].to_array_view::<f32>().map_err(|e| {
        NanoError::Execution(format!("local embedding probe output type failed: {}", e))
    })?;
    let shape = output.shape();
    if shape.len() < 2 {
        return Err(NanoError::Execution(format!(
            "local embedding model output has unexpected shape: {:?}",
            shape
        )));
    }
    Ok(shape[shape.len() - 1])
}

fn truncate_or_pad(vec: &mut Vec<i64>, len: usize) {
    vec.truncate(len);
    vec.resize(len, 0);
}

fn normalize_l2(v: &mut [f32]) {
    let sum: f32 = v.iter().map(|x| x * x).sum();
    if sum <= f32::EPSILON {
        return;
    }
    let norm = sum.sqrt();
    for x in v.iter_mut() {
        *x /= norm;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hf_model_id_strips_prefix() {
        assert_eq!(
            parse_hf_model_id("hf:sentence-transformers/all-MiniLM-L6-v2"),
            Some("sentence-transformers/all-MiniLM-L6-v2")
        );
    }

    #[test]
    fn parse_hf_model_id_returns_none_for_plain_model() {
        assert_eq!(parse_hf_model_id("text-embedding-3-small"), None);
        assert_eq!(parse_hf_model_id("gemini-embedding-2-preview"), None);
    }

    #[test]
    fn normalize_l2_produces_unit_vector() {
        let mut v = vec![3.0, 4.0];
        normalize_l2(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn normalize_l2_preserves_zero_vector() {
        let mut v = vec![0.0, 0.0, 0.0];
        normalize_l2(&mut v);
        assert_eq!(v, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn truncate_or_pad_truncates_long_input() {
        let mut v = vec![1, 2, 3, 4, 5];
        truncate_or_pad(&mut v, 3);
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn truncate_or_pad_pads_short_input() {
        let mut v = vec![1, 2];
        truncate_or_pad(&mut v, 5);
        assert_eq!(v, vec![1, 2, 0, 0, 0]);
    }

    #[test]
    fn truncate_or_pad_noop_for_exact_length() {
        let mut v = vec![1, 2, 3];
        truncate_or_pad(&mut v, 3);
        assert_eq!(v, vec![1, 2, 3]);
    }
}
