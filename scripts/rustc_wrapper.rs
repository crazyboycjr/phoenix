#!/usr/bin/env bash
#[rustfmt::skip]
#![allow(unused_attributes)] /*
set -euo pipefail
SRC="$0"
OUT=/tmp/rustc_wrapper
[[ ! -e "$OUT" || "$SRC" -nt "$OUT" ]] && rustc "${SRC}" -o ${OUT}
${OUT} "$@"
exit $? # */

use std::collections::HashMap;
use std::env;
use std::fs;
use std::process::{exit, Command};

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
            // println!("parent_process_name: {}", name);
            if name == "build-script-build" {
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

fn is_building_build_script(args: &[String]) -> bool {
    args.windows(2)
        .any(|pair| pair == ["--crate-name", "build_script_build"])
        || std::env::var("CARGO_CRATE_NAME")
            .map(|value| value == "build_script_build")
            .unwrap_or_default()
}

/// Executes `rustc` with the provided arguments.
fn exec_rustc(args: &[String]) -> ! {
    let mut command = Command::new(args[0].clone());
    command.args(&args[1..]);

    let status = command.status().expect("Failed to execute rustc");
    exit(status.code().unwrap_or(1));
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        eprintln!("Usage: <program> <args...>");
        exit(1);
    }

    // 🔥 Always execute `rustc` if `-vV` is present
    if args.iter().any(|arg| arg == "-vV") {
        exec_rustc(&args);
    }
    if args.iter().any(|arg| arg.contains("--print=")) {
        exec_rustc(&args);
    }
    if is_building_build_script(&args) {
        exec_rustc(&args)
    }

    if is_parent_build_script() {
        // Inside `build-script-build`, faithfully execute rustc
        exec_rustc(&args);
    } else {
        // Outside `build-script-build`, print env + args instead of compiling
        let ppid = std::os::unix::process::parent_id();
        let self_env = get_environ_map("self");
        let parent_env = get_environ_map(&ppid.to_string());
        let parent_process_name = get_process_name(ppid);
        let pppid = get_parent_pid(ppid).unwrap();
        let parent_parent_env = get_environ_map(&pppid.to_string());
        let parent_parent_process_name = get_process_name(pppid);

        // Only keep added envs
        let self_envs: Vec<String> = self_env
            .iter()
            .map(|(k, v)| format!("{}='{}'", k, v))
            .collect();
        let parent_parent_envs: Vec<String> = parent_parent_env
            .iter()
            .map(|(k, v)| format!("{}='{}'", k, v))
            .collect();
        let explicit_envs: Vec<String> = self_env
            .iter()
            .filter(|(key, value)| parent_parent_env.get(*key) != Some(value))
            .map(|(key, value)| format!("{}='{}'", key, value))
            .collect();
        // println!("{} {}", explicit_envs.join(" "), args.join(" "));
        use std::io::Write;
        let mut f = std::fs::File::options()
            .create(true)
            .append(true)
            .open("/tmp/rustc_wrapper.log")
            .unwrap();
        // writeln!(f, "parent_parent_process_name: {}", parent_parent_process_name.unwrap());
        writeln!(f, "{} {}", explicit_envs.join(" "), args.join(" "));
    }
}
