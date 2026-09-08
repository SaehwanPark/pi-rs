//! The `prompts` and `prompt` subcommands: what templates exist, and what one becomes.
//!
//! Same split as `skills`: the listing and the expanded prompt go to stdout raw, because
//! the likely next consumer is a pipe or a model, and the decisions go to stderr, because
//! "this file was skipped" is commentary about the listing rather than part of it.
//!
//! `pi-rs prompt <name> <args>` is the surface half of Pi's `/name <args>`. It prints the
//! expanded template and nothing else, so `pi-rs run --prompt "$(pi-rs prompt review)"`
//! is possible without a model in the loop. Typing `/name` inside a running session is
//! not wired up yet -- see `ROADMAP.md`.

use std::path::Path;

use pi_rs_compat::{
  prompt::{self, Warning},
  scan::{Discovery, Trust},
};

use crate::cli::{PromptArgs, PromptsArgs};

/// List the templates a session would offer, and report the ones that were skipped.
pub fn list(args: PromptsArgs) -> Result<(), String> {
  let discovery = discovery(args.project)?;
  let scan = prompt::discover(&discovery);

  for template in &scan.templates {
    let mut line = format!("{}  {}", template.source.as_str(), template.name);
    if let Some(hint) = &template.argument_hint {
      line.push_str(&format!("  {hint}"));
    }
    println!("{line}");
    // Pi falls back to the template's first line when the author wrote no `description`.
    // The text is shown either way; saying where it came from keeps an unauthored line
    // from reading like a summary somebody wrote.
    if template.description_from_body {
      println!("{} (first line of the template)", template.description);
    } else {
      println!("{}", template.description);
    }
  }

  report(&scan, &discovery);
  Ok(())
}

/// Expand one template and print the prompt it becomes.
pub fn expand(args: PromptArgs) -> Result<(), String> {
  let discovery = discovery(args.project)?;
  let scan = prompt::discover(&discovery);
  report(&scan, &discovery);
  let template = scan.named(&args.name).ok_or_else(|| {
    format!(
      "no prompt template named '{}'; run 'pi-rs prompts' to list them",
      args.name
    )
  })?;
  let arguments: Vec<&str> = args.arguments.iter().map(String::as_str).collect();
  // The prompt is prose for a model: stdout, raw, with the trailing newline the author
  // wrote rather than one this command invented.
  let expanded = template.expand(&arguments);
  print!("{expanded}");
  if !expanded.ends_with('\n') {
    println!();
  }
  Ok(())
}

fn discovery(project: bool) -> Result<Discovery, String> {
  let cwd = std::env::current_dir().map_err(|error| format!("current directory: {error}"))?;
  let discovery = Discovery::new(cwd);
  Ok(if project {
    discovery.trusted()
  } else {
    discovery
  })
}

/// The decisions the scan made, on stderr.
fn report(scan: &prompt::Scan, discovery: &Discovery) {
  for warning in &scan.warnings {
    eprintln!("[prompt] {}", describe(warning));
  }
  if discovery.trust == Trust::Untrusted {
    eprintln!("[prompt] project locations were not read; pass --project to read them");
  }
  if scan.templates.is_empty() && scan.warnings.is_empty() {
    eprintln!("[prompt] no prompt templates found");
  }
}

/// What was declined, and why, naming the path.
fn describe(warning: &Warning) -> String {
  match warning {
    Warning::MalformedFrontmatter { path } => {
      format!(
        "{}: the opening --- is never closed, not loaded",
        display(path)
      )
    }
    Warning::Empty { path } => {
      format!(
        "{}: no body to expand into a prompt, not loaded",
        display(path)
      )
    }
    Warning::Duplicate {
      name,
      kept,
      ignored,
    } => format!(
      "{}: /{name} is already claimed by {}",
      display(ignored),
      display(kept)
    ),
    Warning::Unreadable { path, reason } => {
      format!("{}: {}", display(path), reason.trim_end_matches('.'))
    }
  }
}

fn display(path: &Path) -> String {
  path.display().to_string()
}
