//! Official, optional adapter for the `rkb mcp` knowledge service.
//!
//! This crate deliberately depends only on the generic pi-rs MCP and context
//! contracts. Constructing setup and adapter values performs no I/O; a server
//! is connected only by [`RkbAdapter::activate`].

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, fmt, sync::Arc};

use pi_rs_core::{
  ExternalContextItem, ExternalContextRef, ExternalContextSource, McpServerConfig, Tool,
  ToolExecutionState, ToolProgress, ToolRequest,
};
use pi_rs_mcp::{CallToolResult, McpError, McpManager, McpTool, McpTransport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const RKB_SERVER_NAME: &str = "rkb";
pub const GET_AGENT_CONTEXT: &str = "get_agent_context";
pub const SEARCH_DATASETS: &str = "search_datasets";
pub const SEARCH_DOCUMENTS: &str = "search_documents";
pub const SEARCH_VARIABLES: &str = "search_variables";
pub const SEARCH_CHUNKS: &str = "search_chunks";
pub const RKB_PROVIDER: &str = "rkb-rs";
pub const RKB_PROVENANCE: &str = "rkb-rs/citation";

/// Bundled instructions for an agent deciding when and how to use RKB.
pub const SKILL: &str = include_str!("../SKILL.md");

/// Pure RKB setup/discovery information. No process or network is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RkbSetup {
  config: McpServerConfig,
  skill: &'static str,
}

impl Default for RkbSetup {
  fn default() -> Self {
    Self::new()
  }
}

impl RkbSetup {
  pub fn new() -> Self {
    Self::from_config(
      &McpServerConfig::new(RKB_SERVER_NAME, "rkb")
        .with_args(vec!["mcp".into()])
        .with_enabled(false),
    )
    .expect("the built-in RKB setup is recognized")
  }

  /// Recognize a configured RKB stdio/HTTP server without activating it.
  ///
  /// Names `rkb`/`rkb-rs` and commands whose basename is `rkb`/`rkb-rs` are
  /// accepted. The original transport, arguments, and headers are retained so
  /// discovery never silently replaces a caller-owned endpoint.
  pub fn from_config(config: &McpServerConfig) -> Option<Self> {
    let name = config.name.trim().to_ascii_lowercase();
    let command = std::path::Path::new(config.command.trim())
      .file_name()
      .and_then(|value| value.to_str())
      .unwrap_or(config.command.trim())
      .to_ascii_lowercase();
    let recognized =
      matches!(name.as_str(), "rkb" | "rkb-rs") || matches!(command.as_str(), "rkb" | "rkb-rs");
    if !recognized {
      return None;
    }
    let mut config = config.clone();
    config.read_only_tools = merge_read_only_tools(&config.read_only_tools);
    Some(Self {
      config,
      skill: SKILL,
    })
  }

  pub fn discover(configs: &[McpServerConfig]) -> Option<Self> {
    configs.iter().find_map(Self::from_config)
  }

  /// Normalize configured RKB retrieval tools before handing servers to the
  /// composition root's manager. This remains a pure clone-and-replace step;
  /// it does not enable or connect any server.
  pub fn normalize_configs(configs: &[McpServerConfig]) -> Vec<McpServerConfig> {
    let mut normalized = configs.to_vec();
    if let Some(setup) = Self::discover(configs) {
      if let Some(server) = normalized
        .iter_mut()
        .find(|server| server.name == setup.server_name())
      {
        *server = setup.server_config();
      }
    }
    normalized
  }

  pub fn server_name(&self) -> &str {
    &self.config.name
  }

  pub fn command(&self) -> &str {
    &self.config.command
  }

  pub fn args(&self) -> &[String] {
    &self.config.args
  }

  pub fn skill(&self) -> &str {
    self.skill
  }

  /// Configuration consumed by [`McpManager`]. It remains disabled until an
  /// explicit activation call, preserving lazy startup behavior.
  pub fn server_config(&self) -> McpServerConfig {
    self.config.clone()
  }
}

fn merge_read_only_tools(existing: &[String]) -> Vec<String> {
  let mut tools = existing.to_vec();
  for required in [
    GET_AGENT_CONTEXT,
    SEARCH_DATASETS,
    SEARCH_DOCUMENTS,
    SEARCH_VARIABLES,
    SEARCH_CHUNKS,
  ] {
    if !tools.iter().any(|tool| tool == required) {
      tools.push(required.into());
    }
  }
  tools
}

/// Citation and source metadata returned by RKB.
///
/// The citation is kept as a value on each entry (rather than rendered prose)
/// so source coordinates can survive compaction and serialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RkbContextEntry {
  pub citation: String,
  pub record_id: String,
  pub record_type: String,
  pub title: String,
  pub dataset_id: String,
  pub score: f64,
  pub snippet: String,
  #[serde(default)]
  pub source_url: Option<String>,
  #[serde(default)]
  pub source_document: Option<String>,
  #[serde(default)]
  pub page: Option<u64>,
}

impl RkbContextEntry {
  fn validate(&self) -> Result<(), RkbError> {
    if self.record_id.trim().is_empty() {
      return Err(RkbError::MalformedResponse(
        "entry has empty record_id".into(),
      ));
    }
    if self.citation.trim().is_empty() {
      return Err(RkbError::MalformedResponse(format!(
        "entry '{}' has empty citation",
        self.record_id
      )));
    }
    Ok(())
  }

  /// A stable citation label retaining every available source coordinate.
  pub fn citation_label(&self) -> String {
    if self.citation.contains("; url=")
      || self.citation.contains("; document=")
      || self.citation.contains("; page=")
    {
      return self.citation.clone();
    }
    let mut label = self.citation.clone();
    if let Some(url) = &self.source_url {
      label.push_str("; url=");
      label.push_str(url);
    }
    if let Some(document) = &self.source_document {
      label.push_str("; document=");
      label.push_str(document);
    }
    if let Some(page) = self.page {
      label.push_str("; page=");
      label.push_str(&page.to_string());
    }
    label
  }

  pub fn source(&self) -> ExternalContextSource {
    ExternalContextSource {
      provider: RKB_PROVIDER.into(),
      resource_id: self.record_id.clone(),
      provenance: RKB_PROVENANCE.into(),
    }
  }

  /// Convert evidence to model-visible inline context.
  fn source_metadata(&self) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    if let Some(url) = &self.source_url {
      metadata.insert("source_url".into(), url.clone());
    }
    if let Some(document) = &self.source_document {
      metadata.insert("source_document".into(), document.clone());
    }
    if let Some(page) = self.page {
      metadata.insert("page".into(), page.to_string());
    }
    metadata.insert("record_type".into(), self.record_type.clone());
    metadata.insert("title".into(), self.title.clone());
    metadata.insert("dataset_id".into(), self.dataset_id.clone());
    metadata.insert("score".into(), self.score.to_string());
    metadata
  }

  pub fn external_context_item(&self) -> ExternalContextItem {
    ExternalContextItem::inline(
      self.source(),
      self.snippet.clone(),
      Some(self.citation_label()),
    )
    .with_metadata(self.source_metadata())
  }

  pub fn compact_to_reference(&self) -> RkbReference {
    RkbReference {
      citation: self.citation.clone(),
      record_id: self.record_id.clone(),
      record_type: self.record_type.clone(),
      title: self.title.clone(),
      dataset_id: self.dataset_id.clone(),
      score: self.score,
      source_url: self.source_url.clone(),
      source_document: self.source_document.clone(),
      page: self.page,
    }
  }
}

/// Parsed `get_agent_context` response.
#[derive(Debug, Clone, Deserialize)]
struct RkbSearchResult {
  record_id: String,
  record_type: String,
  title: String,
  dataset_id: String,
  score: f64,
  snippet: String,
  source_url: String,
  source_document: String,
  page: Option<u64>,
}

impl RkbSearchResult {
  fn into_entry(self, reference: &RkbReference) -> RkbContextEntry {
    RkbContextEntry {
      citation: reference.citation.clone(),
      record_id: self.record_id,
      record_type: if reference.record_type.is_empty() {
        self.record_type
      } else {
        reference.record_type.clone()
      },
      title: if reference.title.is_empty() {
        self.title
      } else {
        reference.title.clone()
      },
      dataset_id: if reference.dataset_id.is_empty() {
        self.dataset_id
      } else {
        reference.dataset_id.clone()
      },
      score: self.score,
      snippet: self.snippet,
      source_url: optional_source(self.source_url, reference.source_url.clone()),
      source_document: optional_source(self.source_document, reference.source_document.clone()),
      page: self.page.or(reference.page),
    }
  }
}

fn optional_source(value: String, fallback: Option<String>) -> Option<String> {
  if value.trim().is_empty() {
    fallback.filter(|value| !value.trim().is_empty())
  } else {
    Some(value)
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RkbContext {
  pub query: String,
  pub result_count: usize,
  pub entries: Vec<RkbContextEntry>,
}

impl RkbContext {
  /// Parse the text payload from an MCP tool result.
  pub fn from_call_result(result: &CallToolResult) -> Result<Self, RkbError> {
    if result.is_error == Some(true) {
      return Err(RkbError::MalformedResponse(
        "RKB tool returned isError".into(),
      ));
    }
    Self::parse_json(&result.text_content())
  }

  pub fn parse_json(text: &str) -> Result<Self, RkbError> {
    let context: Self = serde_json::from_str(text).map_err(|error| {
      RkbError::MalformedResponse(format!("invalid get_agent_context JSON: {error}"))
    })?;
    if context.result_count != context.entries.len() {
      return Err(RkbError::MalformedResponse(format!(
        "result_count {} does not match {} entries",
        context.result_count,
        context.entries.len()
      )));
    }
    for entry in &context.entries {
      entry.validate()?;
    }
    Ok(context)
  }

  pub fn compact_to_references(&self) -> Vec<RkbReference> {
    self
      .entries
      .iter()
      .map(RkbContextEntry::compact_to_reference)
      .collect()
  }

  pub fn external_context_items(&self) -> Vec<ExternalContextItem> {
    self
      .entries
      .iter()
      .map(RkbContextEntry::external_context_item)
      .collect()
  }
}

/// A citation is represented by the same durable shape used for compaction.
/// This alias keeps citation-oriented call sites explicit without introducing a
/// second, lossy metadata model.
pub type RkbCitation = RkbReference;

/// Durable, compact representation of one RKB result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RkbReference {
  pub citation: String,
  pub record_id: String,
  pub record_type: String,
  pub title: String,
  pub dataset_id: String,
  pub score: f64,
  pub source_url: Option<String>,
  pub source_document: Option<String>,
  pub page: Option<u64>,
}

impl RkbReference {
  /// Recover the RKB-specific reference shape from the provider-neutral core
  /// reference kept in session state.
  pub fn from_external_ref(reference: &ExternalContextRef) -> Result<Self, RkbError> {
    if reference.provider != RKB_PROVIDER {
      return Err(RkbError::MalformedResponse(format!(
        "cannot resolve provider '{}' as RKB",
        reference.provider
      )));
    }
    if reference.resource_id.trim().is_empty() {
      return Err(RkbError::MalformedResponse(
        "RKB reference has an empty resource id".into(),
      ));
    }
    let page = reference
      .metadata
      .get("page")
      .map(|value| {
        value.parse::<u64>().map_err(|error| {
          RkbError::MalformedResponse(format!("RKB reference page is invalid: {error}"))
        })
      })
      .transpose()?;
    Ok(Self {
      citation: reference
        .citation
        .clone()
        .unwrap_or_else(|| reference.resource_id.clone()),
      record_id: reference.resource_id.clone(),
      record_type: reference
        .metadata
        .get("record_type")
        .cloned()
        .unwrap_or_default(),
      title: reference.metadata.get("title").cloned().unwrap_or_default(),
      dataset_id: reference
        .metadata
        .get("dataset_id")
        .cloned()
        .unwrap_or_default(),
      score: reference
        .metadata
        .get("score")
        .and_then(|value| value.parse().ok())
        .unwrap_or_default(),
      source_url: reference.metadata.get("source_url").cloned(),
      source_document: reference.metadata.get("source_document").cloned(),
      page,
    })
  }

  pub fn citation_label(&self) -> String {
    if self.citation.contains("; url=")
      || self.citation.contains("; document=")
      || self.citation.contains("; page=")
    {
      return self.citation.clone();
    }
    RkbContextEntry {
      citation: self.citation.clone(),
      record_id: self.record_id.clone(),
      record_type: self.record_type.clone(),
      title: self.title.clone(),
      dataset_id: self.dataset_id.clone(),
      score: self.score,
      snippet: String::new(),
      source_url: self.source_url.clone(),
      source_document: self.source_document.clone(),
      page: self.page,
    }
    .citation_label()
  }

  pub fn external_context_item(&self) -> ExternalContextItem {
    let mut metadata = BTreeMap::new();
    if let Some(url) = &self.source_url {
      metadata.insert("source_url".into(), url.clone());
    }
    if let Some(document) = &self.source_document {
      metadata.insert("source_document".into(), document.clone());
    }
    if let Some(page) = self.page {
      metadata.insert("page".into(), page.to_string());
    }
    metadata.insert("record_type".into(), self.record_type.clone());
    metadata.insert("title".into(), self.title.clone());
    metadata.insert("dataset_id".into(), self.dataset_id.clone());
    metadata.insert("score".into(), self.score.to_string());
    ExternalContextItem::reference(
      ExternalContextSource {
        provider: RKB_PROVIDER.into(),
        resource_id: self.record_id.clone(),
        provenance: RKB_PROVENANCE.into(),
      },
      Some(self.citation_label()),
    )
    .with_metadata(metadata)
  }

  /// Convert to pi-rs' provider-neutral durable reference contract.
  pub fn as_external_ref(&self) -> ExternalContextRef {
    ExternalContextRef::new(
      RKB_PROVIDER,
      self.record_id.clone(),
      RKB_PROVENANCE,
      Some(self.citation_label()),
    )
    .with_metadata(self.external_context_item().metadata)
  }
}

#[derive(Debug)]
pub enum RkbError {
  Mcp(McpError),
  MissingTool(&'static str),
  MalformedResponse(String),
  UnavailableResource(String),
}

impl fmt::Display for RkbError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Mcp(error) => write!(f, "RKB MCP error: {error}"),
      Self::MissingTool(name) => write!(f, "RKB MCP server does not expose '{name}'"),
      Self::MalformedResponse(message) => write!(f, "malformed RKB response: {message}"),
      Self::UnavailableResource(id) => write!(f, "RKB resource unavailable: {id}"),
    }
  }
}

impl std::error::Error for RkbError {}
impl From<McpError> for RkbError {
  fn from(error: McpError) -> Self {
    Self::Mcp(error)
  }
}

/// Resolve compact RKB references on demand.
pub trait RkbResolver {
  fn resolve_reference(&self, reference: &RkbReference) -> Result<ExternalContextItem, RkbError>;
}

fn parse_rehydration_results(
  text: &str,
  reference: &RkbReference,
) -> Result<Vec<RkbContextEntry>, RkbError> {
  if text.trim_start().starts_with('[') {
    serde_json::from_str::<Vec<RkbSearchResult>>(text)
      .map_err(|error| RkbError::MalformedResponse(format!("invalid search_chunks JSON: {error}")))
      .map(|entries| {
        entries
          .into_iter()
          .map(|entry| entry.into_entry(reference))
          .collect()
      })
  } else {
    // A fixture or future RKB version may return the citation-bearing agent
    // context envelope for a search tool. Accept it only after the same
    // structural validation used by get_agent_context.
    Ok(RkbContext::parse_json(text)?.entries)
  }
}

/// Configured adapter. Activation is explicit and lazy.
#[derive(Debug, Clone, Default)]
pub struct RkbAdapter {
  setup: RkbSetup,
}

impl RkbAdapter {
  pub fn new() -> Self {
    Self {
      setup: RkbSetup::new(),
    }
  }

  pub fn with_setup(setup: RkbSetup) -> Self {
    Self { setup }
  }

  pub fn setup(&self) -> &RkbSetup {
    &self.setup
  }

  pub fn activate(&self, manager: &mut McpManager) -> Result<RkbConnection, RkbError> {
    let tools = manager.enable_server(self.setup.server_name())?;
    RkbConnection::from_tools(tools)
  }

  pub fn activate_with_transport(
    &self,
    manager: &mut McpManager,
    transport: Arc<dyn McpTransport>,
  ) -> Result<RkbConnection, RkbError> {
    let tools = manager.enable_server_with_transport(self.setup.server_name(), transport)?;
    RkbConnection::from_tools(tools)
  }
}

/// An activated RKB connection retaining only normalized MCP tools.
#[derive(Clone)]
pub struct RkbConnection {
  context_tool: McpTool,
  search_tool: McpTool,
}

impl RkbConnection {
  fn from_tools(tools: Vec<McpTool>) -> Result<Self, RkbError> {
    let context_tool = tools
      .iter()
      .find(|tool| tool.definition().name == GET_AGENT_CONTEXT)
      .cloned()
      .ok_or(RkbError::MissingTool(GET_AGENT_CONTEXT))?;
    let search_tool = tools
      .iter()
      .find(|tool| tool.definition().name == SEARCH_CHUNKS)
      .cloned()
      .ok_or(RkbError::MissingTool(SEARCH_CHUNKS))?;
    Ok(Self {
      context_tool,
      search_tool,
    })
  }

  pub fn get_agent_context(
    &self,
    query: impl Into<String>,
    result_count: usize,
  ) -> Result<RkbContext, RkbError> {
    let query = query.into();
    let text = self.call(
      &self.context_tool,
      json!({"query": query, "limit": result_count}),
    )?;
    RkbContext::parse_json(&text)
  }

  /// Alias emphasizing that retrieval is a read-only search operation.
  pub fn search(
    &self,
    query: impl Into<String>,
    result_count: usize,
  ) -> Result<RkbContext, RkbError> {
    self.get_agent_context(query, result_count)
  }

  /// Rehydrate a compact reference using its exact durable record id.
  pub fn rehydrate(&self, reference: &RkbReference) -> Result<RkbContextEntry, RkbError> {
    let text = self.call(
      &self.search_tool,
      json!({"query": reference.record_id, "limit": 20}),
    )?;
    let entries = parse_rehydration_results(&text, reference)?;
    entries
      .into_iter()
      .find(|entry| entry.record_id == reference.record_id)
      .ok_or_else(|| RkbError::UnavailableResource(reference.record_id.clone()))
  }

  pub fn rehydrate_resource(&self, resource_id: &str) -> Result<RkbContextEntry, RkbError> {
    let reference = RkbReference {
      citation: resource_id.into(),
      record_id: resource_id.into(),
      record_type: String::new(),
      title: String::new(),
      dataset_id: String::new(),
      score: 0.0,
      source_url: None,
      source_document: None,
      page: None,
    };
    self.rehydrate(&reference)
  }

  pub fn resolve(&self, reference: &RkbReference) -> Result<ExternalContextItem, RkbError> {
    Ok(self.rehydrate(reference)?.external_context_item())
  }

  pub fn resolve_external_ref(
    &self,
    reference: &ExternalContextRef,
  ) -> Result<ExternalContextItem, RkbError> {
    let reference = RkbReference::from_external_ref(reference)?;
    self.resolve(&reference)
  }

  fn call(&self, tool: &McpTool, arguments: Value) -> Result<String, RkbError> {
    struct NoProgress;
    impl ToolProgress for NoProgress {
      fn emit(&mut self, _chunk: &pi_rs_core::ToolChunk) {}
    }
    let request = ToolRequest {
      call_id: pi_rs_core::ids::ToolCallId::new(),
      name: tool.metadata().name,
      arguments,
    };
    let mut progress = NoProgress;
    let outcome = tool
      .execute(&request, &mut progress)
      .map_err(|error| RkbError::MalformedResponse(error.to_string()))?;
    if outcome.state != ToolExecutionState::Succeeded || outcome.is_error {
      return Err(RkbError::UnavailableResource(outcome.text));
    }
    Ok(outcome.text)
  }
}

impl RkbResolver for RkbConnection {
  fn resolve_reference(&self, reference: &RkbReference) -> Result<ExternalContextItem, RkbError> {
    self.resolve(reference)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use pi_rs_mcp::{McpManager, MockTransport};
  use serde_json::json;

  fn fixture() -> Value {
    json!({
      "query": "rust",
      "result_count": 1,
      "entries": [{
        "citation": "[1]",
        "record_id": "rec-1",
        "record_type": "document",
        "title": "Rust",
        "dataset_id": "docs",
        "score": 0.9,
        "snippet": "safe systems programming",
        "source_url": "https://example.test/rust",
        "source_document": "rust.md",
        "page": 4
      }]
    })
  }

  #[test]
  fn setup_is_pure_and_uses_rkb_mcp() {
    let setup = RkbSetup::new();
    assert_eq!(setup.command(), "rkb");
    assert_eq!(setup.args(), &["mcp"]);
    assert!(setup.skill().contains("search_chunks"));
    assert!(!setup.server_config().enabled);
    assert!(
      setup
        .server_config()
        .read_only_tools
        .contains(&SEARCH_DOCUMENTS.to_string())
    );
  }

  #[test]
  fn normalized_configs_mark_discovered_retrieval_tools_read_only() {
    let raw = McpServerConfig::new("rkb", "rkb").with_args(vec!["mcp".into()]);
    let normalized = RkbSetup::normalize_configs(&[raw]);
    let mut manager = McpManager::new(normalized);
    let mock = Arc::new(MockTransport::new());
    mock.on(
      "initialize",
      json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"rkb"}}),
    );
    mock.on(
      "tools/list",
      json!({"tools":[
        {"name":"get_agent_context","inputSchema":{}},
        {"name":"search_chunks","inputSchema":{}}
      ]}),
    );
    let tools = manager
      .enable_server_with_transport("rkb", mock)
      .expect("normalized RKB config activates");
    assert!(tools.iter().all(|tool| tool.metadata().read_only));
  }

  #[test]
  fn setup_discovers_custom_rkb_transport_without_connecting() {
    let config = McpServerConfig::new("rkb-rs", "/opt/rkb")
      .with_args(vec!["mcp".into()])
      .with_enabled(true);
    let setup = RkbSetup::discover(&[config]).expect("RKB config is discovered");
    assert_eq!(setup.server_name(), "rkb-rs");
    assert_eq!(setup.command(), "/opt/rkb");
    assert_eq!(setup.args(), &["mcp"]);
    assert!(
      setup.server_config().enabled,
      "discovery preserves caller state"
    );
  }

  #[test]
  fn parses_and_compacts_with_source_metadata() {
    let context = RkbContext::parse_json(&fixture().to_string()).unwrap();
    let item = context.external_context_items().pop().unwrap();
    assert!(item.citation.unwrap().contains("page=4"));
    assert_eq!(
      item.metadata.get("source_document"),
      Some(&"rust.md".to_string())
    );
    let reference = context.compact_to_references().pop().unwrap();
    assert_eq!(reference.record_id, "rec-1");
    assert!(!reference.external_context_item().inline);
  }

  #[test]
  fn parses_upstream_search_chunks_array_and_requires_exact_id() {
    let reference = RkbReference {
      citation: "[1]".into(),
      record_id: "chunk-1".into(),
      record_type: "chunk".into(),
      title: "Document".into(),
      dataset_id: "dataset".into(),
      score: 0.0,
      source_url: None,
      source_document: None,
      page: None,
    };
    let entries = parse_rehydration_results(
      &json!([{
        "record_id": "chunk-1",
        "record_type": "chunk",
        "title": "Document",
        "dataset_id": "dataset",
        "score": 0.8,
        "snippet": "exact source",
        "source_url": "https://example.test/doc",
        "source_document": "doc.md",
        "page": 2
      }])
      .to_string(),
      &reference,
    )
    .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].record_id, "chunk-1");
    assert_eq!(
      entries[0].source_url.as_deref(),
      Some("https://example.test/doc")
    );
  }

  #[test]
  fn lazy_mock_activation_and_exact_id_rehydration() {
    let mock = Arc::new(MockTransport::new());
    mock.on("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"rkb"}}));
    mock.on(
      "tools/list",
      json!({"tools":[
        {"name":"get_agent_context","inputSchema":{}},
        {"name":"search_chunks","inputSchema":{}}
      ]}),
    );
    mock.on(
      "tools/call",
      json!( {"content":[{"type":"text","text":fixture().to_string()}],"isError":false}),
    );
    let mut manager = McpManager::new(vec![RkbSetup::new().server_config()]);
    assert!(!manager.is_active("rkb"));
    let connection = RkbAdapter::new()
      .activate_with_transport(&mut manager, mock.clone())
      .unwrap();
    let context = connection.get_agent_context("rust", 1).unwrap();
    let reference = context.compact_to_references().pop().unwrap();
    connection.rehydrate(&reference).unwrap();
    let calls = mock.recorded_calls();
    assert_eq!(calls[2].1.as_ref().unwrap()["arguments"]["query"], "rust");
    assert_eq!(calls[3].1.as_ref().unwrap()["arguments"]["query"], "rec-1");
  }
}
