use std::path::PathBuf;

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

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
  let mut args = args.into_iter();
  let Some(command) = args.next() else {
    return Ok(Command::Help(TOP_HELP));
  };
  if command == "--help" || command == "-h" {
    return Ok(Command::Help(TOP_HELP));
  }
  if command != "run" {
    return Err(format!("unknown command '{command}'\n{TOP_HELP}"));
  }

  let remaining: Vec<String> = args.collect();
  if remaining.iter().any(|arg| arg == "--help" || arg == "-h") {
    return Ok(Command::Help(RUN_HELP));
  }

  let mut config = None;
  let mut cwd = None;
  let mut prompt = None;
  let mut index = 0;
  while index < remaining.len() {
    let flag = &remaining[index];
    let slot = match flag.as_str() {
      "--config" => &mut config,
      "--cwd" => &mut cwd,
      "--prompt" => &mut prompt,
      _ => return Err(format!("unknown run argument '{flag}'\n{RUN_HELP}")),
    };
    index += 1;
    let value = remaining
      .get(index)
      .ok_or_else(|| format!("{flag} requires a value\n{RUN_HELP}"))?;
    if slot.replace(value.clone()).is_some() {
      return Err(format!("{flag} may be supplied only once\n{RUN_HELP}"));
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
    config: PathBuf::from(config),
    cwd: PathBuf::from(cwd),
    prompt,
  }))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
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
