fn main() {
    let path = std::env::args().nth(1).expect("usage: qalam <file.pdf>");
    if let Err(e) = qalam_core::inspect(&path) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
