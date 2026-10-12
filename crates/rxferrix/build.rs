// Rebuild when the embedded dashboard assets change (files added or removed, not only edited).
fn main() {
    println!("cargo:rerun-if-changed=dashboard-dist");
}
