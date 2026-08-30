//! The lemmalog REPL: `cargo run --bin lemmalog` (or pipe a script on
//! stdin). Commands: `help` inside.

use lemmalog::session::Session;
use std::io::{BufRead, IsTerminal, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(lemmalog::cli::run(args));
    }
    let mut session = Session::new();
    let interactive = std::io::stdin().is_terminal();
    if interactive {
        println!("lemmalog — datalog memory for agents (help for commands)");
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim() == "quit" || line.trim() == "exit" {
            break;
        }
        let out = session.execute(&line);
        if !out.is_empty() {
            print!("{out}");
        }
        let _ = std::io::stdout().flush();
    }
}
