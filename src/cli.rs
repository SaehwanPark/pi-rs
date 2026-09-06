use std::{
  ffi::{OsStr, OsString},
  path::PathBuf,
};

use pi_rs_tui::{ColorChoice, DiagnosticFilter, TraceSelection};

pub const TOP_HELP: &str = concat!(
  "Usage: pi-rs <command> [options]\n",
  "\n",
  "Commands:\n",
  "  run      Run one durable coding-agent turn\n",
  "  trace    Read a session's transcript back out of the store\n",
  "  skills   List the skills that would be offered to a model\n",
  "  prompts  List the prompt templates a session would offer\n",
  "  prompt   Expand one prompt template and print the prompt it becomes\n",
  "\n",
  "Run `pi-rs <command> --help` for that command's flags.\n",
);
pub const RUN_HELP: &str = concat!(
  "Usage: pi-rs run --config <file> --cwd <workspace> --prompt <text> [surface flags]\n",
  "\n",
  "Runs one durable coding-agent turn. The answer is written to stdout exactly as\n",
  "the model produced it; everything else is written to stderr. Surface flags\n",
  "control only that second stream and never change what is recorded. Value flags\n",
  "accept both `--flag value` and `--flag=value`.\n",
  "\n",
  "Required:\n",
  "  --config <file>          Provider configuration\n",
  "  --cwd <workspace>        Workspace root\n",
  "  --prompt <text>          One-shot prompt\n",
  "\n",
  "Surface:\n",
  "  --color <auto|always|never>\n",
  "                           Colour the transcript (default: auto, which means\n",
  "                           colour only when stderr is a terminal and NO_COLOR is\n",
  "                           unset)\n",
  "  --no-color               Same as --color never\n",
  "  --width <columns>        Transcript column budget; 0 never wraps (default:\n",
  "                           probe stderr, no wrapping when stderr is piped)\n",
  "  --no-reasoning           Do not print reasoning\n",
  "  --verbose                Print routine transcript chrome too (default: only\n",
  "                           warnings, errors, and state changes)\n",
  "  --quiet                  Print only warnings, errors, and tool trouble: a\n",
  "                           successful tool call is routine, a failed, refused, or\n",
  "                           unrecorded one is not\n",
  "  --silent                 Print no transcript at all (the answer on stdout is\n",
  "                           still written, and the session is still recorded)\n",
  // The two commands read the same flag differently on purpose: for `run` the
  // transcript is commentary on an answer, for `trace` it is the answer.
  "\n",
  "See also: pi-rs trace, which reads a session's transcript back out of the store.",
);

pub const TRACE_HELP: &str = concat!(
  "Usage: pi-rs trace [session-id] [options]\n",
  "\n",
  "Reads a session's canonical trace back out of the store and renders it as a\n",
  "transcript. With no session id, reads the most recent session. Unlike `run`, this\n",
  "output is the answer, so it goes to stdout and includes the routine by default.\n",
  "Streamed fragments are folded: one answer line, one reasoning block per provenance.\n",
  "\n",
  "Selection:\n",
  "  [session-id]             Session id or prefix. Newest session when omitted.\n",
  "  --session <id>           Same as the positional form.\n",
  "  --tools                  Tool activity only.\n",
  "  --reasoning              Reasoning only.\n",
  "  --epoch <n>              Only events attributed to model epoch n.\n",
  "  --sequence               Prefix each entry with its sequence number.\n",
  "\n",
  "Surface:\n",
  "  --config <file>          Configuration whose store.root holds the traces.\n",
  "  --color <auto|always|never>\n",
  "                           Colour the transcript (default: auto).\n",
  "  --no-color               Same as --color never.\n",
  "  --width <columns>        Column budget; 0 never wraps (default: probe stdout).\n",
  "  --no-reasoning           Omit reasoning text, show each block's extent.\n",
  "  --quiet                  Only warnings, errors, and tool trouble.\n",
  "  --silent                 No transcript (the footer still reports what was read).\n",
  "  --help                   Show this help.\n",
);

pub const SKILLS_HELP: &str = concat!(
  "Usage: pi-rs skills [--project]\n",
  "\n",
  "Lists the skills that would be offered to a model, one per pair of lines: source\n",
  "and name, then the description the model sees. The listing goes to stdout; every\n",
  "file that was skipped, and why, goes to stderr, so `pi-rs skills | fzf` gets names\n",
  "and nothing else.\n",
  "\n",
  "Reads $HOME/.pi/agent/skills, $HOME/.agents/skills, and --project's\n",
  "<ancestor>/.pi/skills and <ancestor>/.agents/skills up to the git root. A skill is\n",
  "instructions for the model, so project locations are read only when told they may\n",
  "be:\n",
  "\n",
  "  --project                Read the project's own skill locations\n",
  "  --help                   Show this help.\n",
);

pub const PROMPTS_HELP: &str = concat!(
  "Usage: pi-rs prompts [--project]\n",
  "\n",
  "Lists the prompt templates a session would offer, one per pair of lines: source\n",
  "and name -- with the declared argument hint when there is one -- then the\n",
  "description. A description that came from the template's first line rather than\n",
  "its frontmatter says so. The listing goes to stdout; every file that was skipped,\n",
  "and why, goes to stderr.\n",
  "\n",
  "Reads $HOME/.pi/agent/prompts/*.md and --project's <ancestor>/.pi/prompts/*.md up\n",
  "to the git root, non-recursively, because that is where Pi looks. A template is\n",
  "text the model will be sent, so project locations are read only when told they\n",
  "may be:\n",
  "\n",
  "  --project                Read the project's own prompt locations\n",
  "  --help                   Show this help.\n",
  "\n",
  "See also: pi-rs prompt <name>, which expands one of these templates.",
);

pub const PROMPT_HELP: &str = concat!(
  "Usage: pi-rs prompt [--project] <name> [arguments...]\n",
  "\n",
  "Expands one template the way Pi would and writes the prompt to stdout, raw, as\n",
  "the only thing on it. Nothing is sent to a model: this is the expansion, not the\n",
  "run. Options come before the name; everything after the name is an argument to\n",
  "the template, even if it starts with `--`.\n",
  "\n",
  "  $1, $2, ...              positional arguments\n",
  "  $@, $ARGUMENTS           all arguments joined\n",
  "  ${1:-default}            the argument, or the default when it is empty\n",
  "  ${@:-default}            all arguments, or the default when there are none\n",
  "  ${@:N} and ${@:N:L}      a slice of the argument list, 1-indexed\n",
  "\n",
  "  --project                Read the project's own prompt locations\n",
  "  --help                   Show this help.\n",
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
  Help(&'static str),
  Run(RunArgs),
  Trace(TraceArgs),
  Skills(SkillsArgs),
  Prompts(PromptsArgs),
  Prompt(PromptArgs),
}

/// `pi-rs prompts`: the templates a session would offer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptsArgs {
  /// Whether the project's own locations may be read, for the same reason as skills:
  /// a template is text that ends up in front of the model.
  pub project: bool,
}

/// `pi-rs prompt <name> [args...]`: expand one template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptArgs {
  pub name: String,
  /// Passed to the template verbatim. This is why options are parsed before the name:
  /// a template's own arguments must not be mistaken for this command's flags.
  pub arguments: Vec<String>,
  pub project: bool,
}

/// `pi-rs skills`: what a model would be offered, and what was declined.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillsArgs {
  /// Whether the project's own locations may be read. Off by default because a skill
  /// is instructions for the model, and a checkout should not be able to hand the
  /// model instructions that nobody in this session agreed to.
  pub project: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
  pub config: PathBuf,
  pub cwd: PathBuf,
  pub prompt: String,
  pub surface: SurfaceArgs,
}

/// How much transcript to print, and in what form.
///
/// Everything here is presentation. None of it changes what the runtime does or
/// what the session records, which is the point: a quiet transcript and a verbose
/// one must describe the same turn, byte for byte, in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceArgs {
  pub color: ColorChoice,
  /// `Some(n)` overrides the terminal probe (`Some(0)` means never wrap); `None`
  /// probes stderr.
  pub width: Option<usize>,
  pub reasoning: bool,
  pub diagnostics: DiagnosticFilter,
}

impl Default for SurfaceArgs {
  fn default() -> Self {
    Self {
      color: ColorChoice::Auto,
      width: None,
      reasoning: true,
      // Calm by default: a turn that narrates every request reads like a log
      // tail, and the rare event nobody can see is the one that matters.
      diagnostics: DiagnosticFilter::State,
    }
  }
}

/// Which events of a session to read back, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceArgs {
  pub config: PathBuf,
  /// Session id or prefix. Absent means the most recent session in the store.
  pub session: Option<String>,
  pub selection: TraceSelection,
  pub sequence: bool,
  pub surface: SurfaceArgs,
}

impl Default for TraceArgs {
  fn default() -> Self {
    Self {
      config: PathBuf::new(),
      session: None,
      selection: TraceSelection::default(),
      sequence: false,
      // For `trace` the transcript is the answer, so the routine is included by
      // default; `--quiet` is how the reader asks for only the trouble.
      surface: SurfaceArgs {
        diagnostics: DiagnosticFilter::All,
        ..SurfaceArgs::default()
      },
    }
  }
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
  let mut args = args.into_iter();
  let Some(command) = args.next() else {
    return Ok(Command::Help(TOP_HELP));
  };
  if command == "--help" || command == "-h" {
    return Ok(Command::Help(TOP_HELP));
  }
  let remaining: Vec<OsString> = args.collect();
  if command == "run" {
    return parse_run(&remaining);
  }
  if command == "trace" {
    return parse_trace(&remaining);
  }
  if command == "skills" {
    return parse_skills(&remaining);
  }
  if command == "prompts" {
    return parse_prompts(&remaining);
  }
  if command == "prompt" {
    return parse_prompt(&remaining);
  }
  // A bare argument is not a command. Guessing which command the user meant is worse
  // than naming the ones that exist.
  Err(format!(
    "unknown command '{}'\n{TOP_HELP}",
    command.to_string_lossy()
  ))
}

/// `pi-rs skills`: what a model would be offered, and what was declined.
fn parse_skills(remaining: &[OsString]) -> Result<Command, String> {
  let mut project = false;
  for arg in remaining {
    let flag = arg
      .to_str()
      .ok_or_else(|| format!("skills argument is not valid UTF-8\n{SKILLS_HELP}"))?;
    match flag {
      "--project" => project = true,
      "--help" | "-h" => return Ok(Command::Help(SKILLS_HELP)),
      other => {
        return Err(format!("unknown skills argument '{other}'\n{SKILLS_HELP}"));
      }
    };
  }
  Ok(Command::Skills(SkillsArgs { project }))
}

/// `pi-rs prompts`: list the templates.
fn parse_prompts(remaining: &[OsString]) -> Result<Command, String> {
  let mut project = false;
  for arg in remaining {
    let flag = arg
      .to_str()
      .ok_or_else(|| format!("prompts argument is not valid UTF-8\n{PROMPTS_HELP}"))?;
    match flag {
      "--project" => project = true,
      "--help" | "-h" => return Ok(Command::Help(PROMPTS_HELP)),
      other => {
        return Err(format!(
          "unknown prompts argument '{other}'\n{PROMPTS_HELP}"
        ));
      }
    };
  }
  Ok(Command::Prompts(PromptsArgs { project }))
}

/// `pi-rs prompt <name> [args...]`: expand one template. Flags stop at the name, so
/// `pi-rs prompt review --strict` hands `--strict` to the template.
fn parse_prompt(remaining: &[OsString]) -> Result<Command, String> {
  let mut project = false;
  let mut name: Option<String> = None;
  let mut arguments: Vec<String> = Vec::new();
  for arg in remaining {
    let text = arg
      .to_str()
      .ok_or_else(|| format!("prompt argument is not valid UTF-8\n{PROMPT_HELP}"))?;
    if name.is_none() {
      match text {
        "--project" => {
          project = true;
          continue;
        }
        "--help" | "-h" => return Ok(Command::Help(PROMPT_HELP)),
        // An option after the name is template text, so an option before it has to be
        // this command's. Anything else that starts with a dash is a mistake, and saying
        // so beats expanding a template the user did not mean.
        other if other.starts_with('-') && other.len() > 1 => {
          return Err(format!("unknown prompt argument '{other}'\n{PROMPT_HELP}"));
        }
        _ => {}
      }
    }
    match name {
      None => name = Some(text.to_string()),
      Some(_) => arguments.push(text.to_string()),
    }
  }
  let name = name.ok_or_else(|| format!("'prompt' needs a template name\n{PROMPT_HELP}"))?;
  Ok(Command::Prompt(PromptArgs {
    name,
    arguments,
    project,
  }))
}

/// `pi-rs run`: the answer goes to stdout, the transcript goes to stderr.
fn parse_run(remaining: &[OsString]) -> Result<Command, String> {
  if remaining.iter().any(|arg| arg == "--help" || arg == "-h") {
    return Ok(Command::Help(RUN_HELP));
  }

  let mut config: Option<PathBuf> = None;
  let mut cwd: Option<PathBuf> = None;
  let mut prompt: Option<String> = None;
  // Held as options so two flags that decide the same thing can be reported as a
  // conflict instead of silently resolved by whichever came last.
  let mut color: Option<ColorChoice> = None;
  let mut diagnostics: Option<DiagnosticFilter> = None;
  let mut width: Option<usize> = None;
  let mut reasoning = true;
  // `--color=always` is the shape a shell user reaches for first. Expand that form into
  // the two-token form the rest of this parser understands, and only for flags this
  // command actually takes: a prompt or path containing '=' must survive untouched.
  let remaining: &[OsString] = &expand_inline(remaining);
  let mut index = 0;
  while index < remaining.len() {
    let flag = remaining[index]
      .to_str()
      .ok_or_else(|| format!("run argument name is not valid UTF-8\n{RUN_HELP}"))?;
    index += 1;
    match flag {
      "--config" | "--cwd" | "--prompt" | "--color" | "--width" => {
        let value = remaining
          .get(index)
          .ok_or_else(|| format!("{flag} requires a value\n{RUN_HELP}"))?
          .clone();
        index += 1;
        match flag {
          "--config" => set_once(&mut config, PathBuf::from(value), flag)?,
          "--cwd" => set_once(&mut cwd, PathBuf::from(value), flag)?,
          "--prompt" => {
            let value = value
              .into_string()
              .map_err(|_| "--prompt must be valid UTF-8".to_string())?;
            set_once(&mut prompt, value, flag)?;
          }
          "--color" => {
            let value = value
              .into_string()
              .map_err(|_| "--color must be valid UTF-8".to_string())?;
            let choice = ColorChoice::parse(&value)
              .ok_or_else(|| format!("--color must be auto, always, or never\n{RUN_HELP}"))?;
            set_choice(&mut color, choice, flag)?;
          }
          "--width" => {
            let value = value
              .into_string()
              .map_err(|_| "--width must be valid UTF-8".to_string())?;
            let columns = value
              .trim()
              .parse::<usize>()
              .map_err(|_| format!("--width must be a column count\n{RUN_HELP}"))?;
            set_once(&mut width, columns, flag)?;
          }
          _ => unreachable!("matched above"),
        }
      }
      "--no-color" => set_choice(&mut color, ColorChoice::Never, flag)?,
      "--no-reasoning" => reasoning = false,
      "--verbose" => set_choice(&mut diagnostics, DiagnosticFilter::All, flag)?,
      // `--quiet` and `--silent` both decide the transcript level, so they conflict
      // with each other rather than cancelling out.
      "--quiet" => set_choice(&mut diagnostics, DiagnosticFilter::WarnAndError, flag)?,
      "--silent" => set_choice(&mut diagnostics, DiagnosticFilter::None, flag)?,
      _ => return Err(format!("unknown run argument '{flag}'\n{RUN_HELP}")),
    }
  }

  let config = config.ok_or_else(|| format!("--config is required\n{RUN_HELP}"))?;
  let cwd = cwd.ok_or_else(|| format!("--cwd is required\n{RUN_HELP}"))?;
  let prompt = prompt.ok_or_else(|| format!("--prompt is required\n{RUN_HELP}"))?;
  if prompt.trim().is_empty() {
    return Err("--prompt must not be empty".into());
  }
  Ok(Command::Run(RunArgs {
    config,
    cwd,
    prompt,
    surface: SurfaceArgs {
      // The CLI default is the calm one, which is not the renderer's own default:
      // the renderer's job is to be able to render everything, the command's job is
      // to decide what deserves the screen.
      color: color.unwrap_or_else(|| SurfaceArgs::default().color),
      width,
      reasoning,
      diagnostics: diagnostics.unwrap_or_else(|| SurfaceArgs::default().diagnostics),
    },
  }))
}

/// `pi-rs trace`: read a session's transcript back out of the store.
fn parse_trace(remaining: &[OsString]) -> Result<Command, String> {
  if remaining.iter().any(|arg| arg == "--help" || arg == "-h") {
    return Ok(Command::Help(TRACE_HELP));
  }
  let mut config: Option<PathBuf> = None;
  let mut session: Option<String> = None;
  let mut positional: Option<String> = None;
  let mut tools = false;
  let mut reasoning_only = false;
  let mut epoch: Option<u32> = None;
  let mut sequence = false;
  let mut color: Option<ColorChoice> = None;
  let mut diagnostics: Option<DiagnosticFilter> = None;
  let mut width: Option<usize> = None;
  let mut reasoning = true;
  let remaining: &[OsString] = &expand_inline(remaining);
  let mut index = 0;
  while index < remaining.len() {
    let flag = remaining[index]
      .to_str()
      .ok_or_else(|| format!("trace argument name is not valid UTF-8\n{TRACE_HELP}"))?;
    index += 1;
    match flag {
      "--config" | "--color" | "--width" | "--session" | "--epoch" => {
        let value = remaining
          .get(index)
          .ok_or_else(|| format!("{flag} requires a value\n{TRACE_HELP}"))?
          .clone();
        index += 1;
        let value = value
          .into_string()
          .map_err(|_| format!("{flag} must be valid UTF-8\n{TRACE_HELP}"))?;
        match flag {
          "--config" => set_once(&mut config, PathBuf::from(value), flag)?,
          "--session" => set_once(&mut session, value, flag)?,
          "--epoch" => {
            let number = value
              .trim()
              .parse::<u32>()
              .map_err(|_| format!("--epoch must be a model epoch number\n{TRACE_HELP}"))?;
            set_once(&mut epoch, number, flag)?;
          }
          "--color" => {
            let choice = ColorChoice::parse(&value)
              .ok_or_else(|| format!("--color must be auto, always, or never\n{TRACE_HELP}"))?;
            set_choice(&mut color, choice, flag)?;
          }
          _ => {
            let columns = value
              .trim()
              .parse::<usize>()
              .map_err(|_| format!("--width must be a column count\n{TRACE_HELP}"))?;
            set_once(&mut width, columns, flag)?;
          }
        }
      }
      "--no-color" => set_choice(&mut color, ColorChoice::Never, flag)?,
      "--no-reasoning" => reasoning = false,
      "--verbose" => set_choice(&mut diagnostics, DiagnosticFilter::All, flag)?,
      "--quiet" => set_choice(&mut diagnostics, DiagnosticFilter::WarnAndError, flag)?,
      "--silent" => set_choice(&mut diagnostics, DiagnosticFilter::None, flag)?,
      // Two category flags are two statements about what the reader wants to see, and
      // they point at different events. Union would be a guess.
      "--tools" => {
        if reasoning_only {
          return Err(format!(
            "--tools and --reasoning select different categories\\n{TRACE_HELP}"
          ));
        }
        tools = true;
      }
      "--reasoning" => {
        if tools {
          return Err(format!(
            "--tools and --reasoning select different categories\\n{TRACE_HELP}"
          ));
        }
        reasoning_only = true;
      }
      "--sequence" => sequence = true,
      // A bare argument is the session id, because `pi-rs trace 0194...` is how this
      // command actually gets used. A second one is a mistake, not a second session.
      other if !other.starts_with('-') => {
        if positional.is_some() || session.is_some() {
          return Err(format!("expected at most one session id\n{TRACE_HELP}"));
        }
        positional = Some(other.to_string());
      }
      other => return Err(format!("unknown trace argument '{other}'\n{TRACE_HELP}")),
    }
  }
  let config = config.ok_or_else(|| format!("--config is required\n{TRACE_HELP}"))?;
  let session = session.or(positional);
  let default = TraceArgs::default();
  Ok(Command::Trace(TraceArgs {
    config,
    session,
    selection: TraceSelection {
      tools,
      reasoning: reasoning_only,
      epoch,
    },
    sequence,
    surface: SurfaceArgs {
      color: color.unwrap_or(default.surface.color),
      width,
      reasoning,
      diagnostics: diagnostics.unwrap_or(default.surface.diagnostics),
    },
  }))
}

/// Expand `--flag=value` into the two-token form the parsers understand.
fn expand_inline(remaining: &[OsString]) -> Vec<OsString> {
  remaining
    .iter()
    .flat_map(|arg| match inline_value(arg) {
      Some((flag, value)) => vec![OsString::from(flag), value.to_os_string()],
      None => vec![arg.clone()],
    })
    .collect()
}

/// Split `--flag=value` when `flag` is a value-taking flag of `pi-rs run`.
fn inline_value(arg: &std::ffi::OsStr) -> Option<(&str, &std::ffi::OsStr)> {
  let (flag, value) = arg.to_str()?.split_once('=')?;
  matches!(
    flag,
    "--config" | "--cwd" | "--prompt" | "--color" | "--width" | "--session" | "--epoch"
  )
  .then_some((flag, OsStr::new(value)))
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
  if slot.replace(value).is_some() {
    return Err(format!("{flag} may be supplied only once\n{RUN_HELP}"));
  }
  Ok(())
}

/// Set one decision that several different flags can write.
///
/// The distinction from `set_once` is the error message: `--quiet --silent` is not
/// the same flag twice, it is two incompatible statements of intent, and guessing
/// which one the user meant is not the parser's call.
fn set_choice<T: std::fmt::Debug + PartialEq>(
  slot: &mut Option<T>,
  value: T,
  flag: &str,
) -> Result<(), String> {
  if let Some(previous) = slot {
    if *previous != value {
      return Err(format!(
        "{flag} conflicts with an earlier flag ({previous:?})\n{RUN_HELP}"
      ));
    }
    return Ok(());
  }
  *slot = Some(value);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strings(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
  }

  fn run(values: &[&str]) -> SurfaceArgs {
    let mut args = vec!["run", "--config", "c", "--cwd", "w", "--prompt", "p"];
    args.extend_from_slice(values);
    match parse(strings(&args)).unwrap() {
      Command::Run(args) => args.surface,
      _ => panic!("expected run"),
    }
  }

  #[test]
  fn bare_invocation_is_prompt_help() {
    assert_eq!(parse(vec![]).unwrap(), Command::Help(TOP_HELP));
  }

  #[test]
  fn help_flags_return_help() {
    assert_eq!(
      parse(strings(&["--help"])).unwrap(),
      Command::Help(TOP_HELP)
    );
    assert_eq!(
      parse(strings(&["run", "--help"])).unwrap(),
      Command::Help(RUN_HELP)
    );
  }

  #[test]
  fn run_help_needs_no_other_argument() {
    assert_eq!(
      parse(strings(&["run", "--help"])).unwrap(),
      Command::Help(RUN_HELP)
    );
    assert_eq!(
      parse(strings(&["run", "--help", "--config"])).unwrap(),
      Command::Help(RUN_HELP)
    );
  }

  #[test]
  fn prompt_is_preserved_as_one_argument() {
    let args = match parse(strings(&[
      "run",
      "--config",
      "c",
      "--cwd",
      "w",
      "--prompt",
      "  two words  ",
    ]))
    .unwrap()
    {
      Command::Run(args) => args,
      _ => panic!("expected run"),
    };
    assert_eq!(args.prompt, "  two words  ");
  }

  #[test]
  fn value_flags_accept_the_inline_form() {
    // `--color=always` is what a shell user reaches for first; refusing it would be a
    // needless syntax lesson.
    assert_eq!(run(&["--color=never"]).color, ColorChoice::Never);
    assert_eq!(run(&["--width=40"]).width, Some(40));
    assert_eq!(run(&["--color=always"]).color, ColorChoice::Always);
  }

  #[test]
  fn only_known_flags_split_on_equals() {
    // A prompt or path that happens to contain '=' is data, not a flag boundary.
    let parsed = parse(strings(&["run", "--config=c=x", "--cwd=w", "--prompt=a=b"])).unwrap();
    match parsed {
      Command::Run(args) => {
        assert_eq!(args.prompt, "a=b");
        assert_eq!(args.config, PathBuf::from("c=x"));
      }
      _ => panic!("expected run"),
    }
    // An unknown flag keeps its whole name in the error, inline form or not.
    let error = parse(strings(&["run", "--bogus=1"])).unwrap_err();
    assert!(error.contains("--bogus"), "{error}");
  }

  #[test]
  fn unknown_commands_and_flags_are_errors() {
    assert!(parse(strings(&["fly"])).is_err());
    assert!(
      parse(strings(&[
        "run", "--config", "c", "--cwd", "w", "--prompt", "p", "--bogus"
      ]))
      .is_err()
    );
  }

  #[test]
  fn required_arguments_are_still_required() {
    assert!(parse(strings(&["run", "--prompt", "p"])).is_err());
    assert!(matches!(
      parse(strings(&["run", "--config", "c", "--cwd", "w"])),
      Err(message) if message.contains("--prompt is required")
    ));
  }

  #[test]
  fn surface_flags_default_to_a_calm_monochrome_probe() {
    let surface = run(&[]);
    assert_eq!(surface.color, ColorChoice::Auto);
    assert_eq!(surface.width, None);
    assert!(surface.reasoning);
    assert_eq!(surface.diagnostics, DiagnosticFilter::State);
  }

  #[test]
  fn colour_width_and_reasoning_flags_are_honoured() {
    assert_eq!(run(&["--color", "never"]).color, ColorChoice::Never);
    assert_eq!(run(&["--no-color"]).color, ColorChoice::Never);
    assert_eq!(run(&["--width", "0"]).width, Some(0));
    assert_eq!(run(&["--width", "42"]).width, Some(42));
    assert!(!run(&["--no-reasoning"]).reasoning);
  }

  #[test]
  fn transcript_level_flags_are_distinct_levels() {
    assert_eq!(run(&["--verbose"]).diagnostics, DiagnosticFilter::All);
    assert_eq!(
      run(&["--quiet"]).diagnostics,
      DiagnosticFilter::WarnAndError
    );
    assert_eq!(run(&["--silent"]).diagnostics, DiagnosticFilter::None);
  }

  #[test]
  fn colour_flags_agreeing_twice_is_not_a_conflict() {
    assert_eq!(
      run(&["--color", "never", "--no-color"]).color,
      ColorChoice::Never
    );
  }

  #[test]
  fn contradictory_surface_flags_are_rejected_not_silently_ordered() {
    assert!(run_err(&["--color", "always", "--no-color"]).contains("conflicts"));
    assert!(run_err(&["--quiet", "--silent"]).contains("conflicts"));
    assert!(run_err(&["--silent", "--verbose"]).contains("conflicts"));
  }

  #[test]
  fn malformed_surface_values_are_errors() {
    assert!(run_err(&["--color", "neon"]).contains("--color must be"));
    assert!(run_err(&["--width", "wide"]).contains("--width must be"));
    let error = match parse(strings(&[
      "run", "--config", "c", "--cwd", "w", "--prompt", "p", "--width",
    ])) {
      Err(message) => message,
      Ok(_) => panic!("--width with no value must fail"),
    };
    assert!(error.contains("requires a value"), "{error}");
  }

  #[test]
  fn value_flags_do_not_swallow_a_later_boolean_flag() {
    // `--width --no-reasoning` is a missing column count, not a boolean in the
    // wrong slot: silently consuming `--no-reasoning` would print a turn the user
    // asked to be quiet about.
    assert!(run_err(&["--width", "--no-reasoning"]).contains("--width must be"));
  }

  fn run_err(values: &[&str]) -> String {
    let mut args = vec!["run", "--config", "c", "--cwd", "w", "--prompt", "p"];
    args.extend_from_slice(values);
    match parse(strings(&args)) {
      Err(message) => message,
      Ok(_) => panic!("expected an error"),
    }
  }

  #[test]
  fn skills_reads_nothing_from_the_project_unless_told_to() {
    // The default is the conservative one, so it has to be the parsed default too.
    assert_eq!(
      parse(strings(&["skills"])).unwrap(),
      Command::Skills(SkillsArgs { project: false })
    );
    assert_eq!(
      parse(strings(&["skills", "--project"])).unwrap(),
      Command::Skills(SkillsArgs { project: true })
    );
  }

  #[test]
  fn a_skills_flag_that_does_not_exist_is_named() {
    // Skills are instructions for the model, so a mistyped trust flag must not be
    // quietly ignored in favour of reading whatever is on disk.
    let error = match parse(strings(&["skills", "--trust"])) {
      Err(message) => message,
      Ok(command) => panic!("expected an error, got {command:?}"),
    };
    assert!(
      error.contains("unknown skills argument '--trust'"),
      "{error}"
    );
    assert!(
      error.contains("--project"),
      "the error should say what exists: {error}"
    );
  }

  #[test]
  fn prompts_reads_the_trust_flag_and_nothing_else() {
    assert_eq!(
      parse(strings(&["prompts"])).unwrap(),
      Command::Prompts(PromptsArgs { project: false })
    );
    assert_eq!(
      parse(strings(&["prompts", "--project"])).unwrap(),
      Command::Prompts(PromptsArgs { project: true })
    );
    assert!(
      matches!(
        parse(strings(&["prompts", "--help"])),
        Ok(Command::Help(PROMPTS_HELP))
      ),
      "the template grammar belongs in the expansion's help"
    );
  }

  #[test]
  fn a_prompt_invocation_splits_options_from_the_text_meant_for_the_template() {
    assert_eq!(
      parse(strings(&["prompt", "review"])).unwrap(),
      Command::Prompt(PromptArgs {
        name: "review".to_string(),
        arguments: vec![],
        project: false,
      })
    );
    // Options stop at the name: after it, everything is template text, including a
    // leading dash. `/review --strict` in Pi passes `--strict` along, and a user typing
    // it here must get the same expansion.
    assert_eq!(
      parse(strings(&[
        "prompt",
        "--project",
        "lint",
        "--strict",
        "src/"
      ]))
      .unwrap(),
      Command::Prompt(PromptArgs {
        name: "lint".to_string(),
        arguments: vec!["--strict".to_string(), "src/".to_string()],
        project: true,
      })
    );
  }

  #[test]
  fn a_prompt_flag_typed_where_the_name_belongs_is_still_an_error() {
    // `pi-rs prompt --strict review` cannot mean "pass --strict to review": there is no
    // name yet, so it is a flag this command does not have, and guessing would expand a
    // template the user did not ask for.
    let error = match parse(strings(&["prompt", "--strict", "review"])) {
      Err(message) => message,
      Ok(command) => panic!("expected an error, got {command:?}"),
    };
    assert!(
      error.contains("unknown prompt argument '--strict'"),
      "{error}"
    );
    let error = match parse(strings(&["prompt"])) {
      Err(message) => message,
      Ok(command) => panic!("expected an error, got {command:?}"),
    };
    assert!(
      error.contains("needs a template name") && error.contains("--project"),
      "the error should say what is missing and what exists: {error}"
    );
  }

  #[test]
  fn the_new_commands_are_listed_where_commands_are_listed() {
    // A command that is not in the top-level help is a command nobody finds.
    for command in ["prompts", "prompt"] {
      assert!(
        TOP_HELP.contains(&format!("  {command} ")),
        "TOP_HELP should list `{command}`: {TOP_HELP}"
      );
    }
  }
}
