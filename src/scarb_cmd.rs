use crate::tracking;
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::process::Command;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub enum ScarbCommand {
    Build,
    Test,
    Check,
}

pub fn run(cmd: ScarbCommand, args: &[String], verbose: u8) -> Result<()> {
    match cmd {
        ScarbCommand::Build => run_build(args, verbose),
        ScarbCommand::Test => run_test(args, verbose),
        ScarbCommand::Check => run_check(args, verbose),
    }
}

/// Generic scarb command runner with filtering
fn run_scarb_filtered<F>(subcommand: &str, args: &[String], verbose: u8, filter_fn: F) -> Result<()>
where
    F: Fn(&str) -> String,
{
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("scarb");
    cmd.arg(subcommand);
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: scarb {} {}", subcommand, args.join(" "));
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run scarb {}", subcommand))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_fn(&raw);

    if let Some(hint) = crate::tee::tee_and_hint(&raw, &format!("scarb_{}", subcommand), exit_code)
    {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("scarb {} {}", subcommand, args.join(" ")),
        &format!("rtk scarb {} {}", subcommand, args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn run_build(args: &[String], verbose: u8) -> Result<()> {
    run_scarb_filtered("build", args, verbose, filter_scarb_build)
}

fn run_test(args: &[String], verbose: u8) -> Result<()> {
    run_scarb_filtered("test", args, verbose, filter_scarb_test)
}

fn run_check(args: &[String], verbose: u8) -> Result<()> {
    run_scarb_filtered("check", args, verbose, filter_scarb_build)
}

/// Passthrough for unknown scarb subcommands
pub fn run_passthrough(args: &[OsString], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("scarb passthrough: {:?}", args);
    }
    let status = Command::new("scarb")
        .args(args)
        .status()
        .context("Failed to run scarb")?;

    let args_str = tracking::args_display(args);
    timer.track_passthrough(
        &format!("scarb {}", args_str),
        &format!("rtk scarb {} (passthrough)", args_str),
    );

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Known noise warning prefixes produced by scarb infrastructure (not user code)
fn is_noise_warn(line: &str) -> bool {
    line.starts_with("warn: `scarb cairo-test` is deprecated")
        || line.starts_with("warn: `cairo_test` plugin not found")
        || line.starts_with("warn: artefacts produced by this build may be hard to utilize")
}

/// Filter scarb build/check output
/// - Strips Compiling/Checking/noise lines
/// - Keeps user code warnings (warn[EXXX]: or warn: followed by --> pointer)
/// - Keeps error blocks
/// - Summary: "✓ scarb build (N packages compiled)" or error count
pub fn filter_scarb_build(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let n = lines.len();

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut error_count = 0;
    let mut warning_count = 0;
    let mut compiled = 0;
    let mut in_diagnostic = false;
    let mut current_block: Vec<String> = Vec::new();
    let mut current_is_error = false;
    let mut finished_line = String::new();

    let mut i = 0;
    while i < n {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Skip "Compiling" / "Checking" lines (count packages)
        if trimmed.starts_with("Compiling") || trimmed.starts_with("Checking") {
            compiled += 1;
            i += 1;
            continue;
        }

        // Skip lock/download noise
        if trimmed.starts_with("Blocking waiting for file lock")
            || trimmed.starts_with("Downloading")
            || trimmed.starts_with("Downloaded")
            || trimmed.starts_with("Locking")
        {
            i += 1;
            continue;
        }

        // Capture and skip final Finished line
        if trimmed.starts_with("Finished") {
            finished_line = trimmed.to_string();
            i += 1;
            continue;
        }

        // Known noise warn blocks - skip until blank or next top-level token
        if is_noise_warn(line) {
            i += 1;
            while i < n {
                let next = lines[i].trim();
                if next.is_empty()
                    || lines[i].starts_with("warn")
                    || lines[i].starts_with("error")
                    || lines[i].trim_start().starts_with("Compiling")
                    || lines[i].trim_start().starts_with("Checking")
                    || lines[i].trim_start().starts_with("Finished")
                {
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Detect error blocks: error[EXXX]: or error:
        if line.starts_with("error[") || line.starts_with("error:") {
            // Skip "aborting due to" / "could not compile" noise
            if line.contains("aborting due to") || line.contains("could not compile") {
                i += 1;
                continue;
            }
            // Flush previous block
            if in_diagnostic && !current_block.is_empty() {
                if current_is_error {
                    errors.push(current_block.join("\n"));
                } else {
                    warnings.push(current_block.join("\n"));
                }
                current_block.clear();
            }
            error_count += 1;
            in_diagnostic = true;
            current_is_error = true;
            current_block.push(line.to_string());
            i += 1;
            continue;
        }

        // Detect user code warning with explicit code: warn[EXXX]:
        if line.starts_with("warn[") {
            if in_diagnostic && !current_block.is_empty() {
                if current_is_error {
                    errors.push(current_block.join("\n"));
                } else {
                    warnings.push(current_block.join("\n"));
                }
                current_block.clear();
            }
            warning_count += 1;
            in_diagnostic = true;
            current_is_error = false;
            current_block.push(line.to_string());
            i += 1;
            continue;
        }

        // Detect user code warning without code: "warn: <message>"
        // Keep only if the next meaningful line is a --> pointer (user code location)
        if line.starts_with("warn: ") {
            // Look ahead for --> pointer
            let next_meaningful = lines[i + 1..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .copied()
                .unwrap_or("");
            if next_meaningful.trim_start().starts_with("-->") {
                // User code warning - collect block
                if in_diagnostic && !current_block.is_empty() {
                    if current_is_error {
                        errors.push(current_block.join("\n"));
                    } else {
                        warnings.push(current_block.join("\n"));
                    }
                    current_block.clear();
                }
                warning_count += 1;
                in_diagnostic = true;
                current_is_error = false;
                current_block.push(line.to_string());
            } else {
                // Noise warning (no file pointer) - flush and skip
                if in_diagnostic && !current_block.is_empty() {
                    if current_is_error {
                        errors.push(current_block.join("\n"));
                    } else {
                        warnings.push(current_block.join("\n"));
                    }
                    current_block.clear();
                    in_diagnostic = false;
                }
            }
            i += 1;
            continue;
        }

        // Skip summary warning lines: "warning: <package> generated N warnings"
        if line.starts_with("warning:") && line.contains("generated") && line.contains("warning") {
            i += 1;
            continue;
        }

        // Collect continuation lines inside a diagnostic block
        if in_diagnostic {
            if line.trim().is_empty() && current_block.len() > 3 {
                // End of block
                if current_is_error {
                    errors.push(current_block.join("\n"));
                } else {
                    warnings.push(current_block.join("\n"));
                }
                current_block.clear();
                in_diagnostic = false;
            } else {
                current_block.push(line.to_string());
            }
        }

        i += 1;
    }

    // Flush last block
    if !current_block.is_empty() {
        if current_is_error {
            errors.push(current_block.join("\n"));
        } else {
            warnings.push(current_block.join("\n"));
        }
    }

    // All clean
    if error_count == 0 && warning_count == 0 {
        return format!("✓ scarb build ({} packages compiled)", compiled);
    }

    // Build result with warnings and/or errors
    let mut result = String::new();

    if error_count > 0 {
        result.push_str(&format!(
            "scarb build: {} error{}, {} warning{} ({} packages)\n",
            error_count,
            if error_count == 1 { "" } else { "s" },
            warning_count,
            if warning_count == 1 { "" } else { "s" },
            compiled
        ));
        result.push_str("═══════════════════════════════════════\n");

        for (i, err) in errors.iter().enumerate().take(15) {
            result.push_str(err);
            result.push('\n');
            if i < errors.len() - 1 {
                result.push('\n');
            }
        }
        if errors.len() > 15 {
            result.push_str(&format!("\n... +{} more errors\n", errors.len() - 15));
        }
    } else {
        // Warnings only
        result.push_str(&format!(
            "scarb build: {} warning{} ({} packages)\n",
            warning_count,
            if warning_count == 1 { "" } else { "s" },
            compiled
        ));
        result.push_str("═══════════════════════════════════════\n");

        for (i, warn) in warnings.iter().enumerate().take(15) {
            result.push_str(warn);
            result.push('\n');
            if i < warnings.len() - 1 {
                result.push('\n');
            }
        }
        if warnings.len() > 15 {
            result.push_str(&format!("\n... +{} more warnings\n", warnings.len() - 15));
        }
    }

    if !finished_line.is_empty() {
        result.push_str(&format!("\n{}", finished_line));
    }

    result.trim().to_string()
}

/// Aggregated scarb/Cairo test results
#[derive(Debug, Default, Clone)]
struct AggregatedTestResult {
    passed: usize,
    failed: usize,
    ignored: usize,
    filtered_out: usize,
    suites: usize,
}

impl AggregatedTestResult {
    /// Parse scarb test result summary line
    /// Format: "test result: ok. N passed; N failed; N ignored; N filtered out;"
    fn parse_line(line: &str) -> Option<Self> {
        static RE: OnceLock<regex::Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            regex::Regex::new(
                r"test result: (\w+)\.\s+(\d+) passed;\s+(\d+) failed;\s+(\d+) ignored;\s+(\d+) filtered out",
            )
            .expect("invalid scarb test result regex")
        });

        let caps = re.captures(line)?;
        let status = caps.get(1)?.as_str();
        if status != "ok" {
            return None;
        }

        let passed = caps.get(2)?.as_str().parse().ok()?;
        let failed = caps.get(3)?.as_str().parse().ok()?;
        let ignored = caps.get(4)?.as_str().parse().ok()?;
        let filtered_out = caps.get(5)?.as_str().parse().ok()?;

        Some(Self {
            passed,
            failed,
            ignored,
            filtered_out,
            suites: 1,
        })
    }

    fn merge(&mut self, other: &Self) {
        self.passed += other.passed;
        self.failed += other.failed;
        self.ignored += other.ignored;
        self.filtered_out += other.filtered_out;
        self.suites += other.suites;
    }

    fn format_compact(&self) -> String {
        let mut parts = vec![format!("{} passed", self.passed)];
        if self.ignored > 0 {
            parts.push(format!("{} ignored", self.ignored));
        }
        if self.filtered_out > 0 {
            parts.push(format!("{} filtered out", self.filtered_out));
        }

        let suite_text = if self.suites == 1 {
            "1 suite".to_string()
        } else {
            format!("{} suites", self.suites)
        };

        format!("✓ scarb test: {} ({})", parts.join(", "), suite_text)
    }
}

/// Filter scarb test output - show failures + compact summary
/// Strips deprecation warnings, plugin compilation, and passing test lines
pub fn filter_scarb_test(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let n = lines.len();

    let mut failures: Vec<String> = Vec::new();
    let mut summary_lines: Vec<String> = Vec::new();
    let mut in_failure_section = false;
    let mut current_failure: Vec<String> = Vec::new();
    let mut i = 0;

    while i < n {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Skip compilation noise
        if trimmed.starts_with("Compiling") || trimmed.starts_with("Checking") {
            i += 1;
            continue;
        }

        // Skip lock/download noise
        if trimmed.starts_with("Blocking waiting for file lock")
            || trimmed.starts_with("Downloading")
            || trimmed.starts_with("Downloaded")
            || trimmed.starts_with("Locking")
            || trimmed.starts_with("Updating")
        {
            i += 1;
            continue;
        }

        // Skip Finished lines from plugin/dep compilation (keep only test summary)
        if trimmed.starts_with("Finished") {
            i += 1;
            continue;
        }

        // Skip known noise warn blocks (deprecation, cairo-test, artefacts)
        if is_noise_warn(line) {
            i += 1;
            while i < n {
                let next = lines[i].trim();
                if next.is_empty()
                    || lines[i].starts_with("warn")
                    || lines[i].starts_with("error")
                    || lines[i].trim_start().starts_with("Compiling")
                    || lines[i].trim_start().starts_with("Blocking")
                    || lines[i].starts_with("running")
                {
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Skip "running N tests" lines and "test ... ok" lines
        if line.starts_with("running ") || (line.starts_with("test ") && line.ends_with("... ok")) {
            i += 1;
            continue;
        }

        // Detect failures section
        if line == "failures:" {
            in_failure_section = true;
            i += 1;
            continue;
        }

        if in_failure_section {
            if line.starts_with("test result:") {
                in_failure_section = false;
                summary_lines.push(line.to_string());
            } else if line.starts_with("    ") || line.starts_with("---- ") {
                current_failure.push(line.to_string());
            } else if line.trim().is_empty() && !current_failure.is_empty() {
                failures.push(current_failure.join("\n"));
                current_failure.clear();
            } else if !line.trim().is_empty() {
                current_failure.push(line.to_string());
            }
            i += 1;
            continue;
        }

        // Capture test result summary lines
        if !in_failure_section && line.starts_with("test result:") {
            summary_lines.push(line.to_string());
        }

        i += 1;
    }

    if !current_failure.is_empty() {
        failures.push(current_failure.join("\n"));
    }

    let mut result = String::new();

    if failures.is_empty() && !summary_lines.is_empty() {
        // All passed - aggregate
        let mut aggregated: Option<AggregatedTestResult> = None;
        let mut all_parsed = true;

        for line in &summary_lines {
            if let Some(parsed) = AggregatedTestResult::parse_line(line) {
                if let Some(ref mut agg) = aggregated {
                    agg.merge(&parsed);
                } else {
                    aggregated = Some(parsed);
                }
            } else {
                all_parsed = false;
                break;
            }
        }

        if all_parsed {
            if let Some(agg) = aggregated {
                if agg.suites > 0 {
                    return agg.format_compact();
                }
            }
        }

        // Fallback
        for line in &summary_lines {
            result.push_str(&format!("✓ {}\n", line));
        }
        return result.trim().to_string();
    }

    if !failures.is_empty() {
        result.push_str(&format!("FAILURES ({}):\n", failures.len()));
        result.push_str("═══════════════════════════════════════\n");
        for (idx, failure) in failures.iter().enumerate().take(10) {
            result.push_str(&format!("{}. {}\n", idx + 1, failure));
        }
        if failures.len() > 10 {
            result.push_str(&format!("\n... +{} more failures\n", failures.len() - 10));
        }
        result.push('\n');
    }

    for line in &summary_lines {
        result.push_str(&format!("{}\n", line));
    }

    if result.trim().is_empty() {
        // Fallback: show last meaningful lines
        let meaningful: Vec<&str> = output
            .lines()
            .filter(|l| {
                !l.trim().is_empty()
                    && !l.trim_start().starts_with("Compiling")
                    && !l.trim_start().starts_with("Blocking")
            })
            .collect();
        for line in meaningful.iter().rev().take(5).rev() {
            result.push_str(&format!("{}\n", line));
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Build filter tests ──────────────────────────────────────────────────

    #[test]
    fn test_filter_scarb_build_success_no_warnings() {
        let output = r#"   Compiling falcon v0.1.0 (/path/Scarb.toml)
   Compiling falcon_account v0.1.0 (/path/Scarb.toml)
   Compiling falcon_old v0.1.0 (/path/Scarb.toml)
    Finished `dev` profile target(s) in 33 seconds
"#;
        let result = filter_scarb_build(output);
        assert!(
            result.contains("✓ scarb build"),
            "should show success: {}",
            result
        );
        assert!(
            result.contains("3 packages compiled"),
            "should count packages: {}",
            result
        );
        assert!(
            !result.contains("Compiling"),
            "should strip Compiling: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_build_with_user_code_warnings() {
        let output = r#"   Compiling falcon v0.1.0 (/path/Scarb.toml)
warn[E0001]: Unused variable. Consider ignoring by prefixing with `_`.
 --> /home/user/packages/falcon/src/ntt.cairo:1563:9
    let Q = 12289;
        ^

   Compiling falcon_account v0.1.0 (/path/Scarb.toml)
    Finished `dev` profile target(s) in 33 seconds
"#;
        let result = filter_scarb_build(output);
        assert!(
            result.contains("1 warning"),
            "should count user warning: {}",
            result
        );
        assert!(
            result.contains("warn[E0001]"),
            "should show warning code: {}",
            result
        );
        assert!(
            !result.contains("Compiling"),
            "should strip Compiling: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_build_strips_deprecation_noise() {
        let output = r#"warn: `scarb cairo-test` is deprecated, please migrate to starknet foundry
    help: run `scarb add starknet-foundry --dev` to install starknet foundry
    help: see https://foundry.url for more information
    help: to temporarily silence this warning, add `cairo-test.ignore = true` to your Scarb.toml
   Compiling falcon v0.1.0 (/path/Scarb.toml)
    Finished `dev` profile target(s) in 5 seconds
"#;
        let result = filter_scarb_build(output);
        assert!(
            result.contains("✓ scarb build"),
            "should show success after stripping noise: {}",
            result
        );
        assert!(
            !result.contains("deprecated"),
            "should strip deprecation noise: {}",
            result
        );
        assert!(
            !result.contains("migrate"),
            "should strip help lines: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_build_strips_artefacts_noise() {
        let output = r#"   Compiling falcon v0.1.0 (/path/Scarb.toml)
warn: artefacts produced by this build may be hard to utilize due to the build configuration
please make sure your build configuration is correct
help: if you want to compile a Starknet contract, make sure to use the `starknet-contract` target
      in your Scarb.toml file.
      See https://docs.swmansion.com/scarb/docs for help.

    Finished `release` profile target(s) in 31 seconds
"#;
        let result = filter_scarb_build(output);
        assert!(
            result.contains("✓ scarb build"),
            "should succeed after stripping artefacts noise: {}",
            result
        );
        assert!(
            !result.contains("artefacts"),
            "should strip artefacts warning: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_build_keeps_unreachable_code_warning() {
        let output = r#"   Compiling falcon_old v0.1.0 (/path/Scarb.toml)
warn: Unreachable code
 --> /home/user/packages/falcon_old/src/ntt_constants.cairo:121:9
        array![].span()
        ^^^^^^^^^^^^^^^

    Finished `dev` profile target(s) in 10 seconds
"#;
        let result = filter_scarb_build(output);
        assert!(
            result.contains("1 warning"),
            "should count unreachable code warning: {}",
            result
        );
        assert!(
            result.contains("Unreachable"),
            "should show unreachable code: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_build_strips_blocking_file_lock() {
        let output = r#"Blocking waiting for file lock on package cache
   Compiling falcon v0.1.0 (/path/Scarb.toml)
    Finished `dev` profile target(s) in 5 seconds
"#;
        let result = filter_scarb_build(output);
        assert!(
            result.contains("✓ scarb build"),
            "should succeed: {}",
            result
        );
        assert!(
            !result.contains("Blocking"),
            "should strip file lock message: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_build_with_errors() {
        let output = r#"   Compiling falcon v0.1.0 (/path/Scarb.toml)
error[E0001]: Variable not found.
 --> /home/user/packages/falcon/src/lib.cairo:10:5
    undefined_var
    ^^^^^^^^^^^^^

error: could not compile `falcon` due to previous errors.
"#;
        let result = filter_scarb_build(output);
        assert!(
            result.contains("1 error"),
            "should count errors: {}",
            result
        );
        assert!(
            result.contains("E0001"),
            "should show error code: {}",
            result
        );
        assert!(
            result.contains("Variable not found"),
            "should show error message: {}",
            result
        );
        assert!(
            !result.contains("could not compile"),
            "should strip aborting line: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_build_empty_input() {
        let result = filter_scarb_build("");
        assert!(
            result.contains("✓ scarb build") || result.is_empty(),
            "should handle empty input: {}",
            result
        );
    }

    // ── Test filter tests ───────────────────────────────────────────────────

    #[test]
    fn test_filter_scarb_test_all_pass_zero_tests() {
        let output = r#"running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out;
"#;
        let result = filter_scarb_test(output);
        assert!(
            result.contains("✓ scarb test"),
            "should show success: {}",
            result
        );
        assert!(
            result.contains("0 passed"),
            "should show passed count: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_test_strips_deprecation_and_blocking() {
        let output = r#"warn: `scarb cairo-test` is deprecated, please migrate to starknet foundry
    help: run `scarb add starknet-foundry --dev` to install starknet foundry
    help: see https://foundry.url for more
Blocking waiting for file lock on package cache
   Compiling snforge_scarb_plugin v0.1.0 (git+https://github.com/...)
    Finished `release` profile [optimized] target(s) in 5s
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out;
"#;
        let result = filter_scarb_test(output);
        assert!(
            result.contains("✓ scarb test"),
            "should show success: {}",
            result
        );
        assert!(
            !result.contains("deprecated"),
            "should strip deprecation: {}",
            result
        );
        assert!(
            !result.contains("Blocking"),
            "should strip file lock: {}",
            result
        );
        assert!(
            !result.contains("snforge_scarb_plugin"),
            "should strip plugin compilation: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_test_multiple_suites_all_pass() {
        let output = r#"running 5 tests
test falcon::test_a ... ok
test falcon::test_b ... ok
test result: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out;

running 3 tests
test account::test_x ... ok
test account::test_y ... ok
test account::test_z ... ok
test result: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out;
"#;
        let result = filter_scarb_test(output);
        assert!(
            result.contains("✓ scarb test"),
            "should show success: {}",
            result
        );
        assert!(
            result.contains("8 passed"),
            "should aggregate passed: {}",
            result
        );
        assert!(
            result.contains("2 suites"),
            "should show suite count: {}",
            result
        );
        assert!(
            !result.contains("test falcon::test_a"),
            "should strip passing test lines: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_test_with_failures() {
        let output = r#"running 3 tests
test falcon::test_good ... ok
test falcon::test_bad ... FAILED
test falcon::test_another ... ok

failures:

---- falcon::test_bad stdout ----
thread 'falcon::test_bad' panicked at 'assertion failed: left == right'

failures:
    falcon::test_bad

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 filtered out;
"#;
        let result = filter_scarb_test(output);
        assert!(
            result.contains("FAILURES"),
            "should show failures section: {}",
            result
        );
        assert!(
            result.contains("test_bad"),
            "should show failing test: {}",
            result
        );
        assert!(
            result.contains("test result:"),
            "should show summary: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_test_strips_passing_test_lines() {
        let output = r#"running 10 tests
test a::test_1 ... ok
test a::test_2 ... ok
test a::test_3 ... ok
test result: ok. 10 passed; 0 failed; 0 ignored; 0 filtered out;
"#;
        let result = filter_scarb_test(output);
        assert!(
            !result.contains("test a::test_1"),
            "should strip passing test lines: {}",
            result
        );
        assert!(
            result.contains("✓ scarb test: 10 passed"),
            "should show compact summary: {}",
            result
        );
    }

    #[test]
    fn test_filter_scarb_build_token_savings() {
        // Real-world scarb build output with lots of compilation noise
        let input = r#"   Compiling falcon v0.1.0 (/path/Scarb.toml)
warn: `scarb cairo-test` is deprecated, please migrate to starknet foundry
    help: run `scarb add starknet-foundry --dev` to install starknet foundry
    help: see https://foundry.url for more information
    help: to temporarily silence this warning, add `cairo-test.ignore = true` to your Scarb.toml
   Compiling falcon_account v0.1.0 (/path/Scarb.toml)
   Compiling falcon_old v0.1.0 (/path/Scarb.toml)
warn: artefacts produced by this build may be hard to utilize due to the build configuration
please make sure your build configuration is correct
help: if you want to compile a Starknet contract, make sure to use the `starknet-contract` target
      in your Scarb.toml file.
      See https://docs.swmansion.com/scarb/docs for help.
   Compiling falcon_zknox v0.1.0 (/path/Scarb.toml)
    Finished `dev` profile target(s) in 33 seconds
"#;
        let output = filter_scarb_build(input);
        let input_tokens = input.split_whitespace().count();
        let output_tokens = output.split_whitespace().count();
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "Expected ≥60% token savings, got {:.1}% (input: {}, output: {})\nOutput: {}",
            savings,
            input_tokens,
            output_tokens,
            output
        );
    }

    #[test]
    fn test_filter_scarb_test_token_savings() {
        let input = r#"warn: `scarb cairo-test` is deprecated, please migrate to starknet foundry
    help: run `scarb add starknet-foundry --dev` to install starknet foundry
    help: see https://foundry.url for more information
    help: to temporarily silence this warning, add `cairo-test.ignore = true` to your Scarb.toml
Blocking waiting for file lock on package cache
   Compiling snforge_scarb_plugin v0.34.0 (git+https://github.com/foundry-rs/starknet-foundry)
    Finished `release` profile [optimized] target(s) in 45s
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out;
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out;
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out;
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out;
"#;
        let output = filter_scarb_test(input);
        let input_tokens = input.split_whitespace().count();
        let output_tokens = output.split_whitespace().count();
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "Expected ≥60% token savings, got {:.1}% (input: {}, output: {})\nOutput: {}",
            savings,
            input_tokens,
            output_tokens,
            output
        );
    }
}
