use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};
use tensorzero_derive::TensorZeroDeserialize;

use crate::{
    config_parser::{
        path::TomlRelativePath, MetricConfig, MetricConfigLevel, MetricConfigOptimize,
        MetricConfigType, PathWithContents, TimeoutsConfig,
    },
    error::{Error, ErrorDetails},
    function::{FunctionConfig, FunctionConfigJson},
    inference::types::{extra_body::ExtraBodyConfig, extra_headers::ExtraHeadersConfig},
    jsonschema_util::StaticJSONSchema,
    tool::create_implicit_tool_call_config,
    variant::{
        best_of_n_sampling::{
            BestOfNEvaluatorConfig as OnlineEvaluatorConfig, BestOfNSamplingConfig,
        },
        chain_of_thought::ChainOfThoughtConfig,
        chat_completion::ChatCompletionConfig,
        dicl::DiclConfig,
        mixture_of_n::{FuserConfig, MixtureOfNConfig},
        JsonMode, RetryConfig, VariantConfig, VariantInfo,
    },
};

pub const LLM_JUDGE_USER_SCHEMA_TEXT: &str = include_str!("llm_judge_user_schema.json");
pub const LLM_JUDGE_FLOAT_OUTPUT_SCHEMA_TEXT: &str =
    include_str!("llm_judge_float_output_schema.json");
pub const LLM_JUDGE_BOOLEAN_OUTPUT_SCHEMA_TEXT: &str =
    include_str!("llm_judge_boolean_output_schema.json");

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct StaticEvaluationConfig {
    pub evaluators: HashMap<String, EvaluatorConfig>,
    pub function_name: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvaluationConfig {
    Static(StaticEvaluationConfig),
}

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvaluatorConfig {
    ExactMatch(ExactMatchConfig),
    #[serde(rename = "llm_judge")]
    LLMJudge(LLMJudgeConfig),
}

impl EvaluatorConfig {
    pub fn cutoff(&self) -> Option<f32> {
        match self {
            EvaluatorConfig::ExactMatch(config) => config.cutoff,
            EvaluatorConfig::LLMJudge(config) => config.cutoff,
        }
    }

    pub fn optimize(&self) -> MetricConfigOptimize {
        match self {
            EvaluatorConfig::ExactMatch(_) => MetricConfigOptimize::Max,
            EvaluatorConfig::LLMJudge(config) => config.optimize.into(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct ExactMatchConfig {
    #[serde(default)]
    pub cutoff: Option<f32>,
}

#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct LLMJudgeConfig {
    pub input_format: LLMJudgeInputFormat,
    pub output_type: LLMJudgeOutputType,
    pub include: LLMJudgeIncludeConfig,
    pub optimize: LLMJudgeOptimize,
    pub cutoff: Option<f32>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct LLMJudgeIncludeConfig {
    #[serde(default)]
    pub reference_output: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
#[serde(rename_all = "snake_case")]
pub enum LLMJudgeInputFormat {
    #[default]
    Serialized,
    Messages,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
#[serde(rename_all = "snake_case")]
pub enum LLMJudgeOutputType {
    Float,
    Boolean,
}

impl From<LLMJudgeOutputType> for MetricConfigType {
    fn from(output_type: LLMJudgeOutputType) -> Self {
        match output_type {
            LLMJudgeOutputType::Float => MetricConfigType::Float,
            LLMJudgeOutputType::Boolean => MetricConfigType::Boolean,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
#[serde(rename_all = "snake_case")]
pub enum LLMJudgeOptimize {
    Min,
    Max,
}

impl From<LLMJudgeOptimize> for MetricConfigOptimize {
    fn from(optimize: LLMJudgeOptimize) -> Self {
        match optimize {
            LLMJudgeOptimize::Min => MetricConfigOptimize::Min,
            LLMJudgeOptimize::Max => MetricConfigOptimize::Max,
        }
    }
}

pub fn get_llm_judge_function_name(evaluation_name: &str, evaluator_name: &str) -> String {
    format!("tensorzero::llm_judge::{evaluation_name}::{evaluator_name}")
}

pub fn get_evaluator_metric_name(evaluation_name: &str, evaluator_name: &str) -> String {
    format!("tensorzero::evaluation_name::{evaluation_name}::evaluator_name::{evaluator_name}")
}

#[derive(Debug, TensorZeroDeserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum UninitializedEvaluationConfig {
    Static(UninitializedStaticEvaluationConfig),
}

impl UninitializedEvaluationConfig {
    pub fn load(
        self,
        functions: &HashMap<String, Arc<FunctionConfig>>,

        evaluation_name: &str,
    ) -> EvaluationLoadResult {
        match self {
            UninitializedEvaluationConfig::Static(config) => {
                config.load(functions, evaluation_name)
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UninitializedStaticEvaluationConfig {
    evaluators: HashMap<String, UninitializedEvaluatorConfig>,
    function_name: String,
}

type EvaluationLoadResult = Result<
    (
        StaticEvaluationConfig,               // The evaluation itself
        HashMap<String, Arc<FunctionConfig>>, // All functions which the evaluation needs {function_name -> function_config}
        HashMap<String, MetricConfig>, // All metrics which the evaluation needs {metric_name -> metric_config}
    ),
    Error,
>;

impl UninitializedStaticEvaluationConfig {
    pub fn load(
        self,
        functions: &HashMap<String, Arc<FunctionConfig>>,
        evaluation_name: &str,
    ) -> EvaluationLoadResult {
        if !functions.contains_key(&self.function_name) {
            return Err(ErrorDetails::Config {
                message: format!(
                    "Function `{}` not found (referenced in `[evaluations.{evaluation_name}]`)",
                    self.function_name
                ),
            }
            .into());
        }

        // evaluation names cannot have "::" in them since we use it as a delimiter
        if evaluation_name.contains("::") {
            return Err(ErrorDetails::Config {
                message: format!(
                    "evaluation names cannot contain \"::\" (referenced in `[evaluations.{evaluation_name}]`)"
                ),
            }
            .into());
        }
        let evaluator_results = self
            .evaluators
            .into_iter()
            .map(|(name, config)| {
                config.load(evaluation_name, &name).map(
                    |(evaluation_config, func_config, metric_config)| {
                        (name, evaluation_config, func_config, metric_config)
                    },
                )
            })
            .collect::<Result<Vec<_>, Error>>()?;

        // Create HashMaps from the results
        let mut evaluators = HashMap::new();
        let mut function_configs = HashMap::new();
        let mut metric_configs = HashMap::new();
        for (evaluator_name, evaluator_config, function_config, metric_config) in evaluator_results
        {
            // Add to evaluators map
            evaluators.insert(evaluator_name.clone(), evaluator_config);

            // Add to function_configs map if Some
            if let Some(config) = function_config {
                function_configs.insert(
                    get_llm_judge_function_name(evaluation_name, &evaluator_name),
                    Arc::new(config),
                );
            }

            // Add to metric_configs map
            metric_configs.insert(
                get_evaluator_metric_name(evaluation_name, &evaluator_name),
                metric_config,
            );
        }
        Ok((
            StaticEvaluationConfig {
                evaluators,
                function_name: self.function_name,
            },
            function_configs,
            metric_configs,
        ))
    }
}

#[derive(Debug, TensorZeroDeserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum UninitializedEvaluatorConfig {
    ExactMatch(ExactMatchConfig),
    #[serde(rename = "llm_judge")]
    LLMJudge(UninitializedLLMJudgeConfig),
}

#[derive(Debug, Deserialize)]
struct UninitializedLLMJudgeConfig {
    #[serde(default)]
    input_format: LLMJudgeInputFormat,
    variants: HashMap<String, UninitializedLLMJudgeVariantInfo>,
    output_type: LLMJudgeOutputType,
    optimize: LLMJudgeOptimize,
    #[serde(default)]
    include: LLMJudgeIncludeConfig,
    #[serde(default)]
    cutoff: Option<f32>,
}

impl UninitializedEvaluatorConfig {
    pub fn load(
        self,
        evaluation_name: &str,
        evaluator_name: &str,
    ) -> Result<(EvaluatorConfig, Option<FunctionConfig>, MetricConfig), Error> {
        // Evaluator names cannot have "::" in them since we use it as a delimiter in our function names later on
        if evaluator_name.contains("::") {
            return Err(ErrorDetails::Config {
                message: format!(
                    "Evaluator names cannot contain \"::\" (referenced in `[evaluations.{evaluation_name}.{evaluator_name}]`)"
                ),
            }
            .into());
        }
        match self {
            UninitializedEvaluatorConfig::ExactMatch(params) => Ok((
                EvaluatorConfig::ExactMatch(params),
                None,
                MetricConfig {
                    r#type: MetricConfigType::Boolean,
                    optimize: MetricConfigOptimize::Max,
                    level: MetricConfigLevel::Inference,
                },
            )),
            UninitializedEvaluatorConfig::LLMJudge(params) => {
                let mut variants = params
                    .variants
                    .into_iter()
                    .map(|(name, variant)| {
                        variant
                            .load(evaluation_name, evaluator_name, &params.input_format, &name)
                            .map(|v| (name, v))
                    })
                    .collect::<Result<HashMap<_, _>, Error>>()?;
                let nonzero_weights = variants
                    .iter()
                    // Treat a None weight as 0.0 for this check - we only care if we have multiple variants with an explicit positive weight
                    .filter(|(_, variant)| variant.inner.weight().unwrap_or(0.0) > 0.0)
                    .count();
                if nonzero_weights != 1 && variants.len() > 1 {
                    return Err(ErrorDetails::Config {
                        message: format!(
                            "Evaluator `{evaluator_name}` in `[evaluations.{evaluation_name}]` must have exactly 1 variant that is active. Found {nonzero_weights} variants with nonzero weights."
                        ),
                    }
                    .into());
                } else if variants.len() == 1 {
                    // If there is only one variant, it should have weight 1.0
                    let Some((_, variant)) = variants.iter_mut().next() else {
                        return Err(ErrorDetails::Config {
                            message: "Failed to grab first variant from variants map. This should never happen, please file a bug report at https://github.com/tensorzero/tensorzero/discussions/new?category=bug-reports.".to_string(),
                        }.into());
                    };
                    if let Some(weight) = variant.inner.weight() {
                        if weight == 0.0 {
                            return Err(ErrorDetails::Config {
                                message: format!("Evaluator `{evaluator_name}` in `[evaluations.{evaluation_name}]` must have exactly 1 variant that is active. You have specified a single inactive variant."),
                            }
                            .into());
                        }
                    }
                    match &mut variant.inner {
                        VariantConfig::ChatCompletion(variant) => {
                            variant.weight = Some(1.0);
                        }
                        VariantConfig::BestOfNSampling(variant) => {
                            variant.weight = Some(1.0);
                        }
                        VariantConfig::MixtureOfN(variant) => {
                            variant.weight = Some(1.0);
                        }
                        VariantConfig::Dicl(variant) => {
                            variant.weight = Some(1.0);
                        }
                        VariantConfig::ChainOfThought(variant) => {
                            variant.inner.weight = Some(1.0);
                        }
                    };
                }
                let user_schema_value: Option<serde_json::Value> = match params.input_format {
                    LLMJudgeInputFormat::Serialized => Some(serde_json::from_str(LLM_JUDGE_USER_SCHEMA_TEXT)
                        .map_err(|e| {
                            Error::new(ErrorDetails::JsonSchema {
                                message: format!("Failed to parse LLM judge user schema: {e}. This should never happen, please file a bug report at https://github.com/tensorzero/tensorzero/discussions/new?category=bug-reports."),
                            })
                        })?),
                    LLMJudgeInputFormat::Messages => None,
                };
                let output_schema_str = match params.output_type {
                    LLMJudgeOutputType::Float => LLM_JUDGE_FLOAT_OUTPUT_SCHEMA_TEXT,
                    LLMJudgeOutputType::Boolean => LLM_JUDGE_BOOLEAN_OUTPUT_SCHEMA_TEXT,
                };
                let output_schema_value = serde_json::from_str(output_schema_str)
                    .map_err(|e| {
                        Error::new(ErrorDetails::JsonSchema {
                            message: format!("Failed to parse LLM judge output schema: {e}. This should never happen, please file a bug report at https://github.com/tensorzero/tensorzero/discussions/new?category=bug-reports."),
                        })
                    })?;
                let output_schema = StaticJSONSchema::from_value(&output_schema_value)?;
                let implicit_tool_call_config =
                    create_implicit_tool_call_config(output_schema.clone());
                let variants = variants
                    .into_iter()
                    .map(|(name, variant)| (name, Arc::new(variant)))
                    .collect();
                let function_config = FunctionConfig::Json(FunctionConfigJson {
                    variants,
                    system_schema: None,
                    user_schema: user_schema_value
                        .map(|v| StaticJSONSchema::from_value(&v))
                        .transpose()?,
                    assistant_schema: None,
                    output_schema,
                    implicit_tool_call_config,
                    description: None,
                });
                Ok((
                    EvaluatorConfig::LLMJudge(LLMJudgeConfig {
                        input_format: params.input_format,
                        output_type: params.output_type,
                        include: params.include,
                        optimize: params.optimize,
                        cutoff: params.cutoff,
                    }),
                    Some(function_config),
                    MetricConfig {
                        r#type: params.output_type.into(),
                        optimize: params.optimize.into(),
                        level: MetricConfigLevel::Inference,
                    },
                ))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct UninitializedLLMJudgeVariantInfo {
    #[serde(flatten)]
    inner: UninitializedLLMJudgeVariantConfig,
    timeouts: Option<TimeoutsConfig>,
}

#[derive(Debug, TensorZeroDeserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum UninitializedLLMJudgeVariantConfig {
    ChatCompletion(UninitializedLLMJudgeChatCompletionVariantConfig),
    #[serde(rename = "experimental_best_of_n_sampling")]
    BestOfNSampling(UninitializedLLMJudgeBestOfNVariantConfig),
    #[serde(rename = "experimental_mixture_of_n")]
    MixtureOfNSampling(UninitializedLLMJudgeMixtureOfNVariantConfig),
    #[serde(rename = "experimental_dynamic_in_context_learning")]
    Dicl(UninitializedLLMJudgeDiclVariantConfig),
    #[serde(rename = "experimental_chain_of_thought")]
    ChainOfThought(UninitializedLLMJudgeChainOfThoughtVariantConfig),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UninitializedLLMJudgeChatCompletionVariantConfig {
    #[serde(default)]
    active: Option<bool>,
    model: Arc<str>,
    system_instructions: TomlRelativePath,
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_tokens: Option<u32>,
    presence_penalty: Option<f32>,
    frequency_penalty: Option<f32>,
    seed: Option<u32>,
    json_mode: JsonMode, // This is a JSON function
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    retries: RetryConfig,
    #[serde(default)]
    extra_body: Option<ExtraBodyConfig>,
    #[serde(default)]
    extra_headers: Option<ExtraHeadersConfig>,
}

/// Converts a chat completion judge variant config to a chat completion config.
/// This is factored out so that both the chain of thought and chat completion judges
/// can use the same implementation.
fn convert_chat_completion_judge_to_variant(
    evaluation_name: &str,
    evaluator_name: &str,
    variant_name: &str,
    input_format: &LLMJudgeInputFormat,
    params: UninitializedLLMJudgeChatCompletionVariantConfig,
) -> Result<ChatCompletionConfig, Error> {
    let system_instructions = &params.system_instructions.read()?;
    let templated_system_instructions = format!(
        include_str!("llm_judge_system_instructions.txt"),
        system_instructions = system_instructions,
    );
    let system_template_path = get_template_path(
        evaluation_name,
        evaluator_name,
        variant_name,
        "system",
        templated_system_instructions,
    );
    let system_template = PathWithContents::from_path(system_template_path)?;
    let user_template = match input_format {
        LLMJudgeInputFormat::Serialized => Some(PathWithContents::from_path(get_template_path(
            evaluation_name,
            evaluator_name,
            variant_name,
            "user",
            include_str!("llm_judge_user_template.minijinja").to_string(),
        ))?),
        LLMJudgeInputFormat::Messages => None,
    };
    Ok(ChatCompletionConfig {
        weight: get_weight(params.active),
        model: params.model,
        system_template: Some(system_template),
        user_template,
        assistant_template: None,
        temperature: params.temperature,
        top_p: params.top_p,
        max_tokens: params.max_tokens,
        presence_penalty: params.presence_penalty,
        frequency_penalty: params.frequency_penalty,
        seed: params.seed,
        stop_sequences: params.stop_sequences,
        json_mode: Some(params.json_mode),
        retries: params.retries,
        extra_body: params.extra_body,
        extra_headers: params.extra_headers,
    })
}

fn default_timeout() -> f64 {
    300.0
}

#[derive(Debug, Deserialize)]
struct UninitializedLLMJudgeBestOfNVariantConfig {
    #[serde(default)]
    active: Option<bool>,
    #[serde(default = "default_timeout")]
    timeout_s: f64,
    #[serde(default)]
    candidates: Vec<String>,
    evaluator: UninitializedLLMJudgeChatCompletionVariantConfig,
}

#[derive(Debug, Deserialize)]
struct UninitializedLLMJudgeMixtureOfNVariantConfig {
    #[serde(default)]
    active: Option<bool>,
    #[serde(default = "default_timeout")]
    timeout_s: f64,
    #[serde(default)]
    candidates: Vec<String>,
    fuser: UninitializedLLMJudgeChatCompletionVariantConfig,
}

#[derive(Debug, Deserialize)]
struct UninitializedLLMJudgeDiclVariantConfig {
    #[serde(default)]
    active: Option<bool>,
    embedding_model: String,
    k: u32, // k as in k-nearest neighbors
    model: String,
    system_instructions: Option<TomlRelativePath>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    presence_penalty: Option<f32>,
    frequency_penalty: Option<f32>,
    max_tokens: Option<u32>,
    seed: Option<u32>,
    json_mode: Option<JsonMode>,
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    extra_body: Option<ExtraBodyConfig>,
    #[serde(default)]
    retries: RetryConfig,
    #[serde(default)]
    extra_headers: Option<ExtraHeadersConfig>,
}

#[derive(Debug, Deserialize)]
struct UninitializedLLMJudgeChainOfThoughtVariantConfig {
    #[serde(flatten)]
    inner: UninitializedLLMJudgeChatCompletionVariantConfig,
}

fn get_template_path(
    evaluation_name: &str,
    evaluator_name: &str,
    variant_name: &str,
    template_name: &str,
    data: String,
) -> TomlRelativePath {
    TomlRelativePath::new_fake_path(format!(
        "tensorzero::llm_judge::{evaluation_name}::{evaluator_name}::{variant_name}::{template_name}"
    ), data)
}

fn get_weight(active: Option<bool>) -> Option<f64> {
    match active {
        Some(active) => {
            if active {
                Some(1.0)
            } else {
                Some(0.0)
            }
        }
        None => None,
    }
}

impl UninitializedLLMJudgeVariantInfo {
    pub fn load(
        self,
        evaluation_name: &str,
        evaluator_name: &str,
        input_format: &LLMJudgeInputFormat,
        variant_name: &str,
    ) -> Result<VariantInfo, Error> {
        let inner = match self.inner {
            UninitializedLLMJudgeVariantConfig::ChatCompletion(params) => {
                VariantConfig::ChatCompletion(convert_chat_completion_judge_to_variant(
                    evaluation_name,
                    evaluator_name,
                    variant_name,
                    input_format,
                    params,
                )?)
            }
            UninitializedLLMJudgeVariantConfig::BestOfNSampling(params) => {
                let evaluator_system_instructions = &params.evaluator.system_instructions.read()?;
                let templated_evaluator_system_instructions = format!(
                    include_str!("llm_judge_system_instructions.txt"),
                    system_instructions = evaluator_system_instructions,
                );
                let evaluator_system_template = PathWithContents::from_path(get_template_path(
                    evaluation_name,
                    evaluator_name,
                    variant_name,
                    "system",
                    templated_evaluator_system_instructions,
                ))?;
                let evaluator_user_template = match input_format {
                    LLMJudgeInputFormat::Serialized => {
                        Some(PathWithContents::from_path(get_template_path(
                            evaluation_name,
                            evaluator_name,
                            variant_name,
                            "user",
                            include_str!("llm_judge_user_template.minijinja").to_string(),
                        ))?)
                    }
                    LLMJudgeInputFormat::Messages => None,
                };
                VariantConfig::BestOfNSampling(BestOfNSamplingConfig {
                    weight: get_weight(params.active),
                    timeout_s: params.timeout_s,
                    candidates: params.candidates,
                    evaluator: OnlineEvaluatorConfig {
                        inner: ChatCompletionConfig {
                            weight: None,
                            model: params.evaluator.model,
                            system_template: Some(evaluator_system_template),
                            user_template: evaluator_user_template,
                            assistant_template: None,
                            temperature: params.evaluator.temperature,
                            top_p: params.evaluator.top_p,
                            max_tokens: params.evaluator.max_tokens,
                            presence_penalty: params.evaluator.presence_penalty,
                            frequency_penalty: params.evaluator.frequency_penalty,
                            seed: params.evaluator.seed,
                            json_mode: Some(params.evaluator.json_mode),
                            stop_sequences: params.evaluator.stop_sequences,
                            retries: params.evaluator.retries,
                            extra_body: params.evaluator.extra_body,
                            extra_headers: params.evaluator.extra_headers,
                        },
                    },
                })
            }
            UninitializedLLMJudgeVariantConfig::MixtureOfNSampling(params) => {
                let fuser_system_instructions = &params.fuser.system_instructions.read()?;
                let templated_fuser_system_instructions = format!(
                    include_str!("llm_judge_system_instructions.txt"),
                    system_instructions = fuser_system_instructions,
                );
                let fuser_system_template = PathWithContents::from_path(get_template_path(
                    evaluation_name,
                    evaluator_name,
                    variant_name,
                    "system",
                    templated_fuser_system_instructions,
                ))?;
                let fuser_user_template = match input_format {
                    LLMJudgeInputFormat::Serialized => {
                        Some(PathWithContents::from_path(get_template_path(
                            evaluation_name,
                            evaluator_name,
                            variant_name,
                            "user",
                            include_str!("llm_judge_user_template.minijinja").to_string(),
                        ))?)
                    }
                    LLMJudgeInputFormat::Messages => None,
                };
                VariantConfig::MixtureOfN(MixtureOfNConfig {
                    weight: get_weight(params.active),
                    timeout_s: params.timeout_s,
                    candidates: params.candidates,
                    fuser: FuserConfig {
                        inner: ChatCompletionConfig {
                            weight: None,
                            model: params.fuser.model,
                            system_template: Some(fuser_system_template),
                            user_template: fuser_user_template,
                            assistant_template: None,
                            temperature: params.fuser.temperature,
                            top_p: params.fuser.top_p,
                            max_tokens: params.fuser.max_tokens,
                            presence_penalty: params.fuser.presence_penalty,
                            frequency_penalty: params.fuser.frequency_penalty,
                            seed: params.fuser.seed,
                            json_mode: Some(params.fuser.json_mode),
                            retries: params.fuser.retries,
                            stop_sequences: params.fuser.stop_sequences,
                            extra_body: params.fuser.extra_body,
                            extra_headers: params.fuser.extra_headers,
                        },
                    },
                })
            }
            UninitializedLLMJudgeVariantConfig::Dicl(params) => {
                let dicl_system_instructions = params
                    .system_instructions
                    .map(|si| si.read())
                    .transpose()?
                    .map(|si| {
                        format!(
                            include_str!("llm_judge_system_instructions.txt"),
                            system_instructions = si,
                        )
                    })
                    .unwrap_or(crate::variant::dicl::default_system_instructions());
                VariantConfig::Dicl(DiclConfig {
                    weight: get_weight(params.active),
                    embedding_model: params.embedding_model.into(),
                    k: params.k,
                    model: params.model.into(),
                    system_instructions: dicl_system_instructions,
                    temperature: params.temperature,
                    top_p: params.top_p,
                    presence_penalty: params.presence_penalty,
                    frequency_penalty: params.frequency_penalty,
                    max_tokens: params.max_tokens,
                    seed: params.seed,
                    json_mode: params.json_mode,
                    extra_body: params.extra_body,
                    extra_headers: params.extra_headers,
                    retries: params.retries,
                    stop_sequences: params.stop_sequences,
                })
            }
            UninitializedLLMJudgeVariantConfig::ChainOfThought(params) => {
                VariantConfig::ChainOfThought(ChainOfThoughtConfig {
                    inner: convert_chat_completion_judge_to_variant(
                        evaluation_name,
                        evaluator_name,
                        variant_name,
                        input_format,
                        params.inner,
                    )?,
                })
            }
        };
        Ok(VariantInfo {
            inner,
            timeouts: self.timeouts.unwrap_or_default(),
        })
    }
}

/// NOTE: this function should not be called.
/// In the code we already have a conversion from UninitializedLLMJudgeVariantConfig to VariantConfig.
/// We want to make sure that there is an UninitializedLLMJudgeVariantConfig for each VariantConfig.
/// This function should complain at compile time if we forget to update it when adding a new variant type.
#[expect(dead_code)]
#[expect(clippy::unnecessary_wraps)]
fn check_convert_variant_to_llm_judge_variant(
    variant: VariantConfig,
) -> Result<UninitializedLLMJudgeVariantConfig, Error> {
    match variant {
        VariantConfig::ChatCompletion(variant) => {
            Ok(UninitializedLLMJudgeVariantConfig::ChatCompletion(
                UninitializedLLMJudgeChatCompletionVariantConfig {
                    active: Some(false),
                    model: variant.model,
                    system_instructions: TomlRelativePath::new_fake_path(
                        String::new(),
                        String::new(),
                    ),
                    temperature: variant.temperature,
                    top_p: variant.top_p,
                    max_tokens: variant.max_tokens,
                    presence_penalty: variant.presence_penalty,
                    frequency_penalty: variant.frequency_penalty,
                    seed: variant.seed,
                    json_mode: JsonMode::Off,
                    retries: variant.retries,
                    stop_sequences: variant.stop_sequences,
                    extra_body: variant.extra_body,
                    extra_headers: variant.extra_headers,
                },
            ))
        }
        VariantConfig::BestOfNSampling(variant) => {
            Ok(UninitializedLLMJudgeVariantConfig::BestOfNSampling(
                UninitializedLLMJudgeBestOfNVariantConfig {
                    active: Some(false),
                    timeout_s: variant.timeout_s,
                    candidates: variant.candidates,
                    evaluator: UninitializedLLMJudgeChatCompletionVariantConfig {
                        active: Some(false),
                        model: variant.evaluator.inner.model,
                        system_instructions: TomlRelativePath::new_fake_path(
                            String::new(),
                            String::new(),
                        ),
                        temperature: variant.evaluator.inner.temperature,
                        top_p: variant.evaluator.inner.top_p,
                        max_tokens: variant.evaluator.inner.max_tokens,
                        presence_penalty: variant.evaluator.inner.presence_penalty,
                        frequency_penalty: variant.evaluator.inner.frequency_penalty,
                        seed: variant.evaluator.inner.seed,
                        json_mode: JsonMode::Off,
                        retries: variant.evaluator.inner.retries,
                        stop_sequences: variant.evaluator.inner.stop_sequences,
                        extra_body: variant.evaluator.inner.extra_body,
                        extra_headers: variant.evaluator.inner.extra_headers,
                    },
                },
            ))
        }
        VariantConfig::MixtureOfN(variant) => {
            Ok(UninitializedLLMJudgeVariantConfig::MixtureOfNSampling(
                UninitializedLLMJudgeMixtureOfNVariantConfig {
                    active: Some(false),
                    timeout_s: variant.timeout_s,
                    candidates: variant.candidates,
                    fuser: UninitializedLLMJudgeChatCompletionVariantConfig {
                        active: Some(false),
                        model: variant.fuser.inner.model,
                        system_instructions: TomlRelativePath::new_fake_path(
                            String::new(),
                            String::new(),
                        ),
                        temperature: variant.fuser.inner.temperature,
                        top_p: variant.fuser.inner.top_p,
                        max_tokens: variant.fuser.inner.max_tokens,
                        presence_penalty: variant.fuser.inner.presence_penalty,
                        frequency_penalty: variant.fuser.inner.frequency_penalty,
                        seed: variant.fuser.inner.seed,
                        json_mode: JsonMode::Off,
                        retries: variant.fuser.inner.retries,
                        stop_sequences: variant.fuser.inner.stop_sequences,
                        extra_body: variant.fuser.inner.extra_body,
                        extra_headers: variant.fuser.inner.extra_headers,
                    },
                },
            ))
        }
        VariantConfig::Dicl(variant) => Ok(UninitializedLLMJudgeVariantConfig::Dicl(
            UninitializedLLMJudgeDiclVariantConfig {
                active: Some(false),
                embedding_model: variant.embedding_model.to_string(),
                k: variant.k,
                model: variant.model.to_string(),
                system_instructions: None,
                temperature: variant.temperature,
                top_p: variant.top_p,
                presence_penalty: variant.presence_penalty,
                frequency_penalty: variant.frequency_penalty,
                max_tokens: variant.max_tokens,
                seed: variant.seed,
                json_mode: variant.json_mode,
                extra_body: variant.extra_body,
                extra_headers: variant.extra_headers,
                retries: variant.retries,
                stop_sequences: variant.stop_sequences,
            },
        )),
        VariantConfig::ChainOfThought(variant) => {
            Ok(UninitializedLLMJudgeVariantConfig::ChainOfThought(
                UninitializedLLMJudgeChainOfThoughtVariantConfig {
                    inner: UninitializedLLMJudgeChatCompletionVariantConfig {
                        active: Some(false),
                        model: variant.inner.model,
                        system_instructions: TomlRelativePath::new_fake_path(
                            String::new(),
                            String::new(),
                        ),
                        temperature: variant.inner.temperature,
                        top_p: variant.inner.top_p,
                        max_tokens: variant.inner.max_tokens,
                        presence_penalty: variant.inner.presence_penalty,
                        frequency_penalty: variant.inner.frequency_penalty,
                        seed: variant.inner.seed,
                        json_mode: JsonMode::Off,
                        retries: variant.inner.retries,
                        stop_sequences: variant.inner.stop_sequences,
                        extra_body: variant.inner.extra_body,
                        extra_headers: variant.inner.extra_headers,
                    },
                },
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn test_uninitialized_evaluation_config_load() {
        // Setup test fixtures
        let evaluation_name = "test_evaluation";

        // Prepare function configs map with a function referenced in the evaluation
        let mut functions = HashMap::new();
        let function_name = "generate_draft";
        let function_config = FunctionConfig::Json(FunctionConfigJson {
            variants: HashMap::new(),
            system_schema: None,
            user_schema: None,
            assistant_schema: None,
            output_schema: create_test_schema(),
            implicit_tool_call_config: create_implicit_tool_call_config(create_test_schema()),
            description: None,
        });
        functions.insert(function_name.to_string(), Arc::new(function_config));

        // Test case 1: Successful loading with exact match evaluator
        {
            let mut evaluators = HashMap::new();
            evaluators.insert(
                "em_evaluator".to_string(),
                UninitializedEvaluatorConfig::ExactMatch(ExactMatchConfig { cutoff: Some(0.4) }),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: function_name.to_string(),
            };

            let result = uninitialized_config.load(&functions, evaluation_name);
            assert!(result.is_ok());

            let (config, additional_functions, metric_configs) = result.unwrap();
            assert_eq!(config.function_name, function_name);
            assert_eq!(config.evaluators.len(), 1);
            match config.evaluators.get("em_evaluator").unwrap() {
                EvaluatorConfig::ExactMatch(params) => assert_eq!(params.cutoff, Some(0.4)),
                _ => panic!("Expected ExactMatch evaluator"),
            }
            // No additional function configs for exact match
            assert_eq!(additional_functions.len(), 0);

            // Verify the metrics
            assert_eq!(metric_configs.len(), 1);

            // Check the metric name follows expected format
            let metric_config_name = get_evaluator_metric_name(evaluation_name, "em_evaluator");
            assert_eq!(
                metric_config_name,
                "tensorzero::evaluation_name::test_evaluation::evaluator_name::em_evaluator"
            );
            assert!(metric_configs.contains_key(&metric_config_name));

            // Verify all properties of the metric config
            let metric_config = metric_configs.get(&metric_config_name).unwrap();
            assert_eq!(metric_config.r#type, MetricConfigType::Boolean);
            assert_eq!(metric_config.optimize, MetricConfigOptimize::Max);
            assert_eq!(metric_config.level, MetricConfigLevel::Inference);
        }

        // Test case 2: Successful loading with LLM judge evaluator
        {
            let mut variants = HashMap::new();
            variants.insert(
                "test_variant".to_string(),
                UninitializedLLMJudgeVariantInfo {
                    inner: UninitializedLLMJudgeVariantConfig::ChatCompletion(
                        UninitializedLLMJudgeChatCompletionVariantConfig {
                            active: Some(true),
                            model: Arc::from("gpt-3.5-turbo"),
                            system_instructions:
                                "fixtures/config/evaluations/evaluation1/llm_judge_bool/system_instructions.txt"
                                    .into(),
                            temperature: Some(0.7),
                            top_p: None,
                            max_tokens: Some(100),
                            presence_penalty: None,
                            frequency_penalty: None,
                            seed: None,
                            json_mode: JsonMode::ImplicitTool,
                            retries: RetryConfig::default(),
                            extra_body: Default::default(),
                            extra_headers: Default::default(),
                            stop_sequences: None,
                        },
                    ),
                    timeouts: None,
                },
            );

            let llm_judge_config = UninitializedLLMJudgeConfig {
                input_format: LLMJudgeInputFormat::Serialized,
                variants,
                output_type: LLMJudgeOutputType::Boolean,
                optimize: LLMJudgeOptimize::Min,
                include: LLMJudgeIncludeConfig {
                    reference_output: false,
                },
                cutoff: None,
            };

            let mut evaluators = HashMap::new();
            evaluators.insert(
                "llm_judge_evaluation".to_string(),
                UninitializedEvaluatorConfig::LLMJudge(llm_judge_config),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: function_name.to_string(),
            };

            let (config, additional_functions, metric_configs) = uninitialized_config
                .load(&functions, evaluation_name)
                .unwrap();
            assert_eq!(config.evaluators.len(), 1);

            // Verify LLM judge evaluator config
            match config.evaluators.get("llm_judge_evaluation").unwrap() {
                EvaluatorConfig::LLMJudge(judge_config) => {
                    assert!(matches!(
                        judge_config.output_type,
                        LLMJudgeOutputType::Boolean
                    ));
                    assert!(matches!(judge_config.optimize, LLMJudgeOptimize::Min));
                    assert!(!judge_config.include.reference_output);
                }
                _ => panic!("Expected LLMJudge evaluator config"),
            }

            // Verify additional function config was created
            assert_eq!(additional_functions.len(), 1);
            let function_name =
                get_llm_judge_function_name(evaluation_name, "llm_judge_evaluation");
            assert!(additional_functions.contains_key(&function_name));

            // Verify the function config has the correct type
            match additional_functions[&function_name].as_ref() {
                FunctionConfig::Json(json_config) => {
                    assert_eq!(json_config.variants.len(), 1);
                    assert!(json_config.variants.contains_key("test_variant"));
                    assert!(json_config.system_schema.is_none());
                    assert!(json_config.user_schema.is_some());
                    assert!(json_config.output_schema.value.is_object());
                }
                _ => panic!("Expected Json function config"),
            }

            // Verify the metrics
            assert_eq!(metric_configs.len(), 1);

            // Check the metric name follows expected format
            let metric_config_name =
                get_evaluator_metric_name(evaluation_name, "llm_judge_evaluation");
            assert_eq!(
                metric_config_name,
                "tensorzero::evaluation_name::test_evaluation::evaluator_name::llm_judge_evaluation"
            );
            assert!(metric_configs.contains_key(&metric_config_name));

            // Verify all properties of the metric config
            let metric_config = metric_configs.get(&metric_config_name).unwrap();
            assert_eq!(metric_config.r#type, MetricConfigType::Boolean);
            assert_eq!(metric_config.optimize, MetricConfigOptimize::Min);
            assert_eq!(metric_config.level, MetricConfigLevel::Inference);

            // Verify the type conversion from LLMJudgeOutputType to MetricConfigType
            let llm_judge_evaluation = match config.evaluators.get("llm_judge_evaluation").unwrap()
            {
                EvaluatorConfig::LLMJudge(config) => config,
                _ => panic!("Expected LLMJudge evaluator"),
            };
            assert_eq!(
                MetricConfigType::from(llm_judge_evaluation.output_type),
                metric_config.r#type
            );

            // Verify the optimize conversion from LLMJudgeOptimize to MetricConfigOptimize
            assert_eq!(
                MetricConfigOptimize::from(llm_judge_evaluation.optimize),
                metric_config.optimize
            );
        }

        // Test case 2.1: Successful loading with LLM judge evaluator with Float output type
        {
            let mut variants = HashMap::new();
            variants.insert(
                "test_variant".to_string(),
                UninitializedLLMJudgeVariantInfo {
                    inner: UninitializedLLMJudgeVariantConfig::ChatCompletion(
                        UninitializedLLMJudgeChatCompletionVariantConfig {
                            active: Some(true),
                            model: Arc::from("gpt-3.5-turbo"),
                            system_instructions:
                                "fixtures/config/evaluations/evaluation1/llm_judge_bool/system_instructions.txt"
                                    .into(),
                            temperature: Some(0.7),
                            top_p: None,
                            max_tokens: Some(100),
                            presence_penalty: None,
                            frequency_penalty: None,
                            seed: None,
                            json_mode: JsonMode::ImplicitTool,
                            retries: RetryConfig::default(),
                            extra_body: Default::default(),
                            extra_headers: Default::default(),
                            stop_sequences: None,
                        },
                    ),
                    timeouts: None,
                },
            );

            let llm_judge_config = UninitializedLLMJudgeConfig {
                input_format: LLMJudgeInputFormat::Serialized,
                variants,
                output_type: LLMJudgeOutputType::Float,
                optimize: LLMJudgeOptimize::Max,
                include: LLMJudgeIncludeConfig {
                    reference_output: true,
                },
                cutoff: None,
            };

            let mut evaluators = HashMap::new();
            evaluators.insert(
                "llm_judge_float".to_string(),
                UninitializedEvaluatorConfig::LLMJudge(llm_judge_config),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: function_name.to_string(),
            };

            let (config, additional_functions, metric_configs) = uninitialized_config
                .load(&functions, evaluation_name)
                .unwrap();
            assert_eq!(config.evaluators.len(), 1);

            // Verify LLM judge evaluator config
            match config.evaluators.get("llm_judge_float").unwrap() {
                EvaluatorConfig::LLMJudge(judge_config) => {
                    assert!(matches!(
                        judge_config.output_type,
                        LLMJudgeOutputType::Float
                    ));
                    assert!(matches!(judge_config.optimize, LLMJudgeOptimize::Max));
                    assert!(judge_config.include.reference_output);
                }
                _ => panic!("Expected LLMJudge evaluator config"),
            }

            // Verify additional function config was created
            assert_eq!(additional_functions.len(), 1);
            let function_name = get_llm_judge_function_name(evaluation_name, "llm_judge_float");
            assert!(additional_functions.contains_key(&function_name));

            // Verify the metrics
            assert_eq!(metric_configs.len(), 1);

            // Check the metric name follows expected format
            let metric_config_name = get_evaluator_metric_name(evaluation_name, "llm_judge_float");
            assert_eq!(
                metric_config_name,
                "tensorzero::evaluation_name::test_evaluation::evaluator_name::llm_judge_float"
            );
            assert!(metric_configs.contains_key(&metric_config_name));

            // Verify all properties of the metric config
            let metric_config = metric_configs.get(&metric_config_name).unwrap();
            assert_eq!(metric_config.r#type, MetricConfigType::Float);
            assert_eq!(metric_config.optimize, MetricConfigOptimize::Max);
            assert_eq!(metric_config.level, MetricConfigLevel::Inference);

            // Verify the type conversion from LLMJudgeOutputType to MetricConfigType
            let llm_judge_evaluation = match config.evaluators.get("llm_judge_float").unwrap() {
                EvaluatorConfig::LLMJudge(config) => config,
                _ => panic!("Expected LLMJudge evaluator"),
            };
            assert_eq!(
                MetricConfigType::from(llm_judge_evaluation.output_type),
                metric_config.r#type
            );

            // Verify the optimize conversion from LLMJudgeOptimize to MetricConfigOptimize
            assert_eq!(
                MetricConfigOptimize::from(llm_judge_evaluation.optimize),
                metric_config.optimize
            );
        }

        // Test case 3: Error when function doesn't exist
        {
            let mut evaluators = HashMap::new();
            evaluators.insert(
                "em_evaluator".to_string(),
                UninitializedEvaluatorConfig::ExactMatch(ExactMatchConfig { cutoff: None }),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: "nonexistent_function".to_string(),
            };

            let result = uninitialized_config.load(&functions, evaluation_name);
            assert!(result.is_err());
            assert!(matches!(
                *result.unwrap_err().get_details(),
                ErrorDetails::Config { .. }
            ));
        }

        // Test case 4: Error when evaluation name contains "::"
        {
            let mut evaluators = HashMap::new();
            evaluators.insert(
                "em_evaluator".to_string(),
                UninitializedEvaluatorConfig::ExactMatch(ExactMatchConfig { cutoff: None }),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: function_name.to_string(),
            };

            let result = uninitialized_config.load(&functions, "invalid::evaluation::name");
            assert!(result.is_err());
            assert!(matches!(
                *result.unwrap_err().get_details(),
                ErrorDetails::Config { .. }
            ));
        }

        // Test case 5: Error when multiple variants are active in LLM judge
        {
            let mut test_variant1 = HashMap::new();
            test_variant1.insert(
                "test_variant1".to_string(),
                UninitializedLLMJudgeVariantInfo {
                    inner: UninitializedLLMJudgeVariantConfig::ChatCompletion(
                        UninitializedLLMJudgeChatCompletionVariantConfig {
                            active: Some(true),
                            model: Arc::from("gpt-3.5-turbo"),
                            system_instructions:
                                "fixtures/config/evaluations/evaluation1/llm_judge_bool/system_instructions.txt"
                                    .into(),
                            temperature: Some(0.7),
                            top_p: None,
                            max_tokens: Some(100),
                            presence_penalty: None,
                            frequency_penalty: None,
                            seed: None,
                            json_mode: JsonMode::ImplicitTool,
                            retries: RetryConfig::default(),
                            extra_body: Default::default(),
                            extra_headers: Default::default(),
                            stop_sequences: None,
                        },
                    ),
                    timeouts: None,
                },
            );

            let mut test_variant2 = HashMap::new();
            test_variant2.insert(
                "test_variant2".to_string(),
                UninitializedLLMJudgeVariantInfo {
                    inner: UninitializedLLMJudgeVariantConfig::ChatCompletion(
                        UninitializedLLMJudgeChatCompletionVariantConfig {
                            active: Some(true),
                            model: Arc::from("gpt-4"),
                            system_instructions: TomlRelativePath::new_for_tests(PathBuf::from(
                                "fixtures/config/evaluations/evaluation1/llm_judge_bool/system_instructions.txt",
                            ), None),
                            temperature: Some(0.5),
                            top_p: None,
                            max_tokens: Some(200),
                            presence_penalty: None,
                            frequency_penalty: None,
                            seed: None,
                            json_mode: JsonMode::ImplicitTool,
                            retries: RetryConfig::default(),
                            extra_body: Default::default(),
                            extra_headers: Default::default(),
                            stop_sequences: None,
                        },
                    ),
                    timeouts: None,
                },
            );

            // Combine the two variants
            let mut variants = HashMap::new();
            for (k, v) in test_variant1 {
                variants.insert(k, v);
            }
            for (k, v) in test_variant2 {
                variants.insert(k, v);
            }

            let llm_judge_config = UninitializedLLMJudgeConfig {
                input_format: LLMJudgeInputFormat::Serialized,
                variants,
                output_type: LLMJudgeOutputType::Boolean,
                optimize: LLMJudgeOptimize::Min,
                include: LLMJudgeIncludeConfig {
                    reference_output: false,
                },
                cutoff: Some(0.3),
            };

            let mut evaluators = HashMap::new();
            evaluators.insert(
                "multiple_active_variants".to_string(),
                UninitializedEvaluatorConfig::LLMJudge(llm_judge_config),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: function_name.to_string(),
            };

            let result = uninitialized_config.load(&functions, evaluation_name);
            assert!(result.is_err());
            assert_eq!(
                *result.unwrap_err().get_details(),
                ErrorDetails::Config {
                    message: "Evaluator `multiple_active_variants` in `[evaluations.test_evaluation]` must have exactly 1 variant that is active. Found 2 variants with nonzero weights.".to_string(),
                }
            );
        }

        // Test case 6: Error when evaluator name contains "::"
        {
            let evaluation_name = "test_evaluation";
            let function_name = "test_function";

            let mut functions = HashMap::new();
            functions.insert(
                function_name.to_string(),
                Arc::new(FunctionConfig::Json(FunctionConfigJson {
                    variants: HashMap::new(),
                    output_schema: create_test_schema(),
                    system_schema: None,
                    user_schema: None,
                    assistant_schema: None,
                    implicit_tool_call_config: create_implicit_tool_call_config(
                        create_test_schema(),
                    ),
                    description: None,
                })),
            );

            let mut evaluators = HashMap::new();
            evaluators.insert(
                "foo::invalid_name".to_string(),
                UninitializedEvaluatorConfig::ExactMatch(ExactMatchConfig { cutoff: None }),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: function_name.to_string(),
            };

            let result = uninitialized_config.load(&functions, evaluation_name);
            assert!(result.is_err());
            assert_eq!(
                *result.unwrap_err().get_details(),
                ErrorDetails::Config {
                    message:
                        "Evaluator names cannot contain \"::\" (referenced in `[evaluations.test_evaluation.foo::invalid_name]`)"
                            .to_string(),
                }
            );
        }

        // Test case 7: Successful loading with LLM judge evaluator with reference_output = true
        {
            let mut variants = HashMap::new();
            variants.insert(
                "test_variant".to_string(),
                UninitializedLLMJudgeVariantInfo {
                    inner: UninitializedLLMJudgeVariantConfig::ChatCompletion(
                        UninitializedLLMJudgeChatCompletionVariantConfig {
                            active: Some(true),
                            model: Arc::from("gpt-3.5-turbo"),
                            system_instructions: TomlRelativePath::new_for_tests(PathBuf::from(
                                "fixtures/config/evaluations/evaluation1/llm_judge_bool/system_instructions.txt",
                            ), None),
                            temperature: Some(0.7),
                            top_p: None,
                            max_tokens: Some(100),
                            presence_penalty: None,
                            frequency_penalty: None,
                            seed: None,
                            json_mode: JsonMode::ImplicitTool,
                            retries: RetryConfig::default(),
                            extra_body: Default::default(),
                            extra_headers: Default::default(),
                            stop_sequences: None,
                        },
                    ),
                    timeouts: None,
                },
            );

            let llm_judge_config = UninitializedLLMJudgeConfig {
                input_format: LLMJudgeInputFormat::Serialized,
                variants,
                output_type: LLMJudgeOutputType::Boolean,
                optimize: LLMJudgeOptimize::Min,
                include: LLMJudgeIncludeConfig {
                    reference_output: true,
                },
                cutoff: None,
            };

            let mut evaluators = HashMap::new();
            evaluators.insert(
                "llm_judge_with_ref".to_string(),
                UninitializedEvaluatorConfig::LLMJudge(llm_judge_config),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: function_name.to_string(),
            };

            let result = uninitialized_config.load(&functions, evaluation_name);
            assert!(result.is_ok());

            let (config, _additional_functions, _metric_configs) = result.unwrap();

            // Verify LLM judge evaluator config with reference_output = true
            match config.evaluators.get("llm_judge_with_ref").unwrap() {
                EvaluatorConfig::LLMJudge(judge_config) => {
                    assert!(matches!(
                        judge_config.output_type,
                        LLMJudgeOutputType::Boolean
                    ));
                    assert!(matches!(judge_config.optimize, LLMJudgeOptimize::Min));
                    assert!(judge_config.include.reference_output);
                }
                _ => panic!("Expected LLMJudge evaluator config"),
            }
        }

        // Test case 8: Single LLM Judge variant with no 'active' field specified (defaults to active)
        {
            let mut variants = HashMap::new();
            variants.insert(
                "default_active_variant".to_string(),
                UninitializedLLMJudgeVariantInfo {
                    inner: UninitializedLLMJudgeVariantConfig::ChatCompletion(
                        UninitializedLLMJudgeChatCompletionVariantConfig {
                            active: None, // No 'active' field specified
                            model: Arc::from("gpt-3.5-turbo"),
                            system_instructions: TomlRelativePath::new_for_tests(PathBuf::from(
                                "fixtures/config/evaluations/evaluation1/llm_judge_bool/system_instructions.txt",
                            ), None),
                            temperature: Some(0.7),
                            top_p: None,
                            max_tokens: Some(100),
                            presence_penalty: None,
                            frequency_penalty: None,
                            seed: None,
                            json_mode: JsonMode::ImplicitTool,
                            retries: RetryConfig::default(),
                            extra_body: Default::default(),
                            extra_headers: Default::default(),
                            stop_sequences: None,
                        },
                    ),
                    timeouts: None,
                },
            );

            let llm_judge_config = UninitializedLLMJudgeConfig {
                input_format: LLMJudgeInputFormat::Serialized,
                variants,
                output_type: LLMJudgeOutputType::Boolean,
                optimize: LLMJudgeOptimize::Max,
                include: LLMJudgeIncludeConfig::default(),
                cutoff: None,
            };

            let mut evaluators = HashMap::new();
            evaluators.insert(
                "llm_judge_default_active".to_string(),
                UninitializedEvaluatorConfig::LLMJudge(llm_judge_config),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: function_name.to_string(),
            };

            let result = uninitialized_config.load(&functions, evaluation_name);
            assert!(result.is_ok());

            let (_config, additional_functions, _metric_configs) = result.unwrap();
            let function_config_name =
                get_llm_judge_function_name(evaluation_name, "llm_judge_default_active");
            let function_config = additional_functions.get(&function_config_name).unwrap();
            match function_config.as_ref() {
                FunctionConfig::Json(json_config) => {
                    assert_eq!(json_config.variants.len(), 1);
                    let variant = json_config.variants.get("default_active_variant").unwrap();
                    // Check that the weight is Some(1.0) which indicates it defaulted to active
                    match &variant.inner {
                        VariantConfig::ChatCompletion(cc_config) => {
                            assert_eq!(cc_config.weight, Some(1.0));
                        }
                        _ => panic!("Expected ChatCompletion variant config"),
                    }
                }
                _ => panic!("Expected Json function config"),
            }
        }

        // Test case 9: Single LLM Judge variant explicitly set to inactive (active = false)
        {
            let mut variants = HashMap::new();
            variants.insert(
                "inactive_variant".to_string(),
                UninitializedLLMJudgeVariantInfo {
                    inner: UninitializedLLMJudgeVariantConfig::ChatCompletion(
                        UninitializedLLMJudgeChatCompletionVariantConfig {
                            active: Some(false), // Explicitly inactive
                            model: Arc::from("gpt-3.5-turbo"),
                            system_instructions: TomlRelativePath::new_for_tests(PathBuf::from(
                                "fixtures/config/evaluations/evaluation1/llm_judge_bool/system_instructions.txt",
                            ), None),
                            temperature: Some(0.7),
                            top_p: None,
                            max_tokens: Some(100),
                            presence_penalty: None,
                            frequency_penalty: None,
                            seed: None,
                            json_mode: JsonMode::ImplicitTool,
                            retries: RetryConfig::default(),
                            extra_body: Default::default(),
                            extra_headers: Default::default(),
                            stop_sequences: None,
                        },
                    ),
                    timeouts: None,
                },
            );

            let llm_judge_config = UninitializedLLMJudgeConfig {
                input_format: LLMJudgeInputFormat::Serialized,
                variants,
                output_type: LLMJudgeOutputType::Boolean,
                optimize: LLMJudgeOptimize::Max,
                include: LLMJudgeIncludeConfig::default(),
                cutoff: None,
            };

            let mut evaluators = HashMap::new();
            evaluators.insert(
                "llm_judge_inactive".to_string(),
                UninitializedEvaluatorConfig::LLMJudge(llm_judge_config),
            );

            let uninitialized_config = UninitializedStaticEvaluationConfig {
                evaluators,
                function_name: function_name.to_string(),
            };

            let result = uninitialized_config.load(&functions, evaluation_name);
            assert!(result.is_err());
            assert_eq!(
                *result.unwrap_err().get_details(),
                ErrorDetails::Config {
                    message: format!("Evaluator `llm_judge_inactive` in `[evaluations.{evaluation_name}]` must have exactly 1 variant that is active. You have specified a single inactive variant."),
                }
            );
        }
    }

    // Helper functions for tests
    fn create_test_schema() -> StaticJSONSchema {
        let schema_value = serde_json::json!({
            "type": "object",
            "properties": {
                "result": {
                    "type": "string"
                }
            },
            "required": ["result"]
        });
        StaticJSONSchema::from_value(&schema_value).unwrap()
    }
}
