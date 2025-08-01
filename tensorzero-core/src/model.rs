use futures::future::try_join_all;
use futures::StreamExt;
use reqwest::Client;
use secrecy::SecretString;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::{env, fs};
use strum::VariantNames;
use tensorzero_derive::TensorZeroDeserialize;
use tokio::time::error::Elapsed;
use tracing::{span, Level, Span};
use tracing_futures::{Instrument, Instrumented};
use url::Url;

use crate::cache::{
    cache_lookup, cache_lookup_streaming, start_cache_write, start_cache_write_streaming,
    CacheData, ModelProviderRequest, NonStreamingCacheData, StreamingCacheData,
};
use crate::config_parser::{skip_credential_validation, ProviderTypesConfig, TimeoutsConfig};
use crate::endpoints::inference::InferenceClients;
use crate::providers::aws_sagemaker::AWSSagemakerProvider;
#[cfg(any(test, feature = "e2e_tests"))]
use crate::providers::dummy::DummyProvider;
use crate::providers::google_ai_studio_gemini::GoogleAIStudioGeminiProvider;

use crate::inference::types::batch::{
    BatchRequestRow, PollBatchInferenceResponse, StartBatchModelInferenceResponse,
    StartBatchProviderInferenceResponse,
};
use crate::inference::types::extra_body::ExtraBodyConfig;
use crate::inference::types::extra_headers::ExtraHeadersConfig;
use crate::inference::types::{
    current_timestamp, ContentBlock, PeekableProviderInferenceResponseStream,
    ProviderInferenceResponseChunk, ProviderInferenceResponseStreamInner, RequestMessage, Thought,
    Usage,
};
use crate::inference::WrappedProvider;
use crate::model_table::{BaseModelTable, ShorthandModelConfig};
use crate::providers::helpers::peek_first_chunk;
use crate::providers::hyperbolic::HyperbolicProvider;
use crate::providers::sglang::SGLangProvider;
use crate::providers::tgi::TGIProvider;
use crate::{
    endpoints::inference::InferenceCredentials,
    error::{Error, ErrorDetails},
    inference::{
        types::{ModelInferenceRequest, ModelInferenceResponse, ProviderInferenceResponse},
        InferenceProvider,
    },
};
use serde::{Deserialize, Serialize};

use crate::providers::{
    anthropic::AnthropicProvider, aws_bedrock::AWSBedrockProvider, azure::AzureProvider,
    deepseek::DeepSeekProvider, fireworks::FireworksProvider,
    gcp_vertex_anthropic::GCPVertexAnthropicProvider, gcp_vertex_gemini::GCPVertexGeminiProvider,
    groq::GroqProvider, mistral::MistralProvider, openai::OpenAIProvider,
    openrouter::OpenRouterProvider, together::TogetherProvider, vllm::VLLMProvider,
    xai::XAIProvider,
};

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct ModelConfig {
    pub routing: Vec<Arc<str>>, // [provider name A, provider name B, ...]
    pub providers: HashMap<Arc<str>, ModelProvider>, // provider name => provider config
    pub timeouts: TimeoutsConfig,
}

#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
#[serde(deny_unknown_fields)]
pub struct UninitializedModelConfig {
    pub routing: Vec<Arc<str>>, // [provider name A, provider name B, ...]
    pub providers: HashMap<Arc<str>, UninitializedModelProvider>, // provider name => provider config
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
}

impl UninitializedModelConfig {
    pub async fn load(
        self,
        model_name: &str,
        provider_types: &ProviderTypesConfig,
    ) -> Result<ModelConfig, Error> {
        // We want `ModelProvider` to know its own name (from the 'providers' config section).
        // We first deserialize to `HashMap<Arc<str>, UninitializedModelProvider>`, and then
        // build `ModelProvider`s using the name keys from the map.
        let providers = try_join_all(self.providers.into_iter().map(
            |(name, provider)| async move {
                Ok::<_, Error>((
                    name.clone(),
                    ModelProvider {
                        name: name.clone(),
                        config: provider.config.load(provider_types).await.map_err(|e| {
                            Error::new(ErrorDetails::Config {
                                message: format!("models.{model_name}.providers.{name}: {e}"),
                            })
                        })?,
                        extra_body: provider.extra_body,
                        extra_headers: provider.extra_headers,
                        timeouts: provider.timeouts,
                        discard_unknown_chunks: provider.discard_unknown_chunks,
                    },
                ))
            },
        ))
        .await?
        .into_iter()
        .collect::<HashMap<_, _>>();
        Ok(ModelConfig {
            routing: self.routing,
            providers,
            timeouts: self.timeouts,
        })
    }
}

pub struct StreamResponse {
    pub stream: Instrumented<PeekableProviderInferenceResponseStream>,
    pub raw_request: String,
    pub model_provider_name: Arc<str>,
    pub cached: bool,
}

impl StreamResponse {
    pub fn from_cache(
        cache_lookup: CacheData<StreamingCacheData>,
        model_provider_name: Arc<str>,
    ) -> Self {
        let chunks = cache_lookup.output.chunks;
        let chunks_len = chunks.len();

        Self {
            stream: (Box::pin(futures::stream::iter(chunks.into_iter().enumerate().map(
                move |(index, c)| {
                    Ok(ProviderInferenceResponseChunk {
                        content: c.content,
                        raw_response: c.raw_response,
                        // We intentionally don't cache and re-use these values from the original
                        // request:
                        // The new result was 'created' now
                        created: current_timestamp(),
                        // Use the real usage (so that the `ModelInference` row we write is accurate)
                        // The usage returned to over HTTP is adjusted in `InferenceResponseChunk::new`
                        usage: c.usage,
                        // We didn't make any network calls to the model provider, so the latency is 0
                        latency: Duration::from_secs(0),
                        // For all chunks but the last one, the finish reason is None
                        // For the last chunk, the finish reason is the same as the cache lookup
                        finish_reason: if index == chunks_len - 1 {
                            cache_lookup.finish_reason
                        } else {
                            None
                        },
                    })
                },
            ))) as ProviderInferenceResponseStreamInner)
                .peekable()
                .instrument(tracing::info_span!(
                    "stream_from_cache",
                    otel.name = "stream_from_cache"
                )),
            raw_request: cache_lookup.raw_request,
            model_provider_name,
            cached: true,
        }
    }
}

/// Creates a fully-qualified name from a model and provider name, suitable for using
/// in `ContentBlock::Unknown.model_provider_name`
/// Note that 'model_name' is a name from `[models]`, which is not necessarily
/// the same as the underlying name passed to a specific provider api
pub fn fully_qualified_name(model_name: &str, provider_name: &str) -> String {
    format!("tensorzero::model_name::{model_name}::provider_name::{provider_name}")
}

impl ModelConfig {
    fn filter_content_blocks<'a>(
        request: &'a ModelInferenceRequest<'a>,
        model_name: &str,
        provider: &ModelProvider,
    ) -> Cow<'a, ModelInferenceRequest<'a>> {
        let name = fully_qualified_name(model_name, provider.name.as_ref());
        let needs_filter = request.messages.iter().any(|m| {
            m.content.iter().any(|c| match c {
                ContentBlock::Unknown {
                    model_provider_name,
                    data: _,
                } => model_provider_name.as_ref().is_some_and(|n| n != &name),
                ContentBlock::Thought(Thought {
                    text: _,
                    signature: _,
                    provider_type,
                }) => provider_type
                    .as_ref()
                    .is_some_and(|t| t != &provider.config.thought_block_provider_type()),
                _ => false,
            })
        });
        if needs_filter {
            let new_messages = request
                .messages
                .iter()
                .map(|m| RequestMessage {
                    content: m
                        .content
                        .iter()
                        .flat_map(|c| match c {
                            ContentBlock::Unknown {
                                model_provider_name,
                                data: _,
                            } => {
                                if model_provider_name.as_ref().is_some_and(|n| n != &name) {
                                    None
                                } else {
                                    Some(c.clone())
                                }
                            }
                            ContentBlock::Thought(Thought {
                                text: _,
                                signature: _,
                                provider_type,
                            }) => {
                                // When a thought is scoped to a particular provider type, we discard
                                // if it doesn't match our target provider.
                                // Thoughts without a `provider_type` are used for all providers.
                                if provider_type.as_ref().is_some_and(|t| {
                                    t != &provider.config.thought_block_provider_type()
                                }) {
                                    None
                                } else {
                                    Some(c.clone())
                                }
                            }
                            _ => Some(c.clone()),
                        })
                        .collect(),
                    ..m.clone()
                })
                .collect();
            Cow::Owned(ModelInferenceRequest {
                messages: new_messages,
                ..request.clone()
            })
        } else {
            Cow::Borrowed(request)
        }
    }

    /// Performs a non-streaming request to a specific provider, performing a cache lookup if enabled.
    /// We apply model-provider timeouts to the future produced by this function
    /// (as we want to apply the timeout to ClickHouse cache lookups)
    async fn non_streaming_provider_request<'request>(
        &self,
        model_provider_request: ModelProviderRequest<'request>,
        provider: &'request ModelProvider,
        clients: &'request InferenceClients<'request>,
    ) -> Result<ModelInferenceResponse, Error> {
        // TODO: think about how to best handle errors here
        if clients.cache_options.enabled.read() {
            let cache_lookup = cache_lookup(
                clients.clickhouse_connection_info,
                model_provider_request,
                clients.cache_options.max_age_s,
            )
            .await
            .ok()
            .flatten();
            if let Some(cache_lookup) = cache_lookup {
                return Ok(cache_lookup);
            }
        }
        let response = provider
            .infer(
                model_provider_request,
                clients.http_client,
                clients.credentials,
            )
            .instrument(span!(
                Level::INFO,
                "infer",
                provider_name = model_provider_request.provider_name
            ))
            .await?;
        // We already checked the cache above (and returned early if it was a hit), so this response was not from the cache
        Ok(ModelInferenceResponse::new(
            response,
            model_provider_request.provider_name.into(),
            false,
        ))
    }

    /// Performs a streaming request to a specific provider, performing a cache lookup if enabled.
    /// We apply model-provider timeouts to the future produced by this function
    /// (as we want to apply the timeout to ClickHouse cache lookups).
    ///
    /// This function also includes a call to `peek_first_chunk` - this ensure that the
    /// duration of the returned future includes the time taken to get the first chunk
    /// from the model provider.
    async fn streaming_provider_request<'request>(
        &self,
        model_provider_request: ModelProviderRequest<'request>,
        provider: &'request ModelProvider,
        clients: &'request InferenceClients<'request>,
    ) -> Result<StreamResponseAndMessages, Error> {
        // TODO: think about how to best handle errors here
        if clients.cache_options.enabled.read() {
            let cache_lookup = cache_lookup_streaming(
                clients.clickhouse_connection_info,
                model_provider_request,
                clients.cache_options.max_age_s,
            )
            .await
            .ok()
            .flatten();
            if let Some(cache_lookup) = cache_lookup {
                return Ok(StreamResponseAndMessages {
                    response: cache_lookup,
                    messages: model_provider_request.request.messages.clone(),
                });
            }
        }

        let StreamAndRawRequest {
            stream,
            raw_request,
        } = provider
            .infer_stream(
                model_provider_request,
                clients.http_client,
                clients.credentials,
            )
            .await?;

        // Note - we cache the chunks here so that we store the raw model provider input and response chunks
        // in the cache. We don't want this logic in `collect_chunks`, which would cause us to cache the result
        // of higher-level transformations (e.g. dicl)
        let mut stream = if clients.cache_options.enabled.write() {
            let span = stream.span().clone();
            // Note - it's fine to call `stream_with_cache_write` inside of `streaming_provider_request`,
            // as it doesn't immediately kick off a clickhouse cache write. The cache write only occurs
            // when the stream finished, so the model provider TTFT timeout will (correctly) not include
            // the time taken to write to clickhouse.
            stream_with_cache_write(
                raw_request.clone(),
                model_provider_request,
                clients,
                stream.into_inner(),
            )
            .await?
            .instrument(span)
        } else {
            stream
        };
        // Get a single chunk from the stream and make sure it is OK then send to client.
        // We want to do this here so that we can tell that the request is working.
        peek_first_chunk(
            stream.inner_mut(),
            &raw_request,
            model_provider_request.provider_name,
        )
        .await?;
        Ok(StreamResponseAndMessages {
            response: StreamResponse {
                stream,
                raw_request,
                model_provider_name: model_provider_request.provider_name.into(),
                cached: false,
            },
            messages: model_provider_request.request.messages.clone(),
        })
    }

    #[tracing::instrument(skip_all, fields(model_name = model_name, otel.name = "model_inference", stream = false))]
    pub async fn infer<'request>(
        &self,
        request: &'request ModelInferenceRequest<'request>,
        clients: &'request InferenceClients<'request>,
        model_name: &'request str,
    ) -> Result<ModelInferenceResponse, Error> {
        let mut provider_errors: HashMap<String, Error> = HashMap::new();
        let run_all_models = async {
            for provider_name in &self.routing {
                let provider = self.providers.get(provider_name).ok_or_else(|| {
                    Error::new(ErrorDetails::ProviderNotFound {
                        provider_name: provider_name.to_string(),
                    })
                })?;
                let request = Self::filter_content_blocks(request, model_name, provider);
                let model_provider_request = ModelProviderRequest {
                    request: &request,
                    model_name,
                    provider_name,
                };
                let cache_key = model_provider_request.get_cache_key()?;

                let response_fut =
                    self.non_streaming_provider_request(model_provider_request, provider, clients);
                let response = if let Some(timeout) = provider.non_streaming_total_timeout() {
                    tokio::time::timeout(timeout, response_fut)
                        .await
                        // Convert the outer `Elapsed` error into a TensorZero error,
                        // so that it can be handled by the `match response` block below
                        .unwrap_or_else(|_: Elapsed| {
                            Err(Error::new(ErrorDetails::ModelProviderTimeout {
                                provider_name: provider_name.to_string(),
                                timeout,
                                streaming: false,
                            }))
                        })
                } else {
                    response_fut.await
                };

                match response {
                    Ok(response) => {
                        // Perform the cache write outside of the `non_streaming_total_timeout` timeout future,
                        // (in case we ever add a blocking cache write option)
                        if clients.cache_options.enabled.write() {
                            let _ = start_cache_write(
                                clients.clickhouse_connection_info,
                                cache_key,
                                NonStreamingCacheData {
                                    blocks: response.output.clone(),
                                },
                                &response.raw_request,
                                &response.raw_response,
                                &response.usage,
                                response.finish_reason.as_ref(),
                            );
                        }

                        return Ok(response);
                    }
                    Err(error) => {
                        provider_errors.insert(provider_name.to_string(), error);
                    }
                }
            }
            Err(Error::new(ErrorDetails::ModelProvidersExhausted {
                provider_errors,
            }))
        };
        // This is the top-level model timeout, which limits the total time taken to run all providers.
        // Some of the providers may themselves have timeouts, which is fine. Provider timeouts
        // are treated as just another kind of provider error - a timeout of N ms is equivalent
        // to a provider taking N ms, and then producing a normal HTTP error.
        if let Some(timeout) = self.timeouts.non_streaming.total_ms {
            let timeout = Duration::from_millis(timeout);
            tokio::time::timeout(timeout, run_all_models)
                .await
                // Convert the outer `Elapsed` error into a TensorZero error,
                // so that it can be handled by the `match response` block below
                .unwrap_or_else(|_: Elapsed| {
                    Err(Error::new(ErrorDetails::ModelTimeout {
                        model_name: model_name.to_string(),
                        timeout,
                        streaming: false,
                    }))
                })
        } else {
            run_all_models.await
        }
    }

    #[tracing::instrument(skip_all, fields(model_name = model_name, otel.name = "model_inference", stream = true))]
    pub async fn infer_stream<'request>(
        &self,
        request: &'request ModelInferenceRequest<'request>,
        clients: &'request InferenceClients<'request>,
        model_name: &'request str,
    ) -> Result<StreamResponseAndMessages, Error> {
        let mut provider_errors: HashMap<String, Error> = HashMap::new();
        let run_all_models = async {
            for provider_name in &self.routing {
                let provider = self.providers.get(provider_name).ok_or_else(|| {
                    Error::new(ErrorDetails::ProviderNotFound {
                        provider_name: provider_name.to_string(),
                    })
                })?;
                let request = Self::filter_content_blocks(request, model_name, provider);
                let model_provider_request = ModelProviderRequest {
                    request: &request,
                    model_name,
                    provider_name,
                };

                // This future includes a call to `peek_first_chunk`, so applying
                // `streaming_ttft_timeout` is correct.
                let response_fut =
                    self.streaming_provider_request(model_provider_request, provider, clients);
                let response = if let Some(timeout) = provider.streaming_ttft_timeout() {
                    tokio::time::timeout(timeout, response_fut)
                        .await
                        .unwrap_or_else(|_: Elapsed| {
                            Err(Error::new(ErrorDetails::ModelProviderTimeout {
                                provider_name: provider_name.to_string(),
                                timeout,
                                streaming: true,
                            }))
                        })
                } else {
                    response_fut.await
                };

                match response {
                    Ok(response) => return Ok(response),
                    Err(error) => {
                        provider_errors.insert(provider_name.to_string(), error);
                    }
                }
            }
            Err(Error::new(ErrorDetails::ModelProvidersExhausted {
                provider_errors,
            }))
        };
        // See the corresponding `non_streaming.total_ms` timeout in the `infer`
        // method above for more details.
        if let Some(timeout) = self.timeouts.streaming.ttft_ms {
            let timeout = Duration::from_millis(timeout);
            tokio::time::timeout(timeout, run_all_models)
                .await
                // Convert the outer `Elapsed` error into a TensorZero error,
                // so that it can be handled by the `match response` block below
                .unwrap_or_else(|_: Elapsed| {
                    Err(Error::new(ErrorDetails::ModelTimeout {
                        model_name: model_name.to_string(),
                        timeout,
                        streaming: true,
                    }))
                })
        } else {
            run_all_models.await
        }
    }

    pub async fn start_batch_inference<'request>(
        &self,
        requests: &'request [ModelInferenceRequest<'request>],
        client: &'request Client,
        api_keys: &'request InferenceCredentials,
    ) -> Result<StartBatchModelInferenceResponse, Error> {
        let mut provider_errors: HashMap<String, Error> = HashMap::new();
        for provider_name in &self.routing {
            let provider = self.providers.get(provider_name).ok_or_else(|| {
                Error::new(ErrorDetails::ProviderNotFound {
                    provider_name: provider_name.to_string(),
                })
            })?;
            let response = provider
                .start_batch_inference(requests, client, api_keys)
                .instrument(span!(
                    Level::INFO,
                    "start_batch_inference",
                    provider_name = &**provider_name
                ))
                .await;
            match response {
                Ok(response) => {
                    return Ok(StartBatchModelInferenceResponse::new(
                        response,
                        provider_name.clone(),
                    ));
                }
                Err(error) => {
                    provider_errors.insert(provider_name.to_string(), error);
                }
            }
        }
        Err(Error::new(ErrorDetails::ModelProvidersExhausted {
            provider_errors,
        }))
    }
}

async fn stream_with_cache_write(
    raw_request: String,
    model_request: ModelProviderRequest<'_>,
    clients: &InferenceClients<'_>,
    mut stream: PeekableProviderInferenceResponseStream,
) -> Result<PeekableProviderInferenceResponseStream, Error> {
    let cache_key = model_request.get_cache_key()?;
    let clickhouse_info = clients.clickhouse_connection_info.clone();
    Ok((Box::pin(async_stream::stream! {
        let mut buffer = vec![];
        let mut errored = false;
        while let Some(chunk) = stream.next().await {
            if !errored {
                match chunk.as_ref() {
                    Ok(chunk) => {
                        buffer.push(chunk.clone());
                    }
                    Err(e) => {
                        tracing::warn!("Skipping cache write for stream response due to error in stream: {e}");
                        errored = true;
                    }
                }
            }
            yield chunk;
        }
        if !errored {
            let usage = consolidate_usage(&buffer);
            let _ = start_cache_write_streaming(
                &clickhouse_info,
                cache_key,
                buffer,
                &raw_request,
                &usage,
            );
        }
    }) as ProviderInferenceResponseStreamInner).peekable())
}

fn consolidate_usage(chunks: &[ProviderInferenceResponseChunk]) -> Usage {
    let mut input_tokens = 0;
    let mut output_tokens = 0;
    for chunk in chunks {
        if let Some(usage) = &chunk.usage {
            input_tokens += usage.input_tokens;
            output_tokens += usage.output_tokens;
        }
    }
    Usage {
        input_tokens,
        output_tokens,
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct UninitializedModelProvider {
    #[serde(flatten)]
    pub config: UninitializedProviderConfig,
    pub extra_body: Option<ExtraBodyConfig>,
    pub extra_headers: Option<ExtraHeadersConfig>,
    pub timeouts: Option<TimeoutsConfig>,
    /// If `true`, we emit a warning and discard chunks that we don't recognize
    /// (on a best-effort, per-provider basis).
    /// By default, we produce an error in the stream
    /// We can't meaningfully return unknown chunks to the user, as we don't
    /// know how to correctly merge them.
    #[serde(default)]
    pub discard_unknown_chunks: bool,
}

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct ModelProvider {
    pub name: Arc<str>,
    pub config: ProviderConfig,
    pub extra_headers: Option<ExtraHeadersConfig>,
    pub extra_body: Option<ExtraBodyConfig>,
    pub timeouts: Option<TimeoutsConfig>,
    /// See `UninitializedModelProvider.discard_unknown_chunks`.
    pub discard_unknown_chunks: bool,
}

impl ModelProvider {
    fn non_streaming_total_timeout(&self) -> Option<Duration> {
        Some(Duration::from_millis(
            self.timeouts.as_ref()?.non_streaming.total_ms?,
        ))
    }

    fn streaming_ttft_timeout(&self) -> Option<Duration> {
        Some(Duration::from_millis(
            self.timeouts.as_ref()?.streaming.ttft_ms?,
        ))
    }

    /// The name to report in the OTEL `gen_ai.system` attribute
    fn genai_system_name(&self) -> &'static str {
        match &self.config {
            ProviderConfig::Anthropic(_) => "anthropic",
            ProviderConfig::AWSBedrock(_) => "aws_bedrock",
            ProviderConfig::AWSSagemaker(_) => "aws_sagemaker",
            ProviderConfig::Azure(_) => "azure",
            ProviderConfig::Fireworks(_) => "fireworks",
            ProviderConfig::GCPVertexAnthropic(_) => "gcp_vertex_anthropic",
            ProviderConfig::GCPVertexGemini(_) => "gcp_vertex_gemini",
            ProviderConfig::GoogleAIStudioGemini(_) => "google_ai_studio_gemini",
            ProviderConfig::Groq(_) => "groq",
            ProviderConfig::Hyperbolic(_) => "hyperbolic",
            ProviderConfig::Mistral(_) => "mistral",
            ProviderConfig::OpenAI(_) => "openai",
            ProviderConfig::OpenRouter(_) => "openrouter",
            ProviderConfig::Together(_) => "together",
            ProviderConfig::VLLM(_) => "vllm",
            ProviderConfig::XAI(_) => "xai",
            ProviderConfig::TGI(_) => "tgi",
            ProviderConfig::SGLang(_) => "sglang",
            ProviderConfig::DeepSeek(_) => "deepseek",
            #[cfg(any(test, feature = "e2e_tests"))]
            ProviderConfig::Dummy(_) => "dummy",
        }
    }

    /// The model name to report in the OTEL `gen_ai.request.model` attribute
    fn genai_model_name(&self) -> Option<&str> {
        match &self.config {
            ProviderConfig::Anthropic(provider) => Some(provider.model_name()),
            ProviderConfig::AWSBedrock(provider) => Some(provider.model_id()),
            // SageMaker doesn't have a meaningful model name concept, as we just invoke an endpoint
            ProviderConfig::AWSSagemaker(_) => None,
            ProviderConfig::Azure(provider) => Some(provider.deployment_id()),
            ProviderConfig::Fireworks(provider) => Some(provider.model_name()),
            ProviderConfig::GCPVertexAnthropic(provider) => Some(provider.model_id()),
            ProviderConfig::GCPVertexGemini(provider) => Some(provider.model_or_endpoint_id()),
            ProviderConfig::GoogleAIStudioGemini(provider) => Some(provider.model_name()),
            ProviderConfig::Groq(provider) => Some(provider.model_name()),
            ProviderConfig::Hyperbolic(provider) => Some(provider.model_name()),
            ProviderConfig::Mistral(provider) => Some(provider.model_name()),
            ProviderConfig::OpenAI(provider) => Some(provider.model_name()),
            ProviderConfig::OpenRouter(provider) => Some(provider.model_name()),
            ProviderConfig::Together(provider) => Some(provider.model_name()),
            ProviderConfig::VLLM(provider) => Some(provider.model_name()),
            ProviderConfig::XAI(provider) => Some(provider.model_name()),
            // TGI doesn't have a meaningful model name
            ProviderConfig::TGI(_) => None,
            ProviderConfig::SGLang(provider) => Some(provider.model_name()),
            ProviderConfig::DeepSeek(provider) => Some(provider.model_name()),
            #[cfg(any(test, feature = "e2e_tests"))]
            ProviderConfig::Dummy(provider) => Some(provider.model_name()),
        }
    }
}

impl From<&ModelProvider> for ModelProviderRequestInfo {
    fn from(val: &ModelProvider) -> Self {
        ModelProviderRequestInfo {
            provider_name: val.name.clone(),
            extra_headers: val.extra_headers.clone(),
            extra_body: val.extra_body.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModelProviderRequestInfo {
    pub provider_name: Arc<str>,
    pub extra_headers: Option<ExtraHeadersConfig>,
    pub extra_body: Option<ExtraBodyConfig>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub enum ProviderConfig {
    Anthropic(AnthropicProvider),
    #[serde(rename = "aws_bedrock")]
    AWSBedrock(AWSBedrockProvider),
    #[serde(rename = "aws_sagemaker")]
    AWSSagemaker(AWSSagemakerProvider),
    Azure(AzureProvider),
    DeepSeek(DeepSeekProvider),
    Fireworks(FireworksProvider),
    #[serde(rename = "gcp_vertex_anthropic")]
    GCPVertexAnthropic(GCPVertexAnthropicProvider),
    #[serde(rename = "gcp_vertex_gemini")]
    GCPVertexGemini(GCPVertexGeminiProvider),
    #[serde(rename = "google_ai_studio_gemini")]
    GoogleAIStudioGemini(GoogleAIStudioGeminiProvider),
    Groq(GroqProvider),
    Hyperbolic(HyperbolicProvider),
    Mistral(MistralProvider),
    OpenAI(OpenAIProvider),
    OpenRouter(OpenRouterProvider),
    #[serde(rename = "sglang")]
    SGLang(SGLangProvider),
    #[serde(rename = "tgi")]
    TGI(TGIProvider),
    Together(TogetherProvider),
    #[serde(rename = "vllm")]
    VLLM(VLLMProvider),
    #[serde(rename = "xai")]
    XAI(XAIProvider),
    #[cfg(any(test, feature = "e2e_tests"))]
    Dummy(DummyProvider),
}

impl ProviderConfig {
    fn thought_block_provider_type(&self) -> Cow<'static, str> {
        match self {
            ProviderConfig::Anthropic(_) => {
                Cow::Borrowed(crate::providers::anthropic::PROVIDER_TYPE)
            }
            ProviderConfig::AWSBedrock(_) => {
                Cow::Borrowed(crate::providers::aws_bedrock::PROVIDER_TYPE)
            }
            // Note - none of our current  wrapped provider types emit thought blocks
            // If any of them ever start producing thoughts, we'll need to make sure that the `provider_type`
            // field uses `thought_block_provider_type` on the parent SageMaker provider.
            ProviderConfig::AWSSagemaker(sagemaker) => Cow::Owned(format!(
                "aws_sagemaker::{}",
                sagemaker
                    .hosted_provider
                    .thought_block_provider_type_suffix()
            )),
            ProviderConfig::Azure(_) => Cow::Borrowed(crate::providers::azure::PROVIDER_TYPE),
            ProviderConfig::DeepSeek(_) => Cow::Borrowed(crate::providers::deepseek::PROVIDER_TYPE),
            ProviderConfig::Fireworks(_) => {
                Cow::Borrowed(crate::providers::fireworks::PROVIDER_TYPE)
            }
            ProviderConfig::GCPVertexAnthropic(_) => {
                Cow::Borrowed(crate::providers::gcp_vertex_anthropic::PROVIDER_TYPE)
            }
            ProviderConfig::GCPVertexGemini(_) => {
                Cow::Borrowed(crate::providers::gcp_vertex_gemini::PROVIDER_TYPE)
            }
            ProviderConfig::GoogleAIStudioGemini(_) => {
                Cow::Borrowed(crate::providers::google_ai_studio_gemini::PROVIDER_TYPE)
            }
            ProviderConfig::Groq(_) => Cow::Borrowed(crate::providers::groq::PROVIDER_TYPE),
            ProviderConfig::Hyperbolic(_) => {
                Cow::Borrowed(crate::providers::hyperbolic::PROVIDER_TYPE)
            }
            ProviderConfig::Mistral(_) => Cow::Borrowed(crate::providers::mistral::PROVIDER_TYPE),
            ProviderConfig::OpenAI(_) => Cow::Borrowed(crate::providers::openai::PROVIDER_TYPE),
            ProviderConfig::OpenRouter(_) => {
                Cow::Borrowed(crate::providers::openrouter::PROVIDER_TYPE)
            }
            ProviderConfig::SGLang(_) => Cow::Borrowed(crate::providers::sglang::PROVIDER_TYPE),
            ProviderConfig::TGI(_) => Cow::Borrowed(crate::providers::tgi::PROVIDER_TYPE),
            ProviderConfig::Together(_) => Cow::Borrowed(crate::providers::together::PROVIDER_TYPE),
            ProviderConfig::VLLM(_) => Cow::Borrowed(crate::providers::vllm::PROVIDER_TYPE),
            ProviderConfig::XAI(_) => Cow::Borrowed(crate::providers::xai::PROVIDER_TYPE),
            #[cfg(any(test, feature = "e2e_tests"))]
            ProviderConfig::Dummy(_) => Cow::Borrowed(crate::providers::dummy::PROVIDER_TYPE),
        }
    }
}

/// Contains all providers which implement `SelfHostedProvider` - these providers
/// can be used as the target provider hosted by AWS Sagemaker
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
#[serde(rename_all = "lowercase")]
#[serde(deny_unknown_fields)]
pub enum HostedProviderKind {
    OpenAI,
    TGI,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
#[derive(Debug, TensorZeroDeserialize, VariantNames, Serialize)]
#[strum(serialize_all = "lowercase")]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
#[serde(deny_unknown_fields)]
pub enum UninitializedProviderConfig {
    Anthropic {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    #[strum(serialize = "aws_bedrock")]
    #[serde(rename = "aws_bedrock")]
    AWSBedrock {
        model_id: String,
        region: Option<String>,
        #[serde(default)]
        allow_auto_detect_region: bool,
    },
    #[strum(serialize = "aws_sagemaker")]
    #[serde(rename = "aws_sagemaker")]
    AWSSagemaker {
        endpoint_name: String,
        model_name: String,
        region: Option<String>,
        #[serde(default)]
        allow_auto_detect_region: bool,
        hosted_provider: HostedProviderKind,
    },
    Azure {
        deployment_id: String,
        endpoint: Url,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    #[strum(serialize = "gcp_vertex_anthropic")]
    #[serde(rename = "gcp_vertex_anthropic")]
    GCPVertexAnthropic {
        model_id: String,
        location: String,
        project_id: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        credential_location: Option<CredentialLocation>,
    },
    #[strum(serialize = "gcp_vertex_gemini")]
    #[serde(rename = "gcp_vertex_gemini")]
    GCPVertexGemini {
        model_id: Option<String>,
        endpoint_id: Option<String>,
        location: String,
        project_id: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        credential_location: Option<CredentialLocation>,
    },
    #[strum(serialize = "google_ai_studio_gemini")]
    #[serde(rename = "google_ai_studio_gemini")]
    GoogleAIStudioGemini {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    #[strum(serialize = "groq")]
    #[serde(rename = "groq")]
    Groq {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    Hyperbolic {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    #[strum(serialize = "fireworks")]
    #[serde(rename = "fireworks")]
    Fireworks {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
        #[serde(default = "crate::providers::fireworks::default_parse_think_blocks")]
        parse_think_blocks: bool,
    },
    Mistral {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    OpenAI {
        model_name: String,
        api_base: Option<Url>,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    OpenRouter {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    Together {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
        #[serde(default = "crate::providers::together::default_parse_think_blocks")]
        parse_think_blocks: bool,
    },
    VLLM {
        model_name: String,
        api_base: Url,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    XAI {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    TGI {
        api_base: Url,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    SGLang {
        model_name: String,
        api_base: Url,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    DeepSeek {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
    #[cfg(any(test, feature = "e2e_tests"))]
    Dummy {
        model_name: String,
        #[cfg_attr(test, ts(type = "string | null"))]
        api_key_location: Option<CredentialLocation>,
    },
}

impl UninitializedProviderConfig {
    pub async fn load(self, provider_types: &ProviderTypesConfig) -> Result<ProviderConfig, Error> {
        Ok(match self {
            UninitializedProviderConfig::Anthropic {
                model_name,
                api_key_location,
            } => ProviderConfig::Anthropic(AnthropicProvider::new(model_name, api_key_location)?),
            UninitializedProviderConfig::AWSBedrock {
                model_id,
                region,
                allow_auto_detect_region,
            } => {
                let region = region.map(aws_types::region::Region::new);
                if region.is_none() && !allow_auto_detect_region {
                    return Err(Error::new(ErrorDetails::Config { message: "AWS bedrock provider requires a region to be provided, or `allow_auto_detect_region = true`.".to_string() }));
                }

                ProviderConfig::AWSBedrock(AWSBedrockProvider::new(model_id, region).await?)
            }
            UninitializedProviderConfig::AWSSagemaker {
                endpoint_name,
                region,
                allow_auto_detect_region,
                model_name,
                hosted_provider,
            } => {
                let region = region.map(aws_types::region::Region::new);
                if region.is_none() && !allow_auto_detect_region {
                    return Err(Error::new(ErrorDetails::Config { message: "AWS Sagemaker provider requires a region to be provided, or `allow_auto_detect_region = true`.".to_string() }));
                }

                let self_hosted: Box<dyn WrappedProvider + Send + Sync + 'static> =
                    match hosted_provider {
                        HostedProviderKind::OpenAI => Box::new(OpenAIProvider::new(
                            model_name,
                            None,
                            Some(CredentialLocation::None),
                        )?),
                        HostedProviderKind::TGI => Box::new(TGIProvider::new(
                            Url::parse("http://tensorzero-unreachable-domain-please-file-a-bug-report.invalid").map_err(|e| {
                                Error::new(ErrorDetails::InternalError { message: format!("Failed to parse fake TGI endpoint: `{e}`. This should never happen. Please file a bug report: https://github.com/tensorzero/tensorzero/issues/new") })
                            })?,
                            Some(CredentialLocation::None),
                        )?),
                    };

                ProviderConfig::AWSSagemaker(
                    AWSSagemakerProvider::new(endpoint_name, self_hosted, region).await?,
                )
            }
            UninitializedProviderConfig::Azure {
                deployment_id,
                endpoint,
                api_key_location,
            } => ProviderConfig::Azure(AzureProvider::new(
                deployment_id,
                endpoint,
                api_key_location,
            )?),
            UninitializedProviderConfig::Fireworks {
                model_name,
                api_key_location,
                parse_think_blocks,
            } => ProviderConfig::Fireworks(FireworksProvider::new(
                model_name,
                api_key_location,
                parse_think_blocks,
            )?),
            UninitializedProviderConfig::GCPVertexAnthropic {
                model_id,
                location,
                project_id,
                credential_location: api_key_location,
            } => ProviderConfig::GCPVertexAnthropic(
                GCPVertexAnthropicProvider::new(model_id, location, project_id, api_key_location)
                    .await?,
            ),
            UninitializedProviderConfig::GCPVertexGemini {
                model_id,
                endpoint_id,
                location,
                project_id,
                credential_location: api_key_location,
            } => ProviderConfig::GCPVertexGemini(
                GCPVertexGeminiProvider::new(
                    model_id,
                    endpoint_id,
                    location,
                    project_id,
                    api_key_location,
                    provider_types,
                )
                .await?,
            ),
            UninitializedProviderConfig::GoogleAIStudioGemini {
                model_name,
                api_key_location,
            } => ProviderConfig::GoogleAIStudioGemini(GoogleAIStudioGeminiProvider::new(
                model_name,
                api_key_location,
            )?),
            UninitializedProviderConfig::Groq {
                model_name,
                api_key_location,
            } => ProviderConfig::Groq(GroqProvider::new(model_name, api_key_location)?),
            UninitializedProviderConfig::Hyperbolic {
                model_name,
                api_key_location,
            } => ProviderConfig::Hyperbolic(HyperbolicProvider::new(model_name, api_key_location)?),
            UninitializedProviderConfig::Mistral {
                model_name,
                api_key_location,
            } => ProviderConfig::Mistral(MistralProvider::new(model_name, api_key_location)?),
            UninitializedProviderConfig::OpenAI {
                model_name,
                api_base,
                api_key_location,
            } => {
                ProviderConfig::OpenAI(OpenAIProvider::new(model_name, api_base, api_key_location)?)
            }
            UninitializedProviderConfig::OpenRouter {
                model_name,
                api_key_location,
            } => ProviderConfig::OpenRouter(OpenRouterProvider::new(model_name, api_key_location)?),
            UninitializedProviderConfig::Together {
                model_name,
                api_key_location,
                parse_think_blocks,
            } => ProviderConfig::Together(TogetherProvider::new(
                model_name,
                api_key_location,
                parse_think_blocks,
            )?),
            UninitializedProviderConfig::VLLM {
                model_name,
                api_base,
                api_key_location,
            } => ProviderConfig::VLLM(VLLMProvider::new(model_name, api_base, api_key_location)?),
            UninitializedProviderConfig::XAI {
                model_name,
                api_key_location,
            } => ProviderConfig::XAI(XAIProvider::new(model_name, api_key_location)?),
            UninitializedProviderConfig::SGLang {
                model_name,
                api_base,
                api_key_location,
            } => {
                ProviderConfig::SGLang(SGLangProvider::new(model_name, api_base, api_key_location)?)
            }
            UninitializedProviderConfig::TGI {
                api_base,
                api_key_location,
            } => ProviderConfig::TGI(TGIProvider::new(api_base, api_key_location)?),
            UninitializedProviderConfig::DeepSeek {
                model_name,
                api_key_location,
            } => ProviderConfig::DeepSeek(DeepSeekProvider::new(model_name, api_key_location)?),
            #[cfg(any(test, feature = "e2e_tests"))]
            UninitializedProviderConfig::Dummy {
                model_name,
                api_key_location,
            } => ProviderConfig::Dummy(DummyProvider::new(model_name, api_key_location)?),
        })
    }
}

struct StreamAndRawRequest {
    stream: tracing_futures::Instrumented<PeekableProviderInferenceResponseStream>,
    raw_request: String,
}

pub struct StreamResponseAndMessages {
    pub response: StreamResponse,
    pub messages: Vec<RequestMessage>,
}

impl ModelProvider {
    #[tracing::instrument(skip_all, fields(provider_name = &*self.name, otel.name = "model_provider_inference",
        gen_ai.operation.name = "chat",
        gen_ai.system = self.genai_system_name(),
        gen_ai.request.model = self.genai_model_name(),
    stream = false))]
    async fn infer(
        &self,
        request: ModelProviderRequest<'_>,
        client: &Client,
        api_keys: &InferenceCredentials,
    ) -> Result<ProviderInferenceResponse, Error> {
        match &self.config {
            ProviderConfig::Anthropic(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::AWSBedrock(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::AWSSagemaker(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::Azure(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::Fireworks(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::GCPVertexAnthropic(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::GCPVertexGemini(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::Groq(provider) => provider.infer(request, client, api_keys, self).await,
            ProviderConfig::GoogleAIStudioGemini(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::Hyperbolic(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::Mistral(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::OpenAI(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::OpenRouter(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::Together(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::SGLang(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            ProviderConfig::VLLM(provider) => provider.infer(request, client, api_keys, self).await,
            ProviderConfig::XAI(provider) => provider.infer(request, client, api_keys, self).await,
            ProviderConfig::TGI(provider) => provider.infer(request, client, api_keys, self).await,
            ProviderConfig::DeepSeek(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
            #[cfg(any(test, feature = "e2e_tests"))]
            ProviderConfig::Dummy(provider) => {
                provider.infer(request, client, api_keys, self).await
            }
        }
    }

    #[tracing::instrument(skip_all, fields(provider_name = &*self.name, otel.name = "model_provider_inference",
        gen_ai.operation.name = "chat",
        gen_ai.system = self.genai_system_name(),
        gen_ai.request.model = self.genai_model_name(),
        time_to_first_token,
    stream = true))]
    async fn infer_stream(
        &self,
        request: ModelProviderRequest<'_>,
        client: &Client,
        api_keys: &InferenceCredentials,
    ) -> Result<StreamAndRawRequest, Error> {
        let (stream, raw_request) = match &self.config {
            ProviderConfig::Anthropic(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::AWSBedrock(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::AWSSagemaker(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::Azure(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::Fireworks(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::GCPVertexAnthropic(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::GCPVertexGemini(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::GoogleAIStudioGemini(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::Groq(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::Hyperbolic(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::Mistral(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::OpenAI(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::OpenRouter(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::Together(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::SGLang(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::XAI(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::VLLM(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::TGI(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            ProviderConfig::DeepSeek(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
            #[cfg(any(test, feature = "e2e_tests"))]
            ProviderConfig::Dummy(provider) => {
                provider.infer_stream(request, client, api_keys, self).await
            }
        }?;

        // Attach the current `model_provider_inference` span to the stream.
        // This will cause the span to be entered every time the stream is polled,
        // extending the lifetime of the span in OpenTelemetry to include the entire
        // duration of the response stream.
        Ok(StreamAndRawRequest {
            stream: stream.instrument(Span::current()),
            raw_request,
        })
    }

    async fn start_batch_inference<'a>(
        &self,
        requests: &'a [ModelInferenceRequest<'a>],
        client: &'a Client,
        api_keys: &'a InferenceCredentials,
    ) -> Result<StartBatchProviderInferenceResponse, Error> {
        match &self.config {
            ProviderConfig::Anthropic(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::AWSBedrock(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::AWSSagemaker(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::Azure(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::Fireworks(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::GCPVertexAnthropic(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::GCPVertexGemini(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::GoogleAIStudioGemini(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::Groq(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::Hyperbolic(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::Mistral(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::OpenAI(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::OpenRouter(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::Together(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::SGLang(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::VLLM(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::XAI(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::DeepSeek(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            ProviderConfig::TGI(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
            #[cfg(any(test, feature = "e2e_tests"))]
            ProviderConfig::Dummy(provider) => {
                provider
                    .start_batch_inference(requests, client, api_keys)
                    .await
            }
        }
    }

    pub async fn poll_batch_inference<'a>(
        &self,
        batch_request: &'a BatchRequestRow<'_>,
        http_client: &'a reqwest::Client,
        dynamic_api_keys: &'a InferenceCredentials,
    ) -> Result<PollBatchInferenceResponse, Error> {
        match &self.config {
            ProviderConfig::Anthropic(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::AWSBedrock(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::AWSSagemaker(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::Azure(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::Fireworks(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::GCPVertexAnthropic(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::GCPVertexGemini(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::GoogleAIStudioGemini(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::Groq(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::Hyperbolic(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::Mistral(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::OpenAI(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::OpenRouter(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::TGI(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::Together(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::SGLang(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::VLLM(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::XAI(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            ProviderConfig::DeepSeek(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
            #[cfg(any(test, feature = "e2e_tests"))]
            ProviderConfig::Dummy(provider) => {
                provider
                    .poll_batch_inference(batch_request, http_client, dynamic_api_keys)
                    .await
            }
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum CredentialLocation {
    /// Environment variable containing the actual credential
    Env(String),
    /// Environment variable containing the path to a credential file
    PathFromEnv(String),
    /// For dynamic credential resolution
    Dynamic(String),
    /// Direct path to a credential file
    Path(String),
    /// Use a provider-specific SDK to determine credentials
    Sdk,
    None,
}

impl<'de> Deserialize<'de> for CredentialLocation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if let Some(inner) = s.strip_prefix("env::") {
            Ok(CredentialLocation::Env(inner.to_string()))
        } else if let Some(inner) = s.strip_prefix("path_from_env::") {
            Ok(CredentialLocation::PathFromEnv(inner.to_string()))
        } else if let Some(inner) = s.strip_prefix("dynamic::") {
            Ok(CredentialLocation::Dynamic(inner.to_string()))
        } else if let Some(inner) = s.strip_prefix("path::") {
            Ok(CredentialLocation::Path(inner.to_string()))
        } else if s == "sdk" {
            Ok(CredentialLocation::Sdk)
        } else if s == "none" {
            Ok(CredentialLocation::None)
        } else {
            Err(serde::de::Error::custom(format!(
                "Invalid ApiKeyLocation format: {s}"
            )))
        }
    }
}

impl Serialize for CredentialLocation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = match self {
            CredentialLocation::Env(inner) => format!("env::{inner}"),
            CredentialLocation::PathFromEnv(inner) => format!("path_from_env::{inner}"),
            CredentialLocation::Dynamic(inner) => format!("dynamic::{inner}"),
            CredentialLocation::Path(inner) => format!("path::{inner}"),
            CredentialLocation::Sdk => "sdk".to_string(),
            CredentialLocation::None => "none".to_string(),
        };
        serializer.serialize_str(&s)
    }
}

#[derive(Clone)]
pub enum Credential {
    Static(SecretString),
    FileContents(SecretString),
    Dynamic(String),
    Sdk,
    None,
    Missing,
}

/// Builds a credential type from the provided `CredentialLocation` and default location.
/// This is a convenience function that calls `get_creds_with_cache_and_fn` using
/// the `TryFrom<Credential>` implementation for `T`.
///
/// Most providers should be able to use this function to build their credentials,
/// unless they have special requirements (e.g. calling an `async fn`)
pub fn build_creds_caching_default<T: Clone + TryFrom<Credential, Error = Error>>(
    location: Option<CredentialLocation>,
    default_location: CredentialLocation,
    provider_type: &str,
    cache: &OnceLock<T>,
) -> Result<T, Error> {
    build_creds_caching_default_with_fn(location, default_location, provider_type, cache, |creds| {
        T::try_from(creds)
    })
}

/// Builds a credential type from the provided `CredentialLocation` and default location.
/// If the location is `None`, we'll use the provided `OnceLock` to cache the result
/// of `f(default_location)`.
/// Otherwise, we'll call `f(location)` without caching the result.
///
/// **NOTE** - `f` may be run multiple times in parallel even when `default_location` is used,
/// due to a limitation of the current `OnceLock` api.
pub fn build_creds_caching_default_with_fn<T: Clone, F: FnOnce(Credential) -> Result<T, Error>>(
    location: Option<CredentialLocation>,
    default_location: CredentialLocation,
    provider_type: &str,
    cache: &OnceLock<T>,
    f: F,
) -> Result<T, Error> {
    let make_creds = |location| {
        let creds = Credential::try_from((location, provider_type))?;
        let provider_creds = f(creds)?;
        Ok(provider_creds)
    };
    if let Some(location) = location {
        make_creds(location)
    } else {
        racy_get_or_try_init(cache, || make_creds(default_location))
    }
}

/// Gets the value from a `OnceLock` or initializes it with the result of `f`
/// If this is called simultaneously from multiple threads, it may call `f` multiple times
/// If `f` returns an error, the `OnceLock` will remain uninitialized
fn racy_get_or_try_init<T: Clone, E>(
    once_lock: &OnceLock<T>,
    f: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    if let Some(val) = once_lock.get() {
        Ok(val.clone())
    } else {
        let val = f()?;
        // We don't care if the value was est
        let _ = once_lock.set(val.clone());
        Ok(val)
    }
}

impl TryFrom<(CredentialLocation, &str)> for Credential {
    type Error = Error;

    fn try_from(
        (location, provider_type): (CredentialLocation, &str),
    ) -> Result<Self, Self::Error> {
        match location {
            CredentialLocation::Env(key_name) => match env::var(key_name) {
                Ok(value) => Ok(Credential::Static(SecretString::from(value))),
                Err(_) => {
                    if skip_credential_validation() {
                        #[cfg(any(test, feature = "e2e_tests"))]
                        {
                            tracing::warn!(
                            "You are missing the credentials required for a model provider of type {}, so the associated tests will likely fail.",
                            provider_type
                        );
                        }
                        Ok(Credential::Missing)
                    } else {
                        Err(Error::new(ErrorDetails::ApiKeyMissing {
                            provider_name: provider_type.to_string(),
                        }))
                    }
                }
            },
            CredentialLocation::PathFromEnv(env_key) => {
                // First get the path from environment variable
                let path = match env::var(&env_key) {
                    Ok(path) => path,
                    Err(_) => {
                        if skip_credential_validation() {
                            #[cfg(any(test, feature = "e2e_tests"))]
                            {
                                tracing::warn!(
                                "Environment variable {} is required for a model provider of type {} but is missing, so the associated tests will likely fail.",
                                env_key, provider_type

                            );
                            }
                            return Ok(Credential::Missing);
                        } else {
                            return Err(Error::new(ErrorDetails::ApiKeyMissing {
                                provider_name: format!(
                                    "{provider_type}: Environment variable {env_key} for credentials path is missing"
                                ),
                            }));
                        }
                    }
                };
                // Then read the file contents
                match fs::read_to_string(path) {
                    Ok(contents) => Ok(Credential::FileContents(SecretString::from(contents))),
                    Err(e) => {
                        if skip_credential_validation() {
                            #[cfg(any(test, feature = "e2e_tests"))]
                            {
                                tracing::warn!(
                                "Failed to read credentials file for a model provider of type {}, so the associated tests will likely fail: {}",
                                provider_type, e
                            );
                            }
                            Ok(Credential::Missing)
                        } else {
                            Err(Error::new(ErrorDetails::ApiKeyMissing {
                                provider_name: format!(
                                    "{provider_type}: Failed to read credentials file - {e}"
                                ),
                            }))
                        }
                    }
                }
            }
            CredentialLocation::Path(path) => match fs::read_to_string(path) {
                Ok(contents) => Ok(Credential::FileContents(SecretString::from(contents))),
                Err(e) => {
                    if skip_credential_validation() {
                        #[cfg(any(test, feature = "e2e_tests"))]
                        {
                            tracing::warn!(
                                "Failed to read credentials file for a model provider of type {}, so the associated tests will likely fail: {}",
                            provider_type, e
                        );
                        }
                        Ok(Credential::Missing)
                    } else {
                        Err(Error::new(ErrorDetails::ApiKeyMissing {
                            provider_name: format!(
                                "{provider_type}: Failed to read credentials file - {e}"
                            ),
                        }))
                    }
                }
            },
            CredentialLocation::Dynamic(key_name) => Ok(Credential::Dynamic(key_name.clone())),
            CredentialLocation::Sdk => Ok(Credential::Sdk),
            CredentialLocation::None => Ok(Credential::None),
        }
    }
}

const SHORTHAND_MODEL_PREFIXES: &[&str] = &[
    "anthropic::",
    "deepseek::",
    "fireworks::",
    "google_ai_studio_gemini::",
    "gcp_vertex_gemini::",
    "gcp_vertex_anthropic::",
    "hyperbolic::",
    "groq::",
    "mistral::",
    "openai::",
    "openrouter::",
    "together::",
    "xai::",
    "dummy::",
];

pub type ModelTable = BaseModelTable<ModelConfig>;

impl ShorthandModelConfig for ModelConfig {
    const SHORTHAND_MODEL_PREFIXES: &[&str] = SHORTHAND_MODEL_PREFIXES;
    const MODEL_TYPE: &str = "Model";
    async fn from_shorthand(provider_type: &str, model_name: &str) -> Result<Self, Error> {
        let model_name = model_name.to_string();
        let provider_config = match provider_type {
            "anthropic" => ProviderConfig::Anthropic(AnthropicProvider::new(model_name, None)?),
            "deepseek" => ProviderConfig::DeepSeek(DeepSeekProvider::new(model_name, None)?),
            "fireworks" => ProviderConfig::Fireworks(FireworksProvider::new(
                model_name,
                None,
                crate::providers::fireworks::default_parse_think_blocks(),
            )?),
            "google_ai_studio_gemini" => ProviderConfig::GoogleAIStudioGemini(
                GoogleAIStudioGeminiProvider::new(model_name, None)?,
            ),
            "gcp_vertex_gemini" => ProviderConfig::GCPVertexGemini(
                GCPVertexGeminiProvider::new_shorthand(model_name).await?,
            ),
            "gcp_vertex_anthropic" => ProviderConfig::GCPVertexAnthropic(
                GCPVertexAnthropicProvider::new_shorthand(model_name).await?,
            ),
            "groq" => ProviderConfig::Groq(GroqProvider::new(model_name, None)?),
            "hyperbolic" => ProviderConfig::Hyperbolic(HyperbolicProvider::new(model_name, None)?),
            "mistral" => ProviderConfig::Mistral(MistralProvider::new(model_name, None)?),
            "openai" => ProviderConfig::OpenAI(OpenAIProvider::new(model_name, None, None)?),
            "openrouter" => ProviderConfig::OpenRouter(OpenRouterProvider::new(model_name, None)?),
            "together" => ProviderConfig::Together(TogetherProvider::new(
                model_name,
                None,
                crate::providers::together::default_parse_think_blocks(),
            )?),
            "xai" => ProviderConfig::XAI(XAIProvider::new(model_name, None)?),
            #[cfg(any(test, feature = "e2e_tests"))]
            "dummy" => ProviderConfig::Dummy(DummyProvider::new(model_name, None)?),
            _ => {
                return Err(ErrorDetails::Config {
                    message: format!("Invalid provider type: {provider_type}"),
                }
                .into());
            }
        };
        Ok(ModelConfig {
            routing: vec![provider_type.to_string().into()],
            providers: HashMap::from([(
                provider_type.to_string().into(),
                ModelProvider {
                    name: provider_type.into(),
                    config: provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        })
    }

    fn validate(&self, model_name: &str) -> Result<(), Error> {
        // Ensure that the model has at least one provider
        if self.routing.is_empty() {
            return Err(ErrorDetails::Config {
                message: format!("`models.{model_name}`: `routing` must not be empty"),
            }
            .into());
        }

        // Ensure that routing entries are unique and exist as keys in providers
        let mut seen_providers = std::collections::HashSet::new();
        for provider in &self.routing {
            if provider.starts_with("tensorzero::") {
                return Err(ErrorDetails::Config {
                    message: format!("`models.{model_name}.routing`: Provider name cannot start with 'tensorzero::': {provider}"),
                }
                .into());
            }
            if !seen_providers.insert(provider) {
                return Err(ErrorDetails::Config {
                    message: format!("`models.{model_name}.routing`: duplicate entry `{provider}`"),
                }
                .into());
            }

            if !self.providers.contains_key(provider) {
                return Err(ErrorDetails::Config {
            message: format!(
                "`models.{model_name}`: `routing` contains entry `{provider}` that does not exist in `providers`"
            ),
        }
        .into());
            }
        }

        // Validate each provider
        for provider_name in self.providers.keys() {
            if !seen_providers.contains(provider_name) {
                return Err(ErrorDetails::Config {
                    message: format!(
                "`models.{model_name}`: Provider `{provider_name}` is not listed in `routing`"
            ),
                }
                .into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, cell::Cell};

    use crate::cache::CacheEnabledMode;
    use crate::config_parser::SKIP_CREDENTIAL_VALIDATION;
    use crate::tool::{ToolCallConfig, ToolChoice};
    use crate::{
        cache::CacheOptions,
        clickhouse::ClickHouseConnectionInfo,
        inference::types::{
            ContentBlockChunk, FunctionType, ModelInferenceRequestJsonMode, TextChunk,
        },
        model_table::RESERVED_MODEL_PREFIXES,
        providers::dummy::{
            DummyCredentials, DUMMY_INFER_RESPONSE_CONTENT, DUMMY_INFER_RESPONSE_RAW,
            DUMMY_STREAMING_RESPONSE,
        },
    };
    use secrecy::SecretString;
    use tokio_stream::StreamExt;
    use tracing_test::traced_test;
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn test_model_config_infer_routing() {
        let good_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "good".into(),
            credentials: DummyCredentials::None,
        });
        let bad_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "error".into(),
            credentials: DummyCredentials::None,
        });
        let model_config = ModelConfig {
            routing: vec!["good_provider".into()],
            providers: HashMap::from([(
                "good_provider".into(),
                ModelProvider {
                    name: "good_provider".into(),
                    config: good_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let tool_config = ToolCallConfig {
            tools_available: vec![],
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: None,
        };
        let api_keys = InferenceCredentials::default();
        let http_client = Client::new();
        let clickhouse_connection_info = ClickHouseConnectionInfo::Disabled;
        let clients = InferenceClients {
            http_client: &http_client,
            clickhouse_connection_info: &clickhouse_connection_info,
            credentials: &api_keys,
            cache_options: &CacheOptions {
                max_age_s: None,
                enabled: CacheEnabledMode::WriteOnly,
            },
        };

        // Try inferring the good model only
        let request = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![],
            system: None,
            tool_config: Some(Cow::Borrowed(&tool_config)),
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: false,
            json_mode: ModelInferenceRequestJsonMode::Off,
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };
        let model_name = "test model";
        let response = model_config
            .infer(&request, &clients, model_name)
            .await
            .unwrap();
        let content = response.output;
        assert_eq!(
            content,
            vec![DUMMY_INFER_RESPONSE_CONTENT.to_string().into()]
        );
        let raw = response.raw_response;
        assert_eq!(raw, DUMMY_INFER_RESPONSE_RAW);
        let usage = response.usage;
        assert_eq!(
            usage,
            Usage {
                input_tokens: 10,
                output_tokens: 1,
            }
        );
        assert_eq!(&*response.model_provider_name, "good_provider");

        // Try inferring the bad model
        let model_config = ModelConfig {
            routing: vec!["error".into()],
            providers: HashMap::from([(
                "error".into(),
                ModelProvider {
                    name: "error".into(),
                    config: bad_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let response = model_config
            .infer(&request, &clients, model_name)
            .await
            .unwrap_err();
        assert_eq!(
            response,
            ErrorDetails::ModelProvidersExhausted {
                provider_errors: HashMap::from([(
                    "error".to_string(),
                    ErrorDetails::InferenceClient {
                        message: "Error sending request to Dummy provider for model 'error'."
                            .to_string(),
                        status_code: None,
                        provider_type: "dummy".to_string(),
                        raw_request: Some("raw request".to_string()),
                        raw_response: None,
                    }
                    .into()
                )])
            }
            .into()
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_model_config_infer_routing_fallback() {
        // Test that fallback works with bad --> good model provider

        let good_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "good".into(),
            credentials: DummyCredentials::None,
        });
        let bad_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "error".into(),
            credentials: DummyCredentials::None,
        });
        let api_keys = InferenceCredentials::default();
        let http_client = Client::new();
        let clickhouse_connection_info = ClickHouseConnectionInfo::Disabled;
        let clients = InferenceClients {
            http_client: &http_client,
            clickhouse_connection_info: &clickhouse_connection_info,
            credentials: &api_keys,
            cache_options: &CacheOptions {
                max_age_s: None,
                enabled: CacheEnabledMode::WriteOnly,
            },
        };
        // Try inferring the good model only
        let request = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![],
            system: None,
            tool_config: None,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: false,
            json_mode: ModelInferenceRequestJsonMode::Off,
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };

        let model_config = ModelConfig {
            routing: vec![
                "error_provider".to_string().into(),
                "good_provider".to_string().into(),
            ],
            providers: HashMap::from([
                (
                    "error_provider".to_string().into(),
                    ModelProvider {
                        name: "error_provider".into(),
                        config: bad_provider_config,
                        extra_body: Default::default(),
                        extra_headers: Default::default(),
                        timeouts: Default::default(),
                        discard_unknown_chunks: false,
                    },
                ),
                (
                    "good_provider".to_string().into(),
                    ModelProvider {
                        name: "good_provider".into(),
                        config: good_provider_config,
                        extra_body: Default::default(),
                        extra_headers: Default::default(),
                        timeouts: Default::default(),
                        discard_unknown_chunks: false,
                    },
                ),
            ]),
            timeouts: Default::default(),
        };

        let model_name = "test model";
        let response = model_config
            .infer(&request, &clients, model_name)
            .await
            .unwrap();
        // Ensure that the error for the bad provider was logged, but the request worked nonetheless
        assert!(logs_contain(
            "Error sending request to Dummy provider for model 'error'."
        ));
        let content = response.output;
        assert_eq!(
            content,
            vec![DUMMY_INFER_RESPONSE_CONTENT.to_string().into()]
        );
        let raw = response.raw_response;
        assert_eq!(raw, DUMMY_INFER_RESPONSE_RAW);
        let usage = response.usage;
        assert_eq!(
            usage,
            Usage {
                input_tokens: 10,
                output_tokens: 1,
            }
        );
        assert_eq!(&*response.model_provider_name, "good_provider");
    }

    #[tokio::test]
    async fn test_model_config_infer_stream_routing() {
        let good_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "good".into(),
            credentials: DummyCredentials::None,
        });
        let bad_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "error".into(),
            credentials: DummyCredentials::None,
        });
        let api_keys = InferenceCredentials::default();
        let request = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![],
            system: None,
            tool_config: None,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: true,
            json_mode: ModelInferenceRequestJsonMode::Off,
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };

        // Test good model
        let model_config = ModelConfig {
            routing: vec!["good_provider".to_string().into()],
            providers: HashMap::from([(
                "good_provider".to_string().into(),
                ModelProvider {
                    name: "good_provider".into(),
                    config: good_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let StreamResponseAndMessages {
            response:
                StreamResponse {
                    mut stream,
                    raw_request,
                    model_provider_name,
                    cached: _,
                },
            messages: _input,
        } = model_config
            .infer_stream(
                &request,
                &InferenceClients {
                    http_client: &Client::new(),
                    clickhouse_connection_info: &ClickHouseConnectionInfo::Disabled,
                    credentials: &api_keys,
                    cache_options: &CacheOptions {
                        max_age_s: None,
                        enabled: CacheEnabledMode::Off,
                    },
                },
                "my_model",
            )
            .await
            .unwrap();
        let initial_chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(
            initial_chunk.content,
            vec![ContentBlockChunk::Text(TextChunk {
                text: DUMMY_STREAMING_RESPONSE[0].to_string(),
                id: "0".to_string(),
            })],
        );
        assert_eq!(raw_request, "raw request");
        assert_eq!(&*model_provider_name, "good_provider");
        let mut collected_content: Vec<ContentBlockChunk> =
            vec![ContentBlockChunk::Text(TextChunk {
                text: DUMMY_STREAMING_RESPONSE[0].to_string(),
                id: "0".to_string(),
            })];
        let mut stream = Box::pin(stream);
        while let Some(Ok(chunk)) = stream.next().await {
            let mut content = chunk.content;
            assert!(content.len() <= 1);
            if content.len() == 1 {
                collected_content.push(content.pop().unwrap());
            }
        }
        let mut collected_content_str = String::new();
        for content in collected_content {
            match content {
                ContentBlockChunk::Text(text) => collected_content_str.push_str(&text.text),
                _ => panic!("Expected a text content block"),
            }
        }
        assert_eq!(collected_content_str, DUMMY_STREAMING_RESPONSE.join(""));

        // Test bad model
        let model_config = ModelConfig {
            routing: vec!["error".to_string().into()],
            providers: HashMap::from([(
                "error".to_string().into(),
                ModelProvider {
                    name: "error".to_string().into(),
                    config: bad_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let response = model_config
            .infer_stream(
                &request,
                &InferenceClients {
                    http_client: &Client::new(),
                    clickhouse_connection_info: &ClickHouseConnectionInfo::Disabled,
                    credentials: &api_keys,
                    cache_options: &CacheOptions {
                        max_age_s: None,
                        enabled: CacheEnabledMode::Off,
                    },
                },
                "my_model",
            )
            .await;
        assert!(response.is_err());
        let error = match response {
            Err(error) => error,
            Ok(_) => panic!("Expected error, got Ok(_)"),
        };
        assert_eq!(
            error,
            ErrorDetails::ModelProvidersExhausted {
                provider_errors: HashMap::from([(
                    "error".to_string(),
                    ErrorDetails::InferenceClient {
                        message: "Error sending request to Dummy provider for model 'error'."
                            .to_string(),
                        status_code: None,
                        provider_type: "dummy".to_string(),
                        raw_request: Some("raw request".to_string()),
                        raw_response: None,
                    }
                    .into()
                )])
            }
            .into()
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_model_config_infer_stream_routing_fallback() {
        // Test that fallback works with bad --> good model provider (streaming)

        let good_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "good".into(),
            credentials: DummyCredentials::None,
        });
        let bad_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "error".into(),
            credentials: DummyCredentials::None,
        });
        let api_keys = InferenceCredentials::default();
        let request = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![],
            system: None,
            tool_config: None,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: true,
            json_mode: ModelInferenceRequestJsonMode::Off,
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };

        // Test fallback
        let model_config = ModelConfig {
            routing: vec!["error_provider".into(), "good_provider".into()],
            providers: HashMap::from([
                (
                    "error_provider".to_string().into(),
                    ModelProvider {
                        name: "error_provider".to_string().into(),
                        config: bad_provider_config,
                        extra_body: Default::default(),
                        extra_headers: Default::default(),
                        timeouts: Default::default(),
                        discard_unknown_chunks: false,
                    },
                ),
                (
                    "good_provider".to_string().into(),
                    ModelProvider {
                        name: "good_provider".to_string().into(),
                        config: good_provider_config,
                        extra_body: Default::default(),
                        extra_headers: Default::default(),
                        timeouts: Default::default(),
                        discard_unknown_chunks: false,
                    },
                ),
            ]),
            timeouts: Default::default(),
        };
        let StreamResponseAndMessages {
            response:
                StreamResponse {
                    mut stream,
                    raw_request,
                    model_provider_name,
                    cached: _,
                },
            messages: _,
        } = model_config
            .infer_stream(
                &request,
                &InferenceClients {
                    http_client: &Client::new(),
                    clickhouse_connection_info: &ClickHouseConnectionInfo::Disabled,
                    credentials: &api_keys,
                    cache_options: &CacheOptions {
                        max_age_s: None,
                        enabled: CacheEnabledMode::Off,
                    },
                },
                "my_model",
            )
            .await
            .unwrap();
        let initial_chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(&*model_provider_name, "good_provider");
        // Ensure that the error for the bad provider was logged, but the request worked nonetheless
        assert!(logs_contain(
            "Error sending request to Dummy provider for model 'error'"
        ));
        assert_eq!(raw_request, "raw request");

        assert_eq!(
            initial_chunk.content,
            vec![ContentBlockChunk::Text(TextChunk {
                text: DUMMY_STREAMING_RESPONSE[0].to_string(),
                id: "0".to_string(),
            })],
        );

        let mut collected_content = initial_chunk.content;
        let mut stream = Box::pin(stream);
        while let Some(Ok(chunk)) = stream.next().await {
            let mut content = chunk.content;
            assert!(content.len() <= 1);
            if content.len() == 1 {
                collected_content.push(content.pop().unwrap());
            }
        }
        let mut collected_content_str = String::new();
        for content in collected_content {
            match content {
                ContentBlockChunk::Text(text) => collected_content_str.push_str(&text.text),
                _ => panic!("Expected a text content block"),
            }
        }
        assert_eq!(collected_content_str, DUMMY_STREAMING_RESPONSE.join(""));
    }

    #[tokio::test]
    async fn test_dynamic_api_keys() {
        let provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "test_key".into(),
            credentials: DummyCredentials::Dynamic("TEST_KEY".to_string()),
        });
        let model_config = ModelConfig {
            routing: vec!["model".into()],
            providers: HashMap::from([(
                "model".into(),
                ModelProvider {
                    name: "model".into(),
                    config: provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let tool_config = ToolCallConfig {
            tools_available: vec![],
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: None,
        };
        let api_keys = InferenceCredentials::default();
        let http_client = Client::new();
        let clickhouse_connection_info = ClickHouseConnectionInfo::Disabled;
        let clients = InferenceClients {
            http_client: &http_client,
            clickhouse_connection_info: &clickhouse_connection_info,
            credentials: &api_keys,
            cache_options: &CacheOptions {
                max_age_s: None,
                enabled: CacheEnabledMode::WriteOnly,
            },
        };

        let request = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![],
            system: None,
            tool_config: Some(Cow::Borrowed(&tool_config)),
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: false,
            json_mode: ModelInferenceRequestJsonMode::Off,
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };
        let model_name = "test model";
        let error = model_config
            .infer(&request, &clients, model_name)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            ErrorDetails::ModelProvidersExhausted {
                provider_errors: HashMap::from([(
                    "model".to_string(),
                    ErrorDetails::ApiKeyMissing {
                        provider_name: "Dummy".to_string()
                    }
                    .into()
                )])
            }
            .into()
        );

        let api_keys = HashMap::from([(
            "TEST_KEY".to_string(),
            SecretString::from("notgoodkey".to_string()),
        )]);
        let clients = InferenceClients {
            http_client: &http_client,
            clickhouse_connection_info: &clickhouse_connection_info,
            credentials: &api_keys,
            cache_options: &CacheOptions {
                max_age_s: None,
                enabled: CacheEnabledMode::WriteOnly,
            },
        };
        let response = model_config
            .infer(&request, &clients, model_name)
            .await
            .unwrap_err();
        assert_eq!(
            response,
            ErrorDetails::ModelProvidersExhausted {
                provider_errors: HashMap::from([(
                    "model".to_string(),
                    ErrorDetails::InferenceClient {
                        message: "Invalid API key for Dummy provider".to_string(),
                        status_code: None,
                        provider_type: "dummy".to_string(),
                        raw_request: Some("raw request".to_string()),
                        raw_response: None,
                    }
                    .into()
                )])
            }
            .into()
        );

        let provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "test_key".into(),
            credentials: DummyCredentials::Dynamic("TEST_KEY".to_string()),
        });
        let model_config = ModelConfig {
            routing: vec!["model".to_string().into()],
            providers: HashMap::from([(
                "model".to_string().into(),
                ModelProvider {
                    name: "model".to_string().into(),
                    config: provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let tool_config = ToolCallConfig {
            tools_available: vec![],
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: None,
        };
        let api_keys = InferenceCredentials::default();
        let http_client = Client::new();
        let clickhouse_connection_info = ClickHouseConnectionInfo::Disabled;
        let clients = InferenceClients {
            http_client: &http_client,
            clickhouse_connection_info: &clickhouse_connection_info,
            credentials: &api_keys,
            cache_options: &CacheOptions {
                max_age_s: None,
                enabled: CacheEnabledMode::WriteOnly,
            },
        };

        let request = ModelInferenceRequest {
            messages: vec![],
            inference_id: Uuid::now_v7(),
            system: None,
            tool_config: Some(Cow::Borrowed(&tool_config)),
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: false,
            json_mode: ModelInferenceRequestJsonMode::Off,
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };
        let error = model_config
            .infer(&request, &clients, model_name)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            ErrorDetails::ModelProvidersExhausted {
                provider_errors: HashMap::from([(
                    "model".to_string(),
                    ErrorDetails::ApiKeyMissing {
                        provider_name: "Dummy".to_string()
                    }
                    .into()
                )])
            }
            .into()
        );

        let api_keys = HashMap::from([(
            "TEST_KEY".to_string(),
            SecretString::from("good_key".to_string()),
        )]);
        let clients = InferenceClients {
            http_client: &http_client,
            clickhouse_connection_info: &clickhouse_connection_info,
            credentials: &api_keys,
            cache_options: &CacheOptions {
                max_age_s: None,
                enabled: CacheEnabledMode::WriteOnly,
            },
        };
        let response = model_config
            .infer(&request, &clients, model_name)
            .await
            .unwrap();
        assert_eq!(
            response.output,
            vec![DUMMY_INFER_RESPONSE_CONTENT.to_string().into()]
        );
    }

    #[tokio::test]
    async fn test_validate_or_create_model_config() {
        let model_table = ModelTable::default();
        // Test that we can get or create a model config
        model_table.validate("dummy::gpt-4o").unwrap();
        // Shorthand models are not added to the model table
        assert_eq!(model_table.static_model_len(), 0);
        let model_config = model_table
            .get("dummy::gpt-4o")
            .await
            .unwrap()
            .expect("Missing dummy model");
        assert_eq!(model_config.routing, vec!["dummy".into()]);
        let provider_config = &model_config.providers.get("dummy").unwrap().config;
        match provider_config {
            ProviderConfig::Dummy(provider) => assert_eq!(&*provider.model_name, "gpt-4o"),
            _ => panic!("Expected Dummy provider"),
        }

        // Test that it fails if the model is not well-formed
        let model_config = model_table.validate("foo::bar");
        assert!(model_config.is_err());
        assert_eq!(
            model_config.unwrap_err(),
            ErrorDetails::Config {
                message: "Model name 'foo::bar' not found in model table".to_string()
            }
            .into()
        );
        // Test that it works with an initialized model
        let anthropic_provider_config = SKIP_CREDENTIAL_VALIDATION.sync_scope((), || {
            ProviderConfig::Anthropic(AnthropicProvider::new("claude".to_string(), None).unwrap())
        });
        let anthropic_model_config = ModelConfig {
            routing: vec!["anthropic".into()],
            providers: HashMap::from([(
                "anthropic".into(),
                ModelProvider {
                    name: "anthropic".into(),
                    config: anthropic_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let model_table: ModelTable = HashMap::from([("claude".into(), anthropic_model_config)])
            .try_into()
            .unwrap();

        model_table.validate("dummy::claude").unwrap();
    }

    #[test]
    fn test_shorthand_prefixes_subset_of_reserved() {
        for &shorthand in SHORTHAND_MODEL_PREFIXES {
            assert!(
                RESERVED_MODEL_PREFIXES.contains(&shorthand.to_string()),
                "Shorthand prefix '{shorthand}' is not in RESERVED_MODEL_PREFIXES"
            );
        }
    }

    #[test]
    fn test_racy_get_or_try_init() {
        let lock: OnceLock<bool> = OnceLock::new();

        // If the closure returns an error, `racy_get_or_try_init` should return an error
        racy_get_or_try_init(&lock, || {
            Err::<_, Box<dyn std::error::Error>>("Test error".into())
        })
        .expect_err("Test error");
        assert!(
            lock.get().is_none(),
            "OnceLock was initialized after an error"
        );

        racy_get_or_try_init(&lock, || Ok::<_, Box<dyn std::error::Error>>(true))
            .expect("racy_get_or_try_init should succeed with successful closure");

        assert_eq!(lock.get(), Some(&true));
    }

    #[test]
    fn test_cache_default_creds() {
        let make_creds_call_count = Cell::new(0);
        let make_creds = |_| {
            make_creds_call_count.set(make_creds_call_count.get() + 1);
            Ok(())
        };

        let cache = OnceLock::new();

        build_creds_caching_default_with_fn(
            None,
            CredentialLocation::None,
            "test",
            &cache,
            make_creds,
        )
        .expect("Failed to build creds");
        // The first call should initialize the OnceLock, and call `make_creds`
        assert_eq!(make_creds_call_count.get(), 1);
        assert_eq!(cache.get(), Some(&()));

        // Subsequent calls should not call `make_creds`
        build_creds_caching_default_with_fn(
            None,
            CredentialLocation::None,
            "test",
            &cache,
            make_creds,
        )
        .expect("Failed to build creds");
        assert_eq!(make_creds_call_count.get(), 1);
    }

    #[test]
    fn test_dont_cache_non_default_creds() {
        let make_creds_call_count = Cell::new(0);
        let make_creds = |_| {
            make_creds_call_count.set(make_creds_call_count.get() + 1);
            Ok(())
        };

        let cache = OnceLock::new();

        // When we provide a `Some(credential_location)`, we should not cache the creds.
        build_creds_caching_default_with_fn(
            Some(CredentialLocation::None),
            CredentialLocation::None,
            "test",
            &cache,
            make_creds,
        )
        .expect("Failed to build creds");

        assert_eq!(cache.get(), None);
        assert_eq!(make_creds_call_count.get(), 1);
    }
}
