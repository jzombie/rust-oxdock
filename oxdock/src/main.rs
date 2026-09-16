fn main() {
    if let Err(err) = oxdock::run() {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}
