fn main() {
    if let Err(error) = agent_loader::run() {
        eprintln!("al: {error:#}");
        std::process::exit(1);
    }
}
