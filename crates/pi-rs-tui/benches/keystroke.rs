//! Keystroke latency benchmark.
//!
//! Why this exists: the editor is the part of `pi-rs` that has to keep up with a finger, and
//! render throughput says nothing about it. The roadmap asks to measure keystroke latency and to
//! put initial budgets somewhere a regression trips; this is both.
//!
//! A keystroke is one [`Editor::apply`] plus the redraw it forces, so both halves are measured:
//! the apply-only cases time the buffer edit, and the cases ending in `_redraw` time the edit
//! together with the [`Editor::display`] the caller owes the terminal afterwards. The mixed
//! session case is the one closest to a user actually typing.
//!
//! The harness is hand-rolled and takes no dependency, the same shape as `benches/render.rs`.
//! Every pass edits a fresh copy of a fixed template, so a median is a median over one shape of
//! buffer rather than over a buffer that drifts, and reports microseconds per keystroke.
//!
//! Run: `cargo bench -p pi-rs-tui --bench keystroke`, or `bash bench/keystroke.sh`.

use std::env;
use std::fs;
use std::process::ExitCode;
use std::time::Instant;

use pi_rs_tui::{Editor, Intent};

/// Microseconds per keystroke above which a case is a regression.
///
/// These are the roadmap's initial keystroke budgets, and the enforcement point: the numbers live
/// here, in the file that fails when they are exceeded, not in a document that stays true after
/// the code gets slow.
///
/// The rule behind them, so any one of them can be argued with: a budget is the measured median
/// times ten, floored at 2 us. Terminal key repeat is about 30 ms, so a keystroke that costs a
/// millisecond is already late, which makes [`MAX_BUDGET_US`] a ceiling no case may reach; 10x is
/// the margin that survives a loaded CI runner without turning the bench into a coin flip. The
/// floor exists because a keystroke measured in the low hundreds of nanoseconds would otherwise
/// get a budget so tight that allocation noise fails a good commit.
///
/// Baseline recorded 2026-09-07 on an AMD Ryzen AI MAX+ 395 (32 cores), rustc 1.98.1,
/// `cargo bench -p pi-rs-tui --bench keystroke` at the default 200 iterations, medians stable to
/// 0.2 us across three runs:
///
/// ```text
/// insert_end            0.20 us/keystroke
/// tab_completion        0.30 us/keystroke (measured 2026-09-08, 64 candidates)
/// insert_middle         0.10 us/keystroke
/// backspace             0.11 us/keystroke
/// delete_word           0.34 us/keystroke
/// word_left             1.10 us/keystroke
/// word_right            0.92 us/keystroke
/// history_recall        0.71 us/keystroke
/// apply_long_line       0.65 us/keystroke
/// keystroke_redraw80   14.26 us/keystroke
/// redraw40_multiline   23.40 us/keystroke
/// mixed_session         0.14 us/keystroke
/// ```
///
/// Each budget is ten times the median above, rounded up to something worth reading, and floored
/// at 2 us where the median is inside the noise. The two redraw cases dominate, which is the
/// point: an edit is a string operation on a line, and a redraw lays the whole buffer out again.
/// The redraw-only case is priced per redraw, because a redraw is exactly what one keystroke owes
/// the terminal. Re-measure rather than raising a budget because a number moved.
const BUDGETS: &[(&str, f64)] = &[
  ("insert_end", 2.0),
  ("insert_middle", 2.0),
  ("backspace", 2.0),
  ("delete_word", 4.0),
  ("tab_completion", 3.0),
  ("word_left", 12.0),
  ("word_right", 10.0),
  ("history_recall", 8.0),
  ("apply_long_line", 7.0),
  ("keystroke_redraw80", 150.0),
  ("redraw40_multiline", 240.0),
  ("mixed_session", 2.0),
];

/// The ceiling a per-keystroke budget may not exceed: at a millisecond a keystroke is already
/// late even though key repeat only asks for one every 30 ms, so a budget above this would pass a
/// slow editor.
const MAX_BUDGET_US: f64 = 1_000.0;

/// Budget floor, per the rule documented on [`BUDGETS`].
const MIN_BUDGET_US: f64 = 2.0;

const DEFAULT_ITERATIONS: usize = 200;

/// Keystrokes in one measured pass of the single-intent cases. Long enough that the clock's own
/// resolution is negligible, short enough that the buffer never changes shape.
const BURST: usize = 32;

/// Recall steps in one pass of the history case: eight entries back, eight forward again.
const RECALL: usize = 8;

/// History entries the recall case starts from.
const HISTORY: usize = 50;

/// Redraws in one pass of the case that only redraws.
const REDRAWS: usize = 16;

/// Keystrokes in the scripted mixed session.
const SESSION: usize = 1_000;

/// Characters in the held-down-key buffer: a line long enough that an edit pays for the whole of
/// it, with no short-line shortcut to hide behind.
const LONG_LINE: usize = 2_000;

/// Lines in the multiline buffer the narrow redraw case draws.
const MULTILINE_LINES: usize = 50;

/// The prompt prefix every redraw pays for.
const PREFIX: &str = "> ";

/// One measured case: what it is called, the median it produced, and how much work the last pass
/// actually did, so a case that quietly stopped doing work cannot look like a speed-up.
struct Case {
  name: &'static str,
  median_us: f64,
  work: usize,
  unit: &'static str,
}

fn main() -> ExitCode {
  let args: Vec<String> = env::args().skip(1).collect();
  if args.iter().any(|a| a == "--help" || a == "-h") {
    println!("Usage: keystroke [--iterations <N>] [--json <path>]");
    return ExitCode::SUCCESS;
  }
  let iterations = arg_value(&args, "--iterations")
    .and_then(|v| v.parse::<usize>().ok())
    .filter(|n| *n > 0)
    .unwrap_or(DEFAULT_ITERATIONS);
  let json_out = arg_value(&args, "--json").map(String::from);

  // The rule is part of the measurement, so a budget that drifted past the ceiling is a failure
  // rather than a number that quietly stops meaning anything.
  for (name, budget) in BUDGETS {
    if *budget > MAX_BUDGET_US {
      eprintln!("budget for {name} is {budget:.0} us, above the {MAX_BUDGET_US:.0} us ceiling");
      return ExitCode::FAILURE;
    }
    if *budget < MIN_BUDGET_US {
      eprintln!("budget for {name} is {budget:.0} us, below the {MIN_BUDGET_US:.0} us floor");
      return ExitCode::FAILURE;
    }
  }

  let paragraph = paragraph_editor(false);
  let paragraph_middle = paragraph_editor(true);
  let long_line = long_line_editor(false);
  let long_line_head = long_line_editor(true);
  let recall = history_editor();
  let narrow = multiline_editor();
  let completing = completing_editor();

  let mut measured: Vec<Case> = Vec::new();

  // Typing a character at the end of a paragraph: the keystroke a user types most often.
  measured.push(run_case(
    "insert_end",
    "caret",
    iterations,
    BURST,
    &paragraph,
    &mut |editor| {
      insert_burst(editor, BURST);
      editor.cursor().column
    },
  ));

  // The same character in the middle of a paragraph, where every character after the caret has
  // to move over.
  measured.push(run_case(
    "insert_middle",
    "caret",
    iterations,
    BURST,
    &paragraph_middle,
    &mut |editor| {
      insert_burst(editor, BURST);
      editor.cursor().column
    },
  ));

  // One character removed before the caret.
  measured.push(run_case(
    "backspace",
    "caret",
    iterations,
    BURST,
    &paragraph,
    &mut |editor| {
      for _ in 0..BURST {
        editor.apply(Intent::Backspace);
      }
      editor.cursor().column
    },
  ));

  // A whole word removed before the caret, the usual fix for a mistyped word.
  measured.push(run_case(
    "delete_word",
    "caret",
    iterations,
    BURST,
    &paragraph,
    &mut |editor| {
      for _ in 0..BURST {
        editor.apply(Intent::DeleteWordBackward);
      }
      editor.cursor().column
    },
  ));

  // Tab through a command word with 64 candidates to scan per press: the cycle this
  // feature runs on every repeated press, priced per press. The word always walks, so
  // the caret proves the completion fired and the candidate count is the load.
  measured.push(run_case(
    "tab_completion",
    "caret",
    iterations,
    BURST,
    &completing,
    &mut |editor| {
      for _ in 0..BURST {
        editor.apply(Intent::Complete);
      }
      editor.cursor().column
    },
  ));

  // Word motions on a line long enough that each one scans real distance. The caret is the work
  // signal, because a motion that stopped moving would report the column it started at.
  measured.push(run_case(
    "word_left",
    "caret",
    iterations,
    BURST,
    &long_line,
    &mut |editor| {
      for _ in 0..BURST {
        editor.apply(Intent::MoveWordLeft);
      }
      editor.cursor().column
    },
  ));
  measured.push(run_case(
    "word_right",
    "caret",
    iterations,
    BURST,
    &long_line_head,
    &mut |editor| {
      for _ in 0..BURST {
        editor.apply(Intent::MoveWordRight);
      }
      editor.cursor().column
    },
  ));

  // Recall through a 50-entry history in both directions. The history length is the work signal:
  // the draft is empty again by the time a pass ends, so the buffer says nothing.
  measured.push(run_case(
    "history_recall",
    "entries",
    iterations,
    RECALL * 2,
    &recall,
    &mut |editor| {
      for _ in 0..RECALL {
        editor.apply(Intent::MoveUp);
      }
      for _ in 0..RECALL {
        editor.apply(Intent::MoveDown);
      }
      editor.history().len()
    },
  ));

  // A held-down key on a 2000-character single-line buffer: the edit alone, with no redraw and no
  // wrapping to soften it.
  measured.push(run_case(
    "apply_long_line",
    "caret",
    iterations,
    BURST,
    &long_line,
    &mut |editor| {
      insert_burst(editor, BURST);
      editor.cursor().column
    },
  ));

  // The pair the roadmap names: a keystroke and the redraw it forces, at 80 columns on that same
  // 2000-character line, which is where a slow redraw would feel like lag.
  measured.push(run_case(
    "keystroke_redraw80",
    "rows",
    iterations,
    BURST,
    &long_line,
    &mut |editor| {
      let mut rows = 0;
      for step in 0..BURST {
        editor.apply(Intent::Insert(letter(step)));
        rows = editor.display(PREFIX).rows.len();
      }
      rows
    },
  ));

  // A redraw on its own, narrow and tall: what a resize or a repaint of a pasted note costs.
  measured.push(run_case(
    "redraw40_multiline",
    "rows",
    iterations,
    REDRAWS,
    &narrow,
    &mut |editor| {
      let mut rows = 0;
      for _ in 0..REDRAWS {
        rows = editor.display(PREFIX).rows.len();
      }
      rows
    },
  ));

  // The realistic mixed case: a scripted session cycling insert, backspace, word-left,
  // word-right, timed per keystroke. The caret proves the session went somewhere.
  measured.push(run_case(
    "mixed_session",
    "caret",
    iterations,
    SESSION,
    &paragraph,
    &mut |editor| {
      for step in 0..SESSION {
        match step % 4 {
          0 => editor.apply(Intent::Insert(letter(step))),
          1 => editor.apply(Intent::Backspace),
          2 => editor.apply(Intent::MoveWordLeft),
          _ => editor.apply(Intent::MoveWordRight),
        };
      }
      editor.cursor().column
    },
  ));

  println!(
    "TUI keystroke benchmark ({iterations} iterations per case, {LONG_LINE}-character line, \
     {HISTORY}-entry history)"
  );
  let mut breached: Vec<(&str, f64, f64)> = Vec::new();
  for case in &measured {
    let budget = budget_for(case.name);
    let status = if case.median_us > budget {
      breached.push((case.name, case.median_us, budget));
      "OVER BUDGET"
    } else {
      "ok"
    };
    println!(
      "  {name:<20} {us:>7.2} us/keystroke  budget {budget:>6.0} us  {status}  {unit}/pass {work}",
      name = case.name,
      us = case.median_us,
      unit = case.unit,
      work = case.work,
    );
  }

  if let Some(path) = json_out {
    let mut json = String::from("{\n  \"iterations\": ");
    json.push_str(&iterations.to_string());
    json.push_str(",\n  \"cases\": {\n");
    for (index, case) in measured.iter().enumerate() {
      let budget = budget_for(case.name);
      json.push_str(&format!(
        "    \"{}\": {{\"median_us\": {:.3}, \"budget_us\": {budget:.3}, \"work_unit\": \"{}\", \
         \"work_per_pass\": {}}}",
        case.name, case.median_us, case.unit, case.work
      ));
      if index + 1 < measured.len() {
        json.push(',');
      }
      json.push('\n');
    }
    json.push_str("  }\n}\n");
    if let Err(error) = fs::write(path, json) {
      eprintln!("could not write results: {error}");
      return ExitCode::FAILURE;
    }
  }

  if breached.is_empty() {
    ExitCode::SUCCESS
  } else {
    for (name, us, budget) in breached {
      eprintln!("budget exceeded: {name} measured {us:.2} us/keystroke, budget {budget:.0} us");
    }
    ExitCode::FAILURE
  }
}

/// The budget recorded for `name`, or zero when a case was added without one: a case with no
/// budget must fail rather than run unmeasured.
fn budget_for(name: &str) -> f64 {
  BUDGETS
    .iter()
    .find(|(case, _)| *case == name)
    .map(|(_, budget)| *budget)
    .unwrap_or(0.0)
}

/// Times one pass of `work` on a fresh copy of `template`, `iterations` times, and reports the
/// median microseconds per keystroke with the work the last pass produced.
///
/// The copy happens inside the loop and outside the clock: every pass has to edit the same shape
/// of buffer for a median to mean anything, and building that buffer is not a cost a keystroke
/// pays.
fn run_case(
  name: &'static str,
  unit: &'static str,
  iterations: usize,
  keystrokes: usize,
  template: &Editor,
  work: &mut dyn FnMut(&mut Editor) -> usize,
) -> Case {
  let mut per_keystroke = Vec::with_capacity(iterations);
  let mut produced = 0;
  for _ in 0..iterations {
    let mut editor = template.clone();
    let start = Instant::now();
    produced = work(&mut editor);
    per_keystroke.push(start.elapsed().as_secs_f64() * 1_000_000.0 / keystrokes as f64);
  }
  Case {
    name,
    median_us: median(&mut per_keystroke),
    work: produced,
    unit,
  }
}

/// One typed argument's value, e.g. `--iterations 50`.
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
  args
    .iter()
    .position(|a| a == flag)
    .and_then(|i| args.get(i + 1))
    .map(String::as_str)
}

fn median(values: &mut [f64]) -> f64 {
  values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  values[values.len() / 2]
}

/// A paragraph typed as a single soft-wrapped line, caret at the end, or parked in the middle.
/// The Tab case: the word `/c` with 64 candidates that all start with it, so every
/// press scans the whole list and the cycle never runs out of names to walk.
fn completing_editor() -> Editor {
  let mut editor = Editor::with_width(80);
  editor.apply(Intent::Paste("/c".to_string()));
  editor.set_completions((0..64).map(|i| format!("c{i:02}andidate")));
  editor
}

fn paragraph_editor(middle: bool) -> Editor {
  let paragraph = paragraph_text();
  let mut editor = Editor::with_width(80);
  editor.apply(Intent::Paste(paragraph.clone()));
  if middle {
    place_caret(&mut editor, paragraph.chars().count() / 2);
  }
  editor
}

/// Prose, not filler: words of varying length and commas, which is what the buffer actually
/// holds and what word motions and deletions have to scan.
fn paragraph_text() -> String {
  [
    "the loader iterates the entries and drops the parse error, so a broken entry looks exactly \
     like a missing one and the caller cannot tell which happened,",
    "which is the worst failure mode on offer because the config still reads as valid while it \
     behaves as though the file were empty",
  ]
  .join(" ")
}

/// A single line of [`LONG_LINE`] characters, caret at the end, or parked at the start so a
/// word-right motion has the whole line in front of it.
fn long_line_editor(head: bool) -> Editor {
  let mut editor = Editor::with_width(80);
  editor.apply(Intent::Paste(long_line_text()));
  if head {
    place_caret(&mut editor, 0);
  }
  editor
}

/// Words and spaces, so a keystroke on this line pays for word scanning too, and never a
/// multi-byte boundary that would measure UTF-8 handling instead of the editor.
fn long_line_text() -> String {
  let mut text = String::new();
  while text.len() < LONG_LINE {
    text.push_str("word motion ");
  }
  text.truncate(LONG_LINE);
  text
}

/// An empty draft with [`HISTORY`] submitted entries behind it, which is the state a long
/// session leaves the buffer in.
fn history_editor() -> Editor {
  let mut editor = Editor::with_width(80);
  for index in 0..HISTORY {
    editor.apply(Intent::Paste(format!(
      "recall entry {index}: find where the loader swallows the parse error"
    )));
    editor.apply(Intent::Submit);
  }
  editor
}

/// A pasted note of [`MULTILINE_LINES`] lines at 40 columns: many short rows, which is the other
/// shape a redraw pays for.
fn multiline_editor() -> Editor {
  let mut editor = Editor::with_width(40);
  let note = (0..MULTILINE_LINES)
    .map(|line| format!("line {line:02} of a pasted note, long enough to wrap at forty columns"))
    .collect::<Vec<String>>()
    .join("\n");
  editor.apply(Intent::Paste(note));
  editor
}

/// Typing `count` characters, which is what a fast typist or a held-down key does.
fn insert_burst(editor: &mut Editor, count: usize) {
  for step in 0..count {
    editor.apply(Intent::Insert(letter(step)));
  }
}

/// A plausible character for keystroke `step`, always ASCII so a keystroke never measures a
/// multi-byte boundary for reasons that have nothing to do with the editor.
fn letter(step: usize) -> char {
  char::from(b'a' + (step % 26) as u8)
}

/// Park the caret `column` characters into the first line, the way a user does by holding an
/// arrow key. Setup only, never inside a measured pass.
fn place_caret(editor: &mut Editor, column: usize) {
  editor.apply(Intent::MoveBufferStart);
  while editor.cursor().column > column {
    editor.apply(Intent::MoveLeft);
  }
  while editor.cursor().column < column {
    editor.apply(Intent::MoveRight);
  }
}
