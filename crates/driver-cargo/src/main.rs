//! Standard-input standard-output server of the reference project driver.
//!
//! The driver speaks one JSON object per line: it decodes a
//! [`ProjectRequest`] from stdin, executes the phase, and writes the matching
//! [`ProjectResponse`] to stdout before reading the next request. Correlating a
//! response with its request needs no identifier, because the response repeats
//! the request's `type` tag.
//!
//! The repository root is the first command-line argument when present, and the
//! process working directory otherwise.
//!
//! A request stream that cannot be decoded is a protocol defect, not a phase
//! failure: the driver reports it on stderr and exits with
//! [`PROTOCOL_FAILURE`], so the caller fails fast instead of waiting for a
//! response that will never come.

#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use tauqe_driver_cargo::runner::ProjectRunner;
use tauqe_protocol::ProjectRequest;

/// Exit code reported when the request stream cannot be decoded.
const PROTOCOL_FAILURE: u8 = 2;

fn main() -> ExitCode {
    let repo_root = match repository_root() {
        Ok(repo_root) => repo_root,
        Err(err) => {
            eprintln!("tauqe-driver-cargo: {err}");
            return ExitCode::from(PROTOCOL_FAILURE);
        }
    };

    let runner = ProjectRunner::load(repo_root);
    serve(&runner)
}

/// Resolves the repository root the driver executes against.
fn repository_root() -> Result<PathBuf, String> {
    if let Some(argument) = std::env::args_os().nth(1) {
        return Ok(PathBuf::from(argument));
    }
    std::env::current_dir().map_err(|err| format!("cannot resolve the repository root: {err}"))
}

/// Serves requests until the caller closes the stream.
fn serve(runner: &ProjectRunner) -> ExitCode {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut output = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(_) => {}
            Err(err) => {
                eprintln!("tauqe-driver-cargo: cannot read the request stream: {err}");
                return ExitCode::from(PROTOCOL_FAILURE);
            }
        }

        if line.trim().is_empty() {
            continue;
        }

        let request = match serde_json::from_str::<ProjectRequest>(&line) {
            Ok(request) => request,
            Err(err) => {
                eprintln!("tauqe-driver-cargo: cannot decode a request: {err}");
                return ExitCode::from(PROTOCOL_FAILURE);
            }
        };

        let response = runner.handle(&request);
        let encoded = match serde_json::to_string(&response) {
            Ok(encoded) => encoded,
            Err(err) => {
                eprintln!("tauqe-driver-cargo: cannot encode a response: {err}");
                return ExitCode::from(PROTOCOL_FAILURE);
            }
        };

        let written = writeln!(output, "{encoded}").and_then(|()| output.flush());
        if written.is_err() {
            // The caller closed the pipe: the protocol ends here.
            return ExitCode::SUCCESS;
        }
    }
}
