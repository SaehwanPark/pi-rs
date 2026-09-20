//! Parsing Pi-compatible package manifests (`package.json`).
//!
//! A Pi package is a directory with a `package.json` that may declare skills, prompt
//! templates, and TypeScript extensions. This module reads the manifest file, extracts
//! the surfaces rupi supports today, and produces explicit diagnostics for everything
//! it observed but cannot act on. Parsing is observation-only: nothing here becomes
//! runtime state, an event, or a capability unless the runtime deliberately adopts it.
//!
//! # What Pi packages declare
//!
//! A compatible `package.json` may carry:
//!
//! ```text
//! {
//!   "name": "my-package",
//!   "version": "1.0.0",
//!   "description": "...",
//!   "pi": {
//!     "skills":  ["skills/", "skills/pdf-tools"],
//!     "prompts": ["prompts/review.md"]
//!   },
//!   "extensions": ["dist/extension.js"]
//! }
//! ```
//!
//! The `pi.skills` and `pi.prompts` entries are directory or file paths relative to the
//! package root. A package may also have a `skills/` or `prompts/` sub-directory whose
//! files are discovered by convention without manifest entries; that discovery is the
//! responsibility of the caller, not this module. `extensions` are TypeScript/JavaScript
//! entry points that require the Node compatibility host, which is not started yet.
//!
//! # Surface support
//!
//! | Surface      | Support |
//! |--------------|---------|
//! | `pi.skills`  | paths extracted; callers integrate with skill discovery |
//! | `pi.prompts` | paths extracted; callers integrate with prompt discovery |
//! | `extensions` | recorded as [`Warning::UnsupportedSurface`]; explicit Node host activation required |
//! | Unknown Pi-namespace keys | recorded as [`Warning::UnknownSurface`] |
//!
//! Per-surface diagnostics let a caller report compatibility without refusing the whole
//! package because one optional feature is unrecognised.
//!
//! # JSON reading
//!
//! The reader handles the subset of JSON that appears in real Pi package manifests:
//! string scalars, arrays of strings, and a flat or one-level-deep object. It does not
//! attempt to model deeply nested objects, numbers, or booleans beyond detecting their
//! presence. This is the same philosophy as the frontmatter reader: read exactly what
//! the format documents, surface area for nothing beyond it.

use std::{
  fs, io,
  path::{Path, PathBuf},
  process,
  time::{SystemTime, UNIX_EPOCH},
};

use crate::scan::{self, Discovery, Source, Trust};

/// The parsed content of a `package.json` that rupi can act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
  /// The `name` field if present and a string.
  pub name: Option<String>,
  /// The `version` field if present and a string.
  pub version: Option<String>,
  /// The `description` field if present and a string.
  pub description: Option<String>,
  /// Paths declared in `pi.skills`, relative to the package directory. These point to
  /// skill directories or `SKILL.md` files and are integrated by the caller with the
  /// standard skill discovery rules.
  pub skill_paths: Vec<String>,
  /// Paths declared in `pi.prompts`, relative to the package directory. These point to
  /// prompt template files and are integrated by the caller with the standard prompt
  /// discovery rules.
  pub prompt_paths: Vec<String>,
  /// Entry point paths declared in `extensions`, relative to the package directory.
  pub extension_paths: Vec<String>,
}

/// A surface or condition worth reporting to the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
  /// `extensions` was declared. The Node compatibility host is not started by rupi
  /// automatically; callers must explicitly activate the trusted Phase 8 host.
  UnsupportedSurface {
    /// Which surface name (`"extensions"`, etc.).
    surface: String,
    /// Number of entries that were declared (0 for a non-array value).
    count: usize,
  },
  /// A key inside the `pi` namespace that this reader does not know how to act on.
  /// Recorded so a reviewer can judge whether to add support without silently dropping
  /// the information.
  UnknownSurface { key: String, value_kind: ValueKind },
  /// The `package.json` file could not be read (permissions, not UTF-8, etc.).
  Unreadable { path: PathBuf, reason: String },
  /// The file contains text that is not valid JSON according to this reader's model.
  /// The message names the position or structural reason.
  Malformed { path: PathBuf, reason: String },
  /// Two locations declared one package name. The first found was kept, this one was dropped.
  Duplicate {
    name: String,
    kept: PathBuf,
    ignored: PathBuf,
  },
  /// A subdirectory in a package location is missing a `package.json` manifest.
  MissingManifest { path: PathBuf },
  /// A manifest surface path was absolute, escaped with `..`, or resolved through
  /// a symlink outside the package root, so it was not activated.
  InvalidSurfacePath {
    surface: String,
    path: String,
    reason: String,
  },
}

/// A coarse classification of a JSON value, used in unknown-surface diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
  String,
  Array,
  Object,
  Number,
  Bool,
  Null,
  Unknown,
}

impl ValueKind {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::String => "string",
      Self::Array => "array",
      Self::Object => "object",
      Self::Number => "number",
      Self::Bool => "boolean",
      Self::Null => "null",
      Self::Unknown => "unknown",
    }
  }
}

/// The result of parsing a `package.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parse {
  pub manifest: Manifest,
  pub warnings: Vec<Warning>,
}

/// A package discovered on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
  /// The package's name. Taken from `manifest.name` if present; otherwise defaults
  /// to the directory name.
  pub name: String,
  /// Declared version if present.
  pub version: Option<String>,
  /// Declared description if present.
  pub description: Option<String>,
  /// Root directory of the package.
  pub path: PathBuf,
  /// Path to the `package.json` file.
  pub manifest_path: PathBuf,
  /// Source location (Global or Project).
  pub source: Source,
  /// The parsed manifest.
  pub manifest: Manifest,
  /// Warnings specific to this package (e.g. unsupported extensions, unknown surfaces).
  pub warnings: Vec<Warning>,
}

/// A surface status reported for compatibility diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceStatus {
  /// Surface is supported and present in this package.
  Supported,
  /// Surface is present but only a bounded subset is supported in current rupi.
  Partial,
  /// Surface is present but unsupported in current rupi.
  Unsupported,
  /// Surface was not declared or found in this package.
  NotPresent,
}

impl SurfaceStatus {
  pub fn symbol(self) -> &'static str {
    match self {
      Self::Supported => "✓",
      Self::Partial => "△",
      Self::Unsupported => "✗",
      Self::NotPresent => "-",
    }
  }
}

impl Package {
  /// Locations of skills contained within this package.
  ///
  /// If `manifest.skill_paths` is non-empty, each path is resolved relative to the
  /// package root.
  ///
  /// If no paths were declared, checks for conventional skill locations:
  /// - `<package>/skills/` if it exists as a directory
  /// - `<package>/SKILL.md` if it exists as a file
  pub fn skill_locations(&self) -> Vec<PathBuf> {
    if !self.manifest.skill_paths.is_empty() {
      return self
        .manifest
        .skill_paths
        .iter()
        .filter_map(|rel| safe_manifest_path(&self.path, rel))
        .collect();
    }
    if let Some(skills_dir) = safe_manifest_path(&self.path, "skills")
      && skills_dir.is_dir()
    {
      return vec![skills_dir];
    }
    if let Some(skill_md) = safe_manifest_path(&self.path, "SKILL.md")
      && skill_md.is_file()
    {
      return vec![skill_md];
    }
    Vec::new()
  }

  /// Locations of prompt templates contained within this package.
  ///
  /// If `manifest.prompt_paths` is non-empty, each path is resolved relative to the
  /// package root.
  ///
  /// If no paths were declared, checks for conventional prompt locations:
  /// - `<package>/prompts/` if it exists as a directory
  pub fn prompt_locations(&self) -> Vec<PathBuf> {
    if !self.manifest.prompt_paths.is_empty() {
      return self
        .manifest
        .prompt_paths
        .iter()
        .filter_map(|rel| safe_manifest_path(&self.path, rel))
        .collect();
    }
    if let Some(prompts_dir) = safe_manifest_path(&self.path, "prompts")
      && prompts_dir.is_dir()
    {
      return vec![prompts_dir];
    }
    Vec::new()
  }

  /// Locations of extension entry points declared within this package.
  pub fn extension_locations(&self) -> Vec<PathBuf> {
    if !self.manifest.extension_paths.is_empty() {
      return self
        .manifest
        .extension_paths
        .iter()
        .filter_map(|rel| safe_manifest_path(&self.path, rel))
        .collect();
    }
    if let Some(extensions_dir) = safe_manifest_path(&self.path, "extensions")
      && extensions_dir.is_dir()
    {
      return vec![extensions_dir];
    }
    Vec::new()
  }

  /// Whether the package declared TypeScript extension entry points.
  pub fn has_extensions(&self) -> bool {
    self.warnings.iter().any(|w| {
      matches!(
        w,
        Warning::UnsupportedSurface { surface, .. } if surface == "extensions"
      )
    })
  }

  /// Check status of common Pi surfaces in this package.
  pub fn surfaces(&self) -> Vec<(&'static str, SurfaceStatus)> {
    vec![
      (
        "skills",
        if !self.skill_locations().is_empty() {
          SurfaceStatus::Supported
        } else {
          SurfaceStatus::NotPresent
        },
      ),
      (
        "prompts",
        if !self.prompt_locations().is_empty() {
          SurfaceStatus::Supported
        } else {
          SurfaceStatus::NotPresent
        },
      ),
      (
        "extensions",
        if self.has_extensions() {
          SurfaceStatus::Partial
        } else {
          SurfaceStatus::NotPresent
        },
      ),
    ]
  }
}

/// What a package discovery scan found, and everything it declined or warned about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageScan {
  pub packages: Vec<Package>,
  pub warnings: Vec<Warning>,
}

impl PackageScan {
  /// Look up a discovered package by name.
  pub fn named(&self, name: &str) -> Option<&Package> {
    self.packages.iter().find(|pkg| pkg.name == name)
  }
}

/// Result of an explicit local package installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
  /// Name from the copied manifest, not a name inferred from the source path.
  pub name: String,
  /// Final package directory discovered by the normal package scanner.
  pub path: PathBuf,
  /// Whether the destination is project-local rather than global.
  pub project: bool,
}

/// Copy a local Pi package into the selected package directory.
///
/// This is deliberately narrower than the upstream installer: it accepts only a
/// local directory, never follows symlinks, never runs package-manager scripts, and
/// refuses an existing destination. The explicit copy is the user's install action;
/// activation of project-local package content still requires the caller to provide
/// a trusted [`Discovery`].
pub fn install_local(source: &Path, cwd: &Path, project: bool) -> Result<InstalledPackage, String> {
  let (package_root, containment_root) = if project {
    let root = fs::canonicalize(crate::scan::project_root(cwd)).map_err(|error| {
      format!(
        "cannot resolve project package root '{}': {error}",
        crate::scan::project_root(cwd).display()
      )
    })?;
    (root.join(".pi/packages"), Some(root))
  } else {
    let home = crate::scan::home_dir()
      .ok_or_else(|| "global package installation needs HOME or USERPROFILE".to_string())?;
    (home.join(".pi/agent/packages"), None)
  };
  install_local_into(source, project, package_root, containment_root)
}

fn install_local_into(
  source: &Path,
  project: bool,
  package_root: PathBuf,
  containment_root: Option<PathBuf>,
) -> Result<InstalledPackage, String> {
  let source_metadata = fs::symlink_metadata(source).map_err(|error| {
    format!(
      "cannot inspect package source {}: {error}",
      source.display()
    )
  })?;
  if source_metadata.file_type().is_symlink() {
    return Err(format!(
      "refusing symlink package source: {}",
      source.display()
    ));
  }
  if !source_metadata.is_dir() {
    return Err(format!(
      "package source {} is not a directory",
      source.display()
    ));
  }
  let source = fs::canonicalize(source).map_err(|error| {
    format!(
      "cannot resolve package source {}: {error}",
      source.display()
    )
  })?;

  let manifest_path = source.join("package.json");
  let parsed = read(&manifest_path);
  let fatal = parsed.warnings.iter().any(|warning| {
    matches!(
      warning,
      Warning::Malformed { .. } | Warning::Unreadable { .. }
    )
  });
  if fatal {
    return Err(format_manifest_warnings(&parsed.warnings));
  }
  let name = parsed
    .manifest
    .name
    .as_deref()
    .map(str::trim)
    .filter(|name| !name.is_empty())
    .ok_or_else(|| {
      format!(
        "package manifest {} must declare a non-empty name",
        manifest_path.display()
      )
    })?
    .to_string();
  let directory_name = install_directory_name(&name)?;

  if let Some(root) = containment_root {
    ensure_contained_package_root(&root, &package_root)?;
  } else {
    fs::create_dir_all(&package_root).map_err(|error| {
      format!(
        "cannot create package destination {}: {error}",
        package_root.display()
      )
    })?;
  }
  let canonical_package_root = fs::canonicalize(&package_root).map_err(|error| {
    format!(
      "cannot resolve package destination {}: {error}",
      package_root.display()
    )
  })?;
  let destination = package_root.join(directory_name);
  if fs::symlink_metadata(&destination).is_ok() {
    return Err(format!(
      "package destination {} already exists; remove it before installing",
      destination.display()
    ));
  }
  if source.starts_with(&canonical_package_root) {
    return Err(format!(
      "package source {} is already inside destination root {}",
      source.display(),
      package_root.display()
    ));
  }

  let temporary = temporary_destination(&destination);
  if temporary.exists() {
    return Err(format!(
      "temporary package destination {} already exists",
      temporary.display()
    ));
  }
  let result = copy_tree(&source, &temporary).and_then(|()| {
    fs::rename(&temporary, &destination).map_err(|error| {
      format!(
        "cannot activate package at {}: {error}",
        destination.display()
      )
    })
  });
  if result.is_err() {
    let _ = fs::remove_dir_all(&temporary);
  }
  result.map(|()| InstalledPackage {
    name,
    path: destination,
    project,
  })
}

fn ensure_contained_package_root(root: &Path, package_root: &Path) -> Result<(), String> {
  let relative = package_root.strip_prefix(root).map_err(|_| {
    format!(
      "package destination {} is outside project root {}",
      package_root.display(),
      root.display()
    )
  })?;
  let mut current = root.to_path_buf();
  for component in relative.components() {
    let std::path::Component::Normal(name) = component else {
      return Err(format!(
        "package destination {} contains an invalid path component",
        package_root.display()
      ));
    };
    current.push(name);
    match fs::symlink_metadata(&current) {
      Ok(metadata) if metadata.file_type().is_symlink() => {
        return Err(format!(
          "project package destination component {} must not be a symlink",
          current.display()
        ));
      }
      Ok(metadata) if !metadata.is_dir() => {
        return Err(format!(
          "project package destination component {} is not a directory",
          current.display()
        ));
      }
      Ok(_) => {}
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
        fs::create_dir(&current).map_err(|error| {
          format!(
            "cannot create project package destination {}: {error}",
            current.display()
          )
        })?;
      }
      Err(error) => {
        return Err(format!(
          "cannot inspect project package destination {}: {error}",
          current.display()
        ));
      }
    }
  }
  let canonical = fs::canonicalize(&current).map_err(|error| {
    format!(
      "cannot resolve project package destination {}: {error}",
      current.display()
    )
  })?;
  let canonical_root = fs::canonicalize(root)
    .map_err(|error| format!("cannot resolve project root {}: {error}", root.display()))?;
  if !canonical.starts_with(&canonical_root) {
    return Err(format!(
      "project package destination {} escapes project root {}",
      current.display(),
      root.display()
    ));
  }
  Ok(())
}

fn format_manifest_warnings(warnings: &[Warning]) -> String {
  warnings
    .iter()
    .map(|warning| match warning {
      Warning::Unreadable { path, reason } => format!("{}: {reason}", path.display()),
      Warning::Malformed { path, reason } => format!("{}: {reason}", path.display()),
      other => format!("invalid package manifest: {other:?}"),
    })
    .collect::<Vec<_>>()
    .join("; ")
}

fn install_directory_name(name: &str) -> Result<String, String> {
  if name == "." || name == ".." || name.contains('\0') {
    return Err(format!(
      "package name '{name}' cannot be used as a destination"
    ));
  }
  let directory = name.replace(['/', '\\'], "__");
  if directory.is_empty() || directory == "." || directory == ".." {
    return Err(format!(
      "package name '{name}' cannot be used as a destination"
    ));
  }
  Ok(directory)
}

fn temporary_destination(destination: &Path) -> PathBuf {
  let stamp = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_nanos())
    .unwrap_or_default();
  let pid = process::id();
  PathBuf::from(format!("{}.tmp-{pid}-{stamp}", destination.display()))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
  let metadata = fs::symlink_metadata(source)
    .map_err(|error| format!("cannot inspect {}: {error}", source.display()))?;
  if metadata.file_type().is_symlink() {
    return Err(format!(
      "refusing symlink in package source: {}",
      source.display()
    ));
  }
  if metadata.is_dir() {
    fs::create_dir(destination)
      .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
    let mut entries = fs::read_dir(source)
      .map_err(|error| format!("cannot read {}: {error}", source.display()))?
      .collect::<Result<Vec<_>, _>>()
      .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
      copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
    }
    return Ok(());
  }
  if !metadata.is_file() {
    return Err(format!(
      "refusing unsupported file type in package source: {}",
      source.display()
    ));
  }
  copy_regular_file(source, destination)
}

fn copy_regular_file(source: &Path, destination: &Path) -> Result<(), String> {
  let mut input_options = fs::OpenOptions::new();
  input_options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    input_options.custom_flags(libc::O_NOFOLLOW);
  }
  let mut input = input_options.open(source).map_err(|error| {
    format!(
      "cannot open {} without following a symlink: {error}",
      source.display()
    )
  })?;
  if !input
    .metadata()
    .map_err(|error| format!("cannot inspect {}: {error}", source.display()))?
    .is_file()
  {
    return Err(format!(
      "refusing unsupported file type in package source: {}",
      source.display()
    ));
  }
  let mut output = fs::OpenOptions::new()
    .write(true)
    .create_new(true)
    .open(destination)
    .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
  io::copy(&mut input, &mut output)
    .map_err(|error| format!("cannot copy {}: {error}", source.display()))?;
  output
    .sync_all()
    .map_err(|error| format!("cannot flush {}: {error}", destination.display()))?;
  Ok(())
}

/// Scan the documented package locations.
///
/// Order of discovery:
/// 1. `$HOME/.pi/agent/packages` (Global)
/// 2. `$HOME/.pi/packages` (Global)
/// 3. `<ancestor>/.pi/packages` (Project, only when trusted, nearest to git root)
///
/// Within each location, subdirectories are visited in lexicographical order.
/// If two locations declare the same package name, first found wins and the second
/// produces [`Warning::Duplicate`].
pub fn discover(discovery: &Discovery) -> PackageScan {
  let mut scan = PackageScan::default();
  let mut seen: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();

  if let Some(home) = &discovery.home {
    scan_location(
      &home.join(".pi/agent/packages"),
      Source::Global,
      &mut seen,
      &mut scan,
    );
    scan_location(
      &home.join(".pi/packages"),
      Source::Global,
      &mut seen,
      &mut scan,
    );
  }

  if discovery.trust == Trust::Trusted {
    for project in scan::project_dirs(&discovery.cwd) {
      scan_location(
        &project.join(".pi/packages"),
        Source::Project,
        &mut seen,
        &mut scan,
      );
    }
  }

  scan
}

pub(crate) fn invalid_manifest_paths(root: &Path, manifest: &Manifest) -> Vec<Warning> {
  let mut warnings = Vec::new();
  for (surface, paths) in [
    ("skills", &manifest.skill_paths),
    ("prompts", &manifest.prompt_paths),
    ("extensions", &manifest.extension_paths),
  ] {
    for path in paths {
      if !is_safe_manifest_path(root, path) {
        warnings.push(Warning::InvalidSurfacePath {
          surface: surface.to_string(),
          path: path.clone(),
          reason: "path is absolute, escapes the package, or resolves through an external symlink"
            .to_string(),
        });
      }
    }
  }
  warnings
}

pub(crate) fn safe_manifest_path(root: &Path, relative: &str) -> Option<PathBuf> {
  let path = Path::new(relative);
  if path.is_absolute()
    || path.components().any(|component| {
      matches!(
        component,
        std::path::Component::ParentDir
          | std::path::Component::RootDir
          | std::path::Component::Prefix(_)
      )
    })
  {
    return None;
  }
  let candidate = root.join(path);
  if !is_safe_manifest_path(root, relative) {
    return None;
  }
  Some(candidate)
}

fn is_safe_manifest_path(root: &Path, relative: &str) -> bool {
  let path = Path::new(relative);
  if path.is_absolute()
    || path.components().any(|component| {
      matches!(
        component,
        std::path::Component::ParentDir
          | std::path::Component::RootDir
          | std::path::Component::Prefix(_)
      )
    })
  {
    return false;
  }
  let Ok(root) = fs::canonicalize(root) else {
    return false;
  };
  let candidate = root.join(path);
  let mut existing = candidate.clone();
  while fs::symlink_metadata(&existing).is_err() {
    let Some(parent) = existing.parent() else {
      return false;
    };
    if parent == existing {
      return false;
    }
    existing = parent.to_path_buf();
  }
  fs::canonicalize(existing)
    .map(|resolved| resolved.starts_with(&root))
    .unwrap_or(false)
}

fn scan_location(
  dir: &Path,
  source: Source,
  seen: &mut std::collections::BTreeMap<String, PathBuf>,
  scan: &mut PackageScan,
) {
  if let Ok(metadata) = fs::symlink_metadata(dir)
    && metadata.file_type().is_symlink()
  {
    scan.warnings.push(Warning::Unreadable {
      path: dir.to_path_buf(),
      reason: "refusing to scan a symlinked package location".to_string(),
    });
    return;
  }

  let entries = match std::fs::read_dir(dir) {
    Ok(entries) => entries,
    Err(error) => {
      if error.kind() != std::io::ErrorKind::NotFound {
        scan.warnings.push(Warning::Unreadable {
          path: dir.to_path_buf(),
          reason: error.to_string(),
        });
      }
      return;
    }
  };

  let mut children: Vec<PathBuf> = Vec::new();
  for entry in entries {
    match entry {
      Ok(e) => children.push(e.path()),
      Err(error) => {
        scan.warnings.push(Warning::Unreadable {
          path: dir.to_path_buf(),
          reason: error.to_string(),
        });
      }
    }
  }

  // Sort by filename for reproducible discovery order
  children.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

  for child in children {
    if let Ok(metadata) = fs::symlink_metadata(&child)
      && metadata.file_type().is_symlink()
    {
      scan.warnings.push(Warning::Unreadable {
        path: child,
        reason: "refusing to scan a symlinked package root".to_string(),
      });
      continue;
    }
    if !child.is_dir() {
      continue;
    }

    let manifest_path = child.join("package.json");
    if !manifest_path.is_file() {
      scan.warnings.push(Warning::MissingManifest {
        path: child.clone(),
      });
      continue;
    }

    let parse = read(&manifest_path);

    let has_fatal = parse
      .warnings
      .iter()
      .any(|w| matches!(w, Warning::Malformed { .. } | Warning::Unreadable { .. }));
    if has_fatal {
      for w in parse.warnings {
        scan.warnings.push(w);
      }
      continue;
    }

    let name = parse.manifest.name.clone().unwrap_or_else(|| {
      child
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("")
        .to_string()
    });

    if name.is_empty() {
      scan.warnings.push(Warning::Malformed {
        path: manifest_path,
        reason: "package has no name and directory name is empty".to_string(),
      });
      continue;
    }

    if let Some(kept) = seen.get(&name) {
      scan.warnings.push(Warning::Duplicate {
        name,
        kept: kept.clone(),
        ignored: child,
      });
      continue;
    }

    seen.insert(name.clone(), child.clone());

    let version = parse.manifest.version.clone();
    let description = parse.manifest.description.clone();
    let mut package_warnings = parse.warnings;
    package_warnings.extend(invalid_manifest_paths(&child, &parse.manifest));

    for warning in &package_warnings {
      scan.warnings.push(warning.clone());
    }

    scan.packages.push(Package {
      name,
      version,
      description,
      path: child,
      manifest_path,
      source,
      manifest: parse.manifest,
      warnings: package_warnings,
    });
  }
}

/// Read a `package.json` file at `path` and return what could be extracted.
///
/// An unreadable or unparseable file produces a `Parse` with no manifest data and a
/// single warning explaining what happened; the caller can treat that the same as an
/// empty manifest with a diagnostic, which is the honest representation of "we tried".
pub fn read(path: &Path) -> Parse {
  let text = match std::fs::read_to_string(path) {
    Ok(text) => text,
    Err(error) => {
      return Parse {
        warnings: vec![Warning::Unreadable {
          path: path.to_path_buf(),
          reason: error.to_string(),
        }],
        ..Default::default()
      };
    }
  };
  parse_text(&text, path)
}

/// Parse the text of a `package.json`. Exposed separately so tests can pass strings
/// directly without touching the filesystem.
pub fn parse_text(text: &str, path: &Path) -> Parse {
  let mut manifest = Manifest::default();
  let mut warnings: Vec<Warning> = Vec::new();

  let mut parser = Parser::new(text);
  match parser.parse_top_level(&mut manifest, &mut warnings) {
    Ok(()) => {}
    Err(reason) => {
      warnings.push(Warning::Malformed {
        path: path.to_path_buf(),
        reason,
      });
    }
  }

  Parse { manifest, warnings }
}

// ---------------------------------------------------------------------------
// Minimal hand-rolled JSON reader
// ---------------------------------------------------------------------------
//
// The goals are identical to those of the frontmatter reader: parse exactly
// what Pi package.json manifests use, nothing more. The grammar handled:
//
//   object    ::= '{' (pair (',' pair)*)? '}'
//   pair      ::= string ':' value
//   value     ::= string | array | object | literal
//   string    ::= '"' char* '"'  (\n \t \\ \" recognised; \uXXXX decoded)
//   array     ::= '[' (value (',' value)*)? ']'
//   literal   ::= 'true' | 'false' | 'null' | [-0-9.]+
//
// Comments, trailing commas, single-quoted strings, and multi-document
// structures are not JSON and are not handled.

struct Parser<'a> {
  src: &'a [u8],
  pos: usize,
}

impl<'a> Parser<'a> {
  fn new(text: &'a str) -> Self {
    Self {
      src: text.as_bytes(),
      pos: 0,
    }
  }

  fn peek(&self) -> Option<u8> {
    self.src.get(self.pos).copied()
  }

  fn advance(&mut self) {
    self.pos += 1;
  }

  fn skip_whitespace(&mut self) {
    while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
      self.advance();
    }
  }

  fn expect_byte(&mut self, byte: u8) -> Result<(), String> {
    self.skip_whitespace();
    match self.peek() {
      Some(b) if b == byte => {
        self.advance();
        Ok(())
      }
      Some(b) => Err(format!(
        "expected '{}' at position {}, found '{}'",
        byte as char, self.pos, b as char
      )),
      None => Err(format!(
        "unexpected end of input, expected '{}'",
        byte as char
      )),
    }
  }

  /// Read a JSON string value (the surrounding quotes are consumed).
  fn parse_string(&mut self) -> Result<String, String> {
    self.skip_whitespace();
    match self.peek() {
      Some(b'"') => self.advance(),
      Some(b) => {
        return Err(format!(
          "expected '\"' at position {}, found '{}'",
          self.pos, b as char
        ));
      }
      None => return Err("unexpected end of input, expected '\"'".to_string()),
    }
    let mut out = String::new();
    loop {
      match self.peek() {
        None => return Err("unterminated string".to_string()),
        Some(b'"') => {
          self.advance();
          return Ok(out);
        }
        Some(b'\\') => {
          self.advance();
          match self.peek() {
            None => return Err("unterminated escape".to_string()),
            Some(b'"') => {
              out.push('"');
              self.advance();
            }
            Some(b'\\') => {
              out.push('\\');
              self.advance();
            }
            Some(b'n') => {
              out.push('\n');
              self.advance();
            }
            Some(b't') => {
              out.push('\t');
              self.advance();
            }
            Some(b'r') => {
              out.push('\r');
              self.advance();
            }
            Some(b'/') => {
              out.push('/');
              self.advance();
            }
            Some(b'b') => {
              out.push('\x08');
              self.advance();
            }
            Some(b'f') => {
              out.push('\x0C');
              self.advance();
            }
            Some(b'u') => {
              // 4 hex digits
              self.advance();
              let start = self.pos;
              for _ in 0..4 {
                match self.peek() {
                  Some(b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F') => self.advance(),
                  _ => return Err(format!("invalid unicode escape at position {}", self.pos)),
                }
              }
              let hex = std::str::from_utf8(&self.src[start..self.pos]).expect("ascii hex digits");
              let code_point = u32::from_str_radix(hex, 16)
                .map_err(|_| format!("invalid unicode escape '\\u{hex}'"))?;
              let ch = char::from_u32(code_point)
                .ok_or_else(|| format!("unrepresentable code point U+{code_point:04X}"))?;
              out.push(ch);
            }
            Some(other) => {
              // Unknown escape: pass through verbatim (lenient).
              out.push(other as char);
              self.advance();
            }
          }
        }
        Some(b) => {
          // Decode a UTF-8 character from the byte slice.
          let start = self.pos;
          let width = utf8_char_width(b);
          let end = start + width;
          if end > self.src.len() {
            return Err(format!("truncated UTF-8 sequence at position {start}"));
          }
          let s = std::str::from_utf8(&self.src[start..end])
            .map_err(|_| format!("invalid UTF-8 at position {start}"))?
            .to_string();
          out.push_str(&s);
          self.pos = end;
        }
      }
    }
  }

  /// Parse an array of strings, collecting string items and skipping non-string ones.
  fn parse_string_array(&mut self) -> Result<Vec<String>, String> {
    self.expect_byte(b'[')?;
    self.skip_whitespace();
    let mut items = Vec::new();
    if self.peek() == Some(b']') {
      self.advance();
      return Ok(items);
    }
    loop {
      self.skip_whitespace();
      if self.peek() == Some(b'"') {
        items.push(self.parse_string()?);
      } else {
        self.skip_value()?;
      }
      self.skip_whitespace();
      match self.peek() {
        Some(b',') => {
          self.advance();
        }
        Some(b']') => {
          self.advance();
          return Ok(items);
        }
        Some(b) => {
          return Err(format!(
            "expected ',' or ']' at position {}, found '{}'",
            self.pos, b as char
          ));
        }
        None => return Err("unterminated array".to_string()),
      }
    }
  }

  /// Skip over one JSON value (string, array, object, or literal) without retaining
  /// its content. Used to step past values in keys this reader does not recognise.
  fn skip_value(&mut self) -> Result<(), String> {
    self.skip_whitespace();
    match self.peek() {
      Some(b'"') => {
        self.parse_string()?;
      }
      Some(b'[') => {
        self.advance();
        let mut depth = 1usize;
        while depth > 0 {
          match self.peek() {
            None => return Err("unterminated array in skip".to_string()),
            Some(b'[') => {
              depth += 1;
              self.advance();
            }
            Some(b']') => {
              depth -= 1;
              self.advance();
            }
            Some(b'"') => {
              self.parse_string()?;
            }
            _ => self.advance(),
          }
        }
      }
      Some(b'{') => {
        self.advance();
        let mut depth = 1usize;
        while depth > 0 {
          match self.peek() {
            None => return Err("unterminated object in skip".to_string()),
            Some(b'{') => {
              depth += 1;
              self.advance();
            }
            Some(b'}') => {
              depth -= 1;
              self.advance();
            }
            Some(b'"') => {
              self.parse_string()?;
            }
            _ => self.advance(),
          }
        }
      }
      Some(_) => {
        // Literal: true, false, null, or a number. Read until delimiter.
        while let Some(b) = self.peek() {
          match b {
            b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' => break,
            _ => self.advance(),
          }
        }
      }
      None => return Err("unexpected end of input in value".to_string()),
    }
    Ok(())
  }

  /// Classify a value without consuming it: peek at the first non-whitespace byte.
  fn value_kind(&self) -> ValueKind {
    let mut i = self.pos;
    while i < self.src.len() {
      match self.src[i] {
        b' ' | b'\t' | b'\n' | b'\r' => i += 1,
        b'"' => return ValueKind::String,
        b'[' => return ValueKind::Array,
        b'{' => return ValueKind::Object,
        b't' | b'f' => return ValueKind::Bool,
        b'n' => return ValueKind::Null,
        b'0'..=b'9' | b'-' => return ValueKind::Number,
        _ => return ValueKind::Unknown,
      }
    }
    ValueKind::Unknown
  }

  /// Parse the `pi` namespace object: `{ "skills": [...], "prompts": [...], ... }`.
  fn parse_pi_namespace(
    &mut self,
    manifest: &mut Manifest,
    warnings: &mut Vec<Warning>,
  ) -> Result<(), String> {
    self.expect_byte(b'{')?;
    self.skip_whitespace();
    if self.peek() == Some(b'}') {
      self.advance();
      return Ok(());
    }
    loop {
      self.skip_whitespace();
      let key = self.parse_string()?;
      self.skip_whitespace();
      self.expect_byte(b':')?;
      match key.as_str() {
        "skills" => {
          manifest.skill_paths = self.parse_string_array()?;
        }
        "prompts" => {
          manifest.prompt_paths = self.parse_string_array()?;
        }
        _ => {
          // Unknown key within the pi namespace: record its type and skip.
          let kind = self.value_kind();
          warnings.push(Warning::UnknownSurface {
            key,
            value_kind: kind,
          });
          self.skip_value()?;
        }
      }
      self.skip_whitespace();
      match self.peek() {
        Some(b',') => {
          self.advance();
        }
        Some(b'}') => {
          self.advance();
          return Ok(());
        }
        Some(b) => {
          return Err(format!(
            "expected ',' or '}}' at position {}, found '{}'",
            self.pos, b as char
          ));
        }
        None => return Err("unterminated object".to_string()),
      }
    }
  }

  /// Parse the top-level `package.json` object.
  fn parse_top_level(
    &mut self,
    manifest: &mut Manifest,
    warnings: &mut Vec<Warning>,
  ) -> Result<(), String> {
    self.skip_whitespace();
    self.expect_byte(b'{')?;
    self.skip_whitespace();
    if self.peek() == Some(b'}') {
      self.advance();
      return Ok(());
    }
    loop {
      self.skip_whitespace();
      let key = self.parse_string()?;
      self.skip_whitespace();
      self.expect_byte(b':')?;

      match key.as_str() {
        "name" => {
          manifest.name = Some(self.parse_string()?);
        }
        "version" => {
          manifest.version = Some(self.parse_string()?);
        }
        "description" => {
          manifest.description = Some(self.parse_string()?);
        }
        "pi" => {
          // The Pi-namespace object: contains "skills", "prompts", and future surfaces.
          self.parse_pi_namespace(manifest, warnings)?;
        }
        "extensions" => {
          // Node/TypeScript entry points are observed here; execution requires an
          // explicit trusted host activation and is reported as a partial surface.
          let kind = self.value_kind();
          let count = if kind == ValueKind::Array {
            let paths = self.parse_string_array()?;
            let count = paths.len();
            manifest.extension_paths = paths;
            count
          } else {
            self.skip_value()?;
            0
          };
          warnings.push(Warning::UnsupportedSurface {
            surface: "extensions".to_string(),
            count,
          });
        }
        _ => {
          // Standard npm metadata keys (main, license, author, keywords, repository,
          // scripts, dependencies, devDependencies, …) are not Pi surfaces and are
          // silently skipped so they do not pollute the diagnostic output with noise.
          self.skip_value()?;
        }
      }

      self.skip_whitespace();
      match self.peek() {
        Some(b',') => {
          self.advance();
        }
        Some(b'}') => {
          self.advance();
          return Ok(());
        }
        Some(b) => {
          return Err(format!(
            "expected ',' or '}}' at position {}, found '{}'",
            self.pos, b as char
          ));
        }
        None => return Err("unterminated object".to_string()),
      }
    }
  }
}

/// Width of a UTF-8 character from its leading byte.
fn utf8_char_width(b: u8) -> usize {
  match b {
    0x00..=0x7F => 1,
    0xC0..=0xDF => 2,
    0xE0..=0xEF => 3,
    0xF0..=0xF7 => 4,
    _ => 1, // continuation byte or invalid: treat as one byte
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn path() -> &'static Path {
    Path::new("package.json")
  }

  fn parse(text: &str) -> Parse {
    parse_text(text, path())
  }

  fn manifest(text: &str) -> Manifest {
    let p = parse(text);
    assert!(
      p.warnings.is_empty(),
      "unexpected warnings: {:?}",
      p.warnings
    );
    p.manifest
  }

  // -------------------------------------------------------------------------
  // Happy-path manifest fields
  // -------------------------------------------------------------------------

  #[test]
  fn an_empty_object_produces_an_empty_manifest() {
    let m = manifest("{}");
    assert_eq!(m.name, None);
    assert_eq!(m.version, None);
    assert_eq!(m.description, None);
    assert!(m.skill_paths.is_empty());
    assert!(m.prompt_paths.is_empty());
  }

  #[test]
  fn name_version_and_description_are_extracted() {
    let m = manifest(
      r#"{
        "name": "my-package",
        "version": "1.2.3",
        "description": "Extracts text from PDFs."
      }"#,
    );
    assert_eq!(m.name.as_deref(), Some("my-package"));
    assert_eq!(m.version.as_deref(), Some("1.2.3"));
    assert_eq!(m.description.as_deref(), Some("Extracts text from PDFs."));
  }

  #[test]
  fn pi_skills_array_is_extracted_as_relative_paths() {
    let m = manifest(
      r#"{
        "name": "pkg",
        "pi": {
          "skills": ["skills/", "skills/pdf-tools", "SKILL.md"]
        }
      }"#,
    );
    assert_eq!(m.skill_paths, ["skills/", "skills/pdf-tools", "SKILL.md"]);
  }

  #[test]
  fn pi_prompts_array_is_extracted_as_relative_paths() {
    let m = manifest(
      r#"{
        "name": "pkg",
        "pi": {
          "prompts": ["prompts/review.md", "prompts/plan.md"]
        }
      }"#,
    );
    assert_eq!(m.prompt_paths, ["prompts/review.md", "prompts/plan.md"]);
  }

  #[test]
  fn empty_pi_skills_and_prompts_arrays_are_fine() {
    let m = manifest(r#"{"name":"pkg","pi":{"skills":[],"prompts":[]}}"#);
    assert!(m.skill_paths.is_empty());
    assert!(m.prompt_paths.is_empty());
  }

  #[test]
  fn empty_pi_namespace_is_fine() {
    let m = manifest(r#"{"name":"pkg","pi":{}}"#);
    assert!(m.skill_paths.is_empty());
    assert!(m.prompt_paths.is_empty());
    assert_eq!(m.name.as_deref(), Some("pkg"));
  }

  // -------------------------------------------------------------------------
  // Unsupported surfaces produce warnings, not errors
  // -------------------------------------------------------------------------

  #[test]
  fn extensions_produces_a_partial_surface_warning() {
    let p = parse(
      r#"{
        "name": "pkg",
        "extensions": ["dist/main.js", "dist/tools.js"]
      }"#,
    );
    assert_eq!(p.manifest.name.as_deref(), Some("pkg"));
    assert_eq!(p.warnings.len(), 1);
    assert!(matches!(
      &p.warnings[0],
      Warning::UnsupportedSurface { surface, count }
        if surface == "extensions" && *count == 2
    ));
  }

  #[test]
  fn extensions_count_is_zero_for_an_empty_array() {
    let p = parse(r#"{"name":"pkg","extensions":[]}"#);
    assert!(matches!(
      &p.warnings[0],
      Warning::UnsupportedSurface { count, .. } if *count == 0
    ));
  }

  #[test]
  fn unknown_pi_namespace_key_produces_unknown_surface_warning() {
    let p = parse(r#"{"name":"pkg","pi":{"skills":["s/"],"themes":["dark.json"]}}"#);
    assert_eq!(p.manifest.skill_paths, ["s/"]);
    assert!(matches!(
      &p.warnings[0],
      Warning::UnknownSurface { key, value_kind: ValueKind::Array }
        if key == "themes"
    ));
  }

  // -------------------------------------------------------------------------
  // Benign npm metadata does not produce noise
  // -------------------------------------------------------------------------

  #[test]
  fn standard_npm_keys_are_silently_skipped() {
    let p = parse(
      r#"{
        "name": "pkg",
        "version": "1.0.0",
        "license": "MIT",
        "author": "Alice",
        "main": "dist/index.js",
        "keywords": ["coding", "agent"],
        "repository": {"type": "git", "url": "https://example.com/repo"},
        "dependencies": {},
        "devDependencies": {}
      }"#,
    );
    // No warnings for pure npm metadata.
    assert!(
      p.warnings.is_empty(),
      "unexpected warnings: {:?}",
      p.warnings
    );
    assert_eq!(p.manifest.name.as_deref(), Some("pkg"));
    assert_eq!(p.manifest.version.as_deref(), Some("1.0.0"));
  }

  // -------------------------------------------------------------------------
  // String escapes
  // -------------------------------------------------------------------------

  #[test]
  fn string_escape_sequences_are_unescaped() {
    let m = manifest(r#"{"name":"a\/b","description":"line1\nline2"}"#);
    assert_eq!(m.name.as_deref(), Some("a/b"));
    assert_eq!(m.description.as_deref(), Some("line1\nline2"));
  }

  #[test]
  fn unicode_escape_in_string_is_decoded() {
    let m = manifest(r#"{"name":"\u0070kg"}"#);
    assert_eq!(m.name.as_deref(), Some("pkg"));
  }

  // -------------------------------------------------------------------------
  // Error cases
  // -------------------------------------------------------------------------

  #[test]
  fn not_a_json_object_is_malformed() {
    let p = parse("[]");
    assert!(matches!(p.warnings.as_slice(), [Warning::Malformed { .. }]));
  }

  #[test]
  fn truncated_object_is_malformed() {
    let p = parse(r#"{"name": "x""#);
    assert!(matches!(p.warnings.as_slice(), [Warning::Malformed { .. }]));
  }

  #[test]
  fn unterminated_string_value_is_malformed() {
    let p = parse(r#"{"name": "unclosed}"#);
    assert!(matches!(p.warnings.as_slice(), [Warning::Malformed { .. }]));
  }

  #[test]
  fn completely_empty_input_is_malformed() {
    let p = parse("");
    assert!(matches!(p.warnings.as_slice(), [Warning::Malformed { .. }]));
  }

  // -------------------------------------------------------------------------
  // Mix of supported and unsupported surfaces in one manifest
  // -------------------------------------------------------------------------

  #[test]
  fn a_full_manifest_with_extensions_extracts_what_it_can() {
    let p = parse(
      r#"{
        "name": "complete-pkg",
        "version": "2.0.0",
        "description": "A complete package.",
        "pi": {
          "skills":  ["skills/"],
          "prompts": ["prompts/review.md"]
        },
        "extensions": ["dist/ext.js"]
      }"#,
    );
    assert_eq!(p.manifest.name.as_deref(), Some("complete-pkg"));
    assert_eq!(p.manifest.skill_paths, ["skills/"]);
    assert_eq!(p.manifest.prompt_paths, ["prompts/review.md"]);
    assert_eq!(p.warnings.len(), 1);
    assert!(matches!(
      &p.warnings[0],
      Warning::UnsupportedSurface { surface, count }
        if surface == "extensions" && *count == 1
    ));
  }

  // -------------------------------------------------------------------------
  // Whitespace and formatting variations
  // -------------------------------------------------------------------------

  #[test]
  fn compact_json_with_no_whitespace_is_parsed() {
    let m =
      manifest(r#"{"name":"pkg","version":"1.0.0","pi":{"skills":["s/"],"prompts":["p.md"]}}"#);
    assert_eq!(m.name.as_deref(), Some("pkg"));
    assert_eq!(m.skill_paths, ["s/"]);
    assert_eq!(m.prompt_paths, ["p.md"]);
  }

  #[test]
  fn deeply_indented_json_is_parsed() {
    let m = manifest(
      "{\n  \"name\": \"indented\",\n  \"pi\": {\n    \"skills\": [\n      \"skills/\"\n    ]\n  }\n}",
    );
    assert_eq!(m.name.as_deref(), Some("indented"));
    assert_eq!(m.skill_paths, ["skills/"]);
  }

  // -------------------------------------------------------------------------
  // Package installation and discovery tests
  // -------------------------------------------------------------------------

  #[test]
  fn install_local_copies_global_package_without_running_scripts() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("skills")).expect("create source");
    std::fs::write(
      source.join("package.json"),
      r#"{"name":"local-pkg","scripts":{"postinstall":"touch should-not-exist"}}"#,
    )
    .expect("write manifest");
    std::fs::write(source.join("skills/SKILL.md"), "# skill\n").expect("write skill");

    let installed = install_local_into(&source, false, home.join(".pi/agent/packages"), None)
      .expect("install package");

    assert_eq!(installed.name, "local-pkg");
    assert!(!installed.project);
    assert!(installed.path.join("skills/SKILL.md").is_file());
    assert!(!installed.path.join("should-not-exist").exists());
    assert_eq!(
      discover(&Discovery {
        home: Some(home),
        cwd: temp.path().to_path_buf(),
        trust: Trust::Untrusted,
      })
      .named("local-pkg")
      .expect("installed package discovered")
      .path,
      installed.path
    );
  }

  #[test]
  fn install_local_uses_git_root_for_project_destination() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let project = temp.path().join("project");
    let nested = project.join("src/nested");
    let source = temp.path().join("source");
    std::fs::create_dir_all(&nested).expect("create nested cwd");
    std::fs::create_dir_all(&source).expect("create source");
    std::fs::create_dir_all(project.join(".git")).expect("create git root");
    std::fs::write(source.join("package.json"), r#"{"name":"project-pkg"}"#)
      .expect("write manifest");

    let installed = install_local(&source, &nested, true).expect("install project package");
    assert_eq!(
      installed.path,
      fs::canonicalize(project.join(".pi/packages/project-pkg")).unwrap()
    );
    assert!(installed.project);
    let scan = discover(&Discovery {
      home: None,
      cwd: nested,
      trust: Trust::Trusted,
    });
    assert_eq!(
      scan
        .named("project-pkg")
        .map(|pkg| fs::canonicalize(&pkg.path).unwrap()),
      Some(installed.path.clone())
    );
  }

  #[cfg(unix)]
  #[test]
  fn install_local_refuses_symlinked_project_destination() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let project = temp.path().join("project");
    let outside = temp.path().join("outside");
    let source = temp.path().join("source");
    std::fs::create_dir_all(project.join(".git")).expect("git root");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::create_dir_all(&source).expect("source");
    std::fs::write(source.join("package.json"), r#"{"name":"pkg"}"#).expect("manifest");
    std::os::unix::fs::symlink(&outside, project.join(".pi")).expect(".pi symlink");

    let error = install_local(&source, &project, true).expect_err("symlink destination refused");
    assert!(error.contains("must not be a symlink"), "{error}");
  }

  #[test]
  fn install_local_refuses_existing_destination_and_symlink_sources() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).expect("create source");
    std::fs::write(source.join("package.json"), r#"{"name":"pkg"}"#).expect("write manifest");

    let package_root = home.join(".pi/agent/packages");
    install_local_into(&source, false, package_root.clone(), None).expect("first install");
    let duplicate =
      install_local_into(&source, false, package_root, None).expect_err("duplicate refused");
    assert!(duplicate.contains("already exists"), "{duplicate}");
    #[cfg(unix)]
    {
      let link = temp.path().join("link");
      std::os::unix::fs::symlink(&source, &link).expect("create source symlink");
      let error = install_local(&link, temp.path(), false).expect_err("symlink refused");
      assert!(error.contains("refusing symlink"), "{error}");
    }
  }

  #[test]
  fn discover_finds_global_and_project_packages_when_trusted() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let project = temp.path().join("project");

    let global_pkg = home.join(".pi/agent/packages/global-one");
    std::fs::create_dir_all(&global_pkg).expect("create global pkg dir");
    std::fs::write(
      global_pkg.join("package.json"),
      r#"{"name": "global-one", "version": "1.0.0"}"#,
    )
    .expect("write global package.json");

    let project_pkg = project.join(".pi/packages/proj-one");
    std::fs::create_dir_all(&project_pkg).expect("create project pkg dir");
    std::fs::write(
      project_pkg.join("package.json"),
      r#"{"name": "proj-one", "version": "2.0.0"}"#,
    )
    .expect("write project package.json");
    // Create git ceiling for project_dirs
    std::fs::create_dir_all(project.join(".git")).expect("create git ceiling");

    let discovery = Discovery {
      home: Some(home),
      cwd: project,
      trust: Trust::Trusted,
    };
    let scan = discover(&discovery);

    assert_eq!(scan.packages.len(), 2);
    let g = scan.named("global-one").expect("find global");
    assert_eq!(g.source, Source::Global);
    assert_eq!(g.version.as_deref(), Some("1.0.0"));

    let p = scan.named("proj-one").expect("find project");
    assert_eq!(p.source, Source::Project);
    assert_eq!(p.version.as_deref(), Some("2.0.0"));
  }

  #[test]
  fn discover_skips_project_packages_when_untrusted() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let project = temp.path().join("project");

    let global_pkg = home.join(".pi/agent/packages/global-one");
    std::fs::create_dir_all(&global_pkg).expect("create global pkg dir");
    std::fs::write(global_pkg.join("package.json"), r#"{"name": "global-one"}"#)
      .expect("write global package.json");

    let project_pkg = project.join(".pi/packages/proj-one");
    std::fs::create_dir_all(&project_pkg).expect("create project pkg dir");
    std::fs::write(project_pkg.join("package.json"), r#"{"name": "proj-one"}"#)
      .expect("write project package.json");
    std::fs::create_dir_all(project.join(".git")).expect("create git ceiling");

    let discovery = Discovery {
      home: Some(home),
      cwd: project,
      trust: Trust::Untrusted,
    };
    let scan = discover(&discovery);

    assert_eq!(scan.packages.len(), 1);
    assert!(scan.named("global-one").is_some());
    assert!(scan.named("proj-one").is_none());
  }

  #[test]
  fn discover_resolves_duplicate_packages_first_found_wins() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let project = temp.path().join("project");

    let global_pkg = home.join(".pi/agent/packages/dup");
    std::fs::create_dir_all(&global_pkg).expect("create global pkg dir");
    std::fs::write(
      global_pkg.join("package.json"),
      r#"{"name": "dup", "version": "1.0.0"}"#,
    )
    .expect("write global package.json");

    let project_pkg = project.join(".pi/packages/dup");
    std::fs::create_dir_all(&project_pkg).expect("create project pkg dir");
    std::fs::write(
      project_pkg.join("package.json"),
      r#"{"name": "dup", "version": "2.0.0"}"#,
    )
    .expect("write project package.json");
    std::fs::create_dir_all(project.join(".git")).expect("create git ceiling");

    let discovery = Discovery {
      home: Some(home),
      cwd: project,
      trust: Trust::Trusted,
    };
    let scan = discover(&discovery);

    assert_eq!(scan.packages.len(), 1);
    let kept = scan.named("dup").expect("find kept");
    assert_eq!(kept.source, Source::Global);
    assert_eq!(kept.version.as_deref(), Some("1.0.0"));

    assert!(scan.warnings.iter().any(|w| matches!(
      w,
      Warning::Duplicate { name, .. } if name == "dup"
    )));
  }

  #[test]
  fn discover_warns_on_directory_missing_package_json() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let not_a_pkg = home.join(".pi/agent/packages/not-a-pkg");
    std::fs::create_dir_all(&not_a_pkg).expect("create empty dir");

    let discovery = Discovery {
      home: Some(home),
      cwd: temp.path().to_path_buf(),
      trust: Trust::Untrusted,
    };
    let scan = discover(&discovery);

    assert!(scan.packages.is_empty());
    assert!(scan.warnings.iter().any(|w| matches!(
      w,
      Warning::MissingManifest { path } if path == &not_a_pkg
    )));
  }

  #[test]
  fn package_skill_and_prompt_locations_fallback_to_conventions() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let pkg_dir = temp.path().join("my-pkg");
    std::fs::create_dir_all(pkg_dir.join("skills")).expect("skills dir");
    std::fs::create_dir_all(pkg_dir.join("prompts")).expect("prompts dir");
    std::fs::write(pkg_dir.join("package.json"), r#"{"name": "my-pkg"}"#)
      .expect("write package.json");

    let p = Package {
      name: "my-pkg".to_string(),
      version: None,
      description: None,
      path: pkg_dir.clone(),
      manifest_path: pkg_dir.join("package.json"),
      source: Source::Global,
      manifest: Manifest::default(),
      warnings: Vec::new(),
    };

    assert_eq!(p.skill_locations(), vec![pkg_dir.join("skills")]);
    assert_eq!(p.prompt_locations(), vec![pkg_dir.join("prompts")]);
  }

  #[cfg(unix)]
  #[test]
  fn conventional_surface_symlinks_outside_package_are_ignored() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let pkg_dir = temp.path().join("pkg");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&pkg_dir).expect("package dir");
    std::fs::create_dir_all(&outside).expect("outside dir");
    std::os::unix::fs::symlink(&outside, pkg_dir.join("skills")).expect("skills symlink");
    std::fs::write(pkg_dir.join("package.json"), r#"{"name":"pkg"}"#).expect("package manifest");

    let package = Package {
      name: "pkg".to_string(),
      version: None,
      description: None,
      path: pkg_dir.clone(),
      manifest_path: pkg_dir.join("package.json"),
      source: Source::Global,
      manifest: Manifest::default(),
      warnings: Vec::new(),
    };
    assert!(package.skill_locations().is_empty());
  }

  #[test]
  fn manifest_surface_paths_cannot_escape_package_root() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let package_root = home.join(".pi/agent/packages/escape");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&package_root).expect("create package");
    std::fs::create_dir_all(&outside).expect("create outside");
    std::fs::write(outside.join("SKILL.md"), "outside").expect("write outside");
    std::fs::write(
      package_root.join("package.json"),
      r#"{"name":"escape","pi":{"skills":["../../outside","skills/missing"]}}"#,
    )
    .expect("write manifest");

    let scan = discover(&Discovery {
      home: Some(home),
      cwd: temp.path().to_path_buf(),
      trust: Trust::Untrusted,
    });
    let package = scan.named("escape").expect("package discovered");
    assert_eq!(
      package.skill_locations(),
      vec![package_root.join("skills/missing")]
    );
    assert!(scan.warnings.iter().any(|warning| matches!(
      warning,
      Warning::InvalidSurfacePath { surface, path, .. }
        if surface == "skills" && path == "../../outside"
    )));
  }

  #[test]
  fn package_surfaces_reports_compatibility_statuses() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let pkg_dir = temp.path().join("pkg");
    std::fs::create_dir_all(pkg_dir.join("skills")).expect("skills dir");
    std::fs::write(
      pkg_dir.join("package.json"),
      r#"{"name": "pkg", "extensions": ["dist/ext.js"]}"#,
    )
    .expect("write package.json");

    let p = Package {
      name: "pkg".to_string(),
      version: None,
      description: None,
      path: pkg_dir.clone(),
      manifest_path: pkg_dir.join("package.json"),
      source: Source::Global,
      manifest: Manifest::default(),
      warnings: vec![Warning::UnsupportedSurface {
        surface: "extensions".to_string(),
        count: 1,
      }],
    };

    let surfaces = p.surfaces();
    assert_eq!(
      surfaces,
      vec![
        ("skills", SurfaceStatus::Supported),
        ("prompts", SurfaceStatus::NotPresent),
        ("extensions", SurfaceStatus::Partial),
      ]
    );
  }
}
