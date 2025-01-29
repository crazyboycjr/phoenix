#!/usr/bin/env bash
#![allow(unused_attributes)] /*
OUT=/tmp/rustc_wrapper && rustc "$0" -o ${OUT} && exec ${OUT} $@ || exit $? #*/

use std::process::{Command, exit};
use std::fs;
use std::env;

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
            if name == "build-script-build" {
                return true;
            }
        }

        current_pid = ppid;
    }

    false
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        eprintln!("Usage: <program> <args...>");
        exit(1);
    }

    if is_parent_build_script() {
        // Execute the command with arguments
        let mut command = Command::new(&args[0]);
        command.args(&args[1..]);

        match command.spawn() {
            Ok(mut child) => {
                let status = child.wait().expect("Failed to wait on child process");
                exit(status.code().unwrap_or(1)); // Exit with child's status code
            }
            Err(e) => {
                eprintln!("Failed to execute command: {}", e);
                exit(1);
            }
        }
    } else {
        // Print the arguments if not inside `build-script-build`
        println!("{}", args.join(" "));
    }
}