//! Project-trust boundary.
//!
//! Several runtime actions are only safe once the user has accepted the origin
//! of the thing being executed: project-local configuration, MCP servers
//! defined by the repository, extensions found in the project, checkpoints
//! written outside the state directory, and native plugins. Trust is decided
//! per scope, recorded durably, and never inferred from "the file was already
//! there".
//!
//! Default posture: unknown scope + risky action = ask the user. An empty trust
//! store must not behave like a trusted store.

use serde::{Deserialize, Serialize};

use crate::ids::now_millis;

/// What is being trusted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "scope")]
pub enum TrustScope {
  /// A project directory whose configuration or files would be honored.
  Project { root: String },
  /// One MCP server as configured by that project.
  McpServer { name: String, command: String },
  /// One extension as configured by that project.
  Extension { name: String, path: String },
  /// A native plugin or shared object.
  NativePlugin { path: String },
  /// A checkpoint or artifact path outside the state directory.
  ExternalPath { path: String },
  /// The environment, when a project asks the runtime to honor env-based
  /// provider credentials it did not create.
  Environment { names: Vec<String> },
}

impl TrustScope {
  /// Stable key used by trust storage.
  pub fn key(&self) -> String {
    match self {
      Self::Project { root } => format!("project:{root}"),
      Self::McpServer { name, command } => format!("mcp:{name}:{command}"),
      Self::Extension { name, path } => format!("extension:{name}:{path}"),
      Self::NativePlugin { path } => format!("native:{path}"),
      Self::ExternalPath { path } => format!("path:{path}"),
      Self::Environment { names } => format!("env:{}", {
        let mut names = names.clone();
        names.sort();
        names.join(",")
      }),
    }
  }
}

/// How much can go wrong, which decides whether an unknown scope is worth
/// asking about at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
  /// No external code runs and no sensitive project data is honored.
  Low,
  /// Project-local behavior is honored, for example a skill or prompt.
  Medium,
  /// Code runs, or sensitive project-local configuration is applied.
  High,
}

/// Recorded decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustDecision {
  /// Always allow in this session and future sessions.
  Trusted,
  /// Always refuse.
  Denied,
  /// Nothing recorded; the runtime must ask.
  Ask,
}

/// One durable trust entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustEntry {
  pub scope: TrustScope,
  pub decision: TrustDecision,
  pub decided_at_ms: u64,
  /// What produced the decision, for example `user-prompt`, `config`, or
  /// `cli-flag`. Kept so that a granted decision is not mysterious later.
  pub source: String,
}

impl TrustEntry {
  pub fn new(scope: TrustScope, decision: TrustDecision, source: impl Into<String>) -> Self {
    Self {
      scope,
      decision,
      decided_at_ms: now_millis(),
      source: source.into(),
    }
  }
}

/// Storage for trust decisions. Implementation lives in the store.
pub trait TrustStore: Send + Sync {
  fn lookup(&self, scope: &TrustScope) -> Option<TrustEntry>;
  fn record(&mut self, entry: TrustEntry) -> Result<(), String>;
}

/// The outcome of asking the trust gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustOutcome {
  /// Proceed.
  Allowed,
  /// Refuse, with a reason that is safe to show.
  Denied { reason: String },
  /// Ask the user this question; nothing may run in the meantime.
  NeedsUser { question: String, scope: TrustScope },
}

impl TrustOutcome {
  pub fn is_allowed(&self) -> bool {
    matches!(self, Self::Allowed)
  }
}

/// Gate that turns "is this okay?" into one question with one answer.
#[derive(Debug, Default, Clone, Copy)]
pub struct TrustGate;

impl TrustGate {
  pub fn new() -> Self {
    Self
  }

  /// Decide whether an action may proceed.
  ///
  /// Low-risk actions proceed silently because asking about them would train
  /// the user to approve everything. High-risk actions are refused outright
  /// when explicitly denied, and otherwise require either a recorded grant or
  /// an explicit session grant handed in by the caller.
  pub fn check(
    &self,
    store: &dyn TrustStore,
    scope: &TrustScope,
    risk: Risk,
    session_grant: bool,
  ) -> TrustOutcome {
    if let Some(entry) = store.lookup(scope) {
      return match entry.decision {
        TrustDecision::Trusted => TrustOutcome::Allowed,
        TrustDecision::Denied => TrustOutcome::Denied {
          reason: format!(
            "{} was denied by user decision ({})",
            scope.key(),
            entry.source
          ),
        },
        TrustDecision::Ask => TrustOutcome::NeedsUser {
          question: question_for(scope, risk),
          scope: scope.clone(),
        },
      };
    }
    if risk == Risk::Low {
      return TrustOutcome::Allowed;
    }
    if session_grant {
      return TrustOutcome::Allowed;
    }
    TrustOutcome::NeedsUser {
      question: question_for(scope, risk),
      scope: scope.clone(),
    }
  }
}

fn question_for(scope: &TrustScope, risk: Risk) -> String {
  let verb = match scope {
    TrustScope::Project { .. } => "honor project-local configuration",
    TrustScope::McpServer { .. } => "launch this MCP server",
    TrustScope::Extension { .. } => "load this extension",
    TrustScope::NativePlugin { .. } => "load this native plugin",
    TrustScope::ExternalPath { .. } => "read or write this path outside the state directory",
    TrustScope::Environment { .. } => "use these environment-provided credentials",
  };
  format!(
    "Allow pi-rs to {verb}? (scope: {}, risk: {})",
    scope.key(),
    match risk {
      Risk::Low => "low",
      Risk::Medium => "medium",
      Risk::High => "high",
    }
  )
}

/// Trust store that remembers nothing, used for tests and for a runtime that
/// has not been given a trust file yet. Nothing is trusted by default.
#[derive(Debug, Default)]
pub struct EmptyTrustStore;

impl TrustStore for EmptyTrustStore {
  fn lookup(&self, _scope: &TrustScope) -> Option<TrustEntry> {
    None
  }

  fn record(&mut self, _entry: TrustEntry) -> Result<(), String> {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Default)]
  struct MemoryStore {
    entries: Vec<TrustEntry>,
  }

  impl TrustStore for MemoryStore {
    fn lookup(&self, scope: &TrustScope) -> Option<TrustEntry> {
      self
        .entries
        .iter()
        .find(|entry| entry.scope == *scope)
        .cloned()
    }

    fn record(&mut self, entry: TrustEntry) -> Result<(), String> {
      self.entries.push(entry);
      Ok(())
    }
  }

  fn server() -> TrustScope {
    TrustScope::McpServer {
      name: "rkb".into(),
      command: "rkb-mcp".into(),
    }
  }

  #[test]
  fn unknown_high_risk_scope_asks() {
    let outcome = TrustGate::new().check(&EmptyTrustStore, &server(), Risk::High, false);
    assert!(
      matches!(outcome, TrustOutcome::NeedsUser { .. }),
      "{outcome:?}"
    );
    assert!(!outcome.is_allowed());
  }

  #[test]
  fn low_risk_does_not_bother_the_user() {
    let scope = TrustScope::Project {
      root: "/repo".into(),
    };
    let outcome = TrustGate::new().check(&EmptyTrustStore, &scope, Risk::Low, false);
    assert_eq!(outcome, TrustOutcome::Allowed);
  }

  #[test]
  fn explicit_denial_beats_session_grant() {
    let mut store = MemoryStore::default();
    store
      .record(TrustEntry::new(
        server(),
        TrustDecision::Denied,
        "user-prompt",
      ))
      .unwrap();
    let outcome = TrustGate::new().check(&store, &server(), Risk::High, true);
    assert!(
      matches!(outcome, TrustOutcome::Denied { .. }),
      "{outcome:?}"
    );
  }

  #[test]
  fn recorded_grant_is_honored_silently() {
    let mut store = MemoryStore::default();
    store
      .record(TrustEntry::new(server(), TrustDecision::Trusted, "config"))
      .unwrap();
    let outcome = TrustGate::new().check(&store, &server(), Risk::High, false);
    assert_eq!(outcome, TrustOutcome::Allowed);
  }

  #[test]
  fn scope_keys_are_stable_and_distinct() {
    let a = TrustScope::McpServer {
      name: "a".into(),
      command: "cmd".into(),
    };
    let b = TrustScope::McpServer {
      name: "b".into(),
      command: "cmd".into(),
    };
    assert_ne!(a.key(), b.key());
    assert_eq!(a.key(), "mcp:a:cmd");
    assert_eq!(b.key(), "mcp:b:cmd", "server name must separate entries");
    let env = TrustScope::Environment {
      names: vec!["B_TOKEN".into(), "A_KEY".into()],
    };
    assert_eq!(
      env.key(),
      "env:A_KEY,B_TOKEN",
      "order must not create a second trust entry"
    );
  }
}
