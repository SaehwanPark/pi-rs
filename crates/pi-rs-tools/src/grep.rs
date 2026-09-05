//! `grep` — find text in the workspace.
//!
//! Search is the tool that most often decides whether the next turn has the
//! right context, so its failure modes matter more than its speed. Three
//! decisions follow from that:
//!
//! - **No regex dependency.** Patterns are literals or simple globs. A regex
//!   engine is a real dependency and a real injection surface for model-supplied
//!   patterns; the literal case covers most agent searches.
//! - **Directory ceilings.** Entry and depth caps stop a search over a large
//!   tree from producing output that dwarfs the context it was meant to feed.
//! - **Paths first, content second.** A capped result states what was skipped,
//!   so the model narrows the query instead of concluding the text is absent.

use std::{
  fs::{self, File},
  io::{BufRead, BufReader},
  path::Path,
};

use pi_rs_core::{Tool, ToolError, ToolMetadata, ToolOutcome, ToolProgress, ToolRequest};
use serde_json::json;

use crate::{Deadline, Runtime, arg_str};

/// Directories never descended into. Build and VCS output is where searches go
/// to die, and it is almost never what a model means to search.
const SKIP_DIRS: &[&str] = &[
  ".git",
  ".hg",
  ".svn",
  "node_modules",
  "target",
  "dist",
  "build",
  ".venv",
  "venv",
  "__pycache__",
  ".pixi",
  ".cache",
];
const MAX_DEPTH: usize = 24;
const SCAN_LIMIT: usize = 200_000;
const GREP_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// The `grep` tool.
pub struct GrepTool {
  runtime: Runtime,
}

impl GrepTool {
  pub(crate) fn new(runtime: Runtime) -> Self {
    Self { runtime }
  }
}

impl Tool for GrepTool {
  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::read_only(
      "grep",
      "Search file contents for a literal string or a glob pattern. Returns path:line matches.",
    )
  }

  fn arguments_schema(&self) -> serde_json::Value {
    json!({
      "type": "object",
      "properties": {
        "pattern": { "type": "string", "description": "Literal text, or a glob when glob=true." },
        "path": { "type": "string", "description": "Directory or file to search. Defaults to the workspace root." },
        "glob": { "type": "boolean", "description": "Treat pattern as a filename glob. Defaults to false." },
        "ignore_case": { "type": "boolean", "description": "Case-insensitive match. Defaults to false." },
        "max_matches": { "type": "integer", "description": "Stop after N matches. Defaults to 200." }
      },
      "required": ["pattern"]
    })
  }

  fn preflight(&self, request: &ToolRequest) -> Result<(), ToolError> {
    let requested_root = request
      .arguments
      .get("path")
      .and_then(|value| value.as_str())
      .unwrap_or(".");
    self
      .runtime
      .workspace
      .search_path(requested_root)
      .map(|_| ())
      .map_err(|error| ToolError::new(error.to_string()))
  }

  fn execute(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
  ) -> Result<ToolOutcome, ToolError> {
    let runtime = self.runtime.clone();
    let pattern = arg_str(request, "pattern")?;
    let as_glob = crate::arg_bool(request, "glob", false);
    let ignore_case = crate::arg_bool(request, "ignore_case", false);
    let max_matches: usize = crate::arg_u64(request, "max_matches")
      .unwrap_or(200)
      .clamp(1, 5_000)
      .try_into()
      .unwrap_or(200);
    let deadline = Deadline::new(GREP_BUDGET);

    // The root is checked even when it defaults to the workspace root, so a
    // workspace configured outside the policy boundary is refused instead of
    // silently walked.
    let requested_root = request
      .arguments
      .get("path")
      .and_then(|v| v.as_str())
      .map(str::to_string)
      .unwrap_or_else(|| ".".to_string());
    let root = match runtime.workspace.search_path(&requested_root) {
      Ok(root) => root,
      Err(error) => return Ok(ToolOutcome::failed(error.to_string())),
    };
    if !root.exists() {
      return Ok(ToolOutcome::failed(format!(
        "grep: '{}' does not exist",
        root.display()
      )));
    }

    let matcher = Matcher::new(pattern, as_glob, ignore_case);
    let mut state = Scan::new(max_matches);
    if root.is_file() {
      scan_file(&root, &root, &matcher, &mut state, &deadline);
    } else {
      scan_dir(&root, &root, &matcher, &mut state, 0, &deadline);
    }

    let mut out = String::new();
    if state.matches.is_empty() {
      out.push_str("no matches\n");
    } else {
      for line in &state.matches {
        out.push_str(line);
        out.push('\n');
      }
    }
    let mut notes = Vec::new();
    if state.matches.is_empty() && (state.entries_hit_cap || state.timed_out || state.skipped > 0) {
      // "no matches" must not be read as "the text is absent" when the walk was
      // cut short or a file was never searched.
      notes.push("the search was incomplete, so absence is not established".to_string());
    }
    if state.hit_limit {
      notes.push(format!(
        "stopped after {max_matches} matches; narrow the pattern or raise max_matches"
      ));
    }
    if state.entries_hit_cap {
      notes.push(format!(
        "stopped after scanning {SCAN_LIMIT} entries; the result is partial, narrow the path"
      ));
    }
    if state.skipped > 0 {
      notes.push(format!(
        "skipped {} binary or unreadable files",
        state.skipped
      ));
    }
    if state.timed_out {
      notes.push(format!(
        "stopped after {}s; narrow the path",
        GREP_BUDGET.as_secs()
      ));
    }
    if !notes.is_empty() {
      out.push_str(&format!("\n[{}]\n", notes.join("; ")));
    }

    crate::BoundedProgress::new(progress, 64 * 1024, deadline).send(&out);
    Ok(runtime.finish(out))
  }
}

struct Scan {
  matches: Vec<String>,
  limit: usize,
  hit_limit: bool,
  entries: usize,
  entries_hit_cap: bool,
  skipped: usize,
  timed_out: bool,
  stopped: bool,
}

impl Scan {
  fn new(limit: usize) -> Self {
    Self {
      matches: Vec::new(),
      limit,
      hit_limit: false,
      entries: 0,
      entries_hit_cap: false,
      skipped: 0,
      timed_out: false,
      stopped: false,
    }
  }

  /// `false` when scanning must stop.
  fn allow(&mut self, deadline: &Deadline) -> bool {
    if self.stopped {
      return false;
    }
    if self.matches.len() >= self.limit {
      self.hit_limit = true;
      self.stopped = true;
      return false;
    }
    if deadline.expired() {
      self.timed_out = true;
      self.stopped = true;
      return false;
    }
    true
  }

  /// Account for one visited directory entry.
  fn visit(&mut self) -> bool {
    if self.stopped {
      return false;
    }
    self.entries += 1;
    if self.entries > SCAN_LIMIT {
      self.entries_hit_cap = true;
      self.stopped = true;
      return false;
    }
    true
  }
}

fn scan_dir(
  dir: &Path,
  base: &Path,
  matcher: &Matcher,
  state: &mut Scan,
  depth: usize,
  deadline: &Deadline,
) {
  if depth > MAX_DEPTH {
    return;
  }
  let Ok(entries) = fs::read_dir(dir) else {
    return;
  };
  let mut names: Vec<_> = entries.filter_map(|e| e.ok()).collect();
  // Deterministic order, so two runs of the same query produce the same trace.
  names.sort_by_key(|e| e.file_name());
  for entry in names {
    if !state.visit() || !state.allow(deadline) {
      return;
    }
    let path = entry.path();
    let file_type = entry.file_type().ok();
    if file_type.is_some_and(|t| t.is_dir()) {
      let name = entry.file_name().to_string_lossy().to_string();
      if SKIP_DIRS.contains(&name.as_str()) {
        continue;
      }
      scan_dir(&path, base, matcher, state, depth + 1, deadline);
    } else if file_type.is_some_and(|t| t.is_file()) && matcher.matches_path(&path) {
      scan_file(&path, base, matcher, state, deadline);
    }
  }
  // A deep tree can exhaust the budget without visiting another entry.
  state.allow(deadline);
}

fn scan_file(path: &Path, base: &Path, matcher: &Matcher, state: &mut Scan, deadline: &Deadline) {
  if !state.allow(deadline) {
    return;
  }
  let display = relative(path, base);
  let Ok(file) = File::open(path) else {
    state.skipped += 1;
    return;
  };
  let mut reader = BufReader::new(file);
  let mut line = String::new();
  let mut number = 0usize;
  loop {
    if state.stopped {
      return;
    }
    line.clear();
    let Ok(read) = reader.read_line(&mut line) else {
      state.skipped += 1;
      return;
    };
    if read == 0 {
      return;
    }
    number += 1;
    if line.contains('\0') {
      state.skipped += 1;
      return;
    }
    if matcher.matches(&line) {
      let trimmed = line.trim_end_matches(['\n', '\r']);
      let shown = clip(trimmed, 300);
      state.matches.push(format!("{display}:{number}: {shown}"));
      if state.matches.len() >= state.limit {
        state.hit_limit = true;
        state.stopped = true;
        return;
      }
    }
    if deadline.expired() {
      state.timed_out = true;
      state.stopped = true;
      return;
    }
  }
}

fn relative(path: &Path, base: &Path) -> String {
  let root = base.parent().unwrap_or(base);
  path
    .strip_prefix(root)
    .unwrap_or(path)
    .to_string_lossy()
    .to_string()
}

fn clip(text: &str, max: usize) -> String {
  if text.len() <= max {
    return text.to_string();
  }
  let mut cut = max;
  while cut > 0 && !text.is_char_boundary(cut) {
    cut -= 1;
  }
  format!("{}…", &text[..cut])
}

/// Literal or glob matching over a model-supplied pattern.
struct Matcher {
  needle: String,
  glob: Option<String>,
  fold: bool,
}

impl Matcher {
  fn new(pattern: &str, as_glob: bool, ignore_case: bool) -> Self {
    if as_glob {
      return Self {
        needle: String::new(),
        glob: Some(if ignore_case {
          pattern.to_lowercase()
        } else {
          pattern.to_string()
        }),
        fold: ignore_case,
      };
    }
    Self {
      needle: if ignore_case {
        pattern.to_lowercase()
      } else {
        pattern.to_string()
      },
      glob: None,
      fold: ignore_case,
    }
  }

  fn matches(&self, text: &str) -> bool {
    match &self.glob {
      Some(_) => true, // glob mode selects *files*; every line of a selected file matches
      None => {
        if self.needle.is_empty() {
          return false;
        }
        // Fast path first: an exact substring needs no allocation at all. Only
        // fall back to a case-insensitive scan when the exact test fails, because
        // lowercasing every scanned line is the dominant cost of a large search.
        if text.contains(&self.needle) {
          return true;
        }
        self.fold && text.to_lowercase().contains(&self.needle)
      }
    }
  }

  /// Whether this file is in scope.
  ///
  /// In glob mode the pattern *is* the file selection, so a non-matching file is
  /// skipped entirely rather than read and then filtered line by line.
  fn matches_path(&self, path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string());
    let Some(name) = name else { return false };
    match &self.glob {
      Some(pattern) => {
        if self.fold {
          glob_match(pattern, &name.to_lowercase())
        } else {
          glob_match(pattern, &name)
        }
      }
      None => true,
    }
  }
}

/// Glob matching for `*`, `?`, `[chars]`, and `{a,b}`, anchored on the file name.
///
/// Iterative with a bounded backtrack stack rather than recursive: the pattern
/// comes from the model, and a recursive matcher would make `'*.rs'`-shaped
/// inputs a stack-overflow vector. Worst case is quadratic in pattern length,
/// which is irrelevant next to the filesystem walk.
fn glob_match(pattern: &str, text: &str) -> bool {
  let t: Vec<char> = text.chars().collect();
  match_atoms(&parse_glob(pattern), &t, 0, 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Atom {
  Literal(char),
  Any,
  /// `*` — any run of characters.
  Star,
  Class {
    chars: Vec<char>,
    negated: bool,
  },
  /// `{a,b}` — one of several literal alternatives.
  Choice(Vec<Vec<char>>),
}

fn parse_glob(pattern: &str) -> Vec<Atom> {
  let p: Vec<char> = pattern.chars().collect();
  let mut atoms = Vec::new();
  let mut i = 0usize;
  while i < p.len() {
    match p[i] {
      '?' => {
        atoms.push(Atom::Any);
        i += 1;
      }
      '*' => {
        // Collapse runs: '**' is one star, matching any run.
        while i < p.len() && p[i] == '*' {
          i += 1;
        }
        atoms.push(Atom::Star);
      }
      '[' => {
        let Some(end) = p[i + 1..].iter().position(|c| *c == ']') else {
          atoms.push(Atom::Literal('['));
          i += 1;
          continue;
        };
        let body: Vec<char> = p[i + 1..i + 1 + end].to_vec();
        let negated = body.first() == Some(&'!');
        let chars = if negated { body[1..].to_vec() } else { body };
        atoms.push(Atom::Class { chars, negated });
        i += end + 2;
      }
      '{' => {
        let Some(end) = p[i + 1..].iter().position(|c| *c == '}') else {
          atoms.push(Atom::Literal('{'));
          i += 1;
          continue;
        };
        let options: Vec<Vec<char>> = p[i + 1..i + 1 + end]
          .split(|c| *c == ',')
          .map(|s| s.to_vec())
          .collect();
        atoms.push(Atom::Choice(options));
        i += end + 2;
      }
      c => {
        atoms.push(Atom::Literal(c));
        i += 1;
      }
    }
  }
  atoms
}

fn match_atoms(atoms: &[Atom], text: &[char], mut ai: usize, mut ti: usize) -> bool {
  loop {
    match atoms.get(ai) {
      None => return ti == text.len(),
      Some(Atom::Literal(c)) if ti < text.len() && text[ti] == *c => {
        ai += 1;
        ti += 1;
      }
      Some(Atom::Any) if ti < text.len() => {
        ai += 1;
        ti += 1;
      }
      Some(Atom::Class { chars, negated }) if ti < text.len() => {
        let hit = chars.contains(&text[ti]);
        if hit != *negated {
          ai += 1;
          ti += 1;
        } else {
          return false;
        }
      }
      Some(Atom::Choice(options)) => {
        let matched = options.iter().any(|option| {
          ti + option.len() <= text.len() && text[ti..ti + option.len()] == option[..]
        });
        if matched {
          let len = options
            .iter()
            .find(|o| ti + o.len() <= text.len() && text[ti..ti + o.len()] == o[..])
            .map(|o| o.len())
            .unwrap_or(0);
          ai += 1;
          ti += len;
        } else {
          return false;
        }
      }
      Some(Atom::Star) => {
        // Try the shortest run first, then extend. Each attempt resumes after the
        // star, so this terminates: every retry advances `ti` by one.
        for skip in 0..=text.len() - ti {
          if match_atoms(atoms, text, ai + 1, ti + skip) {
            return true;
          }
        }
        return false;
      }
      // A literal/any/class that cannot consume the next character.
      Some(_) => return false,
    }
  }
}

#[cfg(test)]
mod tests {
  use pi_rs_core::ToolCallId;

  use super::*;
  use crate::testutil::{Recorder, runtime};

  fn request(arguments: serde_json::Value) -> ToolRequest {
    ToolRequest {
      call_id: ToolCallId::new(),
      name: "grep".into(),
      arguments,
    }
  }

  fn grep(dir: &tempfile::TempDir, arguments: serde_json::Value) -> ToolOutcome {
    let tool = GrepTool::new(runtime(dir));
    let mut recorder = Recorder::default();
    tool.execute(&request(arguments), &mut recorder).unwrap()
  }

  fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::create_dir_all(root.join("target/debug")).unwrap();
    fs::write(
      root.join("src/main.rs"),
      "fn main() {\n    let x = needle_value;\n}\n",
    )
    .unwrap();
    fs::write(
      root.join("src/lib.rs"),
      "// needle in a lib\npub fn f() {}\n",
    )
    .unwrap();
    fs::write(root.join("README.md"), "# Project\nneedle in the readme\n").unwrap();
    fs::write(
      root.join("node_modules/pkg/index.js"),
      "needle in node_modules\n",
    )
    .unwrap();
    fs::write(root.join("target/debug/out.txt"), "needle in target\n").unwrap();
    fs::write(root.join("blob.bin"), "needle\u{0}\n").unwrap();
    dir
  }

  #[test]
  fn finds_matches_with_paths_and_line_numbers() {
    let dir = fixture();
    let outcome = grep(&dir, json!({"pattern": "needle_value"}));
    assert!(outcome.text.contains("src/main.rs:2:"), "{}", outcome.text);
    assert!(!outcome.is_error);
  }

  #[test]
  fn skips_build_and_dependency_directories() {
    let dir = fixture();
    let outcome = grep(&dir, json!({"pattern": "needle"}));
    assert!(outcome.text.contains("src/lib.rs"), "workspace match");
    assert!(outcome.text.contains("README.md"), "workspace match");
    assert!(
      !outcome.text.contains("node_modules"),
      "dependency dirs skipped"
    );
    assert!(!outcome.text.contains("target/"), "build dirs skipped");
  }

  #[test]
  fn reports_binary_files_as_skipped_instead_of_emitting_their_bytes() {
    let dir = fixture();
    let outcome = grep(&dir, json!({"pattern": "needle"}));
    assert!(
      outcome.text.contains("binary or unreadable"),
      "{}",
      outcome.text
    );
    assert!(!outcome.text.contains("needle\u{0}"), "no NUL in output");
  }

  #[test]
  fn a_glob_selects_files() {
    let dir = fixture();
    let outcome = grep(&dir, json!({"pattern": "*.md", "glob": true}));
    // In glob mode the pattern names files; every line of matching files is shown.
    assert!(outcome.text.contains("README.md:2:"), "{}", outcome.text);
    assert!(!outcome.text.contains("src/main.rs"), "only matching files");
  }

  #[test]
  fn case_insensitive_search_finds_both_cases() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "Mixed Case Here\n").unwrap();
    let outcome = grep(&dir, json!({"pattern": "mixed case", "ignore_case": true}));
    assert!(outcome.text.contains("a.txt:1:"), "{}", outcome.text);
    let strict = grep(&dir, json!({"pattern": "mixed case"}));
    assert!(strict.text.starts_with("no matches"), "{}", strict.text);
  }

  #[test]
  fn the_match_limit_is_reported_as_partial() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=1000).map(|i| format!("match {i}\n")).collect();
    fs::write(dir.path().join("many.txt"), &body).unwrap();
    let outcome = grep(&dir, json!({"pattern": "match", "max_matches": 5}));
    let match_lines: Vec<&str> = outcome
      .text
      .lines()
      .filter(|l| l.contains("many.txt:"))
      .collect();
    assert_eq!(match_lines.len(), 5, "{}", outcome.text);
    assert!(
      outcome.text.contains("stopped after 5 matches"),
      "{}",
      outcome.text
    );
  }

  #[test]
  fn an_incomplete_search_never_claims_absence() {
    // The dangerous output is a confident "no matches" after the walk was cut
    // short: the model would stop looking.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("deep.bin"), "needle\u{0}\n").unwrap();
    let outcome = grep(&dir, json!({"pattern": "needle"}));
    assert!(
      outcome.text.contains("search was incomplete"),
      "{}",
      outcome.text
    );
  }

  #[test]
  fn no_matches_is_a_clear_answer_not_an_error() {
    let dir = fixture();
    let outcome = grep(&dir, json!({"pattern": "nothing_like_this"}));
    assert!(!outcome.is_error, "absence is not a failure");
    assert!(outcome.text.starts_with("no matches"), "{}", outcome.text);
  }

  #[test]
  fn searching_a_missing_path_is_a_tool_failure() {
    let dir = fixture();
    let outcome = grep(&dir, json!({"pattern": "x", "path": "nope"}));
    assert!(outcome.is_error);
    assert!(outcome.text.contains("does not exist"), "{}", outcome.text);
  }

  #[test]
  fn search_is_confined_to_the_workspace() {
    let dir = fixture();
    // A traversal that lands above the root must be refused, and refused without
    // walking anything. The tool reports the refusal as a result rather than an
    // infrastructure error, because the model needs to see it and correct itself.
    let outcome = grep(&dir, json!({"pattern": "x", "path": "../../../etc"}));
    assert!(outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("outside"), "{}", outcome.text);
    assert!(
      !outcome.text.contains("bin"),
      "no filesystem listing leaked"
    );

    // A relative walk inside the root is still allowed.
    let inside = grep(&dir, json!({"pattern": "needle", "path": "src"}));
    assert!(inside.text.contains("src/lib.rs"), "{}", inside.text);
  }

  #[test]
  fn glob_matching_handles_star_question_and_sets() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("mod_a.rs"), "hit\n").unwrap();
    fs::write(dir.path().join("mod_b.rs"), "hit\n").unwrap();
    fs::write(dir.path().join("other.rs"), "hit\n").unwrap();
    let outcome = grep(&dir, json!({"pattern": "mod_?.rs", "glob": true}));
    assert!(outcome.text.contains("mod_a.rs"), "{}", outcome.text);
    assert!(outcome.text.contains("mod_b.rs"), "{}", outcome.text);
    assert!(!outcome.text.contains("other.rs"), "{}", outcome.text);
  }

  #[test]
  fn a_pathological_glob_terminates_instead_of_blowing_the_stack() {
    // Model-supplied patterns must not be able to recurse without bound or take
    // quadratic-with-no-exit time. The star-heavy inputs below are the classic
    // exponential shape for naive recursive matchers.
    let many_stars = "*".repeat(200) + "a";
    assert!(!glob_match(&many_stars, &"b".repeat(200)));
    assert!(!glob_match("*a*b*c*d*e*f*g*h*i*j*k*", &"x".repeat(200)));
    assert!(glob_match("*a*b*c*", "axbxc"));
    assert!(!glob_match("*.rs", "main.txt"));
    assert!(glob_match("*", "anything"));
    assert!(glob_match("*.{rs,md}", "README.md"));
    assert!(!glob_match("*.{rs,md}", "main.py"));
    assert!(glob_match("[abc]*", "banana"));
    assert!(!glob_match("[!abc]*", "banana"));
  }

  #[test]
  fn results_are_deterministically_ordered() {
    let dir = fixture();
    let a = grep(&dir, json!({"pattern": "needle"})).text;
    let b = grep(&dir, json!({"pattern": "needle"})).text;
    assert_eq!(a, b, "same query, same trace");
  }
}
