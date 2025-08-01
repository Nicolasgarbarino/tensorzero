use aws_sdk_bedrockruntime::operation::converse::ConverseOutput;
use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamOutput;
use aws_sdk_bedrockruntime::types::{
    AnyToolChoice, AutoToolChoice, ContentBlock as BedrockContentBlock, ContentBlockDelta,
    ContentBlockStart, ConversationRole, ConverseOutput as ConverseOutputType,
    ConverseStreamOutput as ConverseStreamOutputType, DocumentBlock, DocumentFormat,
    DocumentSource, ImageBlock, ImageFormat, ImageSource, InferenceConfiguration, Message,
    SpecificToolChoice, StopReason, SystemContentBlock, Tool, ToolChoice as AWSBedrockToolChoice,
    ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock, ToolSpecification,
    ToolUseBlock,
};
use aws_smithy_types::error::display::DisplayErrorContext;
use aws_types::region::Region;
use futures::StreamExt;
use itertools::Itertools;
use reqwest::StatusCode;
use serde::Serialize;
use std::time::Duration;
use tokio::time::Instant;

use super::anthropic::{prefill_json_chunk_response, prefill_json_response};
use super::aws_common::{self, build_interceptor, InterceptorAndRawBody};
use super::helpers::peek_first_chunk;
use crate::cache::ModelProviderRequest;
use crate::endpoints::inference::InferenceCredentials;
use crate::error::{warn_discarded_thought_block, DisplayOrDebugGateway, Error, ErrorDetails};
use crate::inference::types::batch::BatchRequestRow;
use crate::inference::types::batch::PollBatchInferenceResponse;
use crate::inference::types::file::mime_type_to_ext;
use crate::inference::types::resolved_input::FileWithPath;
use crate::inference::types::{
    batch::StartBatchProviderInferenceResponse, ContentBlock, ContentBlockChunk,
    ContentBlockOutput, FunctionType, Latency, ModelInferenceRequest,
    ModelInferenceRequestJsonMode, PeekableProviderInferenceResponseStream,
    ProviderInferenceResponse, ProviderInferenceResponseChunk,
    ProviderInferenceResponseStreamInner, RequestMessage, Role, Text, TextChunk, Usage,
};
use crate::inference::types::{FinishReason, ProviderInferenceResponseArgs};
use crate::inference::InferenceProvider;
use crate::model::ModelProvider;
use crate::tool::{ToolCall, ToolCallChunk, ToolChoice, ToolConfig};

#[expect(unused)]
const PROVIDER_NAME: &str = "AWS Bedrock";
pub const PROVIDER_TYPE: &str = "aws_bedrock";

// NB: If you add `Clone` someday, you'll need to wrap client in Arc
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct AWSBedrockProvider {
    model_id: String,
    #[serde(skip)]
    client: aws_sdk_bedrockruntime::Client,
    #[serde(skip)]
    base_config: aws_sdk_bedrockruntime::config::Builder,
}

impl AWSBedrockProvider {
    pub async fn new(model_id: String, region: Option<Region>) -> Result<Self, Error> {
        let config = aws_common::config_with_region(PROVIDER_TYPE, region).await?;
        let client = aws_sdk_bedrockruntime::Client::new(&config);

        Ok(Self {
            model_id,
            client,
            base_config: aws_sdk_bedrockruntime::config::Builder::from(&config),
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

impl InferenceProvider for AWSBedrockProvider {
    async fn infer<'a>(
        &'a self,
        ModelProviderRequest {
            request,
            provider_name: _,
            model_name,
        }: ModelProviderRequest<'a>,
        http_client: &'a reqwest::Client,
        _dynamic_api_keys: &'a InferenceCredentials,
        model_provider: &'a ModelProvider,
    ) -> Result<ProviderInferenceResponse, Error> {
        // TODO (#55): add support for guardrails and additional fields

        let mut messages: Vec<Message> = request
            .messages
            .iter()
            .map(Message::try_from)
            .filter_ok(|m| !m.content.is_empty())
            .collect::<Result<Vec<_>, _>>()?;

        if self.model_id.contains("claude")
            && request.function_type == FunctionType::Json
            && matches!(
                request.json_mode,
                ModelInferenceRequestJsonMode::On | ModelInferenceRequestJsonMode::Strict
            )
        {
            prefill_json_message(&mut messages)?;
        }

        let mut inference_config = InferenceConfiguration::builder();
        // TODO (#55): add support for top_p, stop_sequences, etc.
        if let Some(max_tokens) = request.max_tokens {
            inference_config = inference_config.max_tokens(max_tokens as i32);
        }
        if let Some(temperature) = request.temperature {
            inference_config = inference_config.temperature(temperature);
        }
        if let Some(top_p) = request.top_p {
            inference_config = inference_config.top_p(top_p);
        }
        if let Some(stop_sequences) = request.stop_sequences.to_owned() {
            inference_config =
                inference_config.set_stop_sequences(Some(stop_sequences.into_owned()));
        }
        let mut bedrock_request = self
            .client
            .converse()
            .model_id(&self.model_id)
            .set_messages(Some(messages))
            .inference_config(inference_config.build());

        if let Some(system) = &request.system {
            // AWS Bedrock does not support system message "" so we remove it
            if !system.is_empty() {
                let system_block = SystemContentBlock::Text(system.clone());
                bedrock_request = bedrock_request.system(system_block);
            }
        }

        if let Some(tool_config) = &request.tool_config {
            if !matches!(tool_config.tool_choice, ToolChoice::None) {
                let tools: Vec<Tool> = tool_config
                    .tools_available
                    .iter()
                    .map(Tool::try_from)
                    .collect::<Result<Vec<_>, _>>()?;

                let tool_choice: AWSBedrockToolChoice =
                    tool_config.tool_choice.clone().try_into()?;

                let aws_bedrock_tool_config = ToolConfiguration::builder()
                    .set_tools(Some(tools))
                    .tool_choice(tool_choice)
                    .build()
                    .map_err(|e| {
                        Error::new(ErrorDetails::InferenceClient {
                            raw_request: None,
                            raw_response: None,
                            status_code: Some(StatusCode::INTERNAL_SERVER_ERROR),
                            message: format!(
                                "Error configuring AWS Bedrock tool config: {}",
                                DisplayOrDebugGateway::new(e)
                            ),
                            provider_type: PROVIDER_TYPE.to_string(),
                        })
                    })?;

                bedrock_request = bedrock_request.tool_config(aws_bedrock_tool_config);
            }
        }

        let InterceptorAndRawBody {
            interceptor,
            get_raw_request,
        } = build_interceptor(request, model_provider, model_name.to_string());

        let new_config = self
            .base_config
            .clone()
            .http_client(super::aws_http_client::Client::new(http_client.clone()));
        let start_time = Instant::now();
        let output = bedrock_request
            .customize()
            .config_override(new_config)
            .interceptor(interceptor)
            .send()
            .await
            .map_err(|e| {
                Error::new(ErrorDetails::InferenceServer {
                    message: format!(
                        "Error sending request to AWS Bedrock: {:?}",
                        DisplayErrorContext(&e)
                    ),
                    raw_request: None,
                    raw_response: None,
                    provider_type: PROVIDER_TYPE.to_string(),
                })
            })?;

        let latency = Latency::NonStreaming {
            response_time: start_time.elapsed(),
        };

        let raw_request = get_raw_request()?;

        ConverseOutputWithMetadata {
            output,
            latency,
            raw_request,
            system: request.system.clone(),
            input_messages: request.messages.clone(),
            model_id: &self.model_id,
            function_type: &request.function_type,
            json_mode: &request.json_mode,
        }
        .try_into()
    }

    async fn infer_stream<'a>(
        &'a self,
        ModelProviderRequest {
            request,
            provider_name: _,
            model_name,
        }: ModelProviderRequest<'a>,
        http_client: &'a reqwest::Client,
        _dynamic_api_keys: &'a InferenceCredentials,
        model_provider: &'a ModelProvider,
    ) -> Result<(PeekableProviderInferenceResponseStream, String), Error> {
        // TODO (#55): add support for guardrails and additional fields

        let mut messages: Vec<Message> = request
            .messages
            .iter()
            .map(Message::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        if self.model_id.contains("claude")
            && request.function_type == FunctionType::Json
            && matches!(
                request.json_mode,
                ModelInferenceRequestJsonMode::On | ModelInferenceRequestJsonMode::Strict
            )
        {
            prefill_json_message(&mut messages)?;
        }

        let mut inference_config = InferenceConfiguration::builder();
        // TODO (#55): add support for top_p, stop_sequences, etc.
        if let Some(max_tokens) = request.max_tokens {
            inference_config = inference_config.max_tokens(max_tokens as i32);
        }
        if let Some(temperature) = request.temperature {
            inference_config = inference_config.temperature(temperature);
        }
        if let Some(top_p) = request.top_p {
            inference_config = inference_config.top_p(top_p);
        }
        if let Some(stop_sequences) = request.stop_sequences.to_owned() {
            inference_config =
                inference_config.set_stop_sequences(Some(stop_sequences.into_owned()));
        }

        let mut bedrock_request = self
            .client
            .converse_stream()
            .model_id(&self.model_id)
            .set_messages(Some(messages))
            .inference_config(inference_config.build());

        if let Some(system) = &request.system {
            // AWS Bedrock does not support system message "" so we remove it
            if !system.is_empty() {
                let system_block = SystemContentBlock::Text(system.clone());
                bedrock_request = bedrock_request.system(system_block);
            }
        }

        if let Some(tool_config) = &request.tool_config {
            if !matches!(tool_config.tool_choice, ToolChoice::None) {
                let tools: Vec<Tool> = tool_config
                    .tools_available
                    .iter()
                    .map(Tool::try_from)
                    .collect::<Result<Vec<_>, _>>()?;

                let tool_choice: AWSBedrockToolChoice =
                    tool_config.tool_choice.clone().try_into()?;

                let aws_bedrock_tool_config = ToolConfiguration::builder()
                    .set_tools(Some(tools))
                    .tool_choice(tool_choice)
                    .build()
                    .map_err(|e| {
                        Error::new(ErrorDetails::InferenceClient {
                            raw_request: None,
                            raw_response: None,
                            status_code: Some(StatusCode::INTERNAL_SERVER_ERROR),
                            message: format!(
                                "Error configuring AWS Bedrock tool config: {}",
                                DisplayOrDebugGateway::new(e)
                            ),
                            provider_type: PROVIDER_TYPE.to_string(),
                        })
                    })?;

                bedrock_request = bedrock_request.tool_config(aws_bedrock_tool_config);
            }
        }
        let InterceptorAndRawBody {
            interceptor,
            get_raw_request,
        } = build_interceptor(request, model_provider, model_name.to_string());
        let new_config = self
            .base_config
            .clone()
            .http_client(super::aws_http_client::Client::new(http_client.clone()));

        let start_time = Instant::now();
        let stream = bedrock_request
            .customize()
            .config_override(new_config)
            .interceptor(interceptor)
            .send()
            .await
            .map_err(|e| {
                Error::new(ErrorDetails::InferenceServer {
                    message: format!(
                        "Error sending request to AWS Bedrock: {}",
                        DisplayErrorContext(&e)
                    ),
                    raw_request: None,
                    raw_response: None,
                    provider_type: PROVIDER_TYPE.to_string(),
                })
            })?;

        let raw_request = get_raw_request()?;

        let mut stream = stream_bedrock(stream, start_time).peekable();
        let chunk = peek_first_chunk(&mut stream, &raw_request, PROVIDER_TYPE).await?;
        if self.model_id.contains("claude")
            && matches!(
                request.json_mode,
                ModelInferenceRequestJsonMode::On | ModelInferenceRequestJsonMode::Strict
            )
            && matches!(request.function_type, FunctionType::Json)
        {
            prefill_json_chunk_response(chunk);
        }

        Ok((stream, raw_request))
    }

    async fn start_batch_inference<'a>(
        &'a self,
        _requests: &'a [ModelInferenceRequest<'_>],
        _client: &'a reqwest::Client,
        _dynamic_api_keys: &'a InferenceCredentials,
    ) -> Result<StartBatchProviderInferenceResponse, Error> {
        Err(ErrorDetails::UnsupportedModelProviderForBatchInference {
            provider_type: "AWS Bedrock".to_string(),
        }
        .into())
    }

    async fn poll_batch_inference<'a>(
        &'a self,
        _batch_request: &'a BatchRequestRow<'a>,
        _http_client: &'a reqwest::Client,
        _dynamic_api_keys: &'a InferenceCredentials,
    ) -> Result<PollBatchInferenceResponse, Error> {
        Err(ErrorDetails::UnsupportedModelProviderForBatchInference {
            provider_type: PROVIDER_TYPE.to_string(),
        }
        .into())
    }
}

fn stream_bedrock(
    mut stream: ConverseStreamOutput,
    start_time: Instant,
) -> ProviderInferenceResponseStreamInner {
    Box::pin(async_stream::stream! {
        let mut current_tool_id : Option<String> = None;

        loop {
            let ev = stream.stream.recv().await;

            match ev {
                Err(e) => {
                    yield Err(ErrorDetails::InferenceServer {
                        raw_request: None,
                        raw_response: None,
                        message: e.to_string(),
                        provider_type: PROVIDER_TYPE.to_string(),

                    }.into());
                }
                Ok(ev) => match ev {
                    None => break,
                    Some(output) => {
                        // NOTE: AWS Bedrock returns usage (ConverseStreamMetadataEvent) AFTER MessageStop.

                        // Convert the event to a tensorzero stream message
                        let stream_message = bedrock_to_tensorzero_stream_message(output, start_time.elapsed(), &mut current_tool_id);

                        match stream_message {
                            Ok(None) => {},
                            Ok(Some(stream_message)) => yield Ok(stream_message),
                            Err(e) => yield Err(e),
                        }
                    }
                }
            }
        }
    })
}

fn bedrock_to_tensorzero_stream_message(
    output: ConverseStreamOutputType,
    message_latency: Duration,
    current_tool_id: &mut Option<String>,
) -> Result<Option<ProviderInferenceResponseChunk>, Error> {
    match output {
        ConverseStreamOutputType::ContentBlockDelta(message) => {
            let raw_message = serialize_aws_bedrock_struct(&message)?;

            match message.delta {
                Some(delta) => match delta {
                    ContentBlockDelta::Text(text) => Ok(Some(ProviderInferenceResponseChunk::new(
                        vec![ContentBlockChunk::Text(TextChunk {
                            text,
                            id: message.content_block_index.to_string(),
                        })],
                        None,
                        raw_message,
                        message_latency,
                        None,
                    ))),
                    ContentBlockDelta::ToolUse(tool_use) => {
                        Ok(Some(ProviderInferenceResponseChunk::new(
                            // Take the current tool name and ID and use them to create a ToolCallChunk
                            // This is necessary because the ToolCallChunk must always contain the tool name and ID
                            // even though AWS Bedrock only sends the tool ID and name in the ToolUse chunk and not InputJSONDelta
                            vec![ContentBlockChunk::ToolCall(ToolCallChunk {
                                raw_name: None,
                                id: current_tool_id.clone().ok_or_else(|| Error::new(ErrorDetails::InferenceServer {
                                    message: "Got InputJsonDelta chunk from AWS Bedrock without current tool id being set by a ToolUse".to_string(),
                                    provider_type: PROVIDER_TYPE.to_string(),
                                    raw_request: None,
                                    raw_response: None,
                                }))?,
                                raw_arguments: tool_use.input,
                            })],
                            None,
                            raw_message,
                            message_latency,
                            None,
                        )))
                    }
                    _ => Err(ErrorDetails::InferenceServer {
                        raw_request: None,
                        raw_response: None,
                        message: "Unsupported content block delta type for AWS Bedrock".to_string(),
                        provider_type: PROVIDER_TYPE.to_string(),
                    }
                    .into()),
                },
                None => Ok(None),
            }
        }
        ConverseStreamOutputType::ContentBlockStart(message) => {
            let raw_message = serialize_aws_bedrock_struct(&message)?;

            match message.start {
                None => Ok(None),
                Some(ContentBlockStart::ToolUse(tool_use)) => {
                    // This is a new tool call, update the ID for future chunks
                    *current_tool_id = Some(tool_use.tool_use_id.clone());
                    Ok(Some(ProviderInferenceResponseChunk::new(
                        vec![ContentBlockChunk::ToolCall(ToolCallChunk {
                            id: tool_use.tool_use_id,
                            raw_name: Some(tool_use.name),
                            raw_arguments: String::new(),
                        })],
                        None,
                        raw_message,
                        message_latency,
                        None,
                    )))
                }
                _ => Err(ErrorDetails::InferenceServer {
                    raw_request: None,
                    raw_response: None,
                    message: "Unsupported content block start type for AWS Bedrock".to_string(),
                    provider_type: PROVIDER_TYPE.to_string(),
                }
                .into()),
            }
        }
        ConverseStreamOutputType::ContentBlockStop(_) => Ok(None),
        ConverseStreamOutputType::MessageStart(_) => Ok(None),
        ConverseStreamOutputType::MessageStop(message_stop) => {
            let raw_message = serialize_aws_bedrock_struct(&message_stop)?;
            Ok(Some(ProviderInferenceResponseChunk::new(
                vec![],
                None,
                raw_message,
                message_latency,
                aws_stop_reason_to_tensorzero_finish_reason(message_stop.stop_reason),
            )))
        }
        ConverseStreamOutputType::Metadata(message) => {
            let raw_message = serialize_aws_bedrock_struct(&message)?;

            // Note: There are other types of metadata (e.g. traces) but for now we're only interested in usage

            match message.usage {
                None => Ok(None),
                Some(usage) => {
                    let usage = Some(Usage {
                        input_tokens: usage.input_tokens as u32,
                        output_tokens: usage.output_tokens as u32,
                    });

                    Ok(Some(ProviderInferenceResponseChunk::new(
                        vec![],
                        usage,
                        raw_message,
                        message_latency,
                        None,
                    )))
                }
            }
        }
        _ => Err(ErrorDetails::InferenceServer {
            raw_request: None,
            raw_response: None,
            message: "Unknown event type from AWS Bedrock".to_string(),
            provider_type: PROVIDER_TYPE.to_string(),
        }
        .into()),
    }
}

impl From<Role> for ConversationRole {
    fn from(role: Role) -> Self {
        match role {
            Role::User => ConversationRole::User,
            Role::Assistant => ConversationRole::Assistant,
        }
    }
}

/// Prefill messages for AWS Bedrock when conditions are met
fn prefill_json_message(messages: &mut Vec<Message>) -> Result<(), Error> {
    // Add a JSON-prefill message for AWS Bedrock's JSON mode
    messages.push(Message::try_from(&RequestMessage {
        role: Role::Assistant,
        content: vec![ContentBlock::Text(Text {
            text: "Here is the JSON requested:\n{".to_string(),
        })],
    })?);
    Ok(())
}

impl TryFrom<&ContentBlock> for Option<BedrockContentBlock> {
    type Error = Error;

    fn try_from(block: &ContentBlock) -> Result<Self, Self::Error> {
        match block {
            ContentBlock::Text(Text { text }) => Ok(Some(BedrockContentBlock::Text(text.clone()))),
            ContentBlock::ToolCall(tool_call) => {
                // Convert the tool call arguments from String to JSON Value...
                let input = serde_json::from_str(&tool_call.arguments).map_err(|e| {
                    Error::new(ErrorDetails::InferenceClient {
                        raw_request: None,
                        raw_response: Some(tool_call.arguments.clone()),
                        status_code: Some(StatusCode::BAD_REQUEST),
                        message: format!(
                            "Error parsing tool call arguments as JSON Value: {}",
                            DisplayOrDebugGateway::new(e)
                        ),
                        provider_type: PROVIDER_TYPE.to_string(),
                    })
                })?;

                // ...then convert the JSON Value to an AWS SDK Document
                let input = serde_json::from_value(input).map_err(|e| {
                    Error::new(ErrorDetails::InferenceServer {
                        raw_request: None,
                        raw_response: None,
                        message: format!(
                            "Error converting tool call arguments to AWS SDK Document: {e}"
                        ),
                        provider_type: PROVIDER_TYPE.to_string(),
                    })
                })?;

                let tool_use_block = ToolUseBlock::builder()
                    .name(tool_call.name.clone())
                    .input(input)
                    .tool_use_id(tool_call.id.clone())
                    .build()
                    .map_err(|_| {
                        Error::new(ErrorDetails::InferenceClient {
                            raw_request: None,
                            raw_response: None,
                            status_code: Some(StatusCode::BAD_REQUEST),
                            message: "Error serializing tool call block".to_string(),
                            provider_type: PROVIDER_TYPE.to_string(),
                        })
                    })?;

                Ok(Some(BedrockContentBlock::ToolUse(tool_use_block)))
            }
            ContentBlock::ToolResult(tool_result) => {
                let tool_result_block = ToolResultBlock::builder()
                    .tool_use_id(tool_result.id.clone())
                    .content(ToolResultContentBlock::Text(tool_result.result.clone()))
                    // NOTE: The AWS Bedrock SDK doesn't include `name` in the ToolResultBlock
                    .build()
                    .map_err(|_| {
                        Error::new(ErrorDetails::InferenceClient {
                            raw_request: None,
                            raw_response: None,
                            status_code: Some(StatusCode::BAD_REQUEST),
                            message: "Error serializing tool result block".to_string(),
                            provider_type: PROVIDER_TYPE.to_string(),
                        })
                    })?;

                Ok(Some(BedrockContentBlock::ToolResult(tool_result_block)))
            }
            ContentBlock::File(file) => {
                let FileWithPath {
                    file,
                    storage_path: _,
                } = &**file;
                let file_bytes = aws_smithy_types::base64::decode(file.data()?).map_err(|e| {
                    Error::new(ErrorDetails::InferenceClient {
                        raw_request: None,
                        raw_response: None,
                        status_code: Some(StatusCode::BAD_REQUEST),
                        message: format!("File was not valid base64: {e:?}"),
                        provider_type: PROVIDER_TYPE.to_string(),
                    })
                })?;
                if file.mime_type.type_() == mime::IMAGE {
                    let image_block = ImageBlock::builder()
                        .format(ImageFormat::from(file.mime_type.subtype()))
                        .source(ImageSource::Bytes(file_bytes.into()))
                        .build()
                        .map_err(|e| {
                            Error::new(ErrorDetails::InferenceClient {
                                raw_request: None,
                                raw_response: None,
                                status_code: Some(StatusCode::BAD_REQUEST),
                                message: format!("Error serializing image block: {e:?}"),
                                provider_type: PROVIDER_TYPE.to_string(),
                            })
                        })?;
                    Ok(Some(BedrockContentBlock::Image(image_block)))
                } else {
                    // Best-effort attempt to produce an AWS DocumentFormat, as their API doesn't support mime types
                    let suffix = mime_type_to_ext(&file.mime_type)?.ok_or_else(|| {
                        Error::new(ErrorDetails::InvalidMessage {
                            message: format!("Mime type {} has no filetype suffix", file.mime_type),
                        })
                    })?;
                    let document_format = DocumentFormat::from(suffix);
                    let document = DocumentBlock::builder()
                        .format(document_format)
                        // TODO: Should we allow the user to specify the file name?
                        .name("input")
                        .source(DocumentSource::Bytes(file_bytes.into()))
                        .build()
                        .map_err(|e| {
                            Error::new(ErrorDetails::InferenceClient {
                                raw_request: None,
                                raw_response: None,
                                status_code: Some(StatusCode::BAD_REQUEST),
                                message: format!("Error serializing document block: {e:?}"),
                                provider_type: PROVIDER_TYPE.to_string(),
                            })
                        })?;
                    Ok(Some(BedrockContentBlock::Document(document)))
                }
            }
            // We don't support thought blocks being passed in from a request.
            // These are only possible to be passed in in the scenario where the
            // output of a chat completion is used as an input to another model inference,
            // i.e. a judge or something.
            // We don't think the thoughts should be passed in in this case.
            ContentBlock::Thought(thought) => {
                warn_discarded_thought_block(PROVIDER_TYPE, thought);
                Ok(None)
            }
            ContentBlock::Unknown {
                data: _,
                model_provider_name: _,
            } => Err(Error::new(ErrorDetails::UnsupportedContentBlockType {
                content_block_type: "unknown".to_string(),
                provider_type: PROVIDER_TYPE.to_string(),
            })),
        }
    }
}

impl TryFrom<BedrockContentBlock> for ContentBlockOutput {
    type Error = Error;

    fn try_from(block: BedrockContentBlock) -> Result<Self, Self::Error> {
        match block {
            BedrockContentBlock::Text(text) => Ok(text.into()),
            BedrockContentBlock::ToolUse(tool_use) => {
                let arguments = serde_json::to_string(&tool_use.input).map_err(|e| {
                    Error::new(ErrorDetails::InferenceServer {
                        raw_request: None,
                        raw_response: None,
                        message: format!(
                            "Error parsing tool call arguments from AWS Bedrock: {}",
                            DisplayOrDebugGateway::new(e)
                        ),
                        provider_type: PROVIDER_TYPE.to_string(),
                    })
                })?;

                Ok(ContentBlockOutput::ToolCall(ToolCall {
                    name: tool_use.name,
                    arguments,
                    id: tool_use.tool_use_id,
                }))
            }
            _ => Err(Error::new(ErrorDetails::TypeConversion {
                message: format!(
                    "Unsupported content block type for AWS Bedrock: {}",
                    std::any::type_name_of_val(&block)
                ),
            })),
        }
    }
}

impl TryFrom<&RequestMessage> for Message {
    type Error = Error;

    fn try_from(inference_message: &RequestMessage) -> Result<Message, Error> {
        let role: ConversationRole = inference_message.role.into();
        let content: Vec<BedrockContentBlock> = inference_message
            .content
            .iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<Option<BedrockContentBlock>>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        let mut message_builder = Message::builder().role(role).set_content(Some(vec![]));
        for block in content {
            message_builder = message_builder.content(block);
        }
        let message = message_builder.build().map_err(|e| {
            Error::new(ErrorDetails::InvalidMessage {
                message: e.to_string(),
            })
        })?;

        Ok(message)
    }
}

#[derive(Debug, PartialEq)]
struct ConverseOutputWithMetadata<'a> {
    output: ConverseOutput,
    latency: Latency,
    raw_request: String,
    system: Option<String>,
    input_messages: Vec<RequestMessage>,
    model_id: &'a str,
    function_type: &'a FunctionType,
    json_mode: &'a ModelInferenceRequestJsonMode,
}

#[expect(clippy::unnecessary_wraps)]
fn aws_stop_reason_to_tensorzero_finish_reason(stop_reason: StopReason) -> Option<FinishReason> {
    match stop_reason {
        StopReason::ContentFiltered => Some(FinishReason::ContentFilter),
        StopReason::EndTurn => Some(FinishReason::Stop),
        StopReason::GuardrailIntervened => Some(FinishReason::ContentFilter),
        StopReason::MaxTokens => Some(FinishReason::Length),
        StopReason::StopSequence => Some(FinishReason::StopSequence),
        StopReason::ToolUse => Some(FinishReason::ToolCall),
        _ => Some(FinishReason::Unknown),
    }
}

impl TryFrom<ConverseOutputWithMetadata<'_>> for ProviderInferenceResponse {
    type Error = Error;

    fn try_from(value: ConverseOutputWithMetadata) -> Result<Self, Self::Error> {
        let ConverseOutputWithMetadata {
            output,
            latency,
            raw_request,
            system,
            input_messages,
            model_id,
            function_type,
            json_mode,
        } = value;

        let raw_response = serialize_aws_bedrock_struct(&output)?;

        let message = match output.output {
            Some(ConverseOutputType::Message(message)) => Some(message),
            _ => {
                return Err(ErrorDetails::InferenceServer {
                    raw_request: None,
                    raw_response: Some(raw_response.clone()),
                    message: "AWS Bedrock returned an unknown output type.".to_string(),
                    provider_type: PROVIDER_TYPE.to_string(),
                }
                .into());
            }
        };

        let mut content: Vec<ContentBlockOutput> = message
            .ok_or_else(|| {
                Error::new(ErrorDetails::InferenceServer {
                    raw_request: None,
                    raw_response: Some(raw_response.clone()),
                    message: "AWS Bedrock returned an empty message.".to_string(),
                    provider_type: PROVIDER_TYPE.to_string(),
                })
            })?
            .content
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<ContentBlockOutput>, _>>()?;

        if model_id.contains("claude")
            && *function_type == FunctionType::Json
            && (*json_mode == ModelInferenceRequestJsonMode::Strict
                || *json_mode == ModelInferenceRequestJsonMode::On)
        {
            content = prefill_json_response(content)?;
        }

        let usage = output
            .usage
            .map(|u| Usage {
                input_tokens: u.input_tokens as u32,
                output_tokens: u.output_tokens as u32,
            })
            .ok_or_else(|| {
                Error::new(ErrorDetails::InferenceServer {
                    raw_request: None,
                    raw_response: Some(raw_response.clone()),
                    message: "AWS Bedrock returned a message without usage information."
                        .to_string(),
                    provider_type: PROVIDER_TYPE.to_string(),
                })
            })?;

        Ok(ProviderInferenceResponse::new(
            ProviderInferenceResponseArgs {
                output: content,
                system,
                input_messages,
                raw_request,
                raw_response,
                usage,
                latency,
                finish_reason: aws_stop_reason_to_tensorzero_finish_reason(output.stop_reason),
            },
        ))
    }
}

/// Serialize a struct to a JSON string.
///
/// This is necessary because the AWS SDK doesn't implement Serialize.
/// Therefore, we construct this unusual JSON object to store the raw output
///
/// This feature request has been pending since 2022:
/// https://github.com/awslabs/aws-sdk-rust/issues/645
fn serialize_aws_bedrock_struct<T: std::fmt::Debug>(output: &T) -> Result<String, Error> {
    serde_json::to_string(&serde_json::json!({"debug": format!("{:?}", output)})).map_err(|e| {
        Error::new(ErrorDetails::InferenceServer {
            raw_request: None,
            raw_response: Some(format!("{output:?}")),
            message: format!(
                "Error parsing response from AWS Bedrock: {}",
                DisplayOrDebugGateway::new(e)
            ),
            provider_type: PROVIDER_TYPE.to_string(),
        })
    })
}

impl TryFrom<&ToolConfig> for Tool {
    type Error = Error;

    fn try_from(tool_config: &ToolConfig) -> Result<Self, Error> {
        let tool_input_schema = ToolInputSchema::Json(
            serde_json::from_value(tool_config.parameters().clone()).map_err(|e| {
                Error::new(ErrorDetails::InferenceClient {
                    raw_request: None,
                    raw_response: Some(format!("{:?}", tool_config.parameters())),
                    status_code: Some(StatusCode::INTERNAL_SERVER_ERROR),
                    message: format!(
                        "Error parsing tool input schema: {}",
                        DisplayOrDebugGateway::new(e)
                    ),
                    provider_type: PROVIDER_TYPE.to_string(),
                })
            })?,
        );

        let tool_spec = ToolSpecification::builder()
            .name(tool_config.name())
            .description(tool_config.description())
            .input_schema(tool_input_schema)
            .build()
            .map_err(|_| {
                Error::new(ErrorDetails::InferenceClient {
                    raw_request: None,
                    raw_response: Some(format!("{:?}", tool_config.parameters())),
                    status_code: Some(StatusCode::INTERNAL_SERVER_ERROR),
                    message: "Error configuring AWS Bedrock tool choice (this should never happen). Please file a bug report: https://github.com/tensorzero/tensorzero/issues/new"
                        .to_string(),
                    provider_type: PROVIDER_TYPE.to_string(),
                })
            })?;

        Ok(Tool::ToolSpec(tool_spec))
    }
}

impl TryFrom<ToolChoice> for AWSBedrockToolChoice {
    type Error = Error;

    fn try_from(tool_choice: ToolChoice) -> Result<Self, Error> {
        match tool_choice {
            // Workaround for AWS Bedrock API limitation: they don't support explicitly specifying "none"
            // for tool choice. Instead, we return Auto but the request construction will ensure
            // that no tools are sent in the request payload. This achieves the same effect
            // as explicitly telling the model not to use tools, since without any tools
            // being provided, the model cannot make tool calls.
            ToolChoice::None => Ok(AWSBedrockToolChoice::Auto(AutoToolChoice::builder().build())),
            ToolChoice::Auto => Ok(AWSBedrockToolChoice::Auto(
                AutoToolChoice::builder().build(),
            )),
            ToolChoice::Required => Ok(AWSBedrockToolChoice::Any(AnyToolChoice::builder().build())),
            ToolChoice::Specific(tool_name) => Ok(AWSBedrockToolChoice::Tool(
                SpecificToolChoice::builder()
                    .name(tool_name)
                    .build()
                    .map_err(|_| Error::new(ErrorDetails::InferenceClient {
                        raw_request: None,
                        raw_response: None,
                        status_code: Some(StatusCode::INTERNAL_SERVER_ERROR),
                        message:
                            "Error configuring AWS Bedrock tool choice (this should never happen). Please file a bug report: https://github.com/tensorzero/tensorzero/issues/new"
                                .to_string(),
                        provider_type: PROVIDER_TYPE.to_string(),
                    }))?,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tracing_test::traced_test;

    #[tokio::test]
    async fn test_get_aws_bedrock_client_no_aws_credentials() {
        #[traced_test]
        async fn first_run() {
            AWSBedrockProvider::new("test".to_string(), Some(Region::new("uk-hogwarts-1")))
                .await
                .unwrap();

            assert!(logs_contain(
                "Creating new AWS config for region: uk-hogwarts-1"
            ));
        }

        #[traced_test]
        async fn second_run() {
            AWSBedrockProvider::new("test".to_string(), Some(Region::new("uk-hogwarts-1")))
                .await
                .unwrap();

            assert!(logs_contain(
                "Creating new AWS config for region: uk-hogwarts-1"
            ));
        }

        #[traced_test]
        async fn third_run() {
            // We want auto-detection to fail, so we clear this environment variable.
            // We use 'nextest' as our runner, so each test runs in its own process
            std::env::remove_var("AWS_REGION");
            std::env::remove_var("AWS_DEFAULT_REGION");
            let err = AWSBedrockProvider::new("test".to_string(), None)
                .await
                .expect_err("AWS bedrock provider should fail when it cannot detect region");
            let err_msg = err.to_string();
            assert!(
                err_msg.contains("Failed to determine AWS region."),
                "Unexpected error message: {err_msg}"
            );

            assert!(logs_contain("Failed to determine AWS region."));
        }

        #[traced_test]
        async fn fourth_run() {
            AWSBedrockProvider::new("test".to_string(), Some(Region::new("me-shire-2")))
                .await
                .unwrap();

            assert!(logs_contain(
                "Creating new AWS config for region: me-shire-2"
            ));
        }

        // Every call should trigger client creation since each provider has its own AWS Bedrock client
        first_run().await;
        second_run().await;
        third_run().await;
        fourth_run().await;
    }
}
