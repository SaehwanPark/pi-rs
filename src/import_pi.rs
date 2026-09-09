//! `pi-rs import-pi`: bring Pi session files into the pi-rs store.
//!
//! The default is a dry run, and the report *is* the answer to this command, so the report
//! goes to stdout and only the destination line goes to stderr — the split `pi-rs trace`
//! uses for the same reason. Nothing is executed: a tool call inside Pi's file records
//! something that already happened in Pi, and this command never replays it.
//!
//! The path is one session file or a directory of them. A directory imports each `*.jsonl`
//! it holds directly, in name order, each as its own session — which is also the honest
//! statement of what a batch carries: Pi's cross-file lineage (forks and parents recorded
//! as separate files) is not reconstructed, because nothing in the pi-rs session model has
//! the shape that lineage would fold into.
//!
//! Writing refuses when the session id already exists. Re-importing the same file is usually
//! a mistake, and the alternative — a second session with the same content under a different
//! id, or an overwrite of the first — is worse than asking the reader to remove it. Inside a
//! batch that refusal is one file's failure line, not the batch's: one unreadable or already
//! filed session does not cancel the sessions next to it.

use std::{
  collections::BTreeMap,
  fs,
  io::{self, Write},
  path::{Path, PathBuf},
};

use pi_rs_core::RuntimeConfig;
use pi_rs_store::{Store, WritePolicy, pi_import};

use crate::cli::ImportArgs;

pub fn execute(args: ImportArgs) -> Result<(), String> {
  if args.path.is_dir() {
    return execute_batch(args);
  }
  execute_one(args)
}

/// Read and plan, the two steps that can fail before a word of report is printed.
fn plan_file(path: &Path) -> Result<(pi_import::PiSession, pi_import::ImportPlan), String> {
  let source = pi_import::read(path).map_err(|error| error.to_string())?;
  let plan = pi_import::plan(&source).map_err(|error| error.to_string())?;
  Ok((source, plan))
}

/// The report, with the one error a report should swallow: a closed pipe is how
/// `pi-rs import-pi f | head` ends, and it is not an import failure.
fn report(source: &pi_import::PiSession, plan: &pi_import::ImportPlan) -> Result<(), String> {
  let mut out = io::stdout();
  write_report(&mut out, source, plan).map_err(|error| {
    if error.kind() == io::ErrorKind::BrokenPipe {
      String::new()
    } else {
      format!("cannot write the report: {error}")
    }
  })
}

/// One file of a batch, reported on stdout and — with a store in hand — filed.
fn import_one(path: &Path, store: Option<&Store>) -> Result<(), String> {
  let (source, plan) = plan_file(path)?;
  report(&source, &plan)?;
  if let Some(store) = store {
    pi_import::write(store, &plan).map_err(|error| format!("cannot write the session: {error}"))?;
  }
  Ok(())
}

fn execute_one(args: ImportArgs) -> Result<(), String> {
  // Read, plan, and report before any destination is opened, so a file this version of
  // the importer cannot understand never leaves a half-written session behind.
  let (source, plan) = plan_file(&args.path)?;
  report(&source, &plan)?;

  let mut err = io::stderr();
  if !args.write {
    let _ = writeln!(
      err,
      "import: nothing written; pass --write to file this session"
    );
    return Ok(());
  }

  let (root, policy) = destination(&args)?;
  let store = Store::open(&root, policy).map_err(|error| format!("cannot open store: {error}"))?;
  let id = pi_import::write(&store, &plan)
    .map_err(|error| format!("cannot write the session: {error}"))?;
  let _ = writeln!(
    err,
    "import: session {} written under {}",
    id.as_str(),
    root.display()
  );
  Ok(())
}

/// The files of a directory import: every `*.jsonl` directly inside it, in name order.
/// Not recursive — Pi keeps one session per file in one directory, and a recursive walk
/// would silently import whatever a project happened to leave in a nested folder.
fn session_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
  let mut files: Vec<PathBuf> = fs::read_dir(dir)
    .map_err(|error| format!("cannot read {}: {error}", dir.display()))?
    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
    .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl") && path.is_file())
    .collect();
  files.sort();
  if files.is_empty() {
    return Err(format!("no *.jsonl session files under {}", dir.display()));
  }
  Ok(files)
}

fn execute_batch(args: ImportArgs) -> Result<(), String> {
  let files = session_files(&args.path)?;
  // One store for the batch: opening it is the destination's only setup cost, and a
  // per-file failure must not reopen or roll it back.
  let store = if args.write {
    let (root, policy) = destination(&args)?;
    Some((
      Store::open(&root, policy).map_err(|error| format!("cannot open store: {error}"))?,
      root,
    ))
  } else {
    None
  };
  let mut err = io::stderr();
  let mut imported = 0usize;
  let mut failed: Vec<String> = Vec::new();
  for file in &files {
    match import_one(file, store.as_ref().map(|(store, _)| store)) {
      Ok(_) => imported += 1,
      // The empty message is a closed pipe: stop mid-batch, as a closed pipe stops a
      // `sed`; whatever remains on stdout is no longer wanted.
      Err(error) if error.is_empty() => return Err(error),
      Err(error) => {
        let _ = writeln!(err, "import: {}: {error}", file.display());
        failed.push(file.display().to_string());
      }
    }
  }
  match &store {
    None => {
      let _ = writeln!(
        err,
        "import: {} session(s) reported, nothing written; pass --write to file them",
        files.len() - failed.len()
      );
    }
    Some((_, root)) => {
      let _ = writeln!(
        err,
        "import: {} of {} session(s) written under {}",
        imported,
        files.len(),
        root.display()
      );
    }
  }
  if failed.is_empty() {
    return Ok(());
  }
  // A partial batch is not a success a script would want to miss.
  Err(format!(
    "{} of {} file(s) failed",
    failed.len(),
    files.len()
  ))
}

/// Where to write, and under which policy.
///
/// `--config` is not a longer way to spell a directory: it also carries the redaction and
/// retention settings the runtime will use to read the session back, so an imported session
/// is stored under the same rules as one pi-rs recorded itself.
fn destination(args: &ImportArgs) -> Result<(PathBuf, WritePolicy), String> {
  if let Some(root) = &args.store {
    return Ok((root.clone(), WritePolicy::default()));
  }
  let config = args
    .config
    .as_ref()
    .ok_or_else(|| "--write needs --store <dir> or --config <file>".to_string())?;
  let text = fs::read_to_string(config)
    .map_err(|error| format!("cannot read config '{}': {error}", config.display()))?;
  let config = RuntimeConfig::parse(&text).map_err(|error| format!("invalid config: {error}"))?;
  Ok((
    std::path::PathBuf::from(config.state_dir),
    WritePolicy::from_retention(&config.trace, &config.redaction),
  ))
}

/// What the import carries, what it left out, and what Pi recorded that pi-rs cannot hold.
fn write_report(
  out: &mut impl Write,
  source: &pi_import::PiSession,
  plan: &pi_import::ImportPlan,
) -> io::Result<()> {
  let header = &source.header;
  writeln!(out, "Pi session {}", source.path)?;
  writeln!(
    out,
    "  version {}, id {}, model {}",
    header.version,
    if header.id.is_empty() {
      "<none>"
    } else {
      &header.id
    },
    plan.header.model.as_key(),
  )?;
  if let Some(cwd) = &plan.report.cwd {
    writeln!(out, "  cwd {cwd}")?;
  }
  // Two counts because two things are written: the trace's events, and the conversation a
  // resume would read. They differ, and a reader who sees only one cannot tell what a resumed
  // session would actually send.
  writeln!(
    out,
    "  {} events, {} conversation messages, for session {}",
    plan.events.len(),
    plan.report.messages,
    plan.header.session_id.as_str(),
  )?;
  if !source.damaged_lines.is_empty() {
    writeln!(
      out,
      "  {} damaged line(s) skipped",
      source.damaged_lines.len()
    )?;
    for line in source.damaged_lines.iter().take(5) {
      writeln!(out, "    {line}")?;
    }
    let rest = source.damaged_lines.len() - 5.min(source.damaged_lines.len());
    if rest > 0 {
      writeln!(out, "    ... {rest} more")?;
    }
  }

  writeln!(out)?;
  writeln!(out, "imported")?;
  for (kind, count) in &plan.report.imported {
    writeln!(out, "  {count:>4}  {kind}")?;
  }

  writeln!(out)?;
  writeln!(out, "left out")?;
  if plan.report.skipped.is_empty() && plan.report.off_path.is_empty() {
    writeln!(out, "  nothing")?;
  }
  for (kind, skip) in &plan.report.skipped {
    let mut lines = skip.reason.lines();
    writeln!(
      out,
      "  {:>4}  {} — {}",
      skip.count,
      kind,
      lines.next().unwrap_or("")
    )?;
    for line in lines {
      writeln!(out, "         {}", line.trim())?;
    }
  }
  // A separate section would repeat the reason every time; one line says it once.
  let off_path: u32 = plan.report.off_path.values().sum();
  if off_path > 0 {
    writeln!(
      out,
      "  {off_path:>4}  off the active path — {}",
      kinds(&plan.report.off_path)
    )?;
  }

  if !plan.report.content.is_empty() {
    writeln!(out)?;
    writeln!(out, "not carried")?;
    for (what, count) in &plan.report.content {
      writeln!(out, "  {count:>4}  {what}")?;
    }
  }

  if !plan.report.notes.is_empty() {
    writeln!(out)?;
    writeln!(out, "notes")?;
    for note in &plan.report.notes {
      writeln!(out, "  {note}")?;
    }
  }
  Ok(())
}

/// `"3 message, 1 label"`, for the one line that summarises a counted map.
fn kinds(counts: &BTreeMap<String, u32>) -> String {
  counts
    .iter()
    .map(|(kind, count)| format!("{count} {kind}"))
    .collect::<Vec<_>>()
    .join(", ")
}
