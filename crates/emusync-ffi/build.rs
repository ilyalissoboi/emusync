// The C smoke test (tests/c_abi.rs) builds C and C++ programs with the `cc` crate at test time,
// which needs the target and host triples that only build scripts are told.
fn main() {
    for var in ["TARGET", "HOST"] {
        println!("cargo:rustc-env=EMUSYNC_BUILD_{var}={}", std::env::var(var).unwrap_or_default());
    }
    println!("cargo:rerun-if-changed=build.rs");
}
