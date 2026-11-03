use std::process::{Command, ExitStatus, Output};

#[cfg(not(target_os = "windows"))]
use std::os::unix::process::ExitStatusExt;

#[cfg(target_os = "windows")]
use std::os::windows::process::ExitStatusExt;

fn main() {
	println!("cargo:rerun-if-changed=src/");
	let output = String::from_utf8(
		Command::new("git")
			.args(["rev-parse", "HEAD"])
			.output()
			.unwrap_or(Output {
				stdout: vec![],
				stderr: vec![],
				status: ExitStatus::from_raw(0),
			})
			.stdout,
	)
	.unwrap_or_default();
	// `git rev-parse` prints a trailing newline. The atom feed's commit id does
	// not, so the stored hash has to be the bare sha or the update check never matches.
	let git_hash = output.trim();
	let git_hash = if git_hash.is_empty() { "dev" } else { git_hash };
	println!("cargo:rustc-env=GIT_HASH={git_hash}");
}
