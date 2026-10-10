fn main() {
    #[cfg(target_os = "macos")]
    {
        // Use pkg-config so Homebrew/Nix/other setups can resolve dependencies.
        check_pkg_config("xkbcommon", "libxkbcommon");
    }
}

#[cfg(target_os = "macos")]
fn check_pkg_config(pkg: &str, brew_formula: &str) {
    if pkg_config::probe_library(pkg).is_ok() {
        return;
    }

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  ERROR: Missing dependency '{}'", pkg);
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    eprintln!("║  Make sure pkg-config can find it.                           ║");
    eprintln!("║                                                              ║");
    eprintln!("║  Homebrew:                                                   ║");
    eprintln!("║    brew install {}", brew_formula);
    eprintln!("║                                                              ║");
    eprintln!("║  Then rebuild:                                               ║");
    eprintln!("║    cargo clean && cargo build --release                      ║");
    eprintln!("╚══════════════════════════════════════════════════════════════╝");
    eprintln!();
    std::process::exit(1);
}
