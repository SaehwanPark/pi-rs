//! Compatibility inspection for Pi packages, skills, and prompt templates.
//!
//! Evaluates artifacts against Pi behavioral compatibility targets (`COMPATIBILITY.md`),
//! producing typed surface reports with status vocabulary: Supported (`✓`),
//! Partial (`△`), Experimental (`~`), Unsupported (`✗`), and NotPresent (`-`).

use std::path::{Path, PathBuf};

use crate::{
  frontmatter::{self, Extract},
  package::{self, Package, Warning},
  scan::{Discovery, Trust},
};

/// Default Pi compatibility behavioral target.
pub const DEFAULT_PI_COMPAT_TARGET: &str = "Pi 0.50.x+";

/// Kind of target being inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
  Package,
  Skill,
  Prompt,
}

impl TargetKind {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Package => "package",
      Self::Skill => "skill",
      Self::Prompt => "prompt",
    }
  }
}

/// A compatibility level according to COMPATIBILITY.md §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityLevel {
  /// Expected to work and covered by tests.
  Supported,
  /// Useful subset works; limitations are documented.
  Partial,
  /// Available but unstable or incomplete.
  Experimental,
  /// Intentionally not supported.
  Unsupported,
  /// Surface was not declared or found.
  NotPresent,
}

impl CompatibilityLevel {
  pub fn symbol(self) -> &'static str {
    match self {
      Self::Supported => "✓",
      Self::Partial => "△",
      Self::Experimental => "~",
      Self::Unsupported => "✗",
      Self::NotPresent => "-",
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      Self::Supported => "supported",
      Self::Partial => "partial",
      Self::Experimental => "experimental",
      Self::Unsupported => "unsupported",
      Self::NotPresent => "not_present",
    }
  }
}

/// A surface evaluation item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceReport {
  pub name: String,
  pub status: CompatibilityLevel,
  pub detail: Option<String>,
}

/// Result of evaluating an artifact for compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatReport {
  pub target: String,
  pub kind: TargetKind,
  pub pi_target: String,
  pub surfaces: Vec<SurfaceReport>,
  pub diagnostics: Vec<String>,
}

/// Inspect an artifact or package candidate.
pub fn inspect_target(target: &str, discovery: &Discovery) -> Result<CompatReport, String> {
  let path = Path::new(target);

  if path.exists() {
    if path.is_file() {
      let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
      if file_name == "package.json" || file_name.ends_with(".json") {
        return inspect_package(path, None);
      }
      if file_name == "SKILL.md" {
        return inspect_skill(path);
      }
      if file_name.ends_with(".md") {
        let parent = path
          .parent()
          .and_then(|p| p.file_name())
          .and_then(|n| n.to_str());
        if parent == Some("skills") {
          return inspect_skill(path);
        }
        if parent == Some("prompts") {
          return inspect_prompt(path);
        }
        if let Ok(text) = std::fs::read_to_string(path) {
          let is_skill = matches!(
            frontmatter::extract(&text),
            Extract::Found(fm) if fm.get("name").is_some() && fm.get("argument-hint").is_none()
          );
          if is_skill {
            return inspect_skill(path);
          }
        }
        return inspect_prompt(path);
      }
      return inspect_package(path, None);
    }

    if path.is_dir() {
      if path.join("package.json").is_file() {
        return inspect_package(&path.join("package.json"), None);
      }
      if path.join("SKILL.md").is_file() {
        return inspect_skill(&path.join("SKILL.md"));
      }
      if path.join("skills").is_dir() || path.join("prompts").is_dir() {
        return inspect_package(path, None);
      }
      // Check for markdown files in this directory
      if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
          let p = entry.path();
          if p.is_file() && p.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
            return inspect_skill(&p);
          }
        }
      }
      return inspect_package(path, None);
    }
  }

  // Not a filesystem path: try discovering package by name
  let scan = package::discover(discovery);
  if let Some(pkg) = scan.named(target) {
    return inspect_package(&pkg.path, Some(pkg));
  }

  let hint = if discovery.trust == Trust::Untrusted {
    " (pass --project to include project packages)"
  } else {
    ""
  };
  Err(format!(
    "cannot inspect '{target}': no such file, directory, or discovered package{hint}"
  ))
}

/// Inspect a package artifact.
pub fn inspect_package(
  pkg_path: &Path,
  package_opt: Option<&Package>,
) -> Result<CompatReport, String> {
  let (pkg_root, manifest_path) = if pkg_path.is_file() {
    (
      pkg_path.parent().unwrap_or(Path::new(".")),
      pkg_path.to_path_buf(),
    )
  } else {
    (pkg_path, pkg_path.join("package.json"))
  };

  let (manifest, mut warnings) = if let Some(pkg) = package_opt {
    (pkg.manifest.clone(), pkg.warnings.clone())
  } else if manifest_path.is_file() {
    let parse = package::read(&manifest_path);
    (parse.manifest, parse.warnings)
  } else {
    let warnings = vec![Warning::MissingManifest {
      path: pkg_root.to_path_buf(),
    }];
    (package::Manifest::default(), warnings)
  };
  warnings.extend(package::invalid_manifest_paths(pkg_root, &manifest));

  let mut surfaces = Vec::new();
  let mut diagnostics = Vec::new();

  // 1. Package manifest
  if manifest_path.is_file() {
    let has_malformed = warnings
      .iter()
      .any(|w| matches!(w, Warning::Malformed { .. }));
    if has_malformed {
      surfaces.push(SurfaceReport {
        name: "package manifest".to_string(),
        status: CompatibilityLevel::Unsupported,
        detail: Some("malformed JSON manifest".to_string()),
      });
    } else {
      let name = manifest.name.as_deref().unwrap_or("unnamed");
      let version = manifest.version.as_deref().unwrap_or("unversioned");
      surfaces.push(SurfaceReport {
        name: "package manifest".to_string(),
        status: CompatibilityLevel::Supported,
        detail: Some(format!("{name}@{version}")),
      });
    }
  } else {
    surfaces.push(SurfaceReport {
      name: "package manifest".to_string(),
      status: CompatibilityLevel::Unsupported,
      detail: Some("missing package.json manifest".to_string()),
    });
  }

  // 2. Skill discovery
  let skill_locations = if !manifest.skill_paths.is_empty() {
    manifest
      .skill_paths
      .iter()
      .filter_map(|p| package::safe_manifest_path(pkg_root, p))
      .collect::<Vec<_>>()
  } else {
    if let Some(skills_dir) = package::safe_manifest_path(pkg_root, "skills")
      && skills_dir.is_dir()
    {
      vec![skills_dir]
    } else if let Some(skill_md) = package::safe_manifest_path(pkg_root, "SKILL.md")
      && skill_md.is_file()
    {
      vec![skill_md]
    } else {
      Vec::new()
    }
  };

  if !skill_locations.is_empty() {
    let mut count = 0;
    for loc in &skill_locations {
      if loc.is_file() {
        if let Ok(text) = std::fs::read_to_string(loc) {
          let is_valid = matches!(
            frontmatter::extract(&text),
            Extract::Found(fm) if fm.get("name").is_some() && fm.get("description").is_some()
          );
          if is_valid {
            count += 1;
          }
        }
      } else if let Ok(entries) = std::fs::read_dir(loc) {
        for entry in entries.flatten() {
          let p = entry.path();
          let is_skill = (p.is_file()
            && p.file_name().and_then(|n| n.to_str()) == Some("SKILL.md"))
            || (p.is_dir() && p.join("SKILL.md").is_file());
          if is_skill {
            count += 1;
          }
        }
      }
    }
    if count > 0 {
      surfaces.push(SurfaceReport {
        name: "skill discovery".to_string(),
        status: CompatibilityLevel::Supported,
        detail: Some(format!("{count} skill(s) found")),
      });
    } else {
      surfaces.push(SurfaceReport {
        name: "skill discovery".to_string(),
        status: CompatibilityLevel::Partial,
        detail: Some("declared skill locations contain no valid skills".to_string()),
      });
    }
  } else {
    surfaces.push(SurfaceReport {
      name: "skill discovery".to_string(),
      status: CompatibilityLevel::NotPresent,
      detail: None,
    });
  }

  // 3. Prompt templates
  let prompt_locations = if !manifest.prompt_paths.is_empty() {
    manifest
      .prompt_paths
      .iter()
      .filter_map(|p| package::safe_manifest_path(pkg_root, p))
      .collect::<Vec<_>>()
  } else {
    if let Some(prompts_dir) = package::safe_manifest_path(pkg_root, "prompts")
      && prompts_dir.is_dir()
    {
      vec![prompts_dir]
    } else {
      Vec::new()
    }
  };

  if !prompt_locations.is_empty() {
    let mut count = 0;
    for loc in &prompt_locations {
      if loc.is_file() {
        let is_non_empty = std::fs::read_to_string(loc)
          .map(|text| !text.trim().is_empty())
          .unwrap_or(false);
        if is_non_empty {
          count += 1;
        }
      } else if let Ok(entries) = std::fs::read_dir(loc) {
        for entry in entries.flatten() {
          let p = entry.path();
          if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md") {
            count += 1;
          }
        }
      }
    }
    if count > 0 {
      surfaces.push(SurfaceReport {
        name: "prompt templates".to_string(),
        status: CompatibilityLevel::Supported,
        detail: Some(format!("{count} prompt template(s) found")),
      });
    } else {
      surfaces.push(SurfaceReport {
        name: "prompt templates".to_string(),
        status: CompatibilityLevel::Partial,
        detail: Some("declared prompt locations contain no valid templates".to_string()),
      });
    }
  } else {
    surfaces.push(SurfaceReport {
      name: "prompt templates".to_string(),
      status: CompatibilityLevel::NotPresent,
      detail: None,
    });
  }

  // 4. Extensions & static analysis
  let has_ext_decl = !manifest.extension_paths.is_empty()
    || warnings.iter().any(|w| {
      matches!(
        w,
        Warning::UnsupportedSurface { surface, .. } if surface == "extensions"
      )
    })
    || package::safe_manifest_path(pkg_root, "extensions").is_some_and(|path| path.is_dir());

  if has_ext_decl {
    let mut ext_files = Vec::new();
    for p in &manifest.extension_paths {
      if let Some(file_path) = package::safe_manifest_path(pkg_root, p)
        && file_path.is_file()
      {
        ext_files.push(file_path);
      }
    }
    if let Some(path) = package::safe_manifest_path(pkg_root, "extensions") {
      scan_js_ts_files(&path, &mut ext_files);
    }
    if let Some(path) = package::safe_manifest_path(pkg_root, "src") {
      scan_js_ts_files(&path, &mut ext_files);
    }
    if let Some(path) = package::safe_manifest_path(pkg_root, "dist") {
      scan_js_ts_files(&path, &mut ext_files);
    }

    if !ext_files.is_empty() {
      let mut has_register_tool = false;
      let mut has_register_command = false;
      let mut has_context_hook = false;
      let mut has_custom_ui = false;
      let mut has_internal_import = false;

      for file in &ext_files {
        if let Ok(content) = std::fs::read_to_string(file) {
          if content.contains("registerTool") {
            has_register_tool = true;
          }
          if content.contains("registerCommand") {
            has_register_command = true;
          }
          if content.contains("beforeTurn")
            || content.contains("afterTurn")
            || content.contains(".on(\"")
            || content.contains(".on('")
            || content.contains("contextHook")
          {
            has_context_hook = true;
          }
          if content.contains("registerWidget") || content.contains("customUi") {
            has_custom_ui = true;
          }
          if content.contains("@pi/internal")
            || content.contains("pi/internal")
            || content.contains("../internal")
          {
            has_internal_import = true;
          }
        }
      }

      if has_register_tool {
        surfaces.push(SurfaceReport {
          name: "tool registration".to_string(),
          status: CompatibilityLevel::Supported,
          detail: Some("registerTool API detected".to_string()),
        });
      }
      if has_register_command {
        surfaces.push(SurfaceReport {
          name: "command registration".to_string(),
          status: CompatibilityLevel::Supported,
          detail: Some("registerCommand API detected".to_string()),
        });
      }
      if has_context_hook {
        surfaces.push(SurfaceReport {
          name: "context hook".to_string(),
          status: CompatibilityLevel::Partial,
          detail: Some("context lifecycle hook detected".to_string()),
        });
      }
      if has_custom_ui {
        surfaces.push(SurfaceReport {
          name: "custom UI".to_string(),
          status: CompatibilityLevel::Experimental,
          detail: Some("custom TUI widget detected".to_string()),
        });
      }
      if has_internal_import {
        surfaces.push(SurfaceReport {
          name: "unsupported internal import".to_string(),
          status: CompatibilityLevel::Unsupported,
          detail: Some("internal Pi module import detected".to_string()),
        });
      }

      surfaces.push(SurfaceReport {
        name: "extensions".to_string(),
        status: CompatibilityLevel::Partial,
        detail: Some(
          "selected TypeScript APIs available; explicit host activation required".to_string(),
        ),
      });
    } else {
      let entry_count = if !manifest.extension_paths.is_empty() {
        manifest.extension_paths.len()
      } else {
        1
      };
      surfaces.push(SurfaceReport {
        name: "extensions".to_string(),
        status: CompatibilityLevel::Partial,
        detail: Some(format!(
          "{entry_count} declared entry point(s); selected TypeScript APIs available"
        )),
      });
    }
  } else {
    surfaces.push(SurfaceReport {
      name: "extensions".to_string(),
      status: CompatibilityLevel::NotPresent,
      detail: None,
    });
  }

  // 5. Unknown surfaces
  for warning in &warnings {
    if let Warning::UnknownSurface { key, value_kind } = warning {
      surfaces.push(SurfaceReport {
        name: format!("unknown surface ({key})"),
        status: CompatibilityLevel::Unsupported,
        detail: Some(format!("type: {}", value_kind.as_str())),
      });
    }
  }

  // Collect diagnostics
  for warning in warnings {
    diagnostics.push(describe_package_warning(&warning));
  }

  let target_name = manifest.name.as_deref().unwrap_or_else(|| {
    pkg_root
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or("unnamed")
  });

  Ok(CompatReport {
    target: target_name.to_string(),
    kind: TargetKind::Package,
    pi_target: DEFAULT_PI_COMPAT_TARGET.to_string(),
    surfaces,
    diagnostics,
  })
}

/// Inspect a skill artifact.
pub fn inspect_skill(path: &Path) -> Result<CompatReport, String> {
  let text = std::fs::read_to_string(path)
    .map_err(|err| format!("cannot read '{}': {err}", path.display()))?;

  let mut surfaces = Vec::new();
  let mut diagnostics = Vec::new();

  let extract = frontmatter::extract(&text);
  match extract {
    Extract::Found(fm) => {
      surfaces.push(SurfaceReport {
        name: "skill frontmatter".to_string(),
        status: CompatibilityLevel::Supported,
        detail: Some("valid".to_string()),
      });

      // Name
      if let Some(name) = fm.get("name") {
        if let Some(reason) = validate_skill_name(name) {
          surfaces.push(SurfaceReport {
            name: "skill name".to_string(),
            status: CompatibilityLevel::Partial,
            detail: Some(format!("invalid: {reason}")),
          });
          diagnostics.push(format!("name '{name}' is invalid: {reason}"));
        } else if name.len() > 64 {
          surfaces.push(SurfaceReport {
            name: "skill name".to_string(),
            status: CompatibilityLevel::Partial,
            detail: Some("name exceeds 64 characters".to_string()),
          });
          diagnostics.push(format!("name '{name}' exceeds 64 characters"));
        } else {
          surfaces.push(SurfaceReport {
            name: "skill name".to_string(),
            status: CompatibilityLevel::Supported,
            detail: Some(name.to_string()),
          });
        }
      } else {
        surfaces.push(SurfaceReport {
          name: "skill name".to_string(),
          status: CompatibilityLevel::Unsupported,
          detail: Some("missing name field".to_string()),
        });
        diagnostics.push("missing name field in frontmatter".to_string());
      }

      // Description
      if let Some(desc) = fm.get("description") {
        if desc.len() > 1024 {
          surfaces.push(SurfaceReport {
            name: "skill description".to_string(),
            status: CompatibilityLevel::Partial,
            detail: Some("description exceeds 1024 characters".to_string()),
          });
          diagnostics.push("description exceeds 1024 characters".to_string());
        } else {
          surfaces.push(SurfaceReport {
            name: "skill description".to_string(),
            status: CompatibilityLevel::Supported,
            detail: Some(desc.trim().to_string()),
          });
        }
      } else {
        surfaces.push(SurfaceReport {
          name: "skill description".to_string(),
          status: CompatibilityLevel::Unsupported,
          detail: Some("missing description field".to_string()),
        });
        diagnostics.push("missing description field in frontmatter".to_string());
      }

      // Tool restrictions
      if let Some(tools) = fm.get("allowed-tools") {
        surfaces.push(SurfaceReport {
          name: "tool restrictions".to_string(),
          status: CompatibilityLevel::Supported,
          detail: Some(tools.to_string()),
        });
      } else {
        surfaces.push(SurfaceReport {
          name: "tool restrictions".to_string(),
          status: CompatibilityLevel::NotPresent,
          detail: None,
        });
      }

      // Disable model invocation
      if fm.is_true("disable-model-invocation") {
        surfaces.push(SurfaceReport {
          name: "disable-model-invocation".to_string(),
          status: CompatibilityLevel::Supported,
          detail: Some("model prompt suppressed".to_string()),
        });
      } else {
        surfaces.push(SurfaceReport {
          name: "disable-model-invocation".to_string(),
          status: CompatibilityLevel::NotPresent,
          detail: None,
        });
      }

      let body = frontmatter::strip(&text);
      surfaces.push(SurfaceReport {
        name: "skill body".to_string(),
        status: CompatibilityLevel::Supported,
        detail: Some(format!("{} bytes", body.len())),
      });
    }
    Extract::Malformed(_) => {
      surfaces.push(SurfaceReport {
        name: "skill frontmatter".to_string(),
        status: CompatibilityLevel::Unsupported,
        detail: Some("unterminated frontmatter block".to_string()),
      });
      diagnostics.push("unterminated frontmatter block".to_string());
      surfaces.push(SurfaceReport {
        name: "skill name".to_string(),
        status: CompatibilityLevel::Unsupported,
        detail: Some("unparsed due to frontmatter defect".to_string()),
      });
      surfaces.push(SurfaceReport {
        name: "skill description".to_string(),
        status: CompatibilityLevel::Unsupported,
        detail: Some("unparsed due to frontmatter defect".to_string()),
      });
      surfaces.push(SurfaceReport {
        name: "tool restrictions".to_string(),
        status: CompatibilityLevel::NotPresent,
        detail: None,
      });
      surfaces.push(SurfaceReport {
        name: "disable-model-invocation".to_string(),
        status: CompatibilityLevel::NotPresent,
        detail: None,
      });
      let body = frontmatter::strip(&text);
      surfaces.push(SurfaceReport {
        name: "skill body".to_string(),
        status: CompatibilityLevel::Supported,
        detail: Some(format!("{} bytes", body.len())),
      });
    }
    Extract::Absent => {
      surfaces.push(SurfaceReport {
        name: "skill frontmatter".to_string(),
        status: CompatibilityLevel::Unsupported,
        detail: Some("missing frontmatter block".to_string()),
      });
      diagnostics.push("missing frontmatter block".to_string());
      surfaces.push(SurfaceReport {
        name: "skill name".to_string(),
        status: CompatibilityLevel::Unsupported,
        detail: Some("missing name field".to_string()),
      });
      surfaces.push(SurfaceReport {
        name: "skill description".to_string(),
        status: CompatibilityLevel::Unsupported,
        detail: Some("missing description field".to_string()),
      });
      surfaces.push(SurfaceReport {
        name: "tool restrictions".to_string(),
        status: CompatibilityLevel::NotPresent,
        detail: None,
      });
      surfaces.push(SurfaceReport {
        name: "disable-model-invocation".to_string(),
        status: CompatibilityLevel::NotPresent,
        detail: None,
      });
      surfaces.push(SurfaceReport {
        name: "skill body".to_string(),
        status: CompatibilityLevel::Supported,
        detail: Some(format!("{} bytes", text.len())),
      });
    }
  }

  let target_name = path
    .file_name()
    .and_then(|n| n.to_str())
    .unwrap_or_else(|| path.to_str().unwrap_or("skill"));

  Ok(CompatReport {
    target: target_name.to_string(),
    kind: TargetKind::Skill,
    pi_target: DEFAULT_PI_COMPAT_TARGET.to_string(),
    surfaces,
    diagnostics,
  })
}

/// Inspect a prompt template artifact.
pub fn inspect_prompt(path: &Path) -> Result<CompatReport, String> {
  let text = std::fs::read_to_string(path)
    .map_err(|err| format!("cannot read '{}': {err}", path.display()))?;

  let mut surfaces = Vec::new();
  let mut diagnostics = Vec::new();

  let extract = frontmatter::extract(&text);
  let mut has_frontmatter_desc = false;
  match &extract {
    Extract::Found(fm) => {
      surfaces.push(SurfaceReport {
        name: "prompt frontmatter".to_string(),
        status: CompatibilityLevel::Supported,
        detail: Some("valid".to_string()),
      });
      if let Some(desc) = fm.get("description") {
        has_frontmatter_desc = true;
        surfaces.push(SurfaceReport {
          name: "prompt description".to_string(),
          status: CompatibilityLevel::Supported,
          detail: Some(format!("declared: {desc}")),
        });
      }
      if let Some(hint) = fm.get("argument-hint") {
        surfaces.push(SurfaceReport {
          name: "argument hint".to_string(),
          status: CompatibilityLevel::Supported,
          detail: Some(hint.to_string()),
        });
      } else {
        surfaces.push(SurfaceReport {
          name: "argument hint".to_string(),
          status: CompatibilityLevel::NotPresent,
          detail: None,
        });
      }
    }
    Extract::Malformed(_) => {
      surfaces.push(SurfaceReport {
        name: "prompt frontmatter".to_string(),
        status: CompatibilityLevel::Unsupported,
        detail: Some("unterminated frontmatter block".to_string()),
      });
      diagnostics.push("unterminated frontmatter block".to_string());
      surfaces.push(SurfaceReport {
        name: "argument hint".to_string(),
        status: CompatibilityLevel::NotPresent,
        detail: None,
      });
    }
    Extract::Absent => {
      surfaces.push(SurfaceReport {
        name: "prompt frontmatter".to_string(),
        status: CompatibilityLevel::NotPresent,
        detail: None,
      });
      surfaces.push(SurfaceReport {
        name: "argument hint".to_string(),
        status: CompatibilityLevel::NotPresent,
        detail: None,
      });
    }
  }

  let body = frontmatter::strip(&text).trim();
  if body.is_empty() {
    surfaces.push(SurfaceReport {
      name: "prompt body".to_string(),
      status: CompatibilityLevel::Unsupported,
      detail: Some("empty template body".to_string()),
    });
    diagnostics.push("empty template body".to_string());
  } else {
    surfaces.push(SurfaceReport {
      name: "prompt body".to_string(),
      status: CompatibilityLevel::Supported,
      detail: Some(format!("{} bytes", body.len())),
    });
  }

  if !has_frontmatter_desc {
    if let Some(first_line) = body.lines().find(|l| !l.trim().is_empty()) {
      surfaces.push(SurfaceReport {
        name: "prompt description".to_string(),
        status: CompatibilityLevel::Supported,
        detail: Some(format!("inferred from first line: {}", first_line.trim())),
      });
    } else {
      surfaces.push(SurfaceReport {
        name: "prompt description".to_string(),
        status: CompatibilityLevel::Partial,
        detail: Some("no description declared or inferred".to_string()),
      });
    }
  }

  // Check parameter substitution placeholders: $1, $2, $@, ${...}
  let mut placeholders = Vec::new();
  let mut chars = body.chars().peekable();
  while let Some(ch) = chars.next() {
    if ch == '$' {
      match chars.peek() {
        Some('@') => {
          chars.next();
          if !placeholders.contains(&"$@".to_string()) {
            placeholders.push("$@".to_string());
          }
        }
        Some(d) if d.is_ascii_digit() => {
          let mut num = String::from("$");
          while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
              num.push(c);
              chars.next();
            } else {
              break;
            }
          }
          if !placeholders.contains(&num) {
            placeholders.push(num);
          }
        }
        Some('{') => {
          chars.next();
          let mut braced = String::from("${");
          for c in chars.by_ref() {
            braced.push(c);
            if c == '}' {
              break;
            }
          }
          if !placeholders.contains(&braced) {
            placeholders.push(braced);
          }
        }
        _ => {}
      }
    }
  }

  if !placeholders.is_empty() {
    surfaces.push(SurfaceReport {
      name: "parameter substitution".to_string(),
      status: CompatibilityLevel::Supported,
      detail: Some(format!("placeholders: {}", placeholders.join(", "))),
    });
  } else {
    surfaces.push(SurfaceReport {
      name: "parameter substitution".to_string(),
      status: CompatibilityLevel::NotPresent,
      detail: None,
    });
  }

  let target_name = path
    .file_name()
    .and_then(|n| n.to_str())
    .unwrap_or_else(|| path.to_str().unwrap_or("prompt"));

  Ok(CompatReport {
    target: target_name.to_string(),
    kind: TargetKind::Prompt,
    pi_target: DEFAULT_PI_COMPAT_TARGET.to_string(),
    surfaces,
    diagnostics,
  })
}

fn validate_skill_name(name: &str) -> Option<&'static str> {
  if !name
    .chars()
    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
  {
    Some("lowercase letters, digits and hyphens only")
  } else if name.starts_with('-') || name.ends_with('-') {
    Some("must not start or end with a hyphen")
  } else if name.contains("--") {
    Some("must not contain consecutive hyphens")
  } else {
    None
  }
}

fn scan_js_ts_files(dir: &Path, files: &mut Vec<PathBuf>) {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    let is_js_ts = path.is_file()
      && path
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| matches!(ext, "js" | "mjs" | "cjs" | "ts" | "mts" | "cts"))
        .unwrap_or(false);
    if is_js_ts {
      files.push(path);
    }
  }
}

fn describe_package_warning(warning: &Warning) -> String {
  match warning {
    Warning::UnsupportedSurface { surface, count } if surface == "extensions" => {
      format!("extension surface requires explicit trusted Node host activation (count: {count})")
    }
    Warning::UnsupportedSurface { surface, count } => {
      format!("unsupported surface '{surface}' (count: {count})")
    }
    Warning::UnknownSurface { key, value_kind } => {
      format!("unknown surface '{key}' (type: {})", value_kind.as_str())
    }
    Warning::Unreadable { path, reason } => {
      format!("{}: {}", path.display(), reason.trim_end_matches('.'))
    }
    Warning::Malformed { path, reason } => {
      format!("{}: {}", path.display(), reason.trim_end_matches('.'))
    }
    Warning::Duplicate {
      name,
      kept,
      ignored,
    } => {
      format!(
        "{}: package '{name}' is already claimed by {}",
        ignored.display(),
        kept.display()
      )
    }
    Warning::MissingManifest { path } => {
      format!(
        "{}: missing package.json manifest, not loaded",
        path.display()
      )
    }
    Warning::InvalidSurfacePath {
      surface,
      path,
      reason,
    } => format!("invalid {surface} path '{path}': {reason}"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fixture_path(rel: &str) -> String {
    let direct = PathBuf::from(rel);
    if direct.exists() {
      rel.to_string()
    } else {
      let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
      crate_dir.join("../../").join(rel).display().to_string()
    }
  }

  #[test]
  fn test_inspect_minimal_package() {
    let discovery = Discovery::new(PathBuf::from("/nonexistent"));
    let target = fixture_path("tests/compat/packages/minimal");
    let report = inspect_target(&target, &discovery).unwrap();
    assert_eq!(report.kind, TargetKind::Package);
    assert_eq!(report.pi_target, DEFAULT_PI_COMPAT_TARGET);
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "package manifest" && s.status == CompatibilityLevel::Supported)
    );
  }

  #[test]
  fn test_inspect_package_rejects_manifest_escape_paths() {
    let temp = tempfile::TempDir::new().unwrap();
    let package = temp.path().join("package");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
      outside.join("SKILL.md"),
      "---\nname: outside\ndescription: outside\n---\n",
    )
    .unwrap();
    std::fs::write(
      package.join("package.json"),
      r#"{"name":"escape","pi":{"skills":["../../outside"]}}"#,
    )
    .unwrap();

    let report = inspect_package(&package, None).unwrap();
    assert!(
      report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("invalid skills path"))
    );
    assert!(
      report
        .surfaces
        .iter()
        .all(|surface| surface.name != "skill discovery"
          || surface.detail.as_deref() != Some("1 skill(s) found"))
    );
  }

  #[test]
  fn test_inspect_package_with_extension() {
    let discovery = Discovery::new(PathBuf::from("/nonexistent"));
    let target = fixture_path("tests/compat/packages/with-extension");
    let report = inspect_target(&target, &discovery).unwrap();
    assert_eq!(report.kind, TargetKind::Package);
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "tool registration" && s.status == CompatibilityLevel::Supported)
    );
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "command registration" && s.status == CompatibilityLevel::Supported)
    );
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "context hook" && s.status == CompatibilityLevel::Partial)
    );
    assert!(report.surfaces.iter().any(
      |s| s.name == "unsupported internal import" && s.status == CompatibilityLevel::Unsupported
    ));
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "extensions" && s.status == CompatibilityLevel::Partial)
    );
  }

  #[test]
  fn test_inspect_valid_skill() {
    let discovery = Discovery::new(PathBuf::from("/nonexistent"));
    let target = fixture_path("tests/compat/skills/home/.agents/skills/pdf-tools/SKILL.md");
    let report = inspect_target(&target, &discovery).unwrap();
    assert_eq!(report.kind, TargetKind::Skill);
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "skill frontmatter" && s.status == CompatibilityLevel::Supported)
    );
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "skill name" && s.status == CompatibilityLevel::Supported)
    );
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "skill description" && s.status == CompatibilityLevel::Supported)
    );
  }

  #[test]
  fn test_inspect_valid_prompt() {
    let discovery = Discovery::new(PathBuf::from("/nonexistent"));
    let target = fixture_path("tests/compat/prompts/home/.pi/agent/prompts/review.md");
    let report = inspect_target(&target, &discovery).unwrap();
    assert_eq!(report.kind, TargetKind::Prompt);
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "prompt frontmatter" && s.status == CompatibilityLevel::Supported)
    );
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "prompt body" && s.status == CompatibilityLevel::Supported)
    );
    assert!(
      report
        .surfaces
        .iter()
        .any(|s| s.name == "parameter substitution" && s.status == CompatibilityLevel::Supported)
    );
  }

  #[test]
  fn test_inspect_nonexistent_target_returns_error() {
    let discovery = Discovery::new(PathBuf::from("/nonexistent"));
    let err = inspect_target("/path/does/not/exist/ever", &discovery).unwrap_err();
    assert!(err.contains("cannot inspect"));
  }
}
