use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use crate::config_parser::path::TomlRelativePath;
use crate::config_parser::LoadableConfig;
use crate::config_parser::PathWithContents;
use crate::embeddings::{EmbeddingModelTable, EmbeddingResponseWithMetadata};
use crate::endpoints::inference::InferenceModels;
use crate::inference::types::extra_body::{ExtraBodyConfig, FullExtraBodyConfig};
use crate::inference::types::extra_headers::{ExtraHeadersConfig, FullExtraHeadersConfig};
use crate::inference::types::ContentBlock;
use crate::inference::types::ResolvedInput;
use crate::inference::types::ResolvedInputMessageContent;
use crate::inference::types::{
    batch::StartBatchModelInferenceWithMetadata, ModelInferenceRequest, RequestMessage, Role,
};
use crate::model::ModelTable;
use crate::model_table::ShorthandModelConfig;
use crate::{
    embeddings::EmbeddingRequest,
    endpoints::inference::{InferenceClients, InferenceParams},
    error::{Error, ErrorDetails},
    function::FunctionConfig,
    inference::types::{
        ContentBlockChatOutput, InferenceResult, InferenceResultStream, JsonInferenceOutput,
    },
    minijinja_util::TemplateConfig,
};

use super::{
    infer_model_request, infer_model_request_stream, prepare_model_inference_request,
    InferModelRequestArgs, InferenceConfig, JsonMode, ModelUsedInfo, RetryConfig, Variant,
};

/// The primary configuration for the Dicl variant
/// We need a helper to deserialize the config because it relies on
/// a path to a file for system instructions and we need to use the
/// load() step to get the fully qualified path.
#[derive(Debug, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct DiclConfig {
    pub weight: Option<f64>,
    pub embedding_model: Arc<str>,
    pub k: u32, // k as in k-nearest neighbors
    pub model: Arc<str>,
    pub system_instructions: String,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub max_tokens: Option<u32>,
    pub seed: Option<u32>,
    pub json_mode: Option<JsonMode>,
    pub extra_body: Option<ExtraBodyConfig>,
    pub extra_headers: Option<ExtraHeadersConfig>,
    pub retries: RetryConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct UninitializedDiclConfig {
    #[serde(default)]
    pub weight: Option<f64>,
    pub embedding_model: String,
    pub k: u32, // k as in k-nearest neighbors
    pub model: String,
    pub system_instructions: Option<TomlRelativePath>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub max_tokens: Option<u32>,
    pub seed: Option<u32>,
    pub json_mode: Option<JsonMode>,
    #[serde(default)]
    pub extra_body: Option<ExtraBodyConfig>,
    #[serde(default)]
    pub retries: RetryConfig,
    #[serde(default)]
    pub extra_headers: Option<ExtraHeadersConfig>,
}

impl Variant for DiclConfig {
    async fn infer<'a: 'request, 'request>(
        &self,
        input: &ResolvedInput,
        models: &'request InferenceModels<'a>,
        function: &'a FunctionConfig,
        inference_config: &'request InferenceConfig<'request>,
        clients: &'request InferenceClients<'request>,
        inference_params: InferenceParams,
    ) -> Result<InferenceResult, Error> {
        // So this can be mutably borrowed by the prepare_request function
        let mut inference_params = inference_params;

        // Embed the input and grab the relevant examples from the database
        let (relevant_examples, embedding_response) = self
            .retrieve_relevant_examples(
                input,
                models.embedding_models,
                clients,
                inference_config.function_name,
                inference_config.variant_name,
                function,
            )
            .await?;

        // Prepare the request for the model
        let model_inference_request = self.prepare_request(
            input,
            &relevant_examples,
            function,
            inference_config,
            false,
            &mut inference_params,
        )?;

        let model_config = models.models.get(&self.model).await?.ok_or_else(|| {
            Error::new(ErrorDetails::UnknownModel {
                name: self.model.to_string(),
            })
        })?;

        // Instantiate the InferModelRequestArgs struct
        let args = InferModelRequestArgs {
            request: model_inference_request,
            model_name: self.model.clone(),
            model_config: &model_config,
            function,
            inference_config,
            clients,
            inference_params,
            retry_config: &self.retries,
        };

        // Refactored function call using the struct
        let mut inference_response = infer_model_request(args).await?;

        // Add the embedding to the model inference results
        inference_response
            .mut_model_inference_results()
            .push(embedding_response.into());
        Ok(inference_response)
    }

    async fn infer_stream<'request>(
        &self,
        input: &ResolvedInput,
        models: &'request InferenceModels<'_>,
        function: &FunctionConfig,
        inference_config: &'request InferenceConfig<'request>,
        clients: &'request InferenceClients<'request>,
        inference_params: InferenceParams,
    ) -> Result<(InferenceResultStream, ModelUsedInfo), Error> {
        // So this can be mutably borrowed by the prepare_request function
        let mut inference_params = inference_params;

        // Embed the input and grab the relevant examples from the database
        let (relevant_examples, embedding_response) = self
            .retrieve_relevant_examples(
                input,
                models.embedding_models,
                clients,
                inference_config.function_name,
                inference_config.variant_name,
                function,
            )
            .await?;
        // Prepare the request for the model
        let request = self.prepare_request(
            input,
            &relevant_examples,
            function,
            inference_config,
            true,
            &mut inference_params,
        )?;

        let model_config = models.models.get(&self.model).await?.ok_or_else(|| {
            Error::new(ErrorDetails::UnknownModel {
                name: self.model.to_string(),
            })
        })?;

        // Actually run the inference
        let (inference_result_stream, mut model_used_info) = infer_model_request_stream(
            request,
            self.model.clone(),
            &model_config,
            function,
            clients,
            inference_params,
            self.retries,
        )
        .await?;

        // Add the embedding to the model inference results
        model_used_info
            .previous_model_inference_results
            .push(embedding_response.into());
        Ok((inference_result_stream, model_used_info))
    }

    async fn validate(
        &self,
        _function: &FunctionConfig,
        models: &mut ModelTable,
        embedding_models: &EmbeddingModelTable,
        _templates: &TemplateConfig<'_>,
        function_name: &str,
        variant_name: &str,
    ) -> Result<(), Error> {
        // TODO (#360): Add the clickhouse connection to this interface
        // Run a count() query on the DynamicInContextLearningExample table
        // WHERE function_name = function_name and variant_name = variant_name
        // Make sure that the count is positive

        // Validate that weight is non-negative
        if self.weight.is_some_and(|w| w < 0.0) {
            return Err(ErrorDetails::Config {
                message: format!(
                "`functions.{function_name}.variants.{variant_name}`: `weight` must be non-negative"
            ),
            }
            .into());
        }
        // Validate that the generation model and embedding model are valid
        models.validate(&self.model)?;
        let embedding_model = embedding_models
            .get(&self.embedding_model).await?
            .ok_or_else(|| Error::new(ErrorDetails::Config {
                message: format!(
                    "`functions.{function_name}.variants.{variant_name}`: `embedding_model` must be a valid embedding model name"
                ),
            }))?;

        embedding_model
            .validate(&self.embedding_model)
            .map_err(|e| {
                Error::new(ErrorDetails::Config {
                    message: format!(
                "`functions.{function_name}.variants.{variant_name}` and embedding model `{}`: {e}",
                self.embedding_model
                ),
                })
            })?;
        Ok(())
    }

    fn get_all_template_paths(&self) -> Vec<&PathWithContents> {
        vec![]
    }

    async fn start_batch_inference<'a>(
        &'a self,
        _input: &[ResolvedInput],
        _models: &'a InferenceModels<'a>,
        _function: &'a FunctionConfig,
        _inference_configs: &'a [InferenceConfig<'a>],
        _clients: &'a InferenceClients<'a>,
        _inference_params: Vec<InferenceParams>,
    ) -> Result<StartBatchModelInferenceWithMetadata<'a>, Error> {
        // TODO (#493): Implement batch inference for Dicl
        Err(ErrorDetails::UnsupportedVariantForBatchInference { variant_name: None }.into())
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct ChatExample {
    input: ResolvedInput,
    output: Vec<ContentBlockChatOutput>,
}

#[derive(Debug, Deserialize, PartialEq)]
struct JsonExample {
    input: ResolvedInput,
    output: JsonInferenceOutput,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum Example {
    Chat(ChatExample),
    Json(JsonExample),
}

#[derive(Clone, Debug, Deserialize)]
struct RawExample {
    input: String,
    output: String,
}

impl DiclConfig {
    async fn retrieve_relevant_examples<'a>(
        &'a self,
        input: &ResolvedInput,
        embedding_models: &'a EmbeddingModelTable,
        clients: &InferenceClients<'_>,
        function_name: &str,
        variant_name: &str,
        function: &FunctionConfig,
    ) -> Result<(Vec<Example>, EmbeddingResponseWithMetadata), Error> {
        // Serialize the input so that it can be embedded
        let serialized_input = serde_json::to_string(&input).map_err(|e| {
            Error::new(ErrorDetails::Serialization {
                message: format!(
                    "Error in serializing Input in dynamic in-context learning variant: {e}"
                ),
            })
        })?;

        let embedding_model = embedding_models
            .get(&self.embedding_model)
            .await?
            .ok_or_else(|| {
                Error::new(ErrorDetails::Inference {
                    message: format!("Embedding model {} not found", self.embedding_model),
                })
            })?;

        let embedding_request = EmbeddingRequest {
            input: serialized_input.to_string(),
        };

        // Embed the input via an API request
        let embedding_response = embedding_model
            .embed(&embedding_request, &self.embedding_model, clients)
            .await?;

        // Wrap the embedding in a response with metadata
        let embedding_response_with_metadata =
            EmbeddingResponseWithMetadata::new(embedding_response, self.embedding_model.clone());

        // Format the embedding as a string for ClickHouse
        let formatted_embedding = format!(
            "[{}]",
            embedding_response_with_metadata
                .embedding
                .iter()
                .map(|&x| x.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let query = format!(
            r"SELECT input, output, cosineDistance(embedding, {}) as distance
                   FROM DynamicInContextLearningExample
                   WHERE function_name='{}' AND variant_name='{}'
                   ORDER BY distance ASC
                   LIMIT {}
                   FORMAT JSONEachRow",
            formatted_embedding, function_name, variant_name, self.k
        );

        // Run the query on the ClickHouse database to find nearest neighbors
        let result = clients
            .clickhouse_connection_info
            .run_query_synchronous_no_params(query)
            .await?;

        // Parse each line into RawExample (since we will have some serialized JSON strings inside it)
        let raw_examples: Vec<RawExample> = result
            .response
            .lines()
            .map(serde_json::from_str::<RawExample>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                Error::new(ErrorDetails::Serialization {
                    message: format!("Failed to parse raw examples: {e}"),
                })
            })?;

        // Convert RawExamples into Examples (parses those serialized JSON strings)
        let examples = parse_raw_examples(raw_examples, function)?;

        if examples.len() != self.k as usize {
            tracing::warn!(
                "Dynamic in-context learning retrieved {} examples, expected {}",
                examples.len(),
                self.k
            );
        }

        Ok((examples, embedding_response_with_metadata))
    }

    /// Serialize an example into a pair of RequestMessages
    /// The first message is a User message with the input serialized
    /// The second message is an Assistant message with the output as native output blocks
    ///   - For chat messages, this is a simple vector of ContentBlocks
    ///   - For JSON messages, this is a single JSON output block (as Text)
    fn prepare_message(example: &Example) -> Result<Vec<RequestMessage>, Error> {
        let mut messages = Vec::new();
        let input = match example {
            Example::Chat(chat_example) => chat_example.input.clone(),
            Example::Json(json_example) => json_example.input.clone(),
        };

        // Push the input as a user message
        messages.push(RequestMessage {
            role: Role::User,
            content: vec![serde_json::to_string(&input)
                .map_err(|e| {
                    Error::new(ErrorDetails::Serialization {
                        message: format!(
                            "Error in serializing Input in dynamic in-context learning variant: {e}"
                        ),
                    })
                })?
                .into()],
        });

        // Prepare the output
        let content: Vec<ContentBlock> = match example {
            Example::Chat(chat_example) => chat_example
                .output
                .clone()
                .into_iter()
                .map(ContentBlockChatOutput::into)
                .collect(),
            Example::Json(json_example) => {
                vec![json_example.output.raw.clone().unwrap_or_default().into()]
            }
        };

        // Push the output as an assistant message
        messages.push(RequestMessage {
            role: Role::Assistant,
            content,
        });
        Ok(messages)
    }

    fn prepare_input_message(input: &ResolvedInput) -> Result<RequestMessage, Error> {
        let content = vec![serde_json::to_string(&input)
            .map_err(|e| {
                Error::new(ErrorDetails::Serialization {
                    message: format!(
                        "Error in serializing Input in dynamic in-context learning variant: {e}"
                    ),
                })
            })?
            .into()];
        Ok(RequestMessage {
            role: Role::User,
            content,
        })
    }

    fn prepare_request<'a, 'request>(
        &'a self,
        input: &ResolvedInput,
        examples: &[Example],
        function: &'a FunctionConfig,
        inference_config: &'request InferenceConfig<'request>,
        stream: bool,
        inference_params: &mut InferenceParams,
    ) -> Result<ModelInferenceRequest<'request>, Error>
    where
        'a: 'request,
    {
        for message in &input.messages {
            for content in &message.content {
                match content {
                    // We cannot meaningfully embed images into dicl inputs, so reject the request.
                    ResolvedInputMessageContent::File(..) => {
                        return Err(Error::new(ErrorDetails::UnsupportedContentBlockType {
                            content_block_type: "image".to_string(),
                            provider_type: "dicl".to_string(),
                        }));
                    }
                    // 'Unknown' blocks will need special handling (we don't want the literal string "unknown")
                    // to show up in the LLM input, so reject the request for now.
                    ResolvedInputMessageContent::Unknown { .. } => {
                        return Err(Error::new(ErrorDetails::UnsupportedContentBlockType {
                            content_block_type: "unknown".to_string(),
                            provider_type: "dicl".to_string(),
                        }));
                    }
                    _ => {}
                }
            }
        }
        let messages = examples
            .iter()
            .map(Self::prepare_message)
            .collect::<Result<Vec<Vec<RequestMessage>>, _>>()?
            .into_iter()
            .flatten()
            .chain(std::iter::once(Self::prepare_input_message(input)?))
            .collect::<Vec<_>>();

        let system = Some(self.system_instructions.clone());

        inference_params
            .chat_completion
            .backfill_with_variant_params(
                self.temperature,
                self.max_tokens,
                self.seed,
                self.top_p,
                self.presence_penalty,
                self.frequency_penalty,
                self.stop_sequences.clone(),
            );
        if !inference_config.extra_body.is_empty() {
            return Err(ErrorDetails::InvalidRequest {
                message: "Inference-level `extra_body` is not yet supported for dynamic_in_content_learning variant"
                    .to_string(),
            }
            .into());
        }
        let extra_body = FullExtraBodyConfig {
            extra_body: self.extra_body.clone(),
            inference_extra_body: Default::default(),
        };
        let extra_headers = FullExtraHeadersConfig {
            variant_extra_headers: self.extra_headers.clone(),
            inference_extra_headers: inference_config
                .extra_headers
                .clone()
                .into_owned()
                .filter(inference_config.variant_name),
        };
        prepare_model_inference_request(
            messages,
            system,
            function,
            inference_config,
            stream,
            inference_params,
            self.json_mode,
            extra_body,
            extra_headers,
        )
    }
}

// Since the `input` and `output` fields in the ClickHouse table are of type String,
// we need to parse them into the appropriate types before using them and cannot rely
// on Deserialize to do it for us.
fn parse_raw_examples(
    raw_examples: Vec<RawExample>,
    function: &FunctionConfig,
) -> Result<Vec<Example>, Error> {
    let mut examples = Vec::new();
    for raw_example in raw_examples {
        // Parse the `input` string into `Input`
        let input: ResolvedInput = serde_json::from_str(&raw_example.input).map_err(|e| {
            Error::new(ErrorDetails::Serialization {
                message: format!("Failed to parse `input`: {e}"),
            })
        })?;

        for messages in &input.messages {
            for content in &messages.content {
                if let ResolvedInputMessageContent::File(_) = content {
                    return Err(Error::new(ErrorDetails::Serialization {
                        message: "Failed to deserialize raw_example - images are not supported in dynamic in-context learning".to_string(),
                    }));
                }
            }
        }

        match function {
            FunctionConfig::Chat(_) => {
                // Try parsing `output` as `Vec<ContentBlockOutput>` (for ChatExample)
                let output =
                    serde_json::from_str::<Vec<ContentBlockChatOutput>>(&raw_example.output)
                        .map_err(|e| {
                            Error::new(ErrorDetails::Serialization {
                                message: format!(
                                    "Failed to parse `output` in example `{raw_example:?}`: {e}"
                                ),
                            })
                        })?;
                examples.push(Example::Chat(ChatExample { input, output }));
            }
            FunctionConfig::Json(_) => {
                // Try parsing `output` as `JsonInferenceOutput` (for JsonExample)
                let output = serde_json::from_str::<JsonInferenceOutput>(&raw_example.output)
                    .map_err(|e| {
                        Error::new(ErrorDetails::Serialization {
                            message: format!(
                                "Failed to parse `output` in example `{raw_example:?}`: {e}"
                            ),
                        })
                    })?;
                examples.push(Example::Json(JsonExample { input, output }));
            }
        }
    }

    Ok(examples)
}

pub fn default_system_instructions() -> String {
    "You are tasked with learning by induction and then solving a problem below. You will be shown several examples of inputs followed by outputs. Then, in the same format you will be given one last set of inputs. Your job is to use the provided examples to inform your response to the last set of inputs.".to_string()
}

impl LoadableConfig<DiclConfig> for UninitializedDiclConfig {
    fn load(self) -> Result<DiclConfig, Error> {
        let system_instructions = match self.system_instructions {
            Some(path) => path.read()?,
            None => default_system_instructions(),
        };

        Ok(DiclConfig {
            weight: self.weight,
            embedding_model: Arc::from(self.embedding_model),
            k: self.k,
            model: Arc::from(self.model),
            system_instructions,
            temperature: self.temperature,
            top_p: self.top_p,
            presence_penalty: self.presence_penalty,
            frequency_penalty: self.frequency_penalty,
            max_tokens: self.max_tokens,
            seed: self.seed,
            json_mode: self.json_mode,
            retries: self.retries,
            stop_sequences: self.stop_sequences,
            extra_body: self.extra_body,
            extra_headers: self.extra_headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        function::{FunctionConfigChat, FunctionConfigJson},
        inference::types::{
            resolved_input::FileWithPath,
            storage::{StorageKind, StoragePath},
            Base64File, ResolvedInputMessage, ResolvedInputMessageContent, Role, Text,
        },
        tool::{ToolCall, ToolCallOutput},
    };
    use serde_json::json;

    #[test]
    fn test_prepare_message() {
        // ---------- Test with ChatExample ----------

        // Mock Input data
        let input_data = ResolvedInput {
            system: Some(json!({"type": "system", "content": "System message"})),
            messages: vec![
                ResolvedInputMessage {
                    role: Role::User,
                    content: vec![ResolvedInputMessageContent::Text {
                        value: json!("Hello, assistant!"),
                    }],
                },
                ResolvedInputMessage {
                    role: Role::Assistant,
                    content: vec![ResolvedInputMessageContent::Text {
                        value: json!("Hello, user!"),
                    }],
                },
            ],
        };

        // Mock Output data for ChatExample
        let chat_output = vec![
            ContentBlockChatOutput::Text(Text {
                text: "This is a test response.".to_string(),
            }),
            ContentBlockChatOutput::ToolCall(ToolCallOutput {
                id: "tool_call_1".to_string(),
                raw_name: "search_tool".to_string(),
                raw_arguments: "{\"query\": \"rust programming\"}".to_string(),
                name: Some("search_tool".to_string()),
                arguments: Some(json!({"query": "rust programming"})),
            }),
        ];

        let chat_example = Example::Chat(ChatExample {
            input: input_data.clone(),
            output: chat_output.clone(),
        });

        let chat_messages = DiclConfig::prepare_message(&chat_example).unwrap();

        assert_eq!(chat_messages.len(), 2);

        // First message should be from User with serialized input
        let serialized_input = serde_json::to_string(&input_data).unwrap();
        assert_eq!(chat_messages[0].role, Role::User);
        assert_eq!(
            chat_messages[0].content,
            vec![ContentBlock::Text(Text {
                text: serialized_input.clone()
            })]
        );

        // Second message should be from Assistant with content blocks
        let expected_content: Vec<ContentBlock> = chat_output
            .into_iter()
            .map(ContentBlockChatOutput::into)
            .collect();

        assert_eq!(chat_messages[1].role, Role::Assistant);
        assert_eq!(chat_messages[1].content, expected_content);

        // ---------- Test with JsonExample ----------

        // Mock Output data for JsonExample
        let json_output = JsonInferenceOutput {
            raw: Some("{\"result\": \"success\"}".to_string()),
            parsed: Some(json!({"result": "success"})),
        };

        let json_example = Example::Json(JsonExample {
            input: input_data.clone(),
            output: json_output.clone(),
        });

        let json_messages = DiclConfig::prepare_message(&json_example).unwrap();

        // Assertions for JsonExample
        assert_eq!(json_messages.len(), 2);

        // First message should be from User with serialized input
        assert_eq!(json_messages[0].role, Role::User);
        assert_eq!(
            json_messages[0].content,
            vec![ContentBlock::Text(Text {
                text: serialized_input
            })]
        );

        // Second message should be from Assistant with raw JSON output as text
        let expected_content = vec![ContentBlock::Text(Text {
            text: json_output.raw.unwrap().clone(),
        })];

        assert_eq!(json_messages[1].role, Role::Assistant);
        assert_eq!(json_messages[1].content, expected_content);
    }

    #[test]
    fn test_prepare_input_message() {
        // Mock Input data
        let input_data = ResolvedInput {
            system: Some(json!({"assistant_name": "Dr. Mehta"})),
            messages: vec![
                ResolvedInputMessage {
                    role: Role::User,
                    content: vec![
                        ResolvedInputMessageContent::Text {
                            value: json!("Hello, assistant!"),
                        },
                        ResolvedInputMessageContent::ToolCall(ToolCall {
                            id: "tool_call_1".to_string(),
                            name: "search_tool".to_string(),
                            arguments: "{\"query\": \"rust programming\"}".to_string(),
                        }),
                    ],
                },
                ResolvedInputMessage {
                    role: Role::Assistant,
                    content: vec![ResolvedInputMessageContent::Text {
                        value: json!("Here are the search results for rust programming."),
                    }],
                },
            ],
        };

        // Call the prepare_input_message function
        let request_message = DiclConfig::prepare_input_message(&input_data).unwrap();

        // The role should be User
        assert_eq!(request_message.role, Role::User);

        // The content should contain the serialized Input as a Text ContentBlock
        let expected_serialized_input = serde_json::to_string(&input_data).unwrap();
        let expected_content = vec![ContentBlock::Text(Text {
            text: expected_serialized_input.clone(),
        })];
        assert_eq!(request_message.content, expected_content);
    }

    #[test]
    fn test_reject_image_example() {
        // Define sample raw examples with serialized Input and Output
        let raw_examples = vec![
            RawExample {
                input: serde_json::to_string(&ResolvedInput {
                    system: Some(json!({"assistant_name": "Dr. Mehta"})),
                    messages: vec![ResolvedInputMessage {
                        role: Role::User,
                        content: vec![ResolvedInputMessageContent::Text {
                            value: json!("What is the boiling point of water?"),
                        }],
                    }],
                })
                .unwrap(),
                output: serde_json::to_string(&vec![ContentBlockChatOutput::Text(Text {
                    text: "100 degrees Celsius".to_string(),
                })])
                .unwrap(),
            },
            RawExample {
                input: serde_json::to_string(&ResolvedInput {
                    system: Some(json!({"assistant_name": "Pinocchio"})),
                    messages: vec![ResolvedInputMessage {
                        role: Role::User,
                        content: vec![
                            ResolvedInputMessageContent::Text {
                                value: json!("What is the name of the capital city of Japan?"),
                            },
                            ResolvedInputMessageContent::File(Box::new(FileWithPath {
                                file: Base64File {
                                    url: None,
                                    mime_type: mime::IMAGE_PNG,
                                    data: Some("ABC".to_string()),
                                },
                                storage_path: StoragePath {
                                    kind: StorageKind::Disabled,
                                    path: Default::default(),
                                },
                            })),
                        ],
                    }],
                })
                .unwrap(),
                output: serde_json::to_string(&vec![ContentBlockChatOutput::Text(Text {
                    text: "Osaka (nose grows 4 inches)".to_string(),
                })])
                .unwrap(),
            },
        ];

        let function = FunctionConfig::Chat(FunctionConfigChat {
            ..Default::default()
        });
        // Parse the raw examples
        let err = parse_raw_examples(raw_examples.clone(), &function)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("images are not supported in dynamic in-context learning"),
            "Unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_raw_examples() {
        // Define sample raw examples with serialized Input and Output
        let raw_examples = vec![
            RawExample {
                input: serde_json::to_string(&ResolvedInput {
                    system: Some(json!({"assistant_name": "Dr. Mehta"})),
                    messages: vec![ResolvedInputMessage {
                        role: Role::User,
                        content: vec![ResolvedInputMessageContent::Text {
                            value: json!("What is the boiling point of water?"),
                        }],
                    }],
                })
                .unwrap(),
                output: serde_json::to_string(&vec![ContentBlockChatOutput::Text(Text {
                    text: "100 degrees Celsius".to_string(),
                })])
                .unwrap(),
            },
            RawExample {
                input: serde_json::to_string(&ResolvedInput {
                    system: Some(json!({"assistant_name": "Pinocchio"})),
                    messages: vec![ResolvedInputMessage {
                        role: Role::User,
                        content: vec![ResolvedInputMessageContent::Text {
                            value: json!("What is the name of the capital city of Japan?"),
                        }],
                    }],
                })
                .unwrap(),
                output: serde_json::to_string(&vec![ContentBlockChatOutput::Text(Text {
                    text: "Osaka (nose grows 4 inches)".to_string(),
                })])
                .unwrap(),
            },
        ];

        let function = FunctionConfig::Chat(FunctionConfigChat {
            ..Default::default()
        });
        // Parse the raw examples
        let parsed_examples = parse_raw_examples(raw_examples.clone(), &function)
            .expect("Failed to parse raw examples");

        // Define the expected examples
        let expected_examples = vec![
            Example::Chat(ChatExample {
                input: serde_json::from_str(&raw_examples[0].input).unwrap(),
                output: serde_json::from_str(&raw_examples[0].output).unwrap(),
            }),
            Example::Chat(ChatExample {
                input: serde_json::from_str(&raw_examples[1].input).unwrap(),
                output: serde_json::from_str(&raw_examples[1].output).unwrap(),
            }),
        ];

        // Assert that the parsed examples match the expected examples
        assert_eq!(parsed_examples, expected_examples);

        // Test that we can parse a JSON example too
        let json_raw_examples = vec![
            RawExample {
                input: serde_json::to_string(&ResolvedInput {
                    system: Some(json!({"assistant_name": "JsonTester"})),
                    messages: vec![ResolvedInputMessage {
                        role: Role::User,
                        content: vec![ResolvedInputMessageContent::Text {
                            value: json!("Provide a sample JSON response."),
                        }],
                    }],
                })
                .unwrap(),
                output: serde_json::to_string(&JsonInferenceOutput {
                    raw: Some("{\"status\": \"success\", \"data\": {\"id\": 1}}".to_string()),
                    parsed: Some(json!({
                        "status": "success",
                        "data": {
                            "id": 1
                        }
                    })),
                })
                .unwrap(),
            },
            RawExample {
                input: serde_json::to_string(&ResolvedInput {
                    system: Some(json!({"assistant_name": "JsonTester"})),
                    messages: vec![ResolvedInputMessage {
                        role: Role::User,
                        content: vec![ResolvedInputMessageContent::Text {
                            value: json!("Provide another JSON response."),
                        }],
                    }],
                })
                .unwrap(),
                output: serde_json::to_string(&JsonInferenceOutput {
                    raw: Some("{\"result\": [1, 2, 3], \"status\": \"ok\"}".to_string()),
                    parsed: Some(json!({
                        "result": [1, 2, 3],
                        "status": "ok"
                    })),
                })
                .unwrap(),
            },
        ];
        let json_function = FunctionConfig::Json(FunctionConfigJson {
            ..Default::default()
        });

        // Parse the JSON raw examples
        let parsed_json_examples = parse_raw_examples(json_raw_examples.clone(), &json_function)
            .expect("Failed to parse JSON raw examples");

        // Assert that all parsed JSON examples have 'parsed' as Some
        for example in parsed_json_examples {
            if let Example::Json(json_example) = example {
                assert!(json_example.output.parsed.is_some(), "Parsed field is None");
            } else {
                panic!("Expected JsonExample");
            }
        }
    }
}
