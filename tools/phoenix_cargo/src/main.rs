//! Deprecated. Most of the work is migrated to rustc_wrapper.rs
//! Inspired by [Theseus cargo].
//!
//! Different than theseus_cargo, phoenix_cargo
//! + supports dylib and proc_macro
//! + supports choosing a crate among multiple builds (with different versions or features)
//! of the same dependency crate.
//! - it does not currenlty handle cross-compiling
//!
//! [Theseus cargo]: https://github.com/theseus-os/Theseus/blob/89489db4a11f2b0ea398d72740a0258111390f5f/tools/theseus_cargo/src/main.rs
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Parser;

pub mod shell;

#[derive(Debug, Parser)]
#[command(
    about = "A wrapper around cargo to support out-of-tree build of phoenix plugins \
    based on a previous build of phoenix."
)]
struct Opts {
    /// The rlibs built by this command are considered common dependencies of phoenixos and its
    /// plugins. This is a special bootstrap operation and only applies to buildling of
    /// the `phoenix_common` crate.
    #[arg(long)]
    build_common_deps: bool,

    /// The path to the phoenix_common dependencies we will copy to.
    ///
    /// If not specified, it will be the phoenix/common_deps under the target_dir for this build.
    #[arg(long)]
    common_deps: Option<PathBuf>,

    /// Cargo subcommand
    #[arg(raw = true, allow_hyphen_values = true)]
    cargo_subcommand: Vec<String>,
}

fn is_build_command(cargo_subcommand: &[String]) -> bool {
    match cargo_subcommand.first().map(|x| x.as_str()) {
        Some("build") | Some("b") => true,
        _ => false,
    }
}

/// Returns the project's workspace root.
///
/// It is equivalent to running cargo locate-project --workspace [--manifest-path some_path].
fn locate_project_root(cargo_subcommand: &[String]) -> Result<PathBuf> {
    let mut cmd = Command::new("cargo");
    cmd.arg("locate-project").arg("--workspace");

    // Forward the --manifest-path option if present
    if let Some(manifest_path) = cargo_subcommand
        .iter()
        .position(|arg| arg == "--manifest-path")
        .map(|pos| cargo_subcommand[pos + 1].clone())
    {
        cmd.arg("--manifest-path").arg(manifest_path);
    }

    let output = cmd.output().with_context(|| {
        format!(
            "Failed to run cargo command: {:?} {:?}",
            cmd.get_program(),
            cmd.get_args()
        )
    })?;

    let output_json = String::from_utf8(output.stdout)?;

    // Extract the path inside the json
    let manifest_path = output_json
        .trim()
        .strip_prefix(r#"{"root":""#)
        .and_then(|s| s.strip_suffix(r#""}"#))
        .with_context(|| format!("Unexpected output: {}", output_json))?;

    let manifest_path = PathBuf::from(manifest_path);
    let bcx_root = manifest_path.parent().with_context(|| {
        format!(
            "Unable to get parent directory for manifest: {}",
            manifest_path.display(),
        )
    })?;

    Ok(bcx_root.to_path_buf())
}

/// Returns the default target_dir, which is bcx_root/target/phoenix
fn default_target_dir(cargo_subcommand: &[String]) -> Result<PathBuf> {
    // Determine the cargo root workspace directory
    let bcx_root = locate_project_root(cargo_subcommand).with_context(|| {
        format!(
            "Unable to determine project root, subcommand: {:?}",
            cargo_subcommand
        )
    })?;

    Ok(bcx_root.join("target").join("phoenix"))
}

/// Returns the target-dir for this build.
///
/// It returns the value of `--target-dir` is it is present. Otherwise, it returns bcx_root/target.
fn locate_target_dir(cargo_subcommand: &[String]) -> Result<PathBuf> {
    // determine the target-dir in the following order
    // 1. --target-dir command-line flag
    // 2. build.target-dir config value (not implemented)
    // 3. env CARGO_TARGET_DIR/CARGO_BUILD_TARGET_DIR (not implemented)
    // 4. default: bcx_root/target/phoenix
    cargo_subcommand
        .iter()
        .position(|arg| arg == "--target-dir")
        .map_or_else(
            || default_target_dir(cargo_subcommand),
            |pos| Ok(PathBuf::from(&cargo_subcommand[pos + 1])),
        )
}

fn determine_profile(cargo_subcommand: &[String]) -> String {
    // I couldn't find a more reliable yet simple way to do this unless follow the cargo's source
    // code to parse, initialize workspace, and expand the command alias
    if cargo_subcommand
        .iter()
        .find(|s| s.as_str() == "--release")
        .is_some()
        || cargo_subcommand
            .iter()
            .find(|s| s.as_str() == "-r")
            .is_some()
    {
        "release".to_owned()
    } else {
        "debug".to_owned()
    }
}

fn build_common_dependencies<P1: AsRef<Path>, P2: AsRef<Path>>(
    cargo_subcommand: &[String],
    target_dir: P1,
    common_deps_dir: P2,
) -> Result<()> {
    let target_dir = target_dir.as_ref();
    let common_deps_dir = common_deps_dir.as_ref();
    let rustc_wrapper = PathBuf::from("target/release/rustc_wrapper").canonicalize()?;
    let extra_envs = &[("PHOENIX_IS_COMMON_DEP", "1".to_owned())];
    run_cargo_without_capture(
        cargo_subcommand,
        Some(rustc_wrapper),
        extra_envs,
        &target_dir,
    )
    .with_context(|| format!("run_initial_cargo failed for {:?}", cargo_subcommand))?;

    // copy results from target_dir/deps to common_deps_dir
    let profile = determine_profile(cargo_subcommand);
    let src = target_dir.join(profile).join("deps");
    let msg = format!(
        "Copy from {} to {}",
        src.display(),
        common_deps_dir.display()
    );
    println!("{msg}");
    // dircpy::copy_dir(&src, &common_deps_dir).with_context(|| format!("{} failed", msg))?;
    Ok(())
}

fn main() -> Result<()> {
    let mut opts = Opts::parse();

    // Determine the target dir for this build
    let target_dir = locate_target_dir(&opts.cargo_subcommand)?;

    if opts.common_deps.is_none() {
        opts.common_deps = Some(target_dir.join("common_deps"));
    }

    // Create a new empty CompilationDatabase
    let common_deps_dir =
        fs::canonicalize(&opts.common_deps.as_ref().unwrap()).with_context(|| {
            format!(
                "--common-deps arg '{}' was an invalid path.",
                opts.common_deps.as_ref().unwrap().display()
            )
        })?;

    if opts.build_common_deps {
        build_common_dependencies(&opts.cargo_subcommand, &target_dir, &common_deps_dir)
            .context("build_common_dependencies failed")?;

        return Ok(());
    }

    // phoenix_cargo builds in two passes.
    // First pass: set RUSTC_WRAPPER=echo, and capture the stderr for incremental compile log.
    let rustc_wrapper = PathBuf::from("target/release/rustc_wrapper").canonicalize()?;
    run_cargo_without_capture(
        &opts.cargo_subcommand,
        Some(rustc_wrapper),
        &[],
        &target_dir,
    )?;

    if !is_build_command(&opts.cargo_subcommand) {
        println!("Exiting after completing non-'build' cargo command.");
        return Ok(());
    }

    Ok(())
}

fn run_cargo_without_capture<P: AsRef<Path>>(
    full_args: &[String],
    rustc_wrapper: Option<PathBuf>,
    rustc_wrapper_envs: &[(&str, String)],
    target_dir: P,
) -> Result<()> {
    let subcommand = full_args
        .first()
        .context("Missing subcommand argument to `phoenix_cargo` (e.g., `build`)")?;

    if !is_build_command(full_args) {
        bail!(
            "cargo commands other than `build` are not supported. \
            You tried to run subcommand {:?}.",
            subcommand
        );
    }

    let mut cmd = Command::new("cargo");

    for arg in &full_args[..] {
        cmd.arg(arg);
    }

    // TODO: Ensure that we use only the arguments specified by the phoenix build config
    // cmd.args(shlex::split(build_config.cargoflags).unwrap())
    //     .arg("--target").arg(&build_config.target);

    // Use full color output to get a regular terminal-esque display from cargo
    // cmd.arg("--color=always");

    // RUSTC_WRAPPER=echo, but faithfully executes when parent process is build-script-build
    if let Some(rustc_wrapper) = rustc_wrapper {
        cmd.env("RUSTC_WRAPPER", rustc_wrapper.display().to_string());
    }

    cmd.env("CARGO_TARGET_DIR", target_dir.as_ref());
    cmd.env(
        "PHOENIX_COMMON_DEPS_DIR",
        target_dir.as_ref().join("common_deps"),
    );
    for (k, v) in rustc_wrapper_envs {
        cmd.env(k, v);
    }

    // let mut rustflags = String::new();

    // -Zbinary-dep-depinfo allows us to track dependencies of each rlib
    // rustflags.push_str(" -Zunstable-options -Zbinary-dep-depinfo");
    // cmd.env("RUSTFLAGS", rustflags);

    println!("\nRunning initial cargo command:\n{:?}", cmd);
    cmd.get_envs()
        .for_each(|(k, v)| println!("\t### env {:?} = {:?}", k, v));

    // Run the actual cargo command.
    let mut child_process = cmd.spawn().context("Failed to run cargo command.")?;

    let exit_status = child_process
        .wait()
        .context("Failed to wait for cargo process to finish")?;

    match exit_status.code() {
        Some(0) => {}
        Some(code) => bail!("cargo command completed with failed exit code {}", code),
        _ => bail!("cargo command was killed"),
    }

    Ok(())
}
