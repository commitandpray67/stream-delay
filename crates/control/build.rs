//! The web UI is embedded from `ui/dist` at compile time. Tell cargo to rebuild
//! this crate when it changes; otherwise building the UI after a first
//! `cargo build` leaves the binary serving the "web interface was not included"
//! page until something else forces a rebuild.

fn main() {
    println!("cargo:rerun-if-changed=../../ui/dist");
}
