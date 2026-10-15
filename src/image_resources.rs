//! Resolve the example image without depending on the process working directory.
//!
//! Installed contexts contain only the allowlisted image inputs, retaining the
//! `examples/container-images/` prefix used by the unmodified Containerfiles.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

const IMAGE_DIRECTORY: &str = "examples/container-images";
const IMAGE_FILES: &[&str] = &[
    "Containerfile.niri",
    "Containerfile.smoke",
    "fedora-cocoa-way.repo",
    "cocoa-way-clipboard-relay",
    "cocoa-way-audio-relay",
];
const RELAYS: &[&str] = &["cocoa-way-clipboard-relay", "cocoa-way-audio-relay"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExampleImageResources {
    pub(crate) containerfile: PathBuf,
    pub(crate) context: PathBuf,
}

pub(crate) fn resolve_example_image_resources() -> Result<ExampleImageResources, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate the Cocoa-Way executable: {error}"))?;
    resolve_example_image_resources_for(&executable)
}

/// Resolve relative to the physical executable, including symlinked installs.
///
/// Search order: app Resources, adjacent prefix/share/cocoa-way, then a source
/// checkout within eight executable ancestors. An incomplete installed context
/// is an error rather than a reason to silently use a different recipe.
pub(crate) fn resolve_example_image_resources_for(
    executable: &Path,
) -> Result<ExampleImageResources, String> {
    if !executable.is_absolute() {
        return Err("The Cocoa-Way executable path must be absolute; CWD is not used to locate example image resources.".into());
    }
    let executable = fs::canonicalize(executable).map_err(|error| {
        format!(
            "Could not resolve the Cocoa-Way executable '{}': {error}",
            executable.display()
        )
    })?;
    if !executable.is_file() {
        return Err(format!(
            "The Cocoa-Way executable '{}' is not a regular file.",
            executable.display()
        ));
    }
    let directory = executable.parent().expect("absolute file has a parent");
    if directory.file_name().is_some_and(|name| name == "MacOS")
        && let Some(contents) = directory.parent()
        && contents.file_name().is_some_and(|name| name == "Contents")
    {
        return validate_context(&contents.join("Resources/container-images"), true);
    }

    let share = directory
        .file_name()
        .filter(|name| *name == "bin" || *name == "sbin")
        .and_then(|_| directory.parent())
        .map(|prefix| prefix.join("share/cocoa-way/container-images"));
    if let Some(context) = &share
        && context.exists()
    {
        return validate_context(context, true);
    }

    // Cargo's native, cross-target and test executables all live below the
    // checkout. Do not consult CWD, compile-machine paths or a global checkout.
    for root in directory.ancestors().take(8) {
        if root.join("Cargo.toml").is_file()
            && root.join("src/main.rs").is_file()
            && root.join(IMAGE_DIRECTORY).is_dir()
        {
            return validate_context(root, false);
        }
    }
    let share_hint = share
        .map(|path| format!(" Expected installed context: '{}'.", path.display()))
        .unwrap_or_default();
    Err(format!(
        "Example image resources were not found next to '{}'.{share_hint} Reinstall Cocoa-Way with its container-images resources, or run a binary built inside the source checkout. The working directory is not searched.",
        executable.display()
    ))
}

fn validate_context(context: &Path, installed: bool) -> Result<ExampleImageResources, String> {
    let mut problems = Vec::new();
    for name in IMAGE_FILES {
        let path = context.join(IMAGE_DIRECTORY).join(name);
        let check = (|| -> Result<(), String> {
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if !metadata.is_file() || metadata.len() == 0 {
                return Err("must be a nonempty regular file (not a symlink)".into());
            }
            File::open(&path).map_err(|error| error.to_string())?;
            #[cfg(unix)]
            if installed && RELAYS.contains(name) {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    return Err("bundled relay is not executable".into());
                }
            }
            Ok(())
        })();
        if let Err(error) = check {
            problems.push(format!("{}: {error}", path.display()));
        }
    }
    if installed
        && problems.is_empty()
        && let Err(error) = validate_installed_layout(context)
    {
        problems.push(error);
    }
    if !problems.is_empty() {
        return Err(format!(
            "Example image resources are missing or incomplete at '{}': {}. Reinstall Cocoa-Way with its complete container-images context, or restore the example files in the source checkout.",
            context.display(),
            problems.join("; ")
        ));
    }
    let context = fs::canonicalize(context).map_err(|error| {
        format!(
            "Could not resolve image context '{}': {error}",
            context.display()
        )
    })?;
    Ok(ExampleImageResources {
        containerfile: context.join(IMAGE_DIRECTORY).join("Containerfile.niri"),
        context,
    })
}

fn validate_installed_layout(context: &Path) -> Result<(), String> {
    for (relative, expected) in [
        ("", ["examples"].as_slice()),
        ("examples", ["container-images"].as_slice()),
        (IMAGE_DIRECTORY, IMAGE_FILES),
    ] {
        let directory = context.join(relative);
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
        if !metadata.is_dir() {
            return Err(format!(
                "{} must be a directory, not a symlink",
                directory.display()
            ));
        }
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", directory.display()))?;
            if !expected.iter().any(|name| entry.file_name() == *name) {
                return Err(format!(
                    "unexpected bundled file '{}'; only example image inputs are allowed",
                    entry.path().display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "cw-image-resources-{}-{nonce}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str, data: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, data).unwrap();
            path
        }

        fn context(&self, relative: &str) -> PathBuf {
            let context = self.0.join(relative);
            for name in IMAGE_FILES {
                let path = context.join(IMAGE_DIRECTORY).join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let contents = if *name == "Containerfile.niri" {
                    include_bytes!("../examples/container-images/Containerfile.niri").as_slice()
                } else {
                    b"fixture\n"
                };
                fs::write(&path, contents).unwrap();
                #[cfg(unix)]
                if RELAYS.contains(name) {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
            context
        }

        fn checkout(&self) -> PathBuf {
            self.file("checkout/Cargo.toml", b"[package]\nname = \"cocoa-way\"\n");
            self.file("checkout/src/main.rs", b"fn main() {}\n");
            self.context("checkout")
        }

        fn bundler(&self) -> PathBuf {
            self.file(
                "checkout/scripts/bundle-example-resources.sh",
                include_bytes!("../scripts/bundle-example-resources.sh"),
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn installed_app_is_independent_of_cwd() {
        const CHILD_CONTEXT: &str = "COCOA_WAY_IMAGE_RESOURCE_TEST_CONTEXT";
        if let Some(expected) = std::env::var_os(CHILD_CONTEXT) {
            let resources = resolve_example_image_resources().unwrap();
            assert_eq!(resources.context, PathBuf::from(expected));
            assert_eq!(
                resources.containerfile,
                resources
                    .context
                    .join(IMAGE_DIRECTORY)
                    .join("Containerfile.niri")
            );
            return;
        }
        let fixture = Fixture::new();
        let executable = fixture.file("Cocoa Way.app/Contents/MacOS/cocoa-way", b"fixture");
        let context = fixture.context("Cocoa Way.app/Contents/Resources/container-images");
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let unrelated = fixture.0.join("unrelated working directory");
        fs::create_dir(&unrelated).unwrap();
        let output = Command::new(executable)
            .args(["installed_app_is_independent_of_cwd", "--nocapture"])
            .env(CHILD_CONTEXT, &context)
            .current_dir(unrelated)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn resolves_installed_share_and_physical_symlink_target() {
        let fixture = Fixture::new();
        let executable = fixture.file("prefix/bin/cocoa-way", b"fixture");
        let context = fixture.context("prefix/share/cocoa-way/container-images");
        assert_eq!(
            resolve_example_image_resources_for(&executable)
                .unwrap()
                .context,
            context
        );
        #[cfg(unix)]
        {
            let link = fixture.0.join("cocoa-way-link");
            std::os::unix::fs::symlink(executable, &link).unwrap();
            assert_eq!(
                resolve_example_image_resources_for(&link).unwrap().context,
                context
            );
        }
    }

    #[test]
    fn source_checkout_is_anchored_to_native_and_cross_target_executables() {
        let fixture = Fixture::new();
        let context = fixture.checkout();
        // The source recipe already chmods the relays inside the image.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                context.join(IMAGE_DIRECTORY).join(RELAYS[0]),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        for name in [
            "target/debug/cocoa-way",
            "target/aarch64-apple-darwin/release/cocoa-way",
        ] {
            let executable = fixture.file(&format!("checkout/{name}"), b"fixture");
            let resources = resolve_example_image_resources_for(&executable).unwrap();
            assert_eq!(resources.context, context);
            assert!(resources.containerfile.is_absolute());
        }
    }

    #[test]
    fn incomplete_app_does_not_fall_back_to_an_ancestor_checkout() {
        let fixture = Fixture::new();
        fixture.checkout();
        let executable = fixture.file(
            "checkout/Cocoa-Way.app/Contents/MacOS/cocoa-way",
            b"fixture",
        );
        let context = fixture.context("checkout/Cocoa-Way.app/Contents/Resources/container-images");
        for name in IMAGE_FILES {
            let path = context.join(IMAGE_DIRECTORY).join(name);
            let contents = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            let error = resolve_example_image_resources_for(&executable).unwrap_err();
            assert!(
                error.contains(name) && error.contains("Reinstall"),
                "{error}"
            );
            fs::write(path, contents).unwrap();
        }
    }

    #[test]
    fn missing_resources_and_relative_executables_fail_clearly() {
        let fixture = Fixture::new();
        let executable = fixture.file("prefix/bin/cocoa-way", b"fixture");
        let error = resolve_example_image_resources_for(&executable).unwrap_err();
        assert!(
            error.contains("share/cocoa-way/container-images") && error.contains("Reinstall"),
            "{error}"
        );
        let error =
            resolve_example_image_resources_for(Path::new("target/debug/cocoa-way")).unwrap_err();
        assert!(
            error.contains("absolute") && error.contains("CWD"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn installed_relays_must_be_executable() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let executable = fixture.file("prefix/bin/cocoa-way", b"fixture");
        let context = fixture.context("prefix/share/cocoa-way/container-images");
        fs::set_permissions(
            context.join(IMAGE_DIRECTORY).join(RELAYS[0]),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let error = resolve_example_image_resources_for(&executable).unwrap_err();
        assert!(error.contains("not executable"), "{error}");
    }

    #[test]
    fn installed_contexts_reject_extra_files_and_empty_inputs() {
        let fixture = Fixture::new();
        let executable = fixture.file("prefix/bin/cocoa-way", b"fixture");
        let context = fixture.context("prefix/share/cocoa-way/container-images");
        let extra = context.join("dms.toml");
        fs::write(&extra, b"private fixture").unwrap();
        let error = resolve_example_image_resources_for(&executable).unwrap_err();
        assert!(
            error.contains("unexpected bundled file") && error.contains("dms.toml"),
            "{error}"
        );
        fs::remove_file(extra).unwrap();
        fs::write(
            context.join(IMAGE_DIRECTORY).join("fedora-cocoa-way.repo"),
            b"",
        )
        .unwrap();
        let error = resolve_example_image_resources_for(&executable).unwrap_err();
        assert!(
            error.contains("nonempty") && error.contains("fedora-cocoa-way.repo"),
            "{error}"
        );
    }

    #[test]
    fn bundler_includes_copy_inputs_but_not_private_configuration() {
        let fixture = Fixture::new();
        fixture.checkout();
        let bundler = fixture.bundler();
        fixture.file(
            "checkout/examples/container-images/dms.toml",
            b"private fixture",
        );
        fixture.file(
            "checkout/examples/container-images/.env",
            b"private fixture",
        );
        fixture.file("checkout/secrets.env", b"private fixture");
        let executable = fixture.file("Cocoa Way.app/Contents/MacOS/cocoa-way", b"fixture");
        let context = fixture
            .0
            .join("Cocoa Way.app/Contents/Resources/container-images");
        let output = Command::new("bash")
            .arg(&bundler)
            .arg(&context)
            .current_dir(&fixture.0)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let resources = resolve_example_image_resources_for(&executable).unwrap();
        assert_eq!(resources.context, context);
        let recipe = fs::read_to_string(resources.containerfile).unwrap();
        for line in recipe.lines().filter(|line| line.starts_with("COPY ")) {
            let input = line.split_whitespace().nth(1).unwrap();
            assert!(
                resources.context.join(input).is_file(),
                "missing COPY input: {input}"
            );
        }
        assert_eq!(
            fs::read_dir(context.join(IMAGE_DIRECTORY)).unwrap().count(),
            IMAGE_FILES.len()
        );
        assert!(!context.join("secrets.env").exists());
        let output = Command::new("bash")
            .arg(&bundler)
            .arg("--check")
            .arg(&context)
            .current_dir(&fixture.0)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        fs::write(context.join("dms.toml"), b"private fixture").unwrap();
        let output = Command::new("bash")
            .arg(&bundler)
            .arg("--check")
            .arg(&context)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "unexpected context files must be rejected"
        );
    }

    #[test]
    fn bundler_refuses_to_merge_into_an_existing_context() {
        let fixture = Fixture::new();
        fixture.checkout();
        let bundler = fixture.bundler();
        let context = fixture.context("existing");
        let sentinel = fixture.file("existing/private.env", b"do not touch");
        let output = Command::new("bash")
            .arg(bundler)
            .arg(context)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read(sentinel).unwrap(), b"do not touch");
    }

    #[cfg(unix)]
    #[test]
    fn bundler_rejects_symlinked_inputs_before_creating_a_context() {
        let fixture = Fixture::new();
        let checkout = fixture.checkout();
        let bundler = fixture.bundler();
        let input = checkout.join(IMAGE_DIRECTORY).join(RELAYS[0]);
        fs::remove_file(&input).unwrap();
        let secret = fixture.file("private.env", b"private fixture");
        std::os::unix::fs::symlink(secret, input).unwrap();
        let destination = fixture.0.join("context");
        let output = Command::new("bash")
            .arg(bundler)
            .arg(&destination)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!destination.exists());
    }
}
