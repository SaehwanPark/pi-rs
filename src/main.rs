mod cli;
mod run;

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
    cli::Command::Run(args) => {
      if let Err(error) = run::execute(args) {
        eprintln!("error: {error}");
        std::process::exit(1);
      }
    }
  }
}
