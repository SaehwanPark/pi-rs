//! The `packages` subcommand: what packages exist, and what surfaces they provide.
//!
//! The listing goes to stdout raw -- one package per pair of lines, no decoration -- because
//! the likely next consumer is a pipe. The decisions and diagnostics go to stderr, because
//! "this package was skipped" or "unsupported surface" is commentary about the listing
//! rather than part of it.

use std::path::Path;

use pi_rs_compat::{
  package::{self, Warning},
  scan::{Discovery, Trust},
};

use crate::cli::PackagesArgs;

/// Show what packages were discovered, or inspect one package's detailed surfaces.
pub fn execute(args: PackagesArgs) -> Result<(), String> {
  let cwd = std::env::current_dir().map_err(|error| format!("current directory: {error}"))?;
  let discovery = if args.project {
    Discovery::new(cwd).trusted()
  } else {
    Discovery::new(cwd)
  };
  let scan = package::discover(&discovery);

  if let Some(name) = &args.show {
    let pkg = scan
      .named(name)
      .ok_or_else(|| format!("no package named '{name}'; pi-rs packages lists what there is"))?;

    println!(
      "Package: {} ({}) [{}]",
      pkg.name,
      pkg.version.as_deref().unwrap_or("unversioned"),
      pkg.source.as_str()
    );
    if let Some(desc) = &pkg.description {
      println!("Description: {desc}");
    }
    println!("Path: {}", display(&pkg.path));
    println!("Manifest: {}", display(&pkg.manifest_path));

    println!("Surfaces:");
    for (surface, status) in pkg.surfaces() {
      println!("  {} {}", status.symbol(), surface);
    }

    let skills = pkg.skill_locations();
    if !skills.is_empty() {
      println!("Skills:");
      for s in &skills {
        println!("  {}", display(s));
      }
    }

    let prompts = pkg.prompt_locations();
    if !prompts.is_empty() {
      println!("Prompts:");
      for p in &prompts {
        println!("  {}", display(p));
      }
    }

    if !pkg.warnings.is_empty() {
      println!("Diagnostics:");
      for w in &pkg.warnings {
        println!("  {}", describe(w));
      }
    }
  } else {
    for pkg in &scan.packages {
      let version = pkg.version.as_deref().unwrap_or("");
      if version.is_empty() {
        println!("{}  {}", pkg.source.as_str(), pkg.name);
      } else {
        println!("{}  {}  {}", pkg.source.as_str(), pkg.name, version);
      }
      println!("{}", pkg.description.as_deref().unwrap_or(""));
    }
  }

  for warning in &scan.warnings {
    eprintln!("[package] {}", describe(warning));
  }
  if discovery.trust == Trust::Untrusted {
    eprintln!("[package] project locations were not read; pass --project to read them");
  }
  if scan.packages.is_empty() && scan.warnings.is_empty() {
    eprintln!("[package] no packages found");
  }
  Ok(())
}

/// What was declined or warned about, naming the path.
fn describe(warning: &Warning) -> String {
  match warning {
    Warning::UnsupportedSurface { surface, count } => {
      format!("unsupported surface '{surface}' (count: {count})")
    }
    Warning::UnknownSurface { key, value_kind } => {
      format!("unknown surface '{key}' (type: {})", value_kind.as_str())
    }
    Warning::Unreadable { path, reason } => {
      format!("{}: {}", display(path), reason.trim_end_matches('.'))
    }
    Warning::Malformed { path, reason } => {
      format!("{}: {}", display(path), reason.trim_end_matches('.'))
    }
    Warning::Duplicate {
      name,
      kept,
      ignored,
    } => {
      format!(
        "{}: package '{name}' is already claimed by {}",
        display(ignored),
        display(kept)
      )
    }
    Warning::MissingManifest { path } => {
      format!(
        "{}: missing package.json manifest, not loaded",
        display(path)
      )
    }
  }
}

/// A path shortened to `~` or `.` when inside `$HOME` or `cwd`.
fn display(path: &Path) -> String {
  let cwd = std::env::current_dir().ok();
  if let Some(cwd) = cwd {
    if let Ok(rel) = path.strip_prefix(&cwd) {
      return format!("./{}", rel.display());
    }
  }
  if let Some(home) = pi_rs_compat::scan::home_dir() {
    if let Ok(rel) = path.strip_prefix(&home) {
      return format!("~/{}", rel.display());
    }
  }
  path.display().to_string()
}
