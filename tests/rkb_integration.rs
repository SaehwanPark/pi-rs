//! Phase 7 gate fixtures for the first-party RKB adapter.

use std::sync::Arc;

use rupi_core::Tool;
use rupi_mcp::{McpManager, MockTransport};
use rupi_rkb::{RkbAdapter, RkbContext, RkbSetup};
use serde_json::json;

fn context_fixture() -> serde_json::Value {
  json!({
    "query": "BENE_ID",
    "result_count": 1,
    "entries": [{
      "citation": "[1]",
      "record_id": "chunk-42",
      "record_type": "chunk",
      "title": "Beneficiary identifier",
      "dataset_id": "carrier-ffs",
      "score": 0.97,
      "snippet": "The beneficiary identifier is stable across this source.",
      "source_url": "https://example.test/cms/bene-id",
      "source_document": "variables.md",
      "page": 7
    }]
  })
}

fn mock_rkb() -> Arc<MockTransport> {
  let mock = Arc::new(MockTransport::new());
  mock.on(
    "initialize",
    json!({
      "protocolVersion": "2024-11-05",
      "capabilities": { "tools": {} },
      "serverInfo": { "name": "rkb", "version": "0.1.0" }
    }),
  );
  mock.on(
    "tools/list",
    json!({
      "tools": [
        { "name": "get_agent_context", "inputSchema": { "type": "object" } },
        { "name": "search_chunks", "inputSchema": { "type": "object" } }
      ]
    }),
  );
  mock.on(
    "tools/call",
    json!({
      "content": [{ "type": "text", "text": context_fixture().to_string() }],
      "isError": false
    }),
  );
  mock
}

#[test]
fn rkb_setup_is_discovered_without_starting_mcp() {
  let setup = RkbSetup::discover(&[RkbSetup::new().server_config()]).expect("RKB is found");
  let manager = McpManager::new(vec![setup.server_config()]);
  assert!(!manager.is_active(setup.server_name()));
  assert_eq!(manager.all_active_tools().len(), 0);
}

#[test]
fn rkb_raw_config_is_normalized_before_activation() {
  let raw = rupi_core::McpServerConfig::new("rkb", "rkb").with_args(vec!["mcp".into()]);
  let normalized = RkbSetup::normalize_configs(&[raw]);
  let mut manager = McpManager::new(normalized);
  let tools = manager
    .enable_server_with_transport("rkb", mock_rkb())
    .expect("normalized config activates");
  assert!(tools.iter().all(|tool| tool.metadata().read_only));
}

#[test]
fn rkb_gate_retrieves_compacts_and_rehydrates_with_provenance() {
  let setup = RkbSetup::new();
  let mut manager = McpManager::new(vec![setup.server_config()]);
  let connection = RkbAdapter::new()
    .activate_with_transport(&mut manager, mock_rkb())
    .expect("explicit activation succeeds");
  let context = connection.search("BENE_ID", 1).expect("retrieval succeeds");
  let inline = context.external_context_items().pop().expect("one item");
  let reference = context
    .compact_to_references()
    .pop()
    .expect("one reference");

  assert!(inline.inline);
  assert_eq!(inline.source.resource_id, "chunk-42");
  assert!(inline.citation.as_deref().unwrap().contains("page=7"));
  assert_eq!(
    inline.metadata["source_url"],
    "https://example.test/cms/bene-id"
  );
  assert!(!reference.external_context_item().inline);
  assert_eq!(reference.as_external_ref().resource_id, "chunk-42");
  assert_eq!(reference.as_external_ref().provider, "rkb-rs");

  let generic_reference = reference.as_external_ref();
  let rehydrated = connection
    .resolve_external_ref(&generic_reference)
    .expect("resource rehydrates");
  assert!(rehydrated.inline);
  assert_eq!(rehydrated.source.resource_id, inline.source.resource_id);
  assert_eq!(rehydrated.source.provenance, inline.source.provenance);
  assert_eq!(rehydrated.citation, inline.citation);
  assert_eq!(rehydrated.metadata, inline.metadata);
}

#[test]
fn rkb_context_rejects_mismatched_result_count() {
  let mut fixture = context_fixture();
  fixture["result_count"] = json!(2);
  let error = RkbContext::parse_json(&fixture.to_string()).unwrap_err();
  assert!(error.to_string().contains("result_count"));
}
