//! Thin binary wrapper: the real logic lives in the `rgx` library crate so
//! it is testable and coverable in-process.

fn main() {
    std::process::exit(rgx::run());
}
