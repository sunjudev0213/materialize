// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

use std::cell::RefCell;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Utc;
use mz_ore::cli::{self, CliConfig};
use mz_sqllogictest::runner::{self, Outcomes, RunConfig, Runner, WriteFmt};
use mz_sqllogictest::util;
use time::Instant;
use walkdir::WalkDir;

/// Runs sqllogictest scripts to verify database engine correctness.
#[derive(clap::Parser)]
struct Args {
    /// Increase verbosity.
    ///
    /// If specified once, print summary for each source file.
    /// If specified twice, also show descriptions of each error.
    /// If specified thrice, also print each query before it is executed.
    #[clap(short = 'v', long = "verbose", parse(from_occurrences))]
    verbosity: usize,
    /// Don't exit with a failing code if not all queries are successful.
    #[clap(long)]
    no_fail: bool,
    /// Prefix every line of output with the current time.
    #[clap(long)]
    timestamps: bool,
    /// Rewrite expected output based on actual output.
    #[clap(long)]
    rewrite_results: bool,
    /// Generate a JUnit-compatible XML report to the specified file.
    #[clap(long, value_name = "FILE")]
    junit_report: Option<PathBuf>,
    /// PostgreSQL connection URL to use for `persist` consensus and the catalog
    /// stash.
    #[clap(long)]
    postgres_url: String,
    /// Path to sqllogictest script to run.
    #[clap(value_name = "PATH", required = true)]
    paths: Vec<String>,
    /// Stop on first failure.
    #[clap(long)]
    fail_fast: bool,
    /// Inject `CREATE INDEX` after all `CREATE TABLE` statements.
    #[clap(long)]
    auto_index_tables: bool,
    /// Inject `BEGIN` and `COMMIT` to create longer running transactions for faster testing of the
    /// ported SQLite SLT files. Does not work generally, so don't use it for other tests.
    #[clap(long)]
    auto_transactions: bool,
    /// Inject `ALTER SYSTEM SET enable_table_keys = true` before running the SLT file.
    #[clap(long)]
    enable_table_keys: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    mz_ore::panic::set_abort_on_panic();
    mz_ore::test::init_logging_default("warn");

    let args: Args = cli::parse_args(CliConfig::default());

    let config = RunConfig {
        stdout: &OutputStream::new(io::stdout(), args.timestamps),
        stderr: &OutputStream::new(io::stderr(), args.timestamps),
        verbosity: args.verbosity,
        postgres_url: args.postgres_url.clone(),
        no_fail: args.no_fail,
        fail_fast: args.fail_fast,
        auto_index_tables: args.auto_index_tables,
        auto_transactions: args.auto_transactions,
        enable_table_keys: args.enable_table_keys,
    };

    if args.rewrite_results {
        return rewrite(&config, args).await;
    }

    let mut junit = match args.junit_report {
        Some(filename) => match File::create(&filename) {
            Ok(file) => Some((file, junit_report::TestSuite::new("sqllogictest"))),
            Err(err) => {
                writeln!(config.stderr, "creating {}: {}", filename.display(), err);
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    let mut bad_file = false;
    let mut outcomes = Outcomes::default();
    let mut runner = Runner::start(&config).await.unwrap();

    for path in &args.paths {
        for entry in WalkDir::new(path) {
            match entry {
                Ok(entry) if entry.file_type().is_file() => {
                    let start_time = Instant::now();
                    match runner::run_file(&mut runner, entry.path()).await {
                        Ok(o) => {
                            if o.any_failed() || config.verbosity >= 1 {
                                writeln!(
                                    config.stdout,
                                    "{}",
                                    util::indent(&o.display(config.no_fail).to_string(), 4)
                                );
                            }
                            if let Some((_, junit_suite)) = &mut junit {
                                let mut test_case = if o.any_failed() {
                                    junit_report::TestCase::failure(
                                        &entry.path().to_string_lossy(),
                                        start_time.elapsed(),
                                        "failure",
                                        &o.display(false).to_string(),
                                    )
                                } else {
                                    junit_report::TestCase::success(
                                        &entry.path().to_string_lossy(),
                                        start_time.elapsed(),
                                    )
                                };
                                test_case.set_classname("sqllogictest");
                                junit_suite.add_testcase(test_case);
                            }
                            outcomes += o;
                        }
                        Err(err) => {
                            writeln!(
                                config.stderr,
                                "error: running file {}: {}",
                                entry.file_name().to_string_lossy(),
                                err
                            );
                            bad_file = true;
                        }
                    }
                }
                Ok(_) => (),
                Err(err) => {
                    writeln!(config.stderr, "error: reading directory entry: {}", err);
                    bad_file = true;
                }
            }
        }
    }
    if bad_file {
        return ExitCode::FAILURE;
    }

    writeln!(config.stdout, "{}", outcomes.display(config.no_fail));

    if let Some((mut junit_file, junit_suite)) = junit {
        let report = junit_report::ReportBuilder::new()
            .add_testsuite(junit_suite)
            .build();
        match report.write_xml(&mut junit_file) {
            Ok(()) => (),
            Err(err) => {
                writeln!(
                    config.stderr,
                    "error: unable to write junit report: {}",
                    err
                );
                return ExitCode::from(2);
            }
        }
    }

    if outcomes.any_failed() && !args.no_fail {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn rewrite(config: &RunConfig<'_>, args: Args) -> ExitCode {
    if args.junit_report.is_some() {
        writeln!(
            config.stderr,
            "--rewrite-results is not compatible with --junit-report"
        );
        return ExitCode::FAILURE;
    }

    if args.paths.iter().any(|path| path == "-") {
        writeln!(config.stderr, "--rewrite-results cannot be used with stdin");
        return ExitCode::FAILURE;
    }

    let mut bad_file = false;
    let mut runner = Runner::start(config).await.unwrap();

    for path in args.paths {
        for entry in WalkDir::new(path) {
            match entry {
                Ok(entry) => {
                    if entry.file_type().is_file() {
                        if let Err(err) = runner::rewrite_file(&mut runner, entry.path()).await {
                            writeln!(config.stderr, "error: rewriting file: {}", err);
                            bad_file = true;
                        }
                    }
                }
                Err(err) => {
                    writeln!(config.stderr, "error: reading directory entry: {}", err);
                    bad_file = true;
                }
            }
        }
    }
    if bad_file {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

struct OutputStream<W> {
    inner: RefCell<W>,
    need_timestamp: RefCell<bool>,
    timestamps: bool,
}

impl<W> OutputStream<W>
where
    W: Write,
{
    fn new(inner: W, timestamps: bool) -> OutputStream<W> {
        OutputStream {
            inner: RefCell::new(inner),
            need_timestamp: RefCell::new(true),
            timestamps,
        }
    }

    fn emit_str(&self, s: &str) {
        self.inner.borrow_mut().write_all(s.as_bytes()).unwrap();
    }
}

impl<W> WriteFmt for OutputStream<W>
where
    W: Write,
{
    fn write_fmt(&self, fmt: fmt::Arguments<'_>) {
        let s = format!("{}", fmt);
        if self.timestamps {
            // We need to prefix every line in `s` with the current timestamp.

            let timestamp = Utc::now();
            let timestamp_str = timestamp.format("%Y-%m-%d %H:%M:%S.%f %Z");

            // If the last character we outputted was a newline, then output a
            // timestamp prefix at the start of this line.
            if self.need_timestamp.replace(false) {
                self.emit_str(&format!("[{}] ", timestamp_str));
            }

            // Emit `s`, installing a timestamp at the start of every line
            // except the last.
            let (s, last_was_timestamp) = match s.strip_suffix('\n') {
                None => (&*s, false),
                Some(s) => (s, true),
            };
            self.emit_str(&s.replace('\n', &format!("\n[{}] ", timestamp_str)));

            // If the line ended with a newline, output the newline but *not*
            // the timestamp prefix. We want the timestamp to reflect the moment
            // the *next* character is output. So instead we just remember that
            // the last character we output was a newline.
            if last_was_timestamp {
                *self.need_timestamp.borrow_mut() = true;
                self.emit_str("\n");
            }
        } else {
            self.emit_str(&s)
        }
    }
}
