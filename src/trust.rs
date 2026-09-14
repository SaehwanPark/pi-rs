//! Explicit project-trust decisions.
//!
//! This command is intentionally separate from compatibility readers: selecting a
//! project is the caller's decision, and the trust store path is supplied explicitly
//! rather than taken from project-local files.

use std::fs;

use pi_rs_compat::scan::{self, Discovery};
use pi_rs_core::trust::{Risk, TrustDecision, TrustEntry, TrustGate, TrustScope, TrustStore};
use pi_rs_store::FileTrustStore;

use crate::cli::{TrustAction, TrustArgs};

/// Resolve whether project-local compatibility surfaces may be read.
///
/// An explicit `--project` is a session grant when no durable store is supplied. With a
/// store, a recorded denial always wins, a recorded grant is silent, and an unknown scope
/// stays untrusted unless the caller supplied `--project` for this invocation.
pub fn discovery(
  cwd: std::path::PathBuf,
  explicit_project: bool,
  store_root: Option<&std::path::Path>,
) -> Result<Discovery, String> {
  let mut discovery = Discovery::new(cwd.clone());
  let Some(store_root) = store_root else {
    return Ok(if explicit_project {
      discovery.trusted()
    } else {
      discovery
    });
  };
  let store = FileTrustStore::open(store_root).map_err(|error| {
    format!(
      "cannot open trust store '{}': {error}",
      store_root.display()
    )
  })?;
  let project_root = fs::canonicalize(scan::project_root(&cwd))
    .map_err(|error| format!("cannot resolve project root for trust: {error}"))?;
  let root = project_root
    .to_str()
    .ok_or_else(|| "project root is not valid UTF-8".to_string())?;
  let scope = TrustScope::Project {
    root: root.to_string(),
  };
  let outcome = TrustGate::new().check(&store, &scope, Risk::Medium, explicit_project);
  if outcome.is_allowed() {
    discovery = discovery.trusted();
  }
  Ok(discovery)
}

pub fn execute(args: TrustArgs) -> Result<(), String> {
  let mut store = FileTrustStore::open(&args.store).map_err(|error| {
    format!(
      "cannot open trust store '{}': {error}",
      args.store.display()
    )
  })?;
  match args.action {
    TrustAction::List => {
      for entry in store.entries() {
        println!(
          "{}\t{}\t{}",
          entry.scope.key(),
          decision_name(entry.decision),
          entry.source
        );
      }
    }
    TrustAction::Grant | TrustAction::Deny | TrustAction::Clear => {
      let project = args
        .project
        .as_deref()
        .ok_or_else(|| "trust action needs --project <path>".to_string())?;
      let scope = project_scope(project)?;
      match args.action {
        TrustAction::Grant | TrustAction::Deny => {
          let decision = if args.action == TrustAction::Grant {
            TrustDecision::Trusted
          } else {
            TrustDecision::Denied
          };
          store
            .record(TrustEntry::new(scope.clone(), decision, "cli"))
            .map_err(|error| format!("cannot record trust decision: {error}"))?;
          println!("{} {}", decision_name(decision), scope.key());
        }
        TrustAction::Clear => {
          let removed = store
            .remove(&scope)
            .map_err(|error| format!("cannot clear trust decision: {error}"))?;
          if removed {
            println!("cleared {}", scope.key());
          } else {
            println!("no decision recorded for {}", scope.key());
          }
        }
        TrustAction::List => unreachable!(),
      }
    }
  }
  Ok(())
}

fn project_scope(project: &std::path::Path) -> Result<TrustScope, String> {
  let project = fs::canonicalize(project)
    .map_err(|error| format!("cannot resolve project '{}': {error}", project.display()))?;
  if !fs::metadata(&project)
    .map_err(|error| format!("cannot inspect project '{}': {error}", project.display()))?
    .is_dir()
  {
    return Err(format!(
      "project '{}' is not a directory",
      project.display()
    ));
  }
  let root = pi_rs_compat::scan::project_root(&project);
  let root = fs::canonicalize(&root)
    .map_err(|error| format!("cannot resolve project root '{}': {error}", root.display()))?;
  let root = root
    .to_str()
    .ok_or_else(|| "project root is not valid UTF-8".to_string())?;
  Ok(TrustScope::Project {
    root: root.to_string(),
  })
}

fn decision_name(decision: TrustDecision) -> &'static str {
  match decision {
    TrustDecision::Trusted => "trusted",
    TrustDecision::Denied => "denied",
    TrustDecision::Ask => "ask",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use pi_rs_compat::scan::Trust;

  #[test]
  fn discovery_uses_durable_grant_and_denial() {
    let temp = tempfile::TempDir::new().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    let trust_root = temp.path().join("trust");

    let untrusted = discovery(project.clone(), false, Some(&trust_root)).unwrap();
    assert_eq!(untrusted.trust, Trust::Untrusted);

    let mut store = FileTrustStore::open(&trust_root).unwrap();
    let root = fs::canonicalize(&project)
      .unwrap()
      .to_string_lossy()
      .into_owned();
    store
      .record(TrustEntry::new(
        TrustScope::Project { root: root.clone() },
        TrustDecision::Trusted,
        "test",
      ))
      .unwrap();
    let trusted = discovery(project.clone(), false, Some(&trust_root)).unwrap();
    assert_eq!(trusted.trust, Trust::Trusted);

    store
      .record(TrustEntry::new(
        TrustScope::Project { root },
        TrustDecision::Denied,
        "test",
      ))
      .unwrap();
    let denied = discovery(project, true, Some(&trust_root)).unwrap();
    assert_eq!(denied.trust, Trust::Untrusted);
  }
}
