use std::collections::{BTreeSet, HashMap};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{BufReader, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

use ansi_term::Color;
use anyhow::{bail, Context, Result};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

/// Reads the parent process ID (PPID) of a given PID by parsing `/proc/{pid}/stat`
fn get_parent_pid(pid: u32) -> Option<u32> {
    let stat_path = format!("/proc/{}/stat", pid);
    if let Ok(stat) = fs::read_to_string(stat_path) {
        let parts: Vec<&str> = stat.split_whitespace().collect();
        if parts.len() > 3 {
            return parts[3].parse::<u32>().ok();
        }
    }
    None
}

/// Reads the process name from `/proc/{pid}/comm`
fn get_process_name(pid: u32) -> Option<String> {
    let comm_path = format!("/proc/{}/comm", pid);
    fs::read_to_string(comm_path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Checks if any parent process in the hierarchy is `build-script-build`
fn is_parent_build_script() -> bool {
    let mut current_pid = std::process::id();

    while let Some(ppid) = get_parent_pid(current_pid) {
        if ppid == 1 {
            break; // Reached init/systemd, stop checking
        }

        if let Some(name) = get_process_name(ppid) {
            // eprintln!("parent_process_name: {}", name);
            if name == "build-script-build"[..15] || name.starts_with(&"build_script_build-"[..15]) {
            // if name == "build-script-build" || name.starts_with(&"build_script_build-") {
                return true;
            }
        }

        current_pid = ppid;
    }

    false
}

fn get_environ_map(pid: &str) -> HashMap<String, String> {
    let path = format!("/proc/{}/environ", pid);
    if let Ok(content) = fs::read_to_string(path) {
        return content
            .split('\0') // `/proc/self/environ` is seperated with `\0`
            .filter(|s| !s.is_empty())
            .map(|entry| {
                let mut parts = entry.splitn(2, '=');
                (
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                )
            })
            .collect();
    }
    HashMap::new()
}

fn get_parent_cargo_pid() -> Result<u32> {
    let mut ppid = std::os::unix::process::parent_id();
    while ppid != 1 {
        let process_name = get_process_name(ppid).unwrap_or_else(|| "unnamed".to_owned());
        if process_name == "cargo" {
            return Ok(ppid);
        }
        ppid = get_parent_pid(ppid)
            .with_context(|| format!("Unable to get parent pid for {ppid} {process_name}"))?;
    }
    bail!("Unable did not find cargo in parent process")
}

#[allow(unused)]
fn is_building_build_script(args: &[String]) -> bool {
    args.windows(2)
        .any(|pair| pair == ["--crate-name", BUILD_SCRIPT_CRATE_NAME])
        || std::env::var("CARGO_CRATE_NAME")
            .map(|value| value == BUILD_SCRIPT_CRATE_NAME)
            .unwrap_or_default()
}

/// Executes `rustc` with the provided arguments.
fn exec_rustc(args: &[String]) -> ! {
    let mut command = Command::new(args[0].clone());
    command.args(&args[1..]);

    let status = command.status().expect("Failed to execute rustc");
    exit(status.into_raw());
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        eprintln!("Usage: <program> <args...>");
        exit(1);
    }

    // 🔥 Always execute `rustc` if `-vV` is present
    if args.iter().any(|arg| arg == "-vV" || arg == "--version") {
        exec_rustc(&args);
    }
    if args.iter().any(|arg| arg.contains("--print=")) {
        exec_rustc(&args);
    }
    if args.iter().any(|arg| arg.contains("--check-cfg=")) {
        exec_rustc(&args);
    }
    // if is_parent_build_script() {
    //     // rustc is executed within `build-script-build`, so faithfully execute rustc
    //     exec_rustc(&args);
    // }
    // crate 'build_script_build' can't be simply bypassed. The dependency of this crate are dev-dependencies.
    // We couldn't execute rustc as is. This is because a dev-dependency could be a normal
    // dependency at the same time, and we wouldn't know it in rustc_wrapper.
    // So we have to check the
    // --extern args and replace those appeared in common_deps. Otherwise, the crate
    // it is looking for might be missing. The desired crate might appear as a crate
    // with a different metadata suffix in common_deps.
    // if is_building_build_script(&args) {
    //     exec_rustc(&args)
    // }

    // Outside `build-script-build`
    let ppid = get_parent_cargo_pid()?;
    let parent_cargo_env = get_environ_map(&ppid.to_string());
    let self_env = get_environ_map("self");

    // Only keep added envs
    let _self_envs: Vec<String> = self_env
        .iter()
        .map(|(k, v)| format!("{}='{}'", k, v))
        .collect();
    let explicit_envs: Vec<String> = self_env
        .iter()
        .filter(|(key, value)| parent_cargo_env.get(*key) != Some(value))
        .map(|(key, value)| format!("{}={}", key, shlex::try_quote(&value).unwrap()))
        .collect();
    let reconstructed_cmd = format!(
        "{} {}",
        explicit_envs.join(" "),
        shlex::try_join(args.iter().map(|s| s.as_ref())).unwrap()
    );

    let out_dir = PathBuf::from(get_out_dir_arg(&args.join(" "))?);

    let common_deps_dir = PathBuf::from(self_env.get("PHOENIX_COMMON_DEPS_DIR").unwrap());
    let is_common_dep = self_env
        .get("PHOENIX_IS_COMMON_DEP")
        .map(|v| v == "1" || v.to_ascii_lowercase() == "true")
        .unwrap_or_default();
    if is_common_dep {
        // run original rustc command for common deps, as they don't replace --extern args

        let mut c = get_crate_from_rustc_command(&reconstructed_cmd)?.unwrap();

        let old_path = c.path;
        c.path = common_deps_dir.join(old_path.file_name().context("Could not get file_name")?);
        // put crate info onto the filesystem before running rustc
        put_crate(&c)?;

        let mut command = Command::new(args[0].clone());
        command.args(&args[1..]);

        let status = command.status().expect("Failed to execute rustc");
        c.path = old_path;
        copy_to_deps(&c, &common_deps_dir)?;
        exit(status.into_raw());
    }

    let common_deps = CommonDeps::new(&common_deps_dir)?;
    let mut compile_db = CompilationDatabase::new(common_deps_dir, out_dir, common_deps);

    let rustc_task = generate_rustc_task(&reconstructed_cmd, &mut compile_db)?;

    if let Some(task) = rustc_task {
        println!("task.recreated_cmd: {:?}", task.recreated_cmd);
        exec_rustc_task(task)?;
    }

    Ok(())
}

fn copy_to_deps(c: &Crate, destdir: &Path) -> Result<()> {
    // copy the library
    // let destdir = c.path.parent().unwrap().parent().unwrap();
    let from = c.path.clone();
    let to = destdir.join(c.path.file_name().unwrap());
    println!("Copy {} to {}", from.display(), to.display());
    fs::copy(from, to)?;

    // copy the dep file
    let dep = format!("{}-{}.d", c.name, c.metadata);
    let from = c.path.with_file_name(&dep);
    let to = destdir.join(&dep);
    println!("Copy {} to {}", from.display(), to.display());
    fs::copy(from, to)?;

    Ok(())
}

fn copy_result(c: &Crate) -> Result<()> {
    // copy the library
    let destdir = c.path.parent().unwrap().parent().unwrap();
    let destdir = destdir.parent().unwrap().join("artifact");
    let _ = fs::create_dir_all(&destdir);
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

struct RustcTask {
    recreated_cmd: Command,
    c: Crate,
}

fn exec_rustc_task(mut task: RustcTask) -> Result<()> {
    // Err(task.recreated_cmd.exec().into())
    let mut rustc_process = task.recreated_cmd.spawn().expect("Failed to execute rustc");
    let exit_status = rustc_process.wait().expect("Error running rustc");

    if let Some(0) = exit_status.code() {
        println!(
            "{} {}: Ran rustc command (modified for Phoenix) successfully.",
            task.c.name, task.c.pkg_version
        );

        // Copy the compilation result to the parent directory of deps, just like what cargo
        // would do.
        if task.c.is_primary {
            copy_result(&task.c).unwrap();
        }
    }
    exit(exit_status.into_raw());
}

fn put_crate(c: &Crate) -> Result<()> {
    let buf = serde_json::to_string(&c)?;
    let crate_key = format!("{}-{}.json", c.name, c.metadata);
    let deps_dir = PathBuf::from(c.path.parent().expect("path is /"));
    let path = deps_dir.join(crate_key);
    let mut file = fs::File::create(&path)?;
    file.write_all(buf.as_bytes())?;
    Ok(())
}

#[derive(thiserror::Error, Debug)]
enum GetCrateError {
    #[error("File not found")]
    NotFound,
    #[error("Failed to open {0}, {1}")]
    Open(PathBuf, std::io::Error),
    #[error("File {0} found but has invalid format {0}")]
    SerdeJson(PathBuf, serde_json::Error),
}

fn get_crate<P: AsRef<Path>>(path: P) -> Result<Crate, GetCrateError> {
    match fs::exists(&path) {
        Ok(true) => {
            let crate_info_file = fs::File::open(&path)
                .map_err(|e| GetCrateError::Open(path.as_ref().to_path_buf(), e))?;

            let reader = BufReader::new(crate_info_file);
            let c = serde_json::from_reader(reader)
                .map_err(|e| GetCrateError::SerdeJson(path.as_ref().to_path_buf(), e))?;
            Ok(c)
        }
        _ => Err(GetCrateError::NotFound),
    }
}

/// Parse the given verbose rustc command string and return the value of the "--out-dir" argument.
fn get_out_dir_arg(cmd_str: &str) -> Result<String> {
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
}

impl Crate {
    fn crate_name_with_hash(&self) -> String {
        format!("{}-{}", self.name, self.metadata)
    }
}

/// Returns true if the given `arg` should be ignored in our rustc invocation.
#[allow(unused)]
fn ignore_arg(arg: &str) -> bool {
    arg == "--error-format" || arg == "--json" || arg == "--diagnostic-width"
}

// The commands we care about capturing starting with "Running `" and end with "`".
const RUSTC_CMD_START: &str = "rustc --crate-name";
const BUILD_SCRIPT_CRATE_NAME: &str = "build_script_build";

const CARGO_PKG_VERSION: &str = "CARGO_PKG_VERSION";
const CARGO_PRIMARY_PKG: &str = "CARGO_PRIMARY_PACKAGE=1";

// The format of rmeta/rlib file names.
const RMETA_RLIB_FILE_PREFIX: &str = "lib";
const RMETA_FILE_EXTENSION: &str = "rmeta";
const RLIB_FILE_EXTENSION: &str = "rlib";
const DYLIB_FILE_EXTENSION: &str = "so";

fn parse_rustc_command(command: &str) -> Result<Option<(String, &str, clap::ArgMatches)>> {
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

    // println!("rustc_env_vars: {rustc_env_vars}");
    let mut vars =
        shlex::split(rustc_env_vars).unwrap_or_else(|| panic!("rustc_env_vars: {rustc_env_vars}"));
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

/// Parses the given `path` to obtain the part of the filename with a hash suffix
fn get_crate_name_with_hash_from_path(path: &Path) -> Result<&'_ str> {
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
    let (_key, mut pkg_version) = splitted
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
    pkg_version = pkg_version.trim_matches('\'');

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
            // dbg!(value);
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
        pkg_version: Version::parse(pkg_version)
            .with_context(|| format!("pkg_version: {}", pkg_version))?,
        features,
        path,
        dependencies,
        is_primary,
    }))
}

/// It maps a crate-name to a list of candidate crates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CommonDeps(HashMap<String, Vec<Crate>>);

impl CommonDeps {
    fn new(common_deps_dir: &Path) -> Result<Self> {
        let mut crates = HashMap::default();
        // This code below will silently skip directories that the owner of the running process
        // does not have permission to access.
        for entry in walkdir::WalkDir::new(common_deps_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.extension() == Some(std::ffi::OsStr::new("json")) {
                if let Some((name, metadata)) = path
                    .file_stem()
                    .and_then(|s| s.to_str().unwrap().rsplit_once('-'))
                {
                    let crate_info_file = fs::File::open(&path)?;
                    let reader = BufReader::new(crate_info_file);
                    let c: Crate = serde_json::from_reader(reader)?;

                    // perform some checks
                    if c.name != name || c.metadata != metadata {
                        bail!(
                            "crate name ({}) or metadata ({}) doesn't match: crate: {:?}",
                            name,
                            metadata,
                            c
                        );
                    }

                    if c.path.parent().unwrap() != common_deps_dir {
                        bail!(
                            "crate ({}) info doesn't match common_deps_dir: {} vs {}",
                            path.display(),
                            c.path.parent().unwrap().display(),
                            common_deps_dir.display()
                        )
                    }

                    crates
                        .entry(c.name.to_owned())
                        .or_insert_with(Vec::new)
                        .push(c);
                }
            }
        }
        Ok(CommonDeps(crates))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CompilationDatabase {
    /// The directory that contains the common_deps crates for `phoenix_common`.
    common_deps_dir: PathBuf,

    /// The path specified by --out-dir, usually target/[profile]/deps
    out_dir: PathBuf,

    /// A list of search paths specified -L
    search_paths: Vec<PathBuf>,

    /// The dependency closure for `phoenix_common`. Crates in this set are common_deps and will be
    /// used to inject into as dependencies of plugins (if they can replace any compatible
    /// dependent crate of a plugin).
    common_deps: CommonDeps,
}

impl CompilationDatabase {
    fn new(common_deps_dir: PathBuf, out_dir: PathBuf, common_deps: CommonDeps) -> Self {
        Self {
            common_deps_dir,
            out_dir,
            search_paths: Vec::new(),
            common_deps,
        }
    }

    fn add_search_path<P: AsRef<Path>>(&mut self, search_path: P) {
        self.search_paths.push(search_path.as_ref().to_path_buf());
    }

    fn put_crate(&self, c: &Crate) -> Result<()> {
        put_crate(c)
    }

    fn get_crate_from_common_deps(&self, crate_name_with_hash: &str) -> Option<Crate> {
        let fname = format!("{}.json", crate_name_with_hash);
        let path = self.common_deps_dir.join(&fname);
        get_crate(path).ok()
    }

    fn get_crate_from_search_paths<P: AsRef<Path>>(
        &self,
        crate_name_with_hash: &str,
        search_paths: &[P],
    ) -> Option<Crate> {
        if let Some(c) = self.get_crate_from_common_deps(crate_name_with_hash) {
            return Some(c);
        }
        for dir in search_paths {
            let fname = format!("{}.json", crate_name_with_hash);
            let path = dir.as_ref().join(&fname);
            if let Some(c) = get_crate(path).ok() {
                return Some(c);
            }
        }
        None
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
                self.get_crate_from_search_paths(&dep, &self.search_paths)
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
                if cands.is_empty() {
                    println!("desired crate: {:?}", c);
                }
                cands
            })
            .unwrap_or_default()
    }
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
fn generate_rustc_task(
    original_cmd: &str,
    compile_db: &mut CompilationDatabase,
) -> Result<Option<RustcTask>> {
    let common_deps_dir = compile_db.common_deps_dir.clone();

    let Some(c) = get_crate_from_rustc_command(original_cmd)? else {
        // skip invocations of build scripts
        unreachable!("invocation of build scripts, this shouldn't be reachable");
    };

    // This is necessary before proceeding
    compile_db.put_crate(&c)?;

    let crate_name_with_hash = c.crate_name_with_hash();

    println!("\n\nLooking at original command:\n{}", original_cmd);
    let Some((rustc_env_vars, _command_without_env, top_level_matches)) =
        parse_rustc_command(original_cmd)?
    else {
        // skip invocations of build scripts
        unreachable!("invocation of build scripts, this shouldn't be reachable");
    };
    println!("common_deps_dir: {}", common_deps_dir.display());

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
        .no_binary_name(true)
        .color(clap::ColorChoice::Never)
        .try_get_matches_from(args_after_source_file);

    let matches =
        matches.context("Missing support for argument found in captured rustc command")?;

    let search_paths = matches
        .get_raw("-L")
        .unwrap()
        .filter_map(|s| {
            s.to_os_string()
                .to_string_lossy()
                .strip_prefix("dependency=")
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    println!("command has rust search paths: {:?}", search_paths);
    for p in &search_paths {
        compile_db.add_search_path(p);
    }

    // Skip crates that are included in common_deps
    if c.name != BUILD_SCRIPT_CRATE_NAME
        && !compile_db
            .find_compatible_crates_in_common_deps(&c)
            .is_empty()
    {
        println!(
            "\n### Skipping already-built crate {:?}",
            crate_name_with_hash
        );
        return Ok(None);
    }

    if c.name == "phoenix_common" {
        panic!(
            "phoenix_common will be rebuilt, this is usually not an expected behavior. \
            Please check the compile log and tune dependencies if necessary."
        );
    }

    // After adding the initial stuff: rustc command, crate name, (optional --edition), and crate source file,
    // the other arguments are added in the loop below.
    for arg in matches.ids() {
        let values = matches
            .get_raw(arg.as_str())
            .unwrap()
            .map(|s| s.to_os_string())
            .collect::<Vec<_>>();
        println!("Arg {:?} has values:\n\t {:?}", arg, values);

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
                    let extern_crate = compile_db
                        .get_crate_from_search_paths(crate_name_with_hash, &search_paths)
                        .unwrap_or_else(|| {
                            panic!(
                                "Found no information about crate: {:?} in {} or {:?}",
                                crate_name_with_hash,
                                &compile_db.common_deps_dir.display(),
                                &search_paths,
                            )
                        });
                    println!(" ({:?})", extern_crate);

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
                    }
                }
            }

            recreated_cmd.arg(arg.as_str());
            recreated_cmd.arg(new_value.as_ref());
        }
    }

    // Add our directory of common_deps crates as a library search path, for dependency resolution.
    recreated_cmd.arg("-L").arg(common_deps_dir);
    // We also need to add the directory of other common dependencies, e.g., proc macro crates and such.
    // recreated_cmd.arg("-L").arg(TBD);

    // println!("debugging rustc_env_vars: {}", rustc_env_vars);
    for env in shlex::split(&rustc_env_vars).unwrap() {
        let (k, v) = env
            .split_once('=')
            .unwrap_or_else(|| panic!("env: {}", env));
        recreated_cmd.env(k, v);
    }

    // Suppress warnings for dependency crates (non-primary packages)
    // cargo will suppress for us if the return error-format is in json
    // otherwise, we have to append this args.
    // if !c.is_primary {
    // No need to suppress warnings because cargo will do it for us.
    // recreated_cmd.arg("-Awarnings");
    // }

    // println!("\n\n--------------- Inherited Environment Variables ----------------\n");
    // let _env_cmd = Command::new("env").spawn().unwrap().wait().unwrap();

    Ok(Some(RustcTask {
        recreated_cmd,
        c: c.clone(),
    }))
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
        .arg(
            clap::Arg::new("--diagnostic-width")
                .long("diagnostic-width")
                .num_args(1),
        )
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
