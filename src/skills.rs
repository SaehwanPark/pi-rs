//! The `skills` subcommand: what the scan found, and what it declined.
//!
//! The listing goes to stdout raw -- one skill per pair of lines, no decoration -- because
//! the likely next consumer is a pipe. The decisions go to stderr, because "this file was
//! skipped" is commentary about the listing rather than part of it, and a user piping
//! names into another command does not want them interleaved.

use std::path::Path;

use pi_rs_compat::skill::{self, Discovery, SkillWarning, Trust};

use crate::cli::SkillsArgs;

/// List the skills that would be offered to a model, and report the ones that were
/// skipped.
pub fn execute(args: SkillsArgs) -> Result<(), String> {
  let cwd = std::env::current_dir().map_err(|error| format!("current directory: {error}"))?;
  let discovery = if args.project {
    Discovery::new(cwd).trusted()
  } else {
    Discovery::new(cwd)
  };
  let scan = skill::discover(&discovery);

  for skill in &scan.skills {
    let hidden = if skill.disable_model_invocation {
      " (explicit invocation only)"
    } else {
      ""
    };
    println!("{}  {}{}", skill.source.as_str(), skill.name, hidden);
    println!("{}", skill.description);
  }

  for warning in &scan.warnings {
    eprintln!("[skill] {}", describe(warning));
  }
  if discovery.trust == Trust::Untrusted {
    eprintln!("[skill] project locations were not read; pass --project to read them");
  }
  if scan.skills.is_empty() && scan.warnings.is_empty() {
    eprintln!("[skill] no skills found");
  }
  Ok(())
}

/// What was declined, and why, naming the path.
fn describe(warning: &SkillWarning) -> String {
  match warning {
    SkillWarning::MissingName { path } => format!("{}: no name", display(path)),
    SkillWarning::MissingDescription { path } => {
      format!("{}: no description, not loaded", display(path))
    }
    SkillWarning::MalformedFrontmatter { path } => {
      format!(
        "{}: the opening --- is never closed, not loaded",
        display(path)
      )
    }
    SkillWarning::InvalidName { path, name, reason } => {
      format!("{}: name {name} {reason}", display(path))
    }
    SkillWarning::NameTooLong { path, name } => {
      format!(
        "{}: name {name} is longer than 64 characters",
        display(path)
      )
    }
    SkillWarning::DescriptionTooLong { path, name } => {
      format!(
        "{}: description of {name} is longer than 1024 characters",
        display(path)
      )
    }
    SkillWarning::Duplicate {
      name,
      kept,
      ignored,
    } => {
      format!(
        "{}: {name} is already claimed by {}",
        display(ignored),
        display(kept)
      )
    }
    SkillWarning::Unreadable { path, reason } => {
      format!("{}: {}", display(path), reason.trim_end_matches('.'))
    }
    SkillWarning::TooDeep { path } => {
      format!(
        "{}: deeper than the scan goes, contents unseen",
        display(path)
      )
    }
  }
}

fn display(path: &Path) -> String {
  path.display().to_string()
}
