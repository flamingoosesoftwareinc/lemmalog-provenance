//! The persistent lemmalog REPL: `lemmalog` or `lemmalog repl`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        std::process::exit(lemmalog::cli::run_repl(std::iter::empty::<String>()));
    }
    if args.first().map(String::as_str) == Some("repl") {
        std::process::exit(lemmalog::cli::run_repl(args.into_iter().skip(1)));
    }
    if matches!(args[0].as_str(), "--help" | "-h") {
        std::process::exit(lemmalog::cli::run(args));
    }
    if args[0].starts_with('-') {
        std::process::exit(lemmalog::cli::run_repl(args.into_iter()));
    }
    std::process::exit(lemmalog::cli::run(args));
}
