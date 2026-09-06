mod cli;
mod run;
mod skills;
mod trace;

fn main() {
  let command = match cli::parse(std::env::args_os().skip(1)) {
    Ok(command) => command,
    Err(error) => {
      eprintln!("error: {error}");
      std::process::exit(2);
    }
  };

  match command {
    cli::Command::Help(help) => print!("{help}"),
    cli::Command::Run(args) => report(run::execute(args)),
    cli::Command::Trace(args) => report(trace::execute(args)),
    cli::Command::Skills(args) => report(skills::execute(args)),
  }
}

/// A command failure is one line on stderr and a non-zero exit.
fn report(result: Result<(), String>) {
  if let Err(error) = result {
    eprintln!("error: {error}");
    std::process::exit(1);
  }
}
