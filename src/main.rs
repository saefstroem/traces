#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, dead_code, unused_must_use)]

mod certs;
mod config;
mod error;
mod prefix;
mod providers;
mod proxy;
mod resolver;
mod result;
mod run;
mod service;
mod settings;
mod store;
mod upload;

use std::process::ExitCode;

fn main() -> ExitCode {
    run::run()
}
