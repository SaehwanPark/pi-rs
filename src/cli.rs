use std::{ffi::OsString, path::PathBuf};

use pi_rs_tui::{ColorChoice, DiagnosticFilter};

pub const TOP_HELP: &str = "Usage: pi-rs run --config <file> --cwd <workspace> --prompt <text>\n";
pub const RUN_HELP: &str = concat!(
  "Usage: pi-rs run --config <file> --cwd <workspace> --prompt <text> [surface flags]\n",
  "\n",
  "Runs one durable coding-agent turn. The answer is written to stdout exactly as\n",
  "the model produced it; everything else is written to stderr. Surface flags\n",
  "control only that second stream and never change what is recorded.\n",
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
  "  --quiet                  Print only warnings, errors, and the session summary\n",
  "  --silent                 Print no transcript at all\n",
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
  Help(&'static str),
  Run(RunArgs),
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

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
  let mut args = args.into_iter();
  let Some(command) = args.next() else {
    return Ok(Command::Help(TOP_HELP));
  };
  if command == "--help" || command == "-h" {
    return Ok(Command::Help(TOP_HELP));
  }
  if command != "run" {
    return Err(format!(
      "unknown command '{}'\n{TOP_HELP}",
      command.to_string_lossy()
    ));
  }

  let remaining: Vec<OsString> = args.collect();
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
}
