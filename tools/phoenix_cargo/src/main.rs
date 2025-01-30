//! Inspired by [Theseus cargo].
//!
//! Different than theseus_cargo, phoenix_cargo
//! + supports dylib and proc_macro
//! + supports choosing a crate among multiple builds (with different versions or features)
//! of the same dependency crate.
//! - it does not currenlty handle cross-compiling
//!
//! [Theseus cargo]: https://github.com/theseus-os/Theseus/blob/89489db4a11f2b0ea398d72740a0258111390f5f/tools/theseus_cargo/src/main.rs
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use ansi_term::Color;
use anyhow::{bail, Context, Result};
use clap::Parser;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

pub mod shell;
use shell::Shell;

#[derive(Clone)]
struct Available {
    inner: Arc<AvailableInner>,
}

impl fmt::Debug for Available {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Available")
            .field("data", &self.inner.lock)
            .finish()
    }
}

impl Default for Available {
    fn default() -> Self {
        Self::new()
    }
}

impl Available {
    fn new() -> Self {
        Available {
            inner: Arc::new(AvailableInner {
                lock: Mutex::new(false),
                cvar: Condvar::new(),
            }),
        }
    }

    fn wait(&self) {
        let AvailableInner { lock, cvar } = &*self.inner;
        let mut available = lock.lock().unwrap();
        while !*available {
            available = cvar.wait(available).unwrap();
        }
    }

    fn make_available(&self) {
        let AvailableInner { lock, cvar } = &*self.inner;
        let mut available = lock.lock().unwrap();
        *available = true;
        cvar.notify_all();
    }
}

#[derive(Debug)]
struct AvailableInner {
    lock: Mutex<bool>,
    cvar: Condvar,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Crate {
    name: String,
    metadata: String,
    pkg_version: Version,
    features: Vec<String>,
    path: PathBuf,
    // direct dependencies, each is a name with hash suffix/metadata
    dependencies: Vec<String>,
    is_primary: bool,
    // initialize it later
    is_recreated: Option<bool>,
    // whether the crate has been available
    #[serde(with = "serde_availability")]
    available: Available,
}

// Custom Serializer & Deserializer for Available
mod serde_availability {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(a: &Available, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = a.inner.lock.lock().map_err(serde::ser::Error::custom)?;
        serializer.serialize_bool(*value)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Available, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bool::deserialize(deserializer)?;
        let a = Available::new();
        if value {
            a.make_available();
        }
        Ok(a)
    }
}

impl Crate {
    fn crate_name_with_hash(&self) -> String {
        format!("{}-{}", self.name, self.metadata)
    }
}

/// It maps a crate-name to a list of candidate crates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CommonDeps(HashMap<String, Vec<Crate>>);

impl CommonDeps {
    fn new(rustc_commands: &[String], common_deps_dir: &Path) -> Result<Self> {
        let mut crates = HashMap::default();
        for original_cmd in rustc_commands {
            let Some(mut c) = get_crate_from_rustc_command(original_cmd)? else {
                continue;
            };
            c.is_recreated = Some(true);
            c.path = common_deps_dir.join(c.path.file_name().context("Could not get file_name")?);
            // crates in common_deps are by default available (and must be)
            c.available.make_available();
            crates
                .entry(c.name.to_owned())
                .or_insert_with(Vec::new)
                .push(c);
        }
        Ok(CommonDeps(crates))
    }

    fn merge(&mut self, other: &CommonDeps) {
        for (k, v) in other.0.iter() {
            self.0
                .entry(k.clone())
                .or_insert_with(Vec::new)
                .extend(v.iter().cloned());
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum CompliationPhase {
    Started,
    Completed,
}

#[derive(Debug, Serialize, Deserialize)]
struct CompilationRecord {
    is_common_dependency: bool,
    command: String,
    started_ts: std::time::SystemTime,
    crate_info: Crate,
    state: CompliationPhase,
}

#[derive(Debug, Serialize, Deserialize)]
struct CompilationDatabase {
    /// Path to the compile_commands.json
    #[serde(skip)]
    compile_commands_path: PathBuf,

    /// The directory that contains the common_deps crates for `phoenix_common`.
    common_deps_dir: PathBuf,

    /// The dependency closure for `phoenix_common`. Crates in this set are common_deps and will be
    /// used to inject into as dependencies of plugins (if they can replace any compatible
    /// dependent crate of a plugin).
    common_deps: CommonDeps,

    /// Lookup table that returns the crate information for a given cratename-metadata
    crate_info: HashMap<String, Crate>,
}

impl Drop for CompilationDatabase {
    fn drop(&mut self) {
        self.save_to_file().unwrap_or_else(|e| {
            eprintln!(
                "Failed to save to {}: {}",
                self.compile_commands_path.display(),
                e
            )
        });
    }
}

impl CompilationDatabase {
    fn from_opts(opts: &Opts) -> Result<Self> {
        let compile_commands_path = opts.compile_commands.as_ref().unwrap().clone();

        if Path::try_exists(&compile_commands_path).with_context(|| {
            format!(
                "Can't check existence of file: {}",
                compile_commands_path.display()
            )
        })? {
            // The DB exists, initialize from the file
            let compile_commands_file = fs::File::open(&compile_commands_path)?;
            let reader = BufReader::new(compile_commands_file);
            let mut db: CompilationDatabase = serde_json::from_reader(reader)?;
            db.compile_commands_path = compile_commands_path;
            Ok(db)
        } else {
            // Create a new empty CompilationDatabase
            let common_deps_dir = fs::canonicalize(&opts.common_deps.as_ref().unwrap())
                .with_context(|| {
                    format!(
                        "--common-deps arg '{}' was an invalid path.",
                        opts.common_deps.as_ref().unwrap().display()
                    )
                })?;
            Ok(Self {
                compile_commands_path,
                common_deps_dir,
                common_deps: CommonDeps::default(),
                crate_info: Default::default(),
            })
        }
    }

    fn build_common_dependencies<P: AsRef<Path>>(
        &mut self,
        cargo_subcommand: &[String],
        target_dir: P,
    ) -> Result<()> {
        let commands = run_initial_cargo(cargo_subcommand, None, &target_dir)
            .with_context(|| format!("run_initial_cargo failed for {:?}", cargo_subcommand))?;

        // copy results from target_dir/deps to common_deps_dir
        let profile = determine_profile(cargo_subcommand);
        let src = target_dir.as_ref().join(profile).join("deps");
        let msg = format!(
            "Copy from {} to {}",
            src.display(),
            self.common_deps_dir.display()
        );
        println!("{msg}");
        dircpy::copy_dir(&src, &self.common_deps_dir).with_context(|| format!("{} failed", msg))?;

        let new_common_deps = CommonDeps::new(&commands, &self.common_deps_dir)?;

        // Merge new records
        self.common_deps.merge(&new_common_deps);
        let new_crate_info = new_common_deps.0.values().flat_map(|crates| {
            crates
                .iter()
                .map(|c| (format!("{}-{}", c.name, c.metadata), c.clone()))
        });
        self.crate_info.extend(new_crate_info);
        Ok(())
    }

    fn save_to_file(&self) -> Result<()> {
        let buf = serde_json::to_string(self)?;
        let mut file = fs::File::create(&self.compile_commands_path)?;
        file.write_all(buf.as_bytes())?;
        Ok(())
    }

    fn mark_recreated(&mut self, c: &Crate, is_recreated: bool) {
        self.crate_info
            .get_mut(&c.crate_name_with_hash())
            .unwrap_or_else(|| panic!("Not found info for crate: {:?}", c))
            .is_recreated = Some(is_recreated);
    }

    fn insert_crate(&mut self, c: &Crate) {
        self.crate_info.insert(c.crate_name_with_hash(), c.clone());
        // .ok_or(())
        // .unwrap_err();
    }

    fn get_crate(&self, crate_name_with_hash: &str) -> Option<Crate> {
        self.crate_info.get(crate_name_with_hash).cloned()
    }

    // Two crates are _compatible_ if they meets the following conditions:
    // 1. they have the exact same crate name
    // 2. their semantic versions are compatible (check more out on semantic version)
    // 3. the feature set of `desired` is a subset of `provided`
    // 4. their direct dependencies are also _compatible_.
    fn is_compatible(&self, desired: &Crate, provided: &Crate, recurse_level: usize) -> bool {
        let req = VersionReq::parse(&desired.pkg_version.to_string()).unwrap();
        if !req.matches(&provided.pkg_version) {
            println!(
                "{}: desired {}, provided {}",
                Color::Red.paint("version not compatible"),
                Color::Purple.paint(format!("{:?}", desired)),
                Color::Purple.paint(format!("{:?}", provided)),
            );
            return false;
        }
        let desired_features: BTreeSet<_> = desired.features.iter().cloned().collect();
        let provided_features: BTreeSet<_> = provided.features.iter().cloned().collect();
        if !provided_features.is_superset(&desired_features) {
            println!(
                "{}: desired {}, provided {}",
                Color::Red.paint("features not compatible"),
                Color::Purple.paint(format!("{:?}", desired)),
                Color::Purple.paint(format!("{:?}", provided)),
            );
            return false;
        }
        if recurse_level == 0 {
            // The implementation here does not check the compatibability of each crate exactly.
            // Instead, it just checks whether a compatible one can be found in the common_deps_set
            // for all direct dependencies.
            desired.dependencies.iter().all(|dep| {
                self.get_crate(&dep)
                    .map(|dep_crate| {
                        self.contains_compatible_crates_in_common_deps(
                            &dep_crate,
                            recurse_level + 1,
                        )
                    })
                    .unwrap_or(false)
            })
        } else {
            true
        }
    }

    fn contains_compatible_crates_in_common_deps(&self, c: &Crate, recurse_level: usize) -> bool {
        // TODO(cjr): Accelerate this function using a query cache.
        self.common_deps
            .0
            .get(&c.name)
            .map(|candidate_set| {
                candidate_set
                    .iter()
                    .any(|cand| self.is_compatible(c, cand, recurse_level))
            })
            .unwrap_or(false)
    }

    fn find_compatible_crates_in_common_deps(&self, c: &Crate) -> Vec<Crate> {
        self.common_deps
            .0
            .get(&c.name)
            .map(|candidate_set| {
                let mut cands = Vec::new();
                for cand in candidate_set {
                    let msg = Color::Blue.paint(format!("cand: {:?}", cand));
                    println!("{}", msg);
                    if self.is_compatible(c, cand, 0) {
                        cands.push(cand.clone());
                    }
                }
                cands
            })
            .unwrap_or_default()
    }
}

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

    /// The dep file that specifies the dependencies of a latest build of phoenix_common.
    ///
    /// If not specified, it will be the phoenix/phoneix_compile_commands.json under the target_dir
    /// for this build.
    #[arg(long)]
    compile_commands: Option<PathBuf>,

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

fn main() -> Result<()> {
    let mut opts = Opts::parse();

    // Determine the target dir for this build
    let target_dir = locate_target_dir(&opts.cargo_subcommand)?;

    // Adjust `compile_commands` and `common_deps` according to cargo_subcommand if not set by the user
    if opts.compile_commands.is_none() {
        opts.compile_commands = Some(target_dir.join("phoenix_compile_commands.json"));
    }
    if opts.common_deps.is_none() {
        opts.common_deps = Some(target_dir.join("common_deps"));
    }

    // Initialize the compile database from the cargo's log file.
    // This log file should be the file generated during building package phoenix_common.
    let mut compile_db = CompilationDatabase::from_opts(&opts)?;

    dbg!(&compile_db);

    if opts.build_common_deps {
        compile_db
            .build_common_dependencies(&opts.cargo_subcommand, &target_dir)
            .context("build_common_dependencies failed")?;
        return Ok(());
    }

    // phoenix_cargo builds in two passes.
    // First pass: set RUSTC_WRAPPER=echo, and capture the stderr for incremental compile log.
    let rustc_wrapper = PathBuf::from("scripts/rustc_wrapper.rs").canonicalize()?;
    let first_pass_stderr_captured =
        run_initial_cargo(&opts.cargo_subcommand, Some(rustc_wrapper), &target_dir)?;

    dbg!(&first_pass_stderr_captured);

    if !is_build_command(&opts.cargo_subcommand) {
        println!("Exiting after completing non-'build' cargo command.");
        return Ok(());
    }

    // Second pass, re-run the commands captured in the first stage, modify the extern arguments
    // based on the matched crates in the compilation database.

    // Change working directory
    if let Some(manifest_path) = opts
        .cargo_subcommand
        .iter()
        .position(|arg| arg == "--manifest-path")
        .map(|pos| opts.cargo_subcommand[pos + 1].clone())
    {
        let manifest_path = fs::canonicalize(&manifest_path)?;
        let manifest_dir = manifest_path.parent().with_context(|| {
            format!(
                "manifest_path has no parent directory: {}",
                manifest_path.display()
            )
        })?;
        std::env::set_current_dir(manifest_dir)?;
    }

    // Now that we have run the initial cargo build, it has created many redundant dependency artifacts
    // in the local crate's target/ directory, namely the locally re-built versions of phoenix crates,
    // specifically all the crates that are in the set of common_deps.
    // let last_cmd = first_pass_stderr_captured
    //     .last()
    //     .context("No commands captured from stderr during the initial cargo command")?;
    // let out_dir = PathBuf::from(get_out_dir_arg(last_cmd)?);

    // We need to remove those redundant files from the local target/ directory (the "out-dir")
    // such that when we re-issue the rustc commands below, it won't fail with an error about
    // multiple "potentially newer" versions of a given crate dependency.
    // remove_redundant_artifacts(&compile_db, out_dir)?;

    // Re-execute the rustc commands that we captured from the original cargo verbose output.
    rayon::scope(|s| {
        let verbose_level = count_verbose_arg(&opts.cargo_subcommand);
        let mut shell = Shell::new();
        let (sender, receiver) = mpsc::channel();

        let mut shell_print = |(pkg_name, pkg_version)| -> Result<()> {
            // TODO(cjr): pass cmd_str and display_env_str
            match verbose_level {
                0 => shell.status("Compiling", format!("{pkg_name} v{pkg_version}"))?,
                1 => shell.status("Running", format!("{pkg_name} {pkg_version}"))?,
                2.. => {
                    shell.status("Compiling", format!("{pkg_name} v{pkg_version}"))?;
                    shell.status("Running", format!("{pkg_name} {pkg_version}"))?;
                }
            }
            Ok(())
        };

        for original_cmd in &first_pass_stderr_captured {
            match receiver.try_recv() {
                Err(_) => {},
                Ok(print_task) => {
                    shell_print(print_task).unwrap_or_else(|e| panic!("shell_print failed: {e}"))
                }
            }

            // This function will only re-run rustc for crates that don't already exist in the set of common_deps common_dep crates.
            if let Some(mut task) = run_rustc_command(original_cmd, &mut compile_db).unwrap() {
                let sender = sender.clone();
                s.spawn(move |_s| {
                    for dep in task.dependencies {
                        println!("waiting for dep: {}", dep.0);
                        dep.1.wait();
                        println!("resolved dep: {}", dep.0);
                    }

                    // Finally, we run the recreated rustc command.
                    // set_file_mtime("invoked.timestamp", FileTime::now()).unwrap();
                    let mut rustc_process = task
                        .recreated_cmd
                        .spawn()
                        .expect("Failed to run cargo command");

                    // Send to main thread to print status message
                    sender
                        .send((task.c.name.clone(), task.c.pkg_version.clone()))
                        .unwrap_or_else(|e| panic!("fail to send to mpsc::channel: {e}"));

                    let exit_status = rustc_process.wait().expect("Error running rustc");

                    match exit_status.code() {
                        Some(0) => {
                            println!(
                                "{} {}: Ran rustc command (modified for Phoenix) successfully.",
                                task.c.name, task.c.pkg_version
                            );

                            // Copy the compilation result to the parent directory of deps, just like what cargo
                            // would do.
                            if task.c.is_primary {
                                copy_result(&task.c).unwrap();
                            }

                            task.c.available.make_available();
                        }
                        Some(code) => panic!("rustc command exited with failure code {}", code),
                        _ => panic!("rustc command failed and was killed."),
                    }
                });
            }
        }

        drop(sender);
        while let Ok(print_task) = receiver.recv() {
            shell_print(print_task).unwrap_or_else(|e| panic!("shell_print failed: {e}"))
        }
    });

    Ok(())
}

// The commands we care about capturing starting with "Running `" and end with "`".
const COMMAND_COMPILING: &str = "Compiling ";
const COMMAND_START: &str = "Running `";
const COMMAND_END: &str = "`";
const RUSTC_CMD_START: &str = "rustc --crate-name";
const BUILD_SCRIPT_CRATE_NAME: &str = "build_script_build";

const CARGO_PKG_VERSION: &str = "CARGO_PKG_VERSION";
const CARGO_PRIMARY_PKG: &str = "CARGO_PRIMARY_PACKAGE=1";

// The format of rmeta/rlib file names.
const RMETA_RLIB_FILE_PREFIX: &str = "lib";
const RMETA_FILE_EXTENSION: &str = "rmeta";
const RLIB_FILE_EXTENSION: &str = "rlib";
const DYLIB_FILE_EXTENSION: &str = "so";
const PREFIX_END: usize = RMETA_RLIB_FILE_PREFIX.len();

// Captures the `Running` rustc commands printed by cargo. Prints only the user desired output.
fn capture_rustc_commands<R: io::Read>(
    reader: &mut BufReader<R>,
    verbose_level: usize,
) -> (Vec<String>, String) {
    let mut original_stderr = String::new();
    let mut captured_commands = Vec::new();

    // Use regex to strip out the ANSI color codes emitted by the cargo command
    let ansi_escape_regex = regex::Regex::new(r"[\x1B\x9B]\[[^m]+m").unwrap();

    let mut pending_multiline_cmd = false;
    let mut original_multiline = String::new();
    let mut is_primary_pkg = false;

    // Capture every line that cargo writes to stderr.
    // We only re-echo the lines that should be outputted by the verbose level specified.
    // The complexity below is due to the fact that a verbose command printed by cargo
    // may span multiple lines, so we need to detect the beginning and end of a multi-line command
    // and merge it into a single line in our captured output.
    reader
        .lines()
        .filter_map(|line| line.ok())
        .for_each(|original_line| {
            original_stderr.push_str(&original_line);
            original_stderr.push('\n');
            let replaced = ansi_escape_regex.replace_all(&original_line, "");
            let line_stripped = replaced.trim_start();

            let is_final_line = (line_stripped.contains("--crate-name")
                && line_stripped.contains("--crate-type"))
                || line_stripped.ends_with("build-script-build`");

            if line_stripped.starts_with(COMMAND_START) {
                // Here, we've reached the beginning of a rustc command,
                // which we actually do care about.
                is_primary_pkg = false;
                is_primary_pkg |= line_stripped.contains(CARGO_PRIMARY_PKG);
                captured_commands.push(line_stripped.to_string());
                pending_multiline_cmd = !is_final_line;
                original_multiline = String::from(&original_line);
                if !is_final_line {
                    return; // continue to the next line
                }
            } else {
                // Here, we've reached another line, which *may* be the continuation of
                // a previous rustc command, or it may just be a completely irrelevant
                // line of output.
                is_primary_pkg |= line_stripped.contains(CARGO_PRIMARY_PKG);
                if pending_multiline_cmd {
                    // append to the latest line of output instead of adding a new line
                    let last = captured_commands
                        .last_mut()
                        .expect("BUG: captured_commands had no last element");
                    last.push(' ');
                    last.push_str(line_stripped);
                    original_multiline.push('\n');
                    original_multiline.push_str(&original_line);
                    pending_multiline_cmd = !is_final_line;
                    if !is_final_line {
                        return; // continue to the next line
                    }
                } else {
                    // Here: this is an unrelated line of output that isn't a command we want
                    // to capture.
                    original_multiline.clear(); // = String::from(&original_line);
                }
            }

            // In the above cargo command, we added a verbose argument to capture the commands
            // issued from cargo to rustc.
            // But if the user didn't ask for that, then we shouldn't print that verbose output here.
            // Verbose output lines start with "Running `", "+ ", or "[".
            let should_print = |stripped_line: &str, is_primary: bool| {
                // println!(
                //     "debuggin: verbose_level: {}, stripped_line: {}",
                //     verbose_level,
                //     &stripped_line[0..30.min(stripped_line.len())]
                // );
                // TODO(cjr): cargo displays warnings for local package, but I did not find a
                // convenient way to determine if a package is local, so here we just show
                // warnings for primary packages we are building.
                let show_warnings = is_primary
                    && !stripped_line.starts_with("+ ")
                    && !stripped_line.starts_with("[")
                    && !stripped_line.starts_with(COMMAND_START);
                match verbose_level {
                    0 => stripped_line.starts_with(COMMAND_COMPILING) || show_warnings,
                    1 => {
                        // print only "Compiling" and warning/error lines if not verbose
                        stripped_line.starts_with(COMMAND_COMPILING)
                            || stripped_line.starts_with(COMMAND_START)
                            || show_warnings
                    }
                    2.. => true, // print everything if verbose
                }
            };

            if !original_multiline.is_empty() && is_final_line {
                let original_multiline_replaced =
                    ansi_escape_regex.replace_all(&original_multiline, "");
                let original_multiline_stripped = original_multiline_replaced.trim_start();
                if should_print(original_multiline_stripped, is_primary_pkg) {
                    eprintln!("{}", original_multiline)
                }
            } else if should_print(line_stripped, is_primary_pkg) {
                eprintln!("{}", original_line);
            }
        });

    // Return original_stderr for error handling
    (captured_commands, original_stderr)
}

/// Runs the actual cargo build command.
///
/// Returns the captured content of content written to `stderr` by the cargo command, as a list of lines.
fn run_initial_cargo<P: AsRef<Path>>(
    full_args: &[String],
    rustc_wrapper: Option<PathBuf>,
    target_dir: P,
) -> Result<Vec<String>> {
    let verbose_level = count_verbose_arg(full_args);

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
    cmd.arg(subcommand)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped());

    for arg in &full_args[1..] {
        cmd.arg(arg);
    }

    // TODO: Ensure that we use only the arguments specified by the phoenix build config
    // cmd.args(shlex::split(build_config.cargoflags).unwrap())
    //     .arg("--target").arg(&build_config.target);

    // Ensure that we run the cargo command with the maximum verbosity level, which is -vv.
    cmd.arg("-vv");

    // Use full color output to get a regular terminal-esque display from cargo
    cmd.arg("--color=always");

    // RUSTC_WRAPPER=echo, but faithfully executes when parent process is build-script-build
    if let Some(rustc_wrapper) = rustc_wrapper {
        cmd.env("RUSTC_WRAPPER", rustc_wrapper.display().to_string());
    }

    cmd.env("CARGO_TARGET_DIR", target_dir.as_ref());

    // let mut rustflags = String::new();

    // -Zbinary-dep-depinfo allows us to track dependencies of each rlib
    // rustflags.push_str(" -Zunstable-options -Zbinary-dep-depinfo");
    // cmd.env("RUSTFLAGS", rustflags);

    println!("\nRunning initial cargo command:\n{:?}", cmd);
    cmd.get_envs()
        .for_each(|(k, v)| println!("\t### env {:?} = {:?}", k, v));

    // Run the actual cargo command.
    let mut child_process = cmd.spawn().context("Failed to run cargo command.")?;

    // We read the stderr output in this thread and create a new thread to thread the stdout
    // output.
    let stdout = child_process
        .stdout
        .take()
        .context("Could not capture stdout")?;
    let t = thread::spawn(move || {
        let stdout_reader = BufReader::new(stdout);
        let mut stdout_logs = Vec::new();
        stdout_reader
            .lines()
            .filter_map(|line| line.ok())
            .for_each(|line| {
                // Cargo only prints to stdout for build script output only if very verbose.
                if verbose_level >= 2 {
                    println!("{}", line);
                }
                stdout_logs.push(line);
            });
        stdout_logs
    });

    let stderr = child_process
        .stderr
        .take()
        .context("Could not capture stderr.")?;
    let mut stderr_reader = BufReader::new(stderr);
    let (stderr_logs, original_stderr) = capture_rustc_commands(&mut stderr_reader, verbose_level);

    let _stdout_logs = t.join().unwrap();
    let exit_status = child_process
        .wait()
        .context("Failed to wait for cargo process to finish")?;
    match exit_status.code() {
        Some(0) => {}
        Some(code) => {
            println!("{}", original_stderr);
            bail!("cargo command completed with failed exit code {}", code);
        }
        _ => {
            println!("{}", original_stderr);
            bail!("cargo command was killed");
        }
    }

    Ok(stderr_logs)
}

/// Returns true if the given `arg` should be ignored in our rustc invocation.
fn ignore_arg(arg: &str) -> bool {
    arg == "--error-format" || arg == "--json"
}

fn parse_rustc_command(original_cmd: &str) -> Result<Option<(String, &str, clap::ArgMatches)>> {
    let command = if original_cmd.starts_with(COMMAND_START) && original_cmd.ends_with(COMMAND_END)
    {
        let end_index = original_cmd.len() - COMMAND_END.len();
        &original_cmd[COMMAND_START.len()..end_index]
    } else {
        bail!(
            "Unexpected formatting in capture command (must start with {:?} and end with {:?}. \
            Command: {:?}",
            original_cmd,
            COMMAND_START,
            COMMAND_END,
        );
    };

    // Skip invocations of build scripts, as I don't think we need to re-run those.
    // If this turns out to be wrong and we do need to run them, we need to change this logic to simply re-run it
    // and skip pretty much the rest of this entire function.
    if command.ends_with("build-script-build") {
        return Ok(None);
    }

    let start_of_rustc_cmd = command.find(RUSTC_CMD_START).with_context(|| {
        format!(
            "Couldn't find {:?} in command:\n{:?}",
            RUSTC_CMD_START, command
        )
    })?;
    let rustc_env_vars = &command[..start_of_rustc_cmd];
    let command_without_env = &command[start_of_rustc_cmd..];

    let mut vars = shlex::split(rustc_env_vars).unwrap();
    // The rustc could have a path prefix, like
    // /root/.rustup/toolchains/nightly-2024-05-01-x86_64-unknown-linux-gnu/bin/rustc --crate-name
    // therefore, the last env_var needs to be stripped
    while let Some(env) = vars.last() {
        if !env.contains('=') {
            vars.pop();
        } else {
            break;
        }
    }

    // This is okay to unwrap because the only error can be returned is QuoteError::Nul.
    let rustc_env_vars = shlex::try_join(vars.iter().map(|s| s.as_ref())).unwrap();

    // The arguments in the command that we care about are:
    //  *  "-L dependency=<dir>"
    //  *  "--extern <crate_name>=<crate_file>.rmeta"
    //
    // Below, we use `clap` to find those argumnets and replace them.
    //
    // First, we parse the following part:
    // "rustc --crate-name <crate_name> <crate_source_file> <all_other_args>"
    let top_level_matches = rustc_clap_options("rustc")
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .allow_external_subcommands(true)
        .color(clap::ColorChoice::Never)
        .try_get_matches_from(shlex::split(command_without_env).unwrap());

    let top_level_matches = top_level_matches
        .context("Missing support for argument found in captured rustc command")?;

    Ok(Some((
        rustc_env_vars,
        command_without_env,
        top_level_matches,
    )))
}

fn get_crate_from_rustc_command(original_cmd: &str) -> Result<Option<Crate>> {
    let Some((rustc_env_vars, command_without_env, top_level_matches)) =
        parse_rustc_command(original_cmd)?
    else {
        // skip invocations of build scripts
        return Ok(None);
    };

    // crate-name
    // Clap will parse the args as such:
    // * the --crate-name will be the first argument
    // * the path to the crate's main file will be the first subcommand
    // * that subcommand's arguments will include ALL OTHER arguments that we care about, specified below.
    let crate_name = top_level_matches
        .get_one::<String>("--crate-name")
        .expect("rustc command did not have required --crate-name argument");

    // pkg_version
    let splitted = shlex::split(&rustc_env_vars);
    let (_key, pkg_version) = splitted
        .as_ref()
        .unwrap()
        .iter()
        .map(|env| env.split_once('=').unwrap())
        .find(|&(k, _v)| k == CARGO_PKG_VERSION)
        .with_context(|| {
            format!(
                "Could not find {CARGO_PKG_VERSION} in envs: {:?}",
                rustc_env_vars
            )
        })?;

    let is_primary = rustc_env_vars.contains(CARGO_PRIMARY_PKG);

    // metadata
    let (_crate_source_file, additional_args) = top_level_matches
        .subcommand()
        .context("Missing crate source files and addition args after rustc")?;
    let args_after_source_file = additional_args.get_many::<OsString>("").unwrap();

    let matches = rustc_clap_options("")
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .allow_external_subcommands(true)
        .color(clap::ColorChoice::Never)
        .try_get_matches_from(args_after_source_file);

    let matches =
        matches.context("Missing support for argument found in captured rustc command")?;

    let crate_type = matches
        .get_one::<String>("--crate-type")
        .expect("rustc command did not have required --crate-type argument");

    let codegen_opts = matches
        .get_many::<String>("-C")
        .expect("rustc command did not have required -C argument");
    let metadata = codegen_opts
        .into_iter()
        .find_map(|opt| opt.strip_prefix("metadata="))
        .context("rustc command did not have metadata specified")?;

    // features
    let mut features = Vec::new();
    if let Some(values) = matches.get_many::<String>("--cfg") {
        for value in values {
            dbg!(value);
            if let Some(feature) = value.strip_prefix("feature=") {
                features.push(feature.to_owned());
            }
        }
    }

    // crate path
    let out_dir = PathBuf::from(get_out_dir_arg(command_without_env)?);
    let path = match crate_type.as_str() {
        "bin" => out_dir.join(format!("{}-{}", crate_name, metadata)),
        "lib" | "rlib" => out_dir.join(format!(
            "{}{}-{}.rlib",
            RMETA_RLIB_FILE_PREFIX, crate_name, metadata
        )),
        "proc-macro" | "dylib" => out_dir.join(format!(
            "{}{}-{}.so",
            RMETA_RLIB_FILE_PREFIX, crate_name, metadata
        )),
        _ => {
            panic!("Todo: support this crate-type: {}", crate_type)
        }
    };

    // direct dependencies
    let mut dependencies = Vec::new();
    if let Some(values) = matches.get_many::<String>("--extern") {
        for value in values {
            if value == "proc_macro" {
                dependencies.push(value.clone());
            } else {
                if let Some((_crate_name, crate_path)) = value.split_once('=') {
                    let crate_path = Path::new(crate_path);
                    let crate_name_with_hash = get_crate_name_with_hash_from_path(crate_path)?
                        .strip_prefix(RMETA_RLIB_FILE_PREFIX)
                        .with_context(|| {
                            format!(
                                "Found .rlib or .rmeta file after '--extern' \
                                    that didn't start with 'lib' prefix: {}",
                                crate_path.display()
                            )
                        })?;
                    dependencies.push(crate_name_with_hash.to_owned());
                } else {
                    panic!("Found --extern '{}' that does not have a exact path", value);
                }
            }
        }
    }

    Ok(Some(Crate {
        name: crate_name.to_owned(),
        metadata: metadata.to_owned(),
        pkg_version: Version::parse(pkg_version)?,
        features,
        path,
        dependencies,
        is_primary,
        is_recreated: None,
        available: Available::new(),
    }))
}

struct RustcTask {
    recreated_cmd: Command,
    c: Crate,
    dependencies: Vec<(String, Available)>,
}

/// Takes the given `original_cmd` that was captured from the verbose output of cargo,
/// and parses/modifies it to link against (depend on) the corresponding crate of the same name
/// from the list of common_deps crates.
///
/// The actual dependency files (.rmeta/.rlib) for the common_deps crates should be located in the
/// `common_deps_dir`.
/// The target specification JSON file should be found in the `target_dir_path`.
/// These two directories are usually the same directory.
///
/// # Return
/// * Returns `Ok(task)` that contains that rustc task about to execute.
/// * Returns `Ok(None)` if no action needs to be taken.
///   This occurs if `original_cmd` is for building a build script (currently ignored),
///   or if `original_cmd` is for building a crate that already exists in the set of `common_deps_crates`.
/// * Returns an error if the command fails to parse.
fn run_rustc_command(
    original_cmd: &str,
    compile_db: &mut CompilationDatabase,
) -> Result<Option<RustcTask>> {
    let common_deps_dir = compile_db.common_deps_dir.clone();

    let Some(c) = get_crate_from_rustc_command(original_cmd)? else {
        // skip invocations of build scripts
        return Ok(None);
    };

    compile_db.insert_crate(&c);

    let crate_name_with_hash = c.crate_name_with_hash();

    // Skip build script invocations, as we may not need to re-run those.
    if c.name == BUILD_SCRIPT_CRATE_NAME {
        println!("\n### Skipping build script build");
        return Ok(None);
    }

    // Skip crates that have already been built. (Not sure if this is always 100% correct)
    let crate_to_build = compile_db
        .get_crate(&crate_name_with_hash)
        .unwrap_or_else(|| panic!("Found no crate named: {:?}", crate_name_with_hash));
    if !compile_db
        .find_compatible_crates_in_common_deps(&crate_to_build)
        .is_empty()
    {
        println!(
            "\n### Skipping already-built crate {:?}",
            crate_name_with_hash
        );
        compile_db.mark_recreated(&c, false);
        return Ok(None);
    }

    println!("\n\nLooking at original command:\n{}", original_cmd);
    let Some((rustc_env_vars, _command_without_env, top_level_matches)) =
        parse_rustc_command(original_cmd)?
    else {
        // skip invocations of build scripts
        return Ok(None);
    };

    let (crate_source_file, additional_args) = top_level_matches
        .subcommand()
        .context("Missing crate source files and addition args after rustc")?;

    // Now, re-create the rustc command invocation with the proper arguments.
    // First, we handle the --crate-name and --edition arguments, which may come before the crate source file path.
    let mut recreated_cmd = Command::new("rustc");
    recreated_cmd.arg("--crate-name").arg(&c.name);
    if let Some(edition) = top_level_matches.get_one::<String>("--edition") {
        recreated_cmd.arg("--edition").arg(edition);
    }
    recreated_cmd.arg(crate_source_file);

    let args_after_source_file = additional_args.get_many::<OsString>("").unwrap();

    // Second, we parse all other args in the command that followed the crate source file.
    // Note that the arg name, the parameter in with_name(), in each arg below MUST BE exactly how it is invoked by cargo.
    let matches = rustc_clap_options("")
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .allow_external_subcommands(true)
        .color(clap::ColorChoice::Never)
        .try_get_matches_from(args_after_source_file);

    let matches =
        matches.context("Missing support for argument found in captured rustc command")?;

    let mut dependencies = Vec::new();
    let mut args_or_deps_changed = false;

    // After adding the initial stuff: rustc command, crate name, (optional --edition), and crate source file,
    // the other arguments are added in the loop below.
    for arg in matches.ids() {
        let values = matches
            .get_raw(arg.as_str())
            .unwrap()
            .map(|s| s.to_os_string())
            .collect::<Vec<_>>();
        println!("Arg {:?} has values:\n\t {:?}", arg, values);
        if ignore_arg(arg.as_str()) {
            continue;
        }

        for value in values {
            let value = value.to_string_lossy();
            let mut new_value = value.to_owned();

            if arg == "--extern" {
                let rmeta_or_rlib_extension = if value.ends_with(RMETA_FILE_EXTENSION) {
                    Some(RMETA_FILE_EXTENSION)
                } else if value.ends_with(RLIB_FILE_EXTENSION) {
                    Some(RLIB_FILE_EXTENSION)
                } else if value.ends_with(DYLIB_FILE_EXTENSION) {
                    Some(DYLIB_FILE_EXTENSION)
                } else if value == "proc_macro" {
                    None
                } else {
                    // println!("Skipping non-rlib or non-dylib --extern value: {:?}", value);
                    bail!(
                        "Unsupported --extern arg value {:?}. \
                        We only support '.rlib', '.rmeta', or '.so' files",
                        value
                    );
                };

                if let Some(_extension) = rmeta_or_rlib_extension {
                    let (extern_crate_name, crate_rmeta_path) = value
                        .find('=')
                        .map(|idx| value.split_at(idx))
                        .map(|(name, path)| (name, &path[1..]))
                        .with_context(|| {
                            format!(
                                "Failed to parse value of --extern arg as CRATENAME=PATH: {:?}",
                                value
                            )
                        })?;
                    let msg = Color::Green.paint(format!(
                        "Found --extern arg, {:?} --> {:?}",
                        extern_crate_name, crate_rmeta_path
                    ));
                    print!("{}", msg);
                    let crate_rmeta_path = Path::new(crate_rmeta_path);
                    let crate_name_with_hash =
                        get_crate_name_with_hash_from_path(crate_rmeta_path)?
                            .strip_prefix(RMETA_RLIB_FILE_PREFIX)
                            .with_context(|| {
                                format!(
                                    "Found .rlib or .rmeta file in out_dir that \
                                    didn't start with 'lib' prefix: {}",
                                    crate_rmeta_path.display()
                                )
                            })?;
                    let extern_crate =
                        compile_db
                            .get_crate(crate_name_with_hash)
                            .unwrap_or_else(|| {
                                panic!(
                                    "Found no information about crate: {:?}",
                                    crate_name_with_hash
                                )
                            });
                    println!(" ({:?})", extern_crate);

                    args_or_deps_changed |= extern_crate
                        .is_recreated
                        .expect("field `is_recreated` not properly initialized");
                    let candidates =
                        compile_db.find_compatible_crates_in_common_deps(&extern_crate);
                    if candidates.len() > 1 {
                        println!(
                            "WARNING: found multiple candidates: {:?}, using the first one",
                            candidates
                        );
                    }
                    if !candidates.is_empty() {
                        let common_deps_crate = candidates.first().unwrap();
                        let msg = format!(
                            "{} {} with common_deps crate at {} ({:?})",
                            Color::Yellow.paint("#### Replacing crate"),
                            Color::Blue.paint(format!("{:?}", extern_crate_name)),
                            common_deps_crate.path.display(),
                            common_deps_crate,
                        );
                        println!("{}", msg);
                        new_value =
                            format!("{}={}", extern_crate_name, common_deps_crate.path.display())
                                .into();
                        args_or_deps_changed = true;

                        dependencies.push((
                            common_deps_crate.path.display().to_string(),
                            common_deps_crate.available.clone(),
                        ));
                    } else {
                        dependencies.push((
                            extern_crate.path.display().to_string(),
                            extern_crate.available.clone(),
                        ));
                    }
                }
            } else if arg == "-L" {
                let (kind, _path) = value
                    .as_ref()
                    .find('=')
                    .map(|idx| value.split_at(idx))
                    .map(|(kind, path)| (kind, &path[1..])) // ignore the '=' delimiter
                    .with_context(|| {
                        format!("Failed to parse value of -L arg as KIND=PATH: {:?}", value)
                    })?;
                // println!("Found -L arg, {:?} --> {:?}", kind, _path);
                if !(kind == "dependency" || kind == "native") {
                    println!("WARNING: Unsupported -L arg value {:?}. We only support 'dependency=PATH' or 'native=PATH'.", value);
                }
                // TODO: if we need to actually modify any -L argument values, then set `new_value` accordingly here.
            }

            if value != new_value.as_ref() {
                args_or_deps_changed = true;
            }
            recreated_cmd.arg(arg.as_str());
            recreated_cmd.arg(new_value.as_ref());
        }
    }

    if c.name == "phoenix_common" {
        panic!(
            "phoenix_common will be rebuilt, this is usually not an expected behavior. \
            Please check the compile log and tune dependencies if necessary."
        );
    }

    // If any args actually changed, we need to run the re-created command.
    compile_db.mark_recreated(&c, args_or_deps_changed);
    // Add our directory of common_deps crates as a library search path, for dependency resolution.
    // This is okay because we removed all of the potentially conflicting crates from the local target/ directory,
    // which ensures that adding in the directory of common_deps crate .rmeta/.rlib files won't cause rustc to complain
    // about multiple "potentially newer" versions of a given crate.
    // COMMENT(cjr): we didn't really remove all the redundant crates in the newer version.
    recreated_cmd.arg("-L").arg(common_deps_dir);
    // We also need to add the directory of other common dependencies, e.g., proc macro crates and such.
    // recreated_cmd.arg("-L").arg(TBD);

    println!("rustc_env_vars: {}", rustc_env_vars);
    for env in shlex::split(&rustc_env_vars).unwrap() {
        let (k, v) = env
            .split_once('=')
            .unwrap_or_else(|| panic!("env: {}", env));
        recreated_cmd.env(k, v);
    }

    // Suppress warnings for dependency crates
    if !c.is_primary {
        recreated_cmd.arg("-Awarnings");
    }
    // println!("\n\n--------------- Inherited Environment Variables ----------------\n");
    // let _env_cmd = Command::new("env").spawn().unwrap().wait().unwrap();

    if args_or_deps_changed {
        println!(
            "About to execute recreated_cmd that had changed arguments or updated dependencies:\n{:?}",
            recreated_cmd
        );
    } else {
        println!(
            "### Args did not change, running the original_cmd:\n{:?}",
            recreated_cmd /* args did not change */
        );
    }

    Ok(Some(RustcTask {
        recreated_cmd,
        c: c.clone(),
        dependencies,
    }))
}

fn copy_result(c: &Crate) -> Result<()> {
    // copy the library
    let destdir = c.path.parent().unwrap().parent().unwrap();
    let (result_name, is_binary) = if let Some(ext) = c.path.extension() {
        // lib
        (format!("lib{}.{}", c.name, ext.to_string_lossy()), false)
    } else {
        // binary
        (c.name.clone(), true)
    };

    let to = destdir.join(result_name);
    println!("Copy {} to {}", c.path.display(), to.display());
    fs::copy(&c.path, to)?;

    // copy the dep file
    let from = c
        .path
        .with_file_name(format!("{}-{}.d", c.name, c.metadata));
    let to = if !is_binary {
        destdir.join(format!("lib{}.d", c.name))
    } else {
        destdir.join(format!("{}.d", c.name))
    };
    println!("Copy {} to {}", from.display(), to.display());
    fs::copy(from, to)?;

    Ok(())
}

/// Iterates over the contents of the given directory to find crates within it.
///
/// This directory should contain one .rmeta and .rlib file per crate,
/// and those files are named as such:
/// `"lib<crate_name>-<hash>.[rmeta]"`
///
/// This function only looks at the `.rmeta` files in the given directory
/// and extracts from that file name the name of the crate name as a String.
///
/// Returns the set of discovered crates as a map, in which the key is the simple crate name
/// ("my_crate") and the value is the full crate name with the hash included ("my_crate-43462c60d48a531a").
/// The value can be used to define the path to crate's actual .rmeta/.rlib file.
#[allow(unused)]
fn populate_crates_from_dir<P: AsRef<Path>>(dir_path: P) -> io::Result<HashMap<String, String>> {
    let mut crates = HashMap::default();

    // let dir_iter = WalkDir::new(dir_path)
    //     .into_iter()
    //     .filter_map(|res| res.ok());
    let dir_iter = fs::read_dir(dir_path)?
        .into_iter()
        .filter_map(|res| res.ok());

    for entry in dir_iter {
        if !entry.file_type().unwrap().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|p| p.to_str()) == Some(RMETA_FILE_EXTENSION) {
            let filestem = path
                .file_stem()
                .expect("no valid file stem")
                .to_string_lossy();
            if filestem.starts_with("lib") {
                let crate_name_with_hash = &filestem[PREFIX_END..];
                let crate_name_without_hash = crate_name_with_hash.split('-').next().unwrap();
                crates.insert(
                    crate_name_without_hash.to_string(),
                    crate_name_with_hash.to_string(),
                );
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "File {:?} is an .rmeta file that does not begin with 'lib' as expected.",
                        path
                    ),
                ));
            }
        }
    }

    Ok(crates)
}

#[allow(unused)]
fn populate_crates_from_dep_file<P: AsRef<Path>>(
    dep_path: P,
) -> io::Result<HashMap<String, String>> {
    // Parse dependency closure
    let content = fs::read_to_string(dep_path)?;
    let mut all_deps = Vec::new();
    for line in content.lines() {
        // name:[ dep]*
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, _deps)) = line.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Dep file does not have a valid format",
            ));
        };
        dbg!(&name);
        let path = PathBuf::from(name);
        let filestem = path
            .file_stem()
            .expect("no valid file stem")
            .to_string_lossy();
        if filestem.starts_with(RMETA_RLIB_FILE_PREFIX) {
            all_deps.push(name.to_owned());
        }
    }
    // deduplicate
    all_deps.sort();
    all_deps.dedup();
    // filter out .rs, .d, keep .rlib, .so and transform .rmeta to .rlib
    let mut all_deps: Vec<String> = all_deps
        .into_iter()
        .map(|x| {
            x.strip_suffix(".rmeta")
                .map_or(x.clone(), |y| y.to_owned() + ".rlib")
        })
        .collect();
    all_deps.retain(|x| x.ends_with(".rlib") || x.ends_with(".so"));

    let mut crates = HashMap::default();
    for dep in all_deps {
        let path = PathBuf::from(dep);
        if path.extension().and_then(|p| p.to_str()) == Some(RLIB_FILE_EXTENSION) {
            let filestem = path
                .file_stem()
                .expect("no valid file stem")
                .to_string_lossy();
            if filestem.starts_with("lib") {
                let crate_name_with_hash = &filestem[PREFIX_END..];
                let crate_name_without_hash = crate_name_with_hash.split('-').next().unwrap();
                crates.insert(
                    crate_name_without_hash.to_string(),
                    crate_name_with_hash.to_string(),
                );
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "File {:?} is an .rlib file that does not begin with 'lib' as expected.",
                        path
                    ),
                ));
            }
        }
    }

    Ok(crates)
}

/// Parses the given `path` to obtain the part of the filename before the crate name delimiter '-'.
#[allow(unused)]
fn get_plain_crate_name_from_path<'p>(path: &'p Path) -> anyhow::Result<&'p str> {
    path.file_stem()
        .and_then(|os_str| os_str.to_str())
        .with_context(|| {
            format!(
                "Couldn't get file name of file in out_dir: {}",
                path.display()
            )
        })?
        .split('-')
        .next()
        .with_context(|| {
            format!(
                "File in out_dir missing delimiter '-' between crate name and hash. {}",
                path.display()
            )
        })
}

/// Parses the given `path` to obtain the part of the filename with a hash suffix
fn get_crate_name_with_hash_from_path<'p>(path: &'p Path) -> anyhow::Result<&'p str> {
    path.file_stem()
        .and_then(|os_str| os_str.to_str())
        .with_context(|| {
            format!(
                "Couldn't get file name of file in out_dir: {}",
                path.display()
            )
        })?
        .split('.')
        .next()
        .with_context(|| {
            format!(
                "File in out_dir missing delimiter '.' between crate name and suffix. {}",
                path.display()
            )
        })
}

/// Counts the level of verbosity specified by arguments into `cargo`.
fn count_verbose_arg<'i, S: AsRef<str> + 'i, I: IntoIterator<Item = &'i S>>(args: I) -> usize {
    let mut count = 0;
    for arg in args
        .into_iter()
        .flat_map(|a| shlex::split(a.as_ref()).unwrap())
    {
        count += match arg.as_ref() {
            "--verbose" | "-v" => 1,
            "-vv" => 2,
            _ => 0,
        };
    }
    count
}

/// Parse the given verbose rustc command string and return the value of the "--out-dir" argument.
fn get_out_dir_arg(cmd_str: &str) -> anyhow::Result<String> {
    let out_dir_str_start = cmd_str
        .find(" --out-dir")
        .map(|idx| &cmd_str[idx..])
        .context("Captured rustc command did not have an --out-dir argument")?;
    let out_dir_parse = rustc_clap_options("")
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .allow_external_subcommands(true)
        .no_binary_name(true)
        .color(clap::ColorChoice::Never)
        .try_get_matches_from(shlex::split(out_dir_str_start).unwrap());
    let matches =
        out_dir_parse.context("Could not parse --out-dir argument in captured rustc command.")?;
    matches
        .get_one::<String>("--out-dir")
        .cloned()
        .context("--out-dir argument did not have a value")
}

fn remove_redundant_artifacts<P: AsRef<Path>>(
    compile_db: &CompilationDatabase,
    out_dir: P,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(&out_dir)? {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            println!(
                "Found unexpected directory entry in out_dir: {}",
                path.display()
            );
            continue;
        }
        if !entry.file_type().unwrap().is_file() {
            bail!(
                "Found unexpected non-file entry in out_dir: {}",
                path.display()
            );
        }

        // We should remove all potential redundant files, including:
        // * <crate_name>-<hash>.o
        // * lib<crate_name>-<hash>.rmeta
        // * lib<crate_name>-<hash>.rlib
        // * lib<crate_name>-<hash>.so
        //
        // DO NOT remove * <crate_name>-<hash>.d
        //
        // We do not know the exact hash value appended to each crate, we only know the plain crate name.
        // Here, extract the plain crate_name from the file name.
        let crate_name_with_hash = match path.extension().and_then(|os_str| os_str.to_str()) {
            Some("d") | Some("o") => get_crate_name_with_hash_from_path(&path)?,
            Some(RMETA_FILE_EXTENSION) | Some(RLIB_FILE_EXTENSION) | Some(DYLIB_FILE_EXTENSION) => {
                let libcrate_name = get_crate_name_with_hash_from_path(&path)?;
                if libcrate_name.starts_with(RMETA_RLIB_FILE_PREFIX) {
                    &libcrate_name[PREFIX_END..]
                } else {
                    bail!("Found .rlib or .rmeta file in out_dir that didn't start with 'lib' prefix: {}", path.display());
                }
            }
            _ => {
                println!(
                    "Removing potentially-redundant file with unexpected extension: {}",
                    path.display()
                );
                fs::remove_file(&path).with_context(|| {
                    format!(
                        "Failed to remove potentially-redundant file with unexpected extension: {}",
                        path.display()
                    )
                })?;
                continue;
            }
        };

        // See if that crate already exists in our set of common_deps crates.
        if compile_db.get_crate(crate_name_with_hash).map(|c| {
            !compile_db
                .find_compatible_crates_in_common_deps(&c)
                .is_empty()
        }) == Some(true)
        {
            // remove the redundant file
            println!("### Removing redundant crate file {}", path.display());
            fs::remove_file(&path).with_context(|| {
                format!(
                    "Failed to remove redundant crate file in out_dir: {}",
                    path.display(),
                )
            })?;
        } else {
            // Here, do nothing. We must keep the non-redundant files,
            // as they represent new dependencies that were not part of
            // the original in-tree Theseus build.
        }
    }

    Ok(())
}

/// Creates a `Clap::App` instance that handles all (most) of the command-line arguments
/// accepted by the `rustc` executable.
///
/// I obtained this by looking at the output of `rustc --help --verbose`.
fn rustc_clap_options(app_name: &'static str) -> clap::Command {
    clap::Command::new(app_name)
        // The first argument that we want to see, --crate-name.
        .arg(
            clap::Arg::new("--crate-name")
                .long("crate-name")
                .num_args(1),
        )
        // Note: add any other arguments that you encounter in a rustc invocation here.
        .arg(
            clap::Arg::new("-L")
                .short('L')
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("-l")
                .short('l')
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("--extern")
                .long("extern")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("-C")
                .short('C')
                .long("codegen")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("-W")
                .short('W')
                .long("warn")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("-A")
                .short('A')
                .long("allow")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("-D")
                .short('D')
                .long("deny")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("-F")
                .short('F')
                .long("forbid")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("--cap-lints")
                .long("cap-lints")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("-Z")
                .short('Z')
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("--crate-type")
                .long("crate-type")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("--emit")
                .long("emit")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(clap::Arg::new("--edition").long("edition").num_args(1))
        .arg(clap::Arg::new("-g").short('g'))
        .arg(clap::Arg::new("-O").short('O'))
        .arg(clap::Arg::new("--out-dir").long("out-dir").num_args(1))
        .arg(
            clap::Arg::new("--error-format")
                .long("error-format")
                .num_args(1),
        )
        .arg(clap::Arg::new("--json").long("json").num_args(1))
        .arg(clap::Arg::new("--target").long("target").num_args(1))
        .arg(clap::Arg::new("--sysroot").long("sysroot").num_args(1))
        .arg(clap::Arg::new("--edition").long("edition").num_args(1))
        .arg(
            clap::Arg::new("--cfg")
                .long("cfg")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("--check-cfg")
                .long("check-cfg")
                .num_args(1)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("--verbose")
                .short('v')
                .long("verbose")
                .num_args(0)
                .action(clap::ArgAction::Append),
        )
        .arg(
            clap::Arg::new("--remap-path-prefix")
                .long("remap-path-prefix")
                .num_args(0)
                .action(clap::ArgAction::Append),
        )
}
