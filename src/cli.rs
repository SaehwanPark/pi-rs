use std::{ffi::OsString, path::PathBuf};

pub const TOP_HELP: &str = "Usage: pi-rs run --config <file> --cwd <workspace> --prompt <text>\n";
pub const RUN_HELP: &str = "Usage: pi-rs run --config <file> --cwd <workspace> --prompt <text>\n\nRuns one durable coding-agent turn.\n";

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
  let mut index = 0;
  while index < remaining.len() {
    let flag = remaining[index]
      .to_str()
      .ok_or_else(|| format!("run argument name is not valid UTF-8\n{RUN_HELP}"))?;
    index += 1;
    let value = remaining
      .get(index)
      .ok_or_else(|| format!("{flag} requires a value\n{RUN_HELP}"))?
      .clone();
    match flag {
      "--config" => set_once(&mut config, PathBuf::from(value), flag)?,
      "--cwd" => set_once(&mut cwd, PathBuf::from(value), flag)?,
      "--prompt" => {
        let value = value
          .into_string()
          .map_err(|_| "--prompt must be valid UTF-8".to_string())?;
        set_once(&mut prompt, value, flag)?;
      }
      _ => return Err(format!("unknown run argument '{flag}'\n{RUN_HELP}")),
    }
    index += 1;
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
  }))
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
  if slot.replace(value).is_some() {
    return Err(format!("{flag} may be supplied only once\n{RUN_HELP}"));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strings(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
  }

  #[test]
  fn bare_invocation_is_prompt_help() {
    assert_eq!(parse(Vec::new()).unwrap(), Command::Help(TOP_HELP));
  }

  #[test]
  fn run_help_needs_no_other_argument() {
    assert_eq!(
      parse(strings(&["run", "--help"])).unwrap(),
      Command::Help(RUN_HELP)
    );
  }

  #[test]
  fn run_requires_each_named_argument() {
    let error = parse(strings(&["run", "--config", "config.json"])).unwrap_err();
    assert!(error.contains("--cwd is required"), "{error}");
  }

  #[test]
  fn run_preserves_prompt_as_one_argument() {
    let parsed = parse(strings(&[
      "run",
      "--config",
      "config.json",
      "--cwd",
      "/repo",
      "--prompt",
      "write the file",
    ]))
    .unwrap();
    let Command::Run(run) = parsed else {
      panic!("expected run");
    };
    assert_eq!(run.prompt, "write the file");
  }
}
