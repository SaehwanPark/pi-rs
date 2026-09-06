//! `pi-rs trace`: read a session's canonical trace back out of the store.
//!
//! The command is read-only. It opens no provider, runs no tool, and creates no
//! directory, which is why it uses [`Store::new`] rather than [`Store::open`]: asking
//! what happened must not be able to change what is on disk.
//!
//! The transcript goes to stdout because it *is* the answer to this command — the
//! opposite of `pi-rs run`, where the answer is the assistant's text and the transcript
//! is commentary. Metadata about the read (which session, how many entries, whether any
//! line was damaged) goes to stderr, so `pi-rs trace > session.txt` writes a clean
//! transcript and nothing else.

use std::{
  fs,
  io::{self, Write},
};

use pi_rs_core::{RuntimeConfig, SessionId, TraceEntry};
use pi_rs_store::{ReadReport, Store, TraceJournal, WritePolicy};
use pi_rs_tui::{
  DiagnosticFilter, NarrowDecoration, Palette, RenderLine, RenderedEntry, Role, TranscriptOptions,
  render_trace, term::Stream,
};

use crate::cli::TraceArgs;

pub fn execute(args: TraceArgs) -> Result<(), String> {
  // Every input is resolved before anything is written, so a bad session id cannot
  // leave half a transcript on stdout.
  let config_text = fs::read_to_string(&args.config)
    .map_err(|error| format!("cannot read config '{}': {error}", args.config.display()))?;
  let config =
    RuntimeConfig::parse(&config_text).map_err(|error| format!("invalid config: {error}"))?;
  let store = Store::new(
    &config.state_dir,
    WritePolicy::from_retention(&config.trace, &config.redaction),
  );
  let session = resolve_session(&store, args.session.as_deref())?;
  let path = store.layout().trace_path(&session);
  if !path.exists() {
    return Err(format!(
      "session {session} has no trace at {}",
      path.display()
    ));
  }
  let report = TraceJournal::read(&path)
    .map_err(|error| format!("cannot read trace '{}': {error}", path.display()))?;

  let options = trace_options(&args);
  let shown = render_trace(&report.items, &options, &args.selection);
  write_transcript(&shown, &options, args.sequence)?;
  report_read(&session, &shown, &report, &options)
}

/// Which session the reader means, resolved against what the store actually holds.
///
/// A prefix is accepted because session ids are long and a reader copying one from a
/// footer rarely keeps the whole thing. An ambiguous prefix is an error, never a guess.
fn resolve_session(store: &Store, wanted: Option<&str>) -> Result<SessionId, String> {
  let ids = store
    .layout()
    .list_session_ids()
    .map_err(|error| format!("cannot list sessions: {error}"))?;
  let Some(wanted) = wanted else {
    return ids
      .first()
      .cloned()
      .ok_or_else(|| format!("no sessions recorded under {}", store.root().display()))
      .map(|latest| {
        // Session ids are time-ordered, so the largest is the newest; the reader
        // should know which session they are looking at before the first line.
        eprintln!("trace: newest session {}", latest.as_str());
        latest
      });
  };
  let matches: Vec<&SessionId> = ids
    .iter()
    .filter(|id| id.as_str().starts_with(wanted))
    .collect();
  match matches.len() {
    0 => Err(format!(
      "no session id starts with '{wanted}'; {} sessions recorded, newest {}",
      ids.len(),
      ids.first().map(|id| id.as_str()).unwrap_or("none")
    )),
    1 => Ok(matches[0].clone()),
    count => {
      let mut listed = matches
        .iter()
        .take(3)
        .map(|id| id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
      if count > 3 {
        listed.push_str(", …");
      }
      Err(format!(
        "{count} session ids start with '{wanted}': {listed}; give a longer prefix"
      ))
    }
  }
}

/// Presentation for the trace view.
fn trace_options(args: &TraceArgs) -> TranscriptOptions {
  let stdout = Stream::Stdout;
  TranscriptOptions {
    palette: if args.surface.color.resolve(stdout.is_terminal()) {
      Palette::colored()
    } else {
      Palette::monochrome()
    },
    // A piped trace is not wrapped at all: the reader asked for the record, and a
    // hard newline in the middle of a folded answer would rewrite it.
    width: args
      .surface
      .width
      .unwrap_or_else(|| stdout.width().unwrap_or(0)),
    show_reasoning: args.surface.reasoning,
    diagnostics: args.surface.diagnostics,
    ..TranscriptOptions::default()
  }
}

fn write_transcript(
  shown: &[RenderedEntry],
  options: &TranscriptOptions,
  sequence: bool,
) -> Result<(), String> {
  let mut out = io::stdout().lock();
  for entry in shown {
    let line = if sequence {
      let mut prefixed = RenderLine::text(format!("[{}] ", entry.seq.0), Role::Meta);
      prefixed.push_line(&entry.line);
      prefixed
    } else {
      entry.line.clone()
    };
    for rendered in line.wrapped(options.width, NarrowDecoration::Keep) {
      write_line(&mut out, &rendered.render(options.palette))?;
    }
  }
  out
    .flush()
    .map_err(|error| format!("cannot write transcript: {error}"))
}

fn write_line(out: &mut impl Write, line: &str) -> Result<(), String> {
  writeln!(out, "{line}").map_err(|error| format!("cannot write transcript: {error}"))
}

/// Tell the reader what was read, on the stream that holds everything but the answer.
fn report_read(
  session: &SessionId,
  shown: &[RenderedEntry],
  report: &ReadReport<TraceEntry>,
  options: &TranscriptOptions,
) -> Result<(), String> {
  let stderr = Stream::Stderr;
  let palette = options.palette;
  let width = stderr.width().unwrap_or(0);
  let mut line = RenderLine::text("trace: ", Role::Meta);
  line.push(session.as_str(), Role::Path);
  line.push(
    format!(" · {} entries read", report.items.len()).as_str(),
    Role::Meta,
  );
  line.push(format!(" · {} shown", shown.len()).as_str(), Role::Meta);
  if options.diagnostics == DiagnosticFilter::None {
    line.push(" · transcript suppressed", Role::Muted);
  }
  for rendered in line.wrapped(width, NarrowDecoration::Keep) {
    eprintln!("{}", rendered.render(palette));
  }
  if report.malformed > 0 {
    // Damaged history is never folded into the summary line: a reader who does not
    // know a trace is incomplete will reason as if it were whole.
    let mut warning = RenderLine::text("[warn] ", Role::Warning);
    warning.push(
      format!(
        "{} line(s) in this trace could not be read{}",
        report.malformed,
        // The reader counts lines from one, so the number is shown as the store
        // recorded it rather than shifted again on the way out.
        report
          .first_malformed_line
          .map(|line| format!(" (first at line {line})"))
          .unwrap_or_default()
      )
      .as_str(),
      Role::Warning,
    );
    for rendered in warning.wrapped(width, NarrowDecoration::Keep) {
      eprintln!("{}", rendered.render(palette));
    }
  }
  if shown.is_empty() && options.diagnostics != DiagnosticFilter::None {
    // An empty result from a filter is indistinguishable from an empty session, and
    // a reader who cannot tell those apart draws the wrong conclusion.
    eprintln!(
      "trace: nothing matched the selection ({} entries read); note that --epoch \
             excludes events with no epoch attribution",
      report.items.len()
    );
  }
  Ok(())
}
