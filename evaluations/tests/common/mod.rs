#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use tensorzero::{
    ChatInferenceDatapoint, Client, ClientBuilder, ClientBuilderMode, JsonInferenceDatapoint,
};
use tensorzero_core::clickhouse::{
    test_helpers::{get_clickhouse, CLICKHOUSE_URL},
    TableName,
};
use uuid::Uuid;

/// Takes a chat fixture as a path to a JSONL file and writes the fixture to the dataset.
/// To avoid trampling between tests, we use a mapping from the fixture dataset names to the actual dataset names
/// that are inserted. This way, we can have multiple tests reading the same fixtures, using the same database,
/// but run independently.
pub async fn write_chat_fixture_to_dataset(
    fixture_path: &Path,
    dataset_name_mapping: &HashMap<String, String>,
) {
    let fixture = std::fs::read_to_string(fixture_path).unwrap();
    let fixture = fixture.trim();
    let mut datapoints: Vec<ChatInferenceDatapoint> = Vec::new();
    // Iterate over the lines in the string
    for line in fixture.lines() {
        let mut datapoint: ChatInferenceDatapoint = serde_json::from_str(line).unwrap();
        datapoint.id = Uuid::now_v7();
        if let Some(dataset_name) = dataset_name_mapping.get(&datapoint.dataset_name) {
            datapoint.dataset_name = dataset_name.to_string();
        }
        datapoints.push(datapoint);
    }
    let clickhouse = get_clickhouse().await;
    clickhouse
        .write(&datapoints, TableName::ChatInferenceDatapoint)
        .await
        .unwrap();
}

/// Takes a JSON fixture as a path to a JSONL file and writes the fixture to the dataset.
pub async fn write_json_fixture_to_dataset(
    fixture_path: &Path,
    dataset_name_mapping: &HashMap<String, String>,
) {
    let fixture = std::fs::read_to_string(fixture_path).unwrap();
    let fixture = fixture.trim();
    let mut datapoints: Vec<JsonInferenceDatapoint> = Vec::new();
    // Iterate over the lines in the string
    for line in fixture.lines() {
        let mut datapoint: JsonInferenceDatapoint = serde_json::from_str(line).unwrap();
        datapoint.id = Uuid::now_v7();
        if let Some(dataset_name) = dataset_name_mapping.get(&datapoint.dataset_name) {
            datapoint.dataset_name = dataset_name.to_string();
        }
        datapoints.push(datapoint);
    }
    let clickhouse = get_clickhouse().await;
    clickhouse
        .write(&datapoints, TableName::JsonInferenceDatapoint)
        .await
        .unwrap();
}

pub async fn get_tensorzero_client() -> Client {
    ClientBuilder::new(ClientBuilderMode::EmbeddedGateway {
        config_file: Some(PathBuf::from(&format!(
            "{}/../tensorzero-core/tests/e2e/tensorzero.toml",
            std::env::var("CARGO_MANIFEST_DIR").unwrap()
        ))),
        clickhouse_url: Some(CLICKHOUSE_URL.clone()),
        timeout: None,
        verify_credentials: true,
    })
    .build()
    .await
    .unwrap()
}
