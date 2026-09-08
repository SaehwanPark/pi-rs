//! `pi-rs interactive`: one durable session, many turns, one terminal.
//!
//! The buffer ([`pi_rs_tui::editor`]), the keymap ([`pi_rs_tui::keys`]), and the
//! transcript renderer already exist. What they deliberately do not contain is the
//! loop: reading keys, owning raw mode, and deciding whether a keystroke means
//! "leave" are composition and terminal plumbing, and a crate that renders events
//! must not be the one that decides what a key means. That is why this module lives
//! in the composition root and pulls the runtime in through [`crate::run`].
//!
//! # One screen
//!
//! ```text
//! > what the user is typing           <- Editor::display(PROMPT_PREFIX)
//!   and its continuation rows
//! openai/gpt-5 · idle · enter submits, ctrl-c quits   <- one status line
//! ```
//!
//! There is no alternate screen. A turn's answer and transcript are ordinary writes
//! to stdout and stderr, and they belong in the terminal's scrollback the way any
//! other program's output does; a frame that kept its ordering intact would have to
//! own the whole screen and take the runtime's output with it. So the frame is
//! drawn, erased before each turn, and redrawn underneath whatever the turn printed.
//!
//! # Redraw discipline
//!
//! A frame is written only when something visible changed: the buffer changed, the
//! terminal was resized, or a turn ended. [`Outcome::Unchanged`] and an unmapped key
//! draw nothing, which is what keeps a held-down key from repainting the screen.
//!
//! # Why raw mode is suspended for a turn
//!
//! `crossterm::terminal::enable_raw_mode` is `cfmakeraw`, which clears `OPOST` and
//! `ONLCR`: a `\n` stops carrying the column reset with it. This module writes its
//! own `\r\n`, but a turn's output is written by the runtime through plain writers
//! that do not know they are attached to a raw terminal, and would render as a
//! staircase. Raw mode is therefore handed back for the duration of a turn. The
//! side effect is honest and documented: inside a turn, Ctrl-C is the terminal's own
//! signal rather than a key this loop reads, and interrupting a turn is a separate
//! runtime slice rather than something to imitate here.

use std::io::{self, Write};
use std::ops::Range;

use crossterm::{
  cursor::{MoveToColumn, MoveToPreviousLine},
  event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
  queue,
  terminal::{self, Clear, ClearType},
};

use pi_rs_core::TurnStatus;
use pi_rs_runtime::TurnError;
use pi_rs_tui::{
  ColorChoice, Editor, Intent, Outcome, Palette, RenderLine, display_width, highlight,
  keys::intent, statusline, style::Role, term, truncate,
};

use crate::{
  cli::{InteractiveArgs, SurfaceArgs},
  run::{self, SessionHandle},
};

/// What is drawn before the first row of the buffer.
const PROMPT_PREFIX: &str = "> ";

/// What the loop does with a turn that has ended.
///
/// A cancellation is something the user did on purpose, so it is not a reason to
/// lose the session: the loop says so in the transcript, where there is room to say
/// what happened, and takes the buffer back. Only a failure the loop cannot see a
/// way past ends it.
enum AfterTurn {
  /// The turn ran to the end; it counts.
  Done,
  /// What the loop prints about the user's own cancellation.
  Cancelled(&'static str),
  /// The failure that ends the session.
  Failed(run::SessionError),
}

/// Sort one turn's result into those three cases.
///
/// The status line is not what reports a cancellation. It cannot see why a turn
/// stopped, so a line that tried to say more than `waiting` would be guessing; it
/// goes back to waiting and the loop owns the explanation.
fn after_turn(result: Result<(), TurnError>) -> AfterTurn {
  match result {
    Ok(()) => AfterTurn::Done,
    Err(TurnError::Aborted(TurnStatus::Cancelled)) => AfterTurn::Cancelled("turn cancelled"),
    Err(error) => AfterTurn::Failed(run::SessionError::Turn(error)),
  }
}

/// Columns to assume when the terminal will not say how wide it is.
///
/// Reaching this needs a terminal that answers `is a terminal` but not `size`,
/// which is unusual enough that guessing is better than refusing to draw.
const FALLBACK_COLUMNS: usize = 80;

/// What the status line reports about the session's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
  /// Waiting for input.
  Idle,
  /// A turn is running; no event is read until it ends.
  Working,
}

/// What one terminal event asks the loop to do.
///
/// The whole of the loop's decision-making, made in a function that has never seen
/// a terminal: see [`action`].
#[derive(Debug, PartialEq, Eq)]
pub enum LoopAction {
  /// Hand this intent to the buffer, then repaint if the buffer changed.
  Edit(Intent),
  /// Ctrl-C with something typed: keep the draft and do nothing else.
  KeepDraft,
  /// Leave the loop.
  Quit,
  /// The terminal is `columns` wide now; reflow the buffer and repaint.
  Resize { columns: usize },
}

/// Decide what one event means, without touching a terminal.
///
/// `Ctrl-C` is decided here rather than in the keymap, and the fact the keymap
/// cannot have is whether anything is typed, which `buffer_is_empty` supplies. An
/// empty buffer means the user meant to leave. Anything in it means they did not,
/// and neither exiting nor discarding their text is this loop's to choose.
///
/// Everything else goes to the keymap, including events it reports as
/// [`Intent::Noop`]: an unmapped key is an edit that changed nothing, and "changed
/// nothing" is already how the loop decides not to repaint.
pub fn action(event: &Event, buffer_is_empty: bool) -> LoopAction {
  if is_ctrl_c(event) {
    return if buffer_is_empty {
      LoopAction::Quit
    } else {
      LoopAction::KeepDraft
    };
  }
  if let Event::Resize(columns, _rows) = event {
    // Rows are what a surface that scrolls or pages needs. This one draws at the
    // bottom of the screen and never scrolls, so only the width changes anything.
    return LoopAction::Resize {
      columns: usize::from(*columns),
    };
  }
  LoopAction::Edit(intent(event))
}

/// The one key the keymap deliberately refuses to bind.
///
/// A terminal delivering `Ctrl-C` as the byte `0x03` still arrives here, because
/// crossterm decodes that byte to `Char('c')` with `CONTROL`. A release is ignored:
/// on platforms that report press and release, treating both as keystrokes would
/// double every edit, and a release cannot be the first statement of an intent.
fn is_ctrl_c(event: &Event) -> bool {
  let Event::Key(key) = event else {
    return false;
  };
  key.kind != KeyEventKind::Release
    && key.modifiers.contains(KeyModifiers::CONTROL)
    && matches!(key.code, KeyCode::Char('c' | 'C'))
}

/// What the waiting line offers the user.
const WAITING_HINT: &str = "enter submits, ctrl-c quits";

/// The one status line: which model answers, and what the session is doing.
///
/// The wording belongs to [`statusline`]: it is a projection that can be tested
/// without a terminal, and the loop only says what it honestly knows. The model is
/// named in both states rather than only in one: knowing which model answered is
/// what makes a surprising answer interpretable afterwards.
///
/// `columns` is the width the editor is laid out at, because two width notions in
/// one frame is a bug. A status line that wraps leaves the frame holding more lines
/// than the loop counted, and every later redraw would land on the wrong row; the
/// projection cuts to that budget by dropping whole segments rather than wrapping.
fn status_line(model: &str, state: TurnState, turns: usize, columns: usize) -> String {
  let waiting = matches!(state, TurnState::Idle);
  let line = statusline::line(&statusline::Status {
    model,
    activity: if waiting {
      statusline::Activity::Waiting
    } else {
      statusline::Activity::Running
    },
    turns,
    columns,
    // The hint is what the loop accepts right now. While a turn runs, enter does not
    // submit, so it goes away rather than offering a key that does nothing.
    hint: waiting.then_some(WAITING_HINT),
  });
  // The projection's floor is one word, so it always says something; the loop's
  // budget is the harder rule, because a line of `columns + 1` is a redraw on the
  // wrong row. Where the projection could not drop its way into budget, the tail is
  // cut here, and with no columns at all that leaves nothing to draw.
  let plain = line.plain();
  if line.width() > columns {
    truncate(&plain, columns)
  } else {
    plain
  }
}

/// Rows of the frame for a buffer of `rows` display rows plus the status line.
fn frame_lines(rows: usize) -> usize {
  rows + 1
}

/// Lines to move up from just below the frame to land on display row `caret_row`.
///
/// The frame is `rows` buffer rows then the status line, and the writes above leave
/// the cursor one line below that, so the distance back is the whole frame minus the
/// row the caret is on.
fn caret_lines_up(rows: usize, caret_row: usize) -> usize {
  frame_lines(rows) - caret_row
}

/// A line count for a cursor move; terminals count these in 16 bits.
fn terminal_lines(lines: usize) -> u16 {
  u16::try_from(lines).unwrap_or(u16::MAX)
}

/// A column for a cursor move, on the same terms as [`terminal_lines`].
fn terminal_columns(columns: usize) -> u16 {
  u16::try_from(columns).unwrap_or(u16::MAX)
}

/// Raw mode for exactly as long as this value is alive.
///
/// `Drop` is the only exit, and that is the point: every path out of the loop —
/// Ctrl-C, a turn that failed, an early `?`, a panic unwinding through a caller —
/// passes through here, so the terminal is never left in a mode that nothing is
/// driving any more.
struct RawTerminal;

impl RawTerminal {
  fn enter() -> io::Result<Self> {
    terminal::enable_raw_mode()?;
    Ok(Self)
  }
}

impl Drop for RawTerminal {
  fn drop(&mut self) {
    // A destructor has nowhere to report a failure and nothing to retry it with.
    // Failing here means the terminal is still raw, which the caller cannot fix
    // either; the reason to have entered raw mode is gone, and this is the last
    // thing this program can do about it.
    let _ = terminal::disable_raw_mode();
  }
}

/// The interactive loop: the buffer, the frame it draws, the events it reads.
struct Loop {
  editor: Editor,
  /// What the status line names as the model.
  model: String,
  state: TurnState,
  /// Turns that have finished. The status line counts them; `0` says nothing yet.
  turns: usize,
  /// Columns available to this surface, as the terminal last reported them.
  columns: usize,
  /// Whether this frame is painted in colour, resolved once when the loop is built.
  ///
  /// Not asked per redraw: the answer cannot change while the session is open, and a
  /// redraw happens on every keystroke. A declining answer hands the renderer
  /// [`Palette::monochrome`], which emits exactly the bytes this loop wrote before
  /// the classifier was wired in.
  palette: Palette,
  /// How many lines the cursor sits below the row the current frame starts on.
  ///
  /// Not the frame's height: a frame ends with the cursor parked on the caret, and
  /// the caret is usually above the frame's bottom row. This is the distance an erase
  /// has to travel to get back to the top of what the loop owns, and `0` means the
  /// loop owns no rows — which is the state during and after handing the screen to a
  /// turn, because everything below that point belongs to the turn's output.
  above: usize,
}

impl Loop {
  fn new(model: String, columns: usize) -> Self {
    let mut surface = Self {
      editor: Editor::new(),
      model,
      state: TurnState::Idle,
      turns: 0,
      columns: 1,
      above: 0,
      palette: palette(),
    };
    surface.set_columns(columns);
    surface
  }

  /// Set the width the buffer soft-wraps at and the status line is cut to.
  fn set_columns(&mut self, columns: usize) {
    self.columns = columns.max(1);
    // The prefix is drawn in front of the first row, so it spends columns that the
    // buffer cannot also spend on text.
    let text = self.columns.saturating_sub(display_width(PROMPT_PREFIX));
    self.editor.set_width(text.max(1));
  }

  /// The status line this surface would draw right now.
  ///
  /// Cut to [`Loop::columns`] — the same width the editor is laid out at — and
  /// never more than one line, which is what the frame counted.
  fn status(&self) -> String {
    status_line(&self.model, self.state, self.turns, self.columns)
  }

  /// Read events until the user leaves.
  ///
  /// A turn failure ends the loop with that failure, so the terminal is restored on
  /// the way out and the reason is what gets reported.
  fn run(&mut self, session: &mut SessionHandle<'_>) -> Result<(), String> {
    self.draw().map_err(terminal_failure)?;
    loop {
      let event = event::read().map_err(|error| format!("cannot read the terminal: {error}"))?;
      match action(&event, self.editor.is_empty()) {
        LoopAction::Quit => return Ok(()),
        // Something is typed, so the key meant "stop that", not "leave". The draft
        // stays where it is, and there is nothing new to draw.
        LoopAction::KeepDraft => {}
        LoopAction::Resize { columns } => {
          self.set_columns(columns);
          self.draw().map_err(terminal_failure)?;
        }
        LoopAction::Edit(key) => match self.editor.apply(key) {
          // Nothing moved, so nothing moved on the screen.
          Outcome::Unchanged => {}
          Outcome::Changed | Outcome::Cancelled => self.draw().map_err(terminal_failure)?,
          Outcome::Submit(prompt) => self.turn(session, &prompt)?,
        },
      }
    }
  }

  /// One turn of the open session, with the screen handed over while it runs.
  fn turn(&mut self, session: &mut SessionHandle<'_>, prompt: &str) -> Result<(), String> {
    self.hand_over().map_err(terminal_failure)?;
    // See the module comment: a turn writes plain `\n`s, and raw mode has taken the
    // terminal's own translation of them away. From here until the matching enable,
    // the terminal is the one the runtime's writers expect.
    terminal::disable_raw_mode().map_err(terminal_failure)?;
    let result = session.turn(prompt);
    // Failover changes which model answers, so the frame has to ask the runtime
    // rather than keep saying what the config said when the session opened.
    self.model = session.model().to_string();
    take_line().map_err(terminal_failure)?;
    let outcome = after_turn(result);
    // The loop's own line about a cancellation, written while the terminal still
    // translates it into a row of its own.
    if let AfterTurn::Cancelled(note) = &outcome {
      write_line(&mut io::stdout(), note).map_err(terminal_failure)?;
    }
    terminal::enable_raw_mode().map_err(terminal_failure)?;
    match outcome {
      AfterTurn::Done => self.turns += 1,
      // The note above is the report; the frame below it goes back to saying
      // `waiting`, which is all the projection is allowed to claim.
      AfterTurn::Cancelled(_) => {}
      AfterTurn::Failed(error) => return Err(run::session_error(error)),
    }
    self.state = TurnState::Idle;
    self.draw().map_err(terminal_failure)
  }

  /// Replace the frame with the one line that stays up while a turn runs.
  ///
  /// The buffer is gone because the submitted prompt is what the turn prints first,
  /// and a frame left on the screen would be counted as more lines than the terminal
  /// still holds by the time the next redraw lands.
  fn hand_over(&mut self) -> io::Result<()> {
    let mut out = io::stdout();
    self.erase(&mut out)?;
    self.state = TurnState::Working;
    write_line(&mut out, &self.status())?;
    out.flush()
  }

  /// Write the frame, then leave the terminal's cursor on the caret.
  fn draw(&mut self) -> io::Result<()> {
    let mut out = io::stdout();
    self.erase(&mut out)?;
    let layout = self.editor.display(PROMPT_PREFIX);
    // The rows the editor would print stay the source of every byte drawn; the
    // segmented copy only says which run of a row gets which role. See
    // [`input_rows`].
    let buffer = self.editor.text();
    for line in input_rows(&layout.rows, PROMPT_PREFIX, &buffer) {
      write_line(&mut out, &line.render(self.palette))?;
    }
    write_line(&mut out, &self.status())?;
    let up = caret_lines_up(layout.rows.len(), layout.cursor.line);
    if up > 0 {
      queue!(out, MoveToPreviousLine(terminal_lines(up)))?;
    }
    queue!(out, MoveToColumn(terminal_columns(layout.cursor.column)))?;
    // Recorded after the move, because it says where the cursor ended up rather than
    // how much was written. The move leaves the cursor on the caret, and the caret's
    // line index is exactly its distance from the row the frame starts on.
    self.above = layout.cursor.line;
    out.flush()
  }

  /// Pull the cursor back to the row the last frame started on and erase from there.
  ///
  /// The distance is [`Loop::above`], not the frame's height: overshooting upward
  /// would take the erase into the output of an earlier turn, which is the record the
  /// user came here to read. Erasing downward is safe because a frame is always the
  /// most recent thing on the screen, so nothing below it belongs to anyone else.
  fn erase(&mut self, out: &mut impl Write) -> io::Result<()> {
    if self.above > 0 {
      queue!(out, MoveToPreviousLine(terminal_lines(self.above)))?;
      self.above = 0;
    }
    queue!(out, MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
    out.flush()
  }
}

/// One line of the frame.
///
/// The column reset is written here rather than left to the terminal, because raw
/// mode has removed the translation that would otherwise have supplied it.
fn write_line(out: &mut impl Write, text: &str) -> io::Result<()> {
  out.write_all(text.as_bytes())?;
  out.write_all(b"\r\n")
}

/// Move onto a line of our own.
///
/// A streamed answer is not newline-terminated until the session closes, so after a
/// turn the cursor can be sitting at the end of one. Drawing the next frame without
/// taking a line would overwrite that answer, which is the one thing on the screen
/// the user asked for. Called while the terminal still translates a newline into a
/// row change, so this is the only place a bare `\n` is enough.
fn take_line() -> io::Result<()> {
  let mut out = io::stdout();
  out.write_all(b"\n")?;
  out.flush()
}

/// Whether the frame is painted in colour.
///
/// The frame goes to stdout, and [`execute`] has already refused to run without a
/// terminal there, so this is normally the `NO_COLOR` / `TERM=dumb` half of
/// [`ColorChoice::Auto`]. Those are the only ways colour is declined while the loop
/// is up: `InteractiveArgs` carries no `--color`, so there is no second opinion to
/// reconcile with, and asking the environment once per session rather than once per
/// keystroke keeps a redraw as cheap as it was.
fn palette() -> Palette {
  if ColorChoice::Auto.resolve(term::Stream::Stdout.is_terminal()) {
    Palette::colored()
  } else {
    Palette::monochrome()
  }
}

/// One run of a buffer line, with the byte range it was cut from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
  start: usize,
  end: usize,
  role: Role,
}

/// [`highlight::tokens`] plus where each run it reported actually sits.
///
/// `tokens` returns runs and nothing about their positions, and it deliberately does
/// not reproduce the separators between them, so a caller that paints the original
/// line has to put the positions back. Putting them back is exact for one reason: a
/// run begins on a non-whitespace byte, and the only bytes between the end of one run
/// and the start of the next are whitespace, so the first occurrence of a run at or
/// after the end of the previous run can only be that run.
///
/// If a run ever cannot be found at that offset, classification stops and the line is
/// drawn with no colour. A row drawn without the classifier is the frame this loop
/// wrote before it existed; a row drawn at a guessed offset is a wrong colour on
/// somebody else's word.
fn classify(line: &str) -> Vec<Run> {
  let mut runs = Vec::new();
  let mut cursor = 0usize;
  for segment in highlight::tokens(line) {
    let Some(offset) = line[cursor..].find(segment.text.as_str()) else {
      return Vec::new();
    };
    let start = cursor + offset;
    let end = start + segment.text.len();
    runs.push(Run {
      start,
      end,
      role: segment.role,
    });
    cursor = end;
  }
  runs
}

/// The buffer rows of the frame, segmented for painting.
///
/// `rows` is what [`Editor::display`] prints for this buffer, and it stays the source
/// of every byte: each row is cut at the run boundaries the classifier reported for
/// the buffer line that row came from, and the pieces are pushed in order, so their
/// concatenation is the row — separators, indent, and all. Nothing here is rebuilt
/// from the classifier's segment texts, which carry no whitespace at all.
///
/// Rows follow their buffer line in order and never overlap, which is what lets one
/// cursor walk the buffer alongside them. A row the current line cannot account for
/// begins the next buffer line; a row that neither matches is handed back whole in
/// [`Role::UserText`], which is the uncoloured frame rather than a guess.
fn input_rows(rows: &[String], prefix: &str, buffer: &str) -> Vec<RenderLine> {
  let pad = " ".repeat(display_width(prefix));
  let mut lines = buffer.split('\n');
  let mut line = lines.next().unwrap_or("");
  let mut runs = classify(line);
  let mut taken = 0usize;
  let mut out = Vec::with_capacity(rows.len());
  for (index, row) in rows.iter().enumerate() {
    // The first row carries the prompt, the rest carry the indent that stands under
    // it. Neither is buffer text, so neither is classified.
    let decoration = if index == 0 { prefix } else { pad.as_str() };
    let content = row.strip_prefix(decoration).unwrap_or(row);
    if !line[taken..].starts_with(content) {
      if let Some(next) = lines.next() {
        line = next;
        runs = classify(next);
        taken = 0;
      }
    }
    if line[taken..].starts_with(content) {
      let range = taken..taken + content.len();
      taken = range.end;
      out.push(segment_row(decoration, line, range, &runs));
    } else {
      let mut whole = RenderLine::text(decoration, Role::Prompt);
      whole.push(content, Role::UserText);
      out.push(whole);
    }
  }
  out
}

/// One row of the frame, cut at the run boundaries that fall inside it.
///
/// A run that a wrapped row only shows part of keeps that part's role, so an
/// operation that wraps is still recognisably the operation on the row it lands on.
/// The whitespace the classifier does not emit is pushed in [`Role::UserText`], which
/// paints no background, so it is invisible either way and the coloured frame and the
/// plain frame hold the same characters in the same columns.
fn segment_row(decoration: &str, line: &str, row: Range<usize>, runs: &[Run]) -> RenderLine {
  let mut out = RenderLine::text(decoration, Role::Prompt);
  let mut cursor = row.start;
  for run in runs {
    let start = run.start.max(row.start);
    let end = run.end.min(row.end);
    if start >= end {
      continue;
    }
    if cursor < start {
      out.push(&line[cursor..start], Role::UserText);
    }
    out.push(&line[start..end], run.role);
    cursor = end;
  }
  if cursor < row.end {
    out.push(&line[cursor..row.end], Role::UserText);
  }
  out
}

/// `pi-rs interactive`: a session that holds many turns.
pub fn execute(args: InteractiveArgs) -> Result<(), String> {
  if !term::Stream::Stdout.is_terminal() {
    // Checked before anything is opened, so a piped invocation is one clear line
    // rather than a terminal that nobody put back.
    return Err(
      "interactive needs a terminal on stdout; for one turn in a script use `pi-rs run`"
        .to_string(),
    );
  }
  // The surface is not configurable here. Colour and width come from the terminal
  // the transcript is written to, which this command already requires.
  let surface = SurfaceArgs::default();
  run::open_session(&args.config, &args.cwd, &surface, |session| {
    // Raw mode is entered only once the session is open, so a bad config stays what
    // it was: a line printed on a terminal nothing has rearranged. From here on the
    // guard is what restores it, including when `run` returns an error.
    let _terminal = RawTerminal::enter().map_err(terminal_failure)?;
    let columns = term::Stream::Stdout.width().unwrap_or(FALLBACK_COLUMNS);
    Loop::new(session.model().to_string(), columns).run(session)
  })
}

/// An I/O failure against the terminal, in the words this command reports.
fn terminal_failure(error: io::Error) -> String {
  format!("cannot use the terminal: {error}")
}

#[cfg(test)]
mod tests {
  use super::*;

  use crossterm::event::KeyEvent;

  fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Event {
    Event::Key(KeyEvent::new_with_kind(code, modifiers, kind))
  }

  fn ctrl_c(kind: KeyEventKind) -> Event {
    key(KeyCode::Char('c'), KeyModifiers::CONTROL, kind)
  }

  #[test]
  fn ctrl_c_on_an_empty_buffer_leaves_the_loop() {
    assert_eq!(action(&ctrl_c(KeyEventKind::Press), true), LoopAction::Quit);
  }

  #[test]
  fn ctrl_c_with_text_typed_neither_quits_nor_touches_the_buffer() {
    let action = action(&ctrl_c(KeyEventKind::Press), false);
    // Not `Quit`, and not an intent the buffer would be handed: `KeepDraft` is the
    // action whose only implementation is that the loop does nothing.
    assert_eq!(action, LoopAction::KeepDraft);
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("half a sentence".to_string()));
    if let LoopAction::Edit(intent) = action {
      editor.apply(intent);
    }
    assert_eq!(editor.text(), "half a sentence");
  }

  #[test]
  fn ctrl_c_is_caught_whatever_shape_the_terminal_sends() {
    // The byte `0x03` is decoded to `Char('c')` plus `CONTROL` by crossterm, and a
    // terminal that reports the shift state as well must not be a second case.
    assert_eq!(
      action(&ctrl_c(KeyEventKind::Repeat), false),
      LoopAction::KeepDraft
    );
    assert_eq!(
      action(&ctrl_c(KeyEventKind::Repeat), true),
      LoopAction::Quit
    );
    // A release is not a second statement of an intent.
    assert_eq!(
      action(&ctrl_c(KeyEventKind::Release), true),
      LoopAction::Edit(Intent::Noop)
    );
  }

  #[test]
  fn other_control_keys_reach_the_keymap() {
    // `Ctrl-C` is the only key taken away from the buffer, and taking it by
    // modifier alone would swallow `Ctrl-D`, `Ctrl-K`, and the rest of readline.
    assert_eq!(
      action(
        &key(
          KeyCode::Char('d'),
          KeyModifiers::CONTROL,
          KeyEventKind::Press
        ),
        true
      ),
      LoopAction::Edit(Intent::DeleteForward)
    );
    assert_eq!(
      action(
        &key(KeyCode::Char('c'), KeyModifiers::NONE, KeyEventKind::Press),
        true
      ),
      LoopAction::Edit(Intent::Insert('c'))
    );
  }

  #[test]
  fn everything_else_is_the_keymaps() {
    let enter = key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press);
    assert_eq!(action(&enter, false), LoopAction::Edit(Intent::Submit));
    let escape = key(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Press);
    assert_eq!(action(&escape, false), LoopAction::Edit(Intent::Cancel));
    // An event with no binding is an edit that changes nothing, which is already the
    // loop's signal not to repaint.
    let unknown = key(KeyCode::F(7), KeyModifiers::NONE, KeyEventKind::Press);
    assert_eq!(action(&unknown, true), LoopAction::Edit(Intent::Noop));
    let paste = Event::Paste("two\nlines".to_string());
    assert_eq!(
      action(&paste, true),
      LoopAction::Edit(Intent::Paste("two\nlines".to_string()))
    );
  }

  #[test]
  fn a_resize_reports_the_new_width() {
    let event = Event::Resize(120, 40);
    assert_eq!(action(&event, true), LoopAction::Resize { columns: 120 });
  }

  #[test]
  fn the_status_line_names_the_model_and_the_turn() {
    let idle = status_line("local/vulcan", TurnState::Idle, 0, 80);
    assert!(idle.starts_with("local/vulcan"));
    assert!(idle.contains("idle"));
    let working = status_line("local/vulcan", TurnState::Working, 1, 80);
    assert!(working.starts_with("local/vulcan"));
    assert!(working.contains("turn"));
    assert!(!working.contains("ctrl-c"));
    // One line, whatever the state.
    assert_eq!(idle.matches('\n').count(), 0);
    assert_eq!(working.matches('\n').count(), 0);
  }

  #[test]
  fn the_status_line_never_spills_onto_a_second_row() {
    let wide = status_line(
      "a/very-long-model-name-that-does-not-fit",
      TurnState::Idle,
      3,
      20,
    );
    assert!(display_width(&wide) <= 20, "{wide}");
    // No budget at all: the projection would still say `idle`, but nothing fits in
    // zero columns, and a drawn word there is the spill the frame cannot survive.
    assert_eq!(status_line("a/b", TurnState::Idle, 0, 0), "");
  }

  /// The line the loop draws is the projection's output, separators and all, so the
  /// wording cannot fork back into the loop the moment the projection changes.
  #[test]
  fn the_status_line_is_the_projection_word_for_word() {
    // `turns` is nonzero on purpose: a line that dropped the turn segment would
    // still match a hand-written `model · idle · hint`, so it proves nothing.
    let waiting = status_line("local/vulcan", TurnState::Idle, 2, 120);
    assert_eq!(
      waiting,
      statusline::line(&statusline::Status {
        model: "local/vulcan",
        activity: statusline::Activity::Waiting,
        turns: 2,
        columns: 120,
        hint: Some(WAITING_HINT),
      })
      .plain()
    );
    // The running line is the same projection with the other activity word and no
    // hint, because enter does not submit while a turn is in flight.
    let running = status_line("local/vulcan", TurnState::Working, 2, 120);
    assert_eq!(
      running,
      statusline::line(&statusline::Status {
        model: "local/vulcan",
        activity: statusline::Activity::Running,
        turns: 2,
        columns: 120,
        hint: None,
      })
      .plain()
    );
    assert!(!running.contains(WAITING_HINT));
  }

  #[test]
  fn a_frame_is_the_buffer_rows_plus_one_line() {
    // The empty buffer is one row, so the frame is that row and the status line.
    let editor = Editor::new();
    let layout = editor.display(PROMPT_PREFIX);
    assert_eq!(frame_lines(layout.rows.len()), 2);
    // Caret on the only row of a two-line frame: two lines up from below it.
    assert_eq!(caret_lines_up(1, 0), 2);
    // Caret on the last buffer row of a three-line frame: two lines up, so it never
    // lands on the status line or below it.
    assert_eq!(caret_lines_up(2, 1), 2);
    assert_eq!(terminal_lines(3), 3);
    assert_eq!(terminal_lines(usize::from(u16::MAX) + 5), u16::MAX);
    assert_eq!(terminal_columns(7), 7);
    assert_eq!(terminal_columns(usize::from(u16::MAX) + 5), u16::MAX);
  }

  #[test]
  fn an_erase_returns_to_the_row_the_frame_started_on() {
    // The cursor arithmetic a redraw depends on, as the terminal sees it: writing the
    // frame leaves the cursor one line below it, the move after that parks it on the
    // caret, and the erase has to end on the row the frame began on. A distance that
    // overshoots upward takes the erase into an earlier turn's output, which is the
    // one part of the screen the loop was never allowed to touch.
    for rows in 1..=4usize {
      for caret_row in 0..rows {
        let started_on = 40usize;
        let below_the_frame = started_on + frame_lines(rows);
        let parked_on_caret = below_the_frame - caret_lines_up(rows, caret_row);
        assert_eq!(parked_on_caret, started_on + caret_row);
        // What `Loop::erase` travels is the caret's own line index.
        assert_eq!(parked_on_caret - caret_row, started_on, "{rows} rows");
      }
    }
  }

  #[test]
  fn the_buffer_wraps_inside_the_terminal_minus_the_prefix() {
    let surface = Loop::new("local/vulcan".to_string(), 40);
    assert_eq!(surface.editor.width(), 38);
    // A width the buffer cannot work with still leaves it able to draw one column.
    let narrow = Loop::new("local/vulcan".to_string(), 1);
    assert_eq!(narrow.editor.width(), 1);
    assert_eq!(narrow.columns, 1);
  }

  /// The frame the loop would draw for `buffer` at `columns`: the rows `draw` writes
  /// today, paired with the segmented rows the paint path produces for them.
  ///
  /// Built through `Loop` so the editor is laid out at the width the loop really
  /// uses, prefix spent out of it.
  fn painted(buffer: &str, columns: usize) -> Vec<(String, RenderLine)> {
    let mut surface = Loop::new("local/vulcan".to_string(), columns);
    surface.editor.apply(Intent::Paste(buffer.to_string()));
    let layout = surface.editor.display(PROMPT_PREFIX);
    let segmented = input_rows(&layout.rows, PROMPT_PREFIX, &surface.editor.text());
    assert_eq!(
      layout.rows.len(),
      segmented.len(),
      "{buffer:?} at {columns}"
    );
    layout.rows.into_iter().zip(segmented).collect()
  }

  /// The roles a row was painted with, decoration first.
  fn roles(line: &RenderLine) -> Vec<Role> {
    line.segments.iter().map(|segment| segment.role).collect()
  }

  /// What a terminal would show for a rendered row: the escape sequences removed, the
  /// characters left behind.
  ///
  /// Only `\x1b[` … `m` is stripped, because that is all [`Palette`] emits, and a row
  /// is one line of text: a colour left open across a row end would show up here as a
  /// character that was never typed.
  fn visible(rendered: &str) -> String {
    let mut out = String::new();
    let mut rest = rendered;
    while let Some(start) = rest.find("\x1b[") {
      out.push_str(&rest[..start]);
      let Some(end) = rest[start..].find('m') else {
        out.push_str(&rest[start..]);
        return out;
      };
      rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
  }

  /// Inputs the frame has to survive unchanged, and the terminal widths they are
  /// drawn at: wide glyphs, an emoji, an unterminated quote, two buffer lines, and
  /// widths narrow enough that every run wraps.
  const FRAMES: [(&str, usize); 9] = [
    ("", 80),
    ("read", 80),
    ("read crates/pi-rs-tui/src/lib.rs --offset=10", 80),
    ("가나다 라마바", 80),
    ("\u{1f41a} build --all", 80),
    ("quote \"unterminated tail", 80),
    ("read a\nsecond b", 80),
    ("read crates/x/lib.rs --offset=10", 12),
    ("가나다 \u{1f41a} next", 1),
  ];

  #[test]
  fn painting_a_row_changes_neither_its_characters_nor_its_width() {
    for (buffer, columns) in FRAMES {
      for (row, line) in painted(buffer, columns) {
        // Plain text is the reference rendering: the row the loop drew before the
        // classifier was wired in is still exactly what the painted row says.
        assert_eq!(line.plain(), row, "{buffer:?} at {columns}");
        // Escape sequences carry no columns, so the row still costs what it cost.
        assert_eq!(line.width(), display_width(&row), "{buffer:?} at {columns}");
        // The strongest form of the same rule: strip the sequences from the coloured
        // rendering and what a terminal shows is the row, character for character.
        assert_eq!(
          visible(&line.render(Palette::colored())),
          row,
          "{buffer:?} at {columns}"
        );
      }
    }
  }

  #[test]
  fn a_declining_palette_writes_the_bytes_this_loop_wrote_before_segmentation() {
    for (buffer, columns) in FRAMES {
      for (row, line) in painted(buffer, columns) {
        // Monochrome is not a degraded render, it is the reference one, and it is what
        // `NO_COLOR` and `TERM=dumb` resolve to. It emits no escape at all.
        assert_eq!(
          line.render(Palette::monochrome()),
          row,
          "{buffer:?} at {columns}"
        );
      }
    }
  }

  #[test]
  fn an_empty_buffer_paints_the_prompt_and_no_buffer_text() {
    let (row, line) = &painted("", 80)[0];
    assert_eq!(row, PROMPT_PREFIX);
    assert_eq!(roles(line), [Role::Prompt]);
  }

  /// Frames that cannot fit the width they are given, so every one of them wraps.
  ///
  /// [`FRAMES`] is all single-row, and a line that fits cannot show what happens to a
  /// character that lands on a break. These are the cases PR #40 left out: an argument
  /// longer than the column count, CJK at an odd width where a glyph worth two cannot
  /// be spent evenly, an emoji argument, an unterminated quote long enough to wrap, and
  /// lines that wrap more than twice.
  const WRAPPING_FRAMES: [(&str, usize); 7] = [
    // One argument, no spaces to break on, longer than the whole terminal.
    ("read crates/pi-rs-tui/src/interactive.rs", 12),
    // Two columns a glyph at an odd width: nine columns for glyphs worth two.
    ("가나다라마바사아자차카타", 11),
    ("한글 입력 테스트 입니다", 7),
    // An emoji argument: two columns wide like the CJK, from another range.
    ("\u{1f41a} \u{1f41a} \u{1f41a} \u{1f41a} build --all", 9),
    // An unterminated quote, so classification stops partway down a wrapped line.
    ("read \"unterminated tail that keeps going", 9),
    // Long enough to wrap more than twice at the narrow widths used here.
    ("read src/main.rs --offset=10 --limit=20", 5),
    ("the quick brown fox jumps over the lazy dog", 6),
  ];

  /// One cluster of five code points: three people held together by zero-width
  /// joiners, which is what a single typed character is when the user pastes one.
  const JOINED_EMOJI: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} ship it now";

  /// The decoration drawn in front of the frame's row at `index`: the prompt on the
  /// first row, the indent that stands under it on every row after.
  fn decoration(index: usize) -> String {
    if index == 0 {
      PROMPT_PREFIX.to_string()
    } else {
      " ".repeat(display_width(PROMPT_PREFIX))
    }
  }

  /// The buffer text a drawn row shows: the row with its decoration taken off.
  fn drawn(row: &str, index: usize) -> &str {
    let decoration = decoration(index);
    row.strip_prefix(decoration.as_str()).unwrap_or(row)
  }

  /// The buffer text of every row in order, with the wrapping-induced breaks removed.
  fn drawn_rows(rows: &[(String, RenderLine)]) -> Vec<&str> {
    rows
      .iter()
      .enumerate()
      .map(|(index, (row, _))| drawn(row, index))
      .collect()
  }

  #[test]
  fn wrapping_removes_nothing_from_the_buffer() {
    for (buffer, columns) in WRAPPING_FRAMES {
      let rows = painted(buffer, columns);
      // A frame that does not wrap would prove nothing; these fixtures are here
      // because they cannot fit.
      assert!(rows.len() > 1, "{buffer:?} at {columns} fits in one row");
      // A wrap is a break in the drawing, not an edit of the text: put the rows back
      // in order, take the decoration off each, and what is left is the input.
      let recomposed: String = drawn_rows(&rows).concat();
      assert_eq!(recomposed, buffer, "{buffer:?} at {columns}");
      // And the painted row says what the drawn row says, so the characters counted
      // above are the characters the paint path carries, not just the ones `draw`
      // happened to write.
      for (row, line) in &rows {
        assert_eq!(&line.plain(), row, "{buffer:?} at {columns}");
      }
    }
  }

  #[test]
  fn every_row_of_a_wrapped_frame_fits_and_draws_something() {
    for (buffer, columns) in WRAPPING_FRAMES {
      let rows = painted(buffer, columns);
      assert!(rows.len() > 1, "{buffer:?} at {columns} fits in one row");
      for (index, (row, line)) in rows.iter().enumerate() {
        // The prefix spends columns the buffer cannot also spend on text, and a
        // double-width glyph placed past the edge of the terminal shows up here.
        assert!(
          display_width(row) <= columns,
          "row {index} of {buffer:?} at {columns} costs {} columns",
          display_width(row)
        );
        // The painted row costs exactly what the drawn row costs.
        assert_eq!(line.width(), display_width(row), "{buffer:?} at {columns}");
        // An empty row is a row the terminal was told to draw and found nothing on.
        // Only an empty buffer may produce one, and these buffers are not empty.
        assert!(
          !drawn(row, index).is_empty(),
          "row {index} of {buffer:?} at {columns} draws nothing"
        );
      }
    }
  }

  #[test]
  fn wrapping_splits_neither_a_cluster_nor_a_double_width_character() {
    for (buffer, columns) in WRAPPING_FRAMES {
      let rows = painted(buffer, columns);
      assert!(rows.len() > 1, "{buffer:?} at {columns} fits in one row");
      let pieces = drawn_rows(&rows);
      let recomposed: String = pieces.concat();
      // Recompose and measure again: a character broken across a row boundary leaves
      // one row holding part of its columns, so the pieces cost a different number of
      // columns than the whole they were cut from.
      let pieces_width: usize = pieces.iter().map(|piece| display_width(piece)).sum();
      assert_eq!(
        display_width(&recomposed),
        pieces_width,
        "{buffer:?} at {columns} was broken inside a character"
      );
      // A break may only be taken where a new character begins. A row that starts on
      // something measuring no columns was started in the middle of a cluster, and a
      // row that ends on a joiner left the joiner behind and put what it joins on the
      // row after it.
      for (index, piece) in pieces.iter().enumerate() {
        if index == 0 {
          // The first row begins where the buffer begins, decoration aside.
          continue;
        }
        let begins = piece.chars().next().unwrap_or(' ');
        let mut bytes = [0u8; 4];
        assert_ne!(
          display_width(begins.encode_utf8(&mut bytes)),
          0,
          "row {index} of {buffer:?} at {columns} begins inside a cluster: {piece:?}"
        );
        assert!(
          !piece.ends_with('\u{200d}'),
          "row {index} of {buffer:?} at {columns} ends on a joiner: {piece:?}"
        );
      }
    }
  }

  #[test]
  fn a_joined_emoji_is_not_broken_across_a_wrapped_row() {
    // The joiners inside [`JOINED_EMOJI`] measure no columns, so a break taken beside
    // one is invisible to every width check: the boundary is the only place it shows.
    for columns in [6, 7, 8] {
      let rows = painted(JOINED_EMOJI, columns);
      assert!(
        rows.len() > 1,
        "{JOINED_EMOJI:?} at {columns} fits in one row"
      );
      for (index, piece) in drawn_rows(&rows).iter().enumerate() {
        assert!(
          !piece.ends_with('\u{200d}'),
          "row {index} of {JOINED_EMOJI:?} at {columns} ends on a joiner: {piece:?}"
        );
        assert!(
          !piece.starts_with('\u{200d}'),
          "row {index} of {JOINED_EMOJI:?} at {columns} begins on a joiner: {piece:?}"
        );
      }
    }
  }

  #[test]
  fn colour_still_adds_up_across_a_wrapped_row() {
    for (buffer, columns) in WRAPPING_FRAMES {
      for (row, line) in painted(buffer, columns) {
        // Monochrome is the reference rendering, and it writes no escape at all — not
        // on a row the buffer was broken onto and not on the row before it.
        let plain = line.render(Palette::monochrome());
        assert_eq!(plain, row, "{buffer:?} at {columns}");
        assert!(
          !plain.contains('\u{1b}'),
          "{buffer:?} at {columns} wrote an escape"
        );
        // Strip the coloured rendering and what a terminal shows is the row and
        // nothing else, so no colour is left open across a row end by the wrap.
        assert_eq!(
          visible(&line.render(Palette::colored())),
          row,
          "{buffer:?} at {columns}"
        );
      }
    }
  }

  #[test]
  fn the_first_run_of_a_line_is_painted_as_the_operation_and_the_rest_as_arguments() {
    // The roles come from `highlight::tokens`, so the wiring has to reach the row with
    // them in the order the classifier reported them: prompt, operation, separator,
    // argument, separator, argument. `tokens` never claims a flag or a path, so this
    // frame must not start claiming one either.
    let (row, line) = &painted("read crates/x/lib.rs --offset=10", 80)[0];
    assert_eq!(row, "> read crates/x/lib.rs --offset=10");
    assert_eq!(
      roles(line),
      [
        Role::Prompt,
        Role::Operation,
        Role::UserText,
        Role::Argument,
        Role::UserText,
        Role::Argument,
      ]
    );
  }

  #[test]
  fn an_unterminated_quote_is_one_argument_to_the_end_of_the_row() {
    let (row, line) = &painted("quote \"unterminated tail", 80)[0];
    assert_eq!(row, "> quote \"unterminated tail");
    assert_eq!(
      roles(line),
      [
        Role::Prompt,
        Role::Operation,
        Role::UserText,
        Role::Argument
      ]
    );
  }

  /// Inputs the paint path has to hand back unchanged: a bare operation, an
  /// operation and a flag, a double-quoted argument carrying a space, an
  /// unterminated quote, a CJK argument, an emoji argument, and leading whitespace
  /// the classifier never emits a run for.
  const PAINT_LINES: [&str; 7] = [
    "status",
    "status --all",
    "commit -m \"fix the wiring\"",
    "\"unterminated",
    "read 가나다.txt",
    "build \u{1f41a}",
    "   status --all",
  ];

  /// A whole line painted at once, as the row of a buffer that fits its width.
  fn paint(line: &str) -> RenderLine {
    segment_row("", line, 0..line.len(), &classify(line))
  }

  #[test]
  fn a_painted_line_holds_the_characters_it_was_given_and_costs_their_width() {
    for line in PAINT_LINES {
      let styled = paint(line);
      // The characters are the input, byte for byte. `tokens` reports runs and no
      // separators, so a wiring that rebuilds the line from run texts drops the
      // spaces, the indent, and the closing quote.
      assert_eq!(styled.plain(), line, "{line:?}");
      // Colour costs no columns: the row is still what the editor measured.
      assert_eq!(styled.width(), display_width(line), "{line:?}");
      // The strongest form of both: strip the escapes from the coloured render and a
      // terminal still shows exactly the input.
      let coloured = styled.render(Palette::colored());
      assert_eq!(visible(&coloured), line, "{line:?}");
    }
  }

  #[test]
  fn the_no_colour_path_yields_the_same_characters_as_the_plain_path() {
    for line in PAINT_LINES {
      let styled = paint(line);
      // Monochrome is what `NO_COLOR` and `TERM=dumb` resolve to. It is not a
      // degraded render of something else: it writes the plain path's characters and
      // no escape at all.
      let rendered = styled.render(Palette::monochrome());
      assert_eq!(rendered, styled.plain(), "{line:?}");
      assert!(!rendered.contains('\x1b'), "{line:?}");
      assert_eq!(visible(&rendered), line, "{line:?}");
    }
  }

  #[test]
  fn a_run_that_wraps_keeps_its_role_on_the_row_it_continues_on() {
    // Six columns of buffer: the operation is split across the first two rows, and the
    // continuation row is the tail of the operation, not a new operation of its own.
    let rows = painted("readmethis next", 8);
    let plain: Vec<&str> = rows.iter().map(|(row, _)| row.as_str()).collect();
    assert_eq!(plain, ["> readme", "  this n", "  ext"]);
    assert_eq!(roles(&rows[0].1), [Role::Prompt, Role::Operation]);
    assert_eq!(
      roles(&rows[1].1),
      [
        Role::Prompt,
        Role::Operation,
        Role::UserText,
        Role::Argument
      ]
    );
    assert_eq!(roles(&rows[2].1), [Role::Prompt, Role::Argument]);
  }

  #[test]
  fn painting_a_frame_loses_no_character_of_the_buffer() {
    for (buffer, columns) in FRAMES {
      let recovered: String = painted(buffer, columns)
        .iter()
        // Segment zero is the row's decoration, which is not buffer text.
        .map(|(_, line)| {
          line.segments[1..]
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>()
        })
        .collect();
      assert_eq!(
        recovered,
        buffer.replace('\n', ""),
        "{buffer:?} at {columns}"
      );
    }
  }
}
