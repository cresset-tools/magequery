//! The `magecommand` binary — a shim over the `magecommand` crate's CLI.
//!
//! It lives in the `magequery` package so both binaries ship from the one
//! dist app: the `magequery-v<version>` tag is a per-app tag (the app is
//! literally named `magequery`), so a second package could never release on
//! it, and a same-named `[[bin]]` in two workspace packages is an output
//! filename collision.

fn main() -> anyhow::Result<std::process::ExitCode> {
    magecommand::cli_main()
}
