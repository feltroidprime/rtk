use crate::tracking;
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::process::Command;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub enum SnforgeCommand {
    Test,
}

pub fn run(cmd: SnforgeCommand, args: &[String], verbose: u8) -> Result<()> {
    match cmd {
        SnforgeCommand::Test => run_snforge_filtered("test", args, verbose, filter_snforge_test),
    }
}

/// Generic snforge command runner with filtering
fn run_snforge_filtered<F>(
    subcommand: &str,
    args: &[String],
    verbose: u8,
    filter_fn: F,
) -> Result<()>
where
    F: Fn(&str) -> String,
{
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("snforge");
    cmd.arg(subcommand);
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: snforge {} {}", subcommand, args.join(" "));
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run snforge {}", subcommand))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_fn(&raw);

    if let Some(hint) =
        crate::tee::tee_and_hint(&raw, &format!("snforge_{}", subcommand), exit_code)
    {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("snforge {} {}", subcommand, args.join(" ")),
        &format!("rtk snforge {} {}", subcommand, args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Passthrough for unknown snforge subcommands
pub fn run_passthrough(args: &[OsString], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("snforge passthrough: {:?}", args);
    }
    let status = Command::new("snforge")
        .args(args)
        .status()
        .context("Failed to run snforge")?;

    let args_str = tracking::args_display(args);
    timer.track_passthrough(
        &format!("snforge {}", args_str),
        &format!("rtk snforge {} (passthrough)", args_str),
    );

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Parse a snforge "Tests summary:" or "Tests:" line
/// Format: "Tests summary: N passed, N failed, N ignored, N filtered out"
/// Format: "Tests: N passed, N failed, N ignored, N filtered out"
struct SnforgeTestCounts {
    passed: usize,
    failed: usize,
    ignored: usize,
    filtered_out: usize,
}

impl SnforgeTestCounts {
    fn parse(line: &str) -> Option<Self> {
        static RE: OnceLock<regex::Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            regex::Regex::new(
                r"(\d+) passed,\s+(\d+) failed,\s+(\d+) ignored,\s+(\d+) filtered out",
            )
            .expect("invalid snforge test counts regex")
        });

        let caps = re.captures(line)?;
        let passed = caps.get(1)?.as_str().parse().ok()?;
        let failed = caps.get(2)?.as_str().parse().ok()?;
        let ignored = caps.get(3)?.as_str().parse().ok()?;
        let filtered_out = caps.get(4)?.as_str().parse().ok()?;

        Some(Self {
            passed,
            failed,
            ignored,
            filtered_out,
        })
    }

    fn format_compact(&self, package_count: usize) -> String {
        let mut parts = vec![format!("{} passed", self.passed)];
        if self.ignored > 0 {
            parts.push(format!("{} ignored", self.ignored));
        }
        if self.filtered_out > 0 {
            parts.push(format!("{} filtered out", self.filtered_out));
        }

        let pkg_text = if package_count == 1 {
            "1 package".to_string()
        } else {
            format!("{} packages", package_count)
        };

        format!("✓ snforge test: {} ({})", parts.join(", "), pkg_text)
    }
}

/// Returns true if the line is compilation/infrastructure noise to strip
fn is_compilation_noise(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Compiling ")
        || trimmed.starts_with("Checking ")
        || trimmed.starts_with("Blocking waiting for file lock")
        || trimmed.starts_with("Downloading ")
        || trimmed.starts_with("Downloaded ")
        || trimmed.starts_with("Locking ")
        || trimmed.starts_with("Updating ")
        || trimmed.starts_with("Finished ")
}

/// Returns true if the line is a gas report table border or separator
fn is_gas_report_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('╭')
        || trimmed.starts_with('╰')
        || trimmed.starts_with('├')
        || trimmed.starts_with('│')
        || trimmed.starts_with('╞')
        || trimmed.starts_with('╡')
        || trimmed.starts_with('╟')
        || trimmed.starts_with('╢')
        || trimmed.starts_with('─')
        || trimmed.starts_with('╤')
        || trimmed.starts_with('╧')
}

/// Returns true if the line is a detailed resources line to strip
fn is_detailed_resources_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("sierra gas:")
        || trimmed.starts_with("syscalls:")
        || trimmed.starts_with("builtins:")
        || trimmed.starts_with("l2_l1_message_sizes:")
}

/// Filter snforge test output — failures only, compact summary on success
///
/// Strips:
/// - All compilation noise (Compiling, Blocking, Finished)
/// - Compiler warnings (warn[EXXX] + --> + code snippet)
/// - `Running N test(s) from src/tests/` lines
/// - `[PASS]` and `[IGNORED]` lines on full success
/// - Gas report tables (╭╰│ border lines)
/// - Detailed resources (sierra gas:, syscalls:)
///
/// Keeps:
/// - `[FAIL]` lines always
/// - `Failure data:` blocks
/// - `Tests: ...` per-package summaries
/// - `Tests summary: ...` global summary
/// - `Collected N test(s) from X package` on failure
pub fn filter_snforge_test(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let n = lines.len();

    let mut failures: Vec<String> = Vec::new();
    let mut per_package_summaries: Vec<String> = Vec::new();
    let mut global_summary: Option<String> = None;
    let mut package_count = 0usize;

    let mut in_failure_data = false;
    let mut current_failure_data: Vec<String> = Vec::new();
    let mut last_fail_line: Option<String> = None;

    // Track if we're inside a compiler warning block to skip
    let mut skip_warn_block = false;

    let mut i = 0;
    while i < n {
        let line = lines[i];
        let trimmed = line.trim();

        // --- Compilation noise ---
        if is_compilation_noise(line) {
            skip_warn_block = false;
            i += 1;
            continue;
        }

        // --- Compiler warning blocks: warn[EXXX]: or warn: followed by --> ---
        // Skip the whole block (message + --> pointer + code snippet)
        if line.starts_with("warn[") || line.starts_with("warn: ") {
            skip_warn_block = true;
            i += 1;
            continue;
        }

        // Skip continuation of warning blocks (-->, 4-space indented code, empty lines)
        if skip_warn_block {
            if trimmed.is_empty() {
                skip_warn_block = false;
                i += 1;
                continue;
            }
            // Continue skipping if it's a --> pointer or indented code
            if trimmed.starts_with("-->")
                || line.starts_with("    ")
                || line.starts_with('\t')
                || trimmed.starts_with('=')
                || trimmed.starts_with('|')
                || trimmed.starts_with('^')
                || trimmed.starts_with('-')
            {
                i += 1;
                continue;
            }
            // Otherwise, this line is not part of the warning - resume normal processing
            skip_warn_block = false;
        }

        // --- Gas report tables ---
        if is_gas_report_line(line) {
            i += 1;
            continue;
        }

        // --- Detailed resources (--detailed-resources output) ---
        if is_detailed_resources_line(line) {
            i += 1;
            continue;
        }

        // --- Running N test(s) from src/ or tests/ ---
        if trimmed.starts_with("Running ") && trimmed.contains("test(s) from") {
            i += 1;
            continue;
        }

        // --- Collected N test(s) from X package ---
        if trimmed.starts_with("Collected ") && trimmed.contains("test(s) from") {
            package_count += 1;
            // We don't print this unless there are failures (handled at output time)
            i += 1;
            continue;
        }

        // --- [PASS] lines: strip ---
        if trimmed.starts_with("[PASS]") || trimmed.starts_with("[PASS] ") {
            // End any open failure data block
            if in_failure_data && !current_failure_data.is_empty() {
                if let Some(fail_line) = last_fail_line.take() {
                    let mut block = vec![fail_line];
                    block.append(&mut current_failure_data);
                    failures.push(block.join("\n"));
                } else {
                    failures.push(current_failure_data.join("\n"));
                    current_failure_data.clear();
                }
                in_failure_data = false;
            }
            i += 1;
            continue;
        }

        // --- [IGNORED] lines: strip ---
        if trimmed.starts_with("[IGNORED]") || trimmed.starts_with("[IGNORED] ") {
            i += 1;
            continue;
        }

        // --- [FAIL] lines: always keep ---
        if trimmed.starts_with("[FAIL]") || trimmed.starts_with("[FAIL] ") {
            // Flush any previous failure data
            if in_failure_data && !current_failure_data.is_empty() {
                if let Some(prev_fail) = last_fail_line.take() {
                    let mut block = vec![prev_fail];
                    block.append(&mut current_failure_data);
                    failures.push(block.join("\n"));
                } else {
                    current_failure_data.clear();
                }
            }
            last_fail_line = Some(line.to_string());
            in_failure_data = false;
            i += 1;
            continue;
        }

        // --- Failure data block ---
        if trimmed == "Failure data:" {
            in_failure_data = true;
            current_failure_data.push(line.to_string());
            i += 1;
            continue;
        }

        if in_failure_data {
            if trimmed.is_empty() {
                // End of failure data block
                if !current_failure_data.is_empty() {
                    if let Some(fail_line) = last_fail_line.take() {
                        let mut block = vec![fail_line];
                        block.append(&mut current_failure_data);
                        failures.push(block.join("\n"));
                    } else {
                        failures.push(current_failure_data.join("\n"));
                        current_failure_data.clear();
                    }
                }
                in_failure_data = false;
                i += 1;
                continue;
            }
            // Indented failure data content
            current_failure_data.push(line.to_string());
            i += 1;
            continue;
        }

        // --- Tests summary: N passed, N failed, ... (global) ---
        if trimmed.starts_with("Tests summary:") {
            global_summary = Some(line.to_string());
            i += 1;
            continue;
        }

        // --- Tests: N passed, N failed, ... (per-package) ---
        if trimmed.starts_with("Tests:") && trimmed.contains("passed") {
            per_package_summaries.push(line.to_string());
            i += 1;
            continue;
        }

        i += 1;
    }

    // Flush any trailing failure data
    if in_failure_data && !current_failure_data.is_empty() {
        if let Some(fail_line) = last_fail_line.take() {
            let mut block = vec![fail_line];
            block.extend(current_failure_data.drain(..));
            failures.push(block.join("\n"));
        } else {
            failures.push(current_failure_data.join("\n"));
        }
    } else if let Some(fail_line) = last_fail_line {
        // [FAIL] with no Failure data block
        failures.push(fail_line);
    }

    // --- Build output ---

    // If no failures and we have a global summary → compact success line
    if failures.is_empty() {
        if let Some(ref summary) = global_summary {
            if let Some(counts) = SnforgeTestCounts::parse(summary) {
                if counts.failed == 0 {
                    return counts.format_compact(package_count.max(1));
                }
            }
        }

        // Fallback: no summary found, use per-package lines
        if !per_package_summaries.is_empty() {
            let mut total = SnforgeTestCounts {
                passed: 0,
                failed: 0,
                ignored: 0,
                filtered_out: 0,
            };
            let mut all_parsed = true;
            for line in &per_package_summaries {
                if let Some(c) = SnforgeTestCounts::parse(line) {
                    total.passed += c.passed;
                    total.failed += c.failed;
                    total.ignored += c.ignored;
                    total.filtered_out += c.filtered_out;
                } else {
                    all_parsed = false;
                    break;
                }
            }
            if all_parsed && total.failed == 0 {
                return total.format_compact(package_count.max(per_package_summaries.len()));
            }
        }

        // Final fallback if output is empty/unparseable
        if global_summary.is_none() && per_package_summaries.is_empty() {
            return String::new();
        }
    }

    // Failures present → show failure details + summaries
    let mut result = String::new();

    for failure in &failures {
        result.push_str(failure);
        result.push('\n');
        result.push('\n');
    }

    // Per-package summaries
    for line in &per_package_summaries {
        result.push_str(line);
        result.push('\n');
    }

    // Global summary
    if let Some(ref summary) = global_summary {
        result.push_str(summary);
        result.push('\n');
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    // ── Success cases ────────────────────────────────────────────────────────

    #[test]
    fn test_filter_snforge_test_all_pass() {
        let input = r#"Collected 34 test(s) from falcon package
Running 10 test(s) from src/
[PASS] falcon::ntt::test_ntt (gas: ~12345)
[PASS] falcon::ntt::test_poly_mul (gas: ~98765)
Running 24 test(s) from tests/
[PASS] falcon::tests::test_integration_1 (gas: ~11111)
[PASS] falcon::tests::test_integration_2 (gas: ~22222)
Tests: 34 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 34 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
        assert!(
            result.contains("34 passed"),
            "should show passed count: {}",
            result
        );
        assert!(
            !result.contains("[PASS]"),
            "should strip [PASS] lines: {}",
            result
        );
        assert!(
            !result.contains("Running"),
            "should strip Running lines: {}",
            result
        );
    }

    #[test]
    fn test_filter_snforge_test_multi_package() {
        let input = r#"Collected 10 test(s) from falcon package
Running 10 test(s) from src/
[PASS] falcon::test_a (gas: ~100)
Tests: 10 passed, 0 failed, 0 ignored, 0 filtered out

Collected 24 test(s) from falcon_account package
Running 24 test(s) from src/
[PASS] falcon_account::test_x (gas: ~200)
Tests: 24 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 34 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
        assert!(
            result.contains("34 passed"),
            "should aggregate passed: {}",
            result
        );
        assert!(
            result.contains("2 package"),
            "should show package count: {}",
            result
        );
    }

    #[test]
    fn test_filter_snforge_test_with_ignored() {
        let input = r#"Collected 10 test(s) from falcon package
Running 10 test(s) from src/
[PASS] falcon::test_a (gas: ~100)
[IGNORED] falcon::test_skip
Tests: 9 passed, 0 failed, 1 ignored, 0 filtered out

Tests summary: 9 passed, 0 failed, 1 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
        assert!(
            result.contains("1 ignored"),
            "should show ignored count: {}",
            result
        );
        assert!(
            !result.contains("[IGNORED]"),
            "should strip [IGNORED] lines: {}",
            result
        );
    }

    // ── Failure cases ────────────────────────────────────────────────────────

    #[test]
    fn test_filter_snforge_test_with_failures() {
        let input = r#"Collected 20 test(s) from falcon package
Running 20 test(s) from src/
[PASS] falcon::test_good (gas: ~100)
[FAIL] falcon::some_module::test_name

Failure data:
    "expected 5 got 3"

Tests: 19 passed, 1 failed, 0 ignored, 0 filtered out

Tests summary: 33 passed, 1 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            result.contains("[FAIL]"),
            "should show [FAIL] line: {}",
            result
        );
        assert!(
            result.contains("Failure data:"),
            "should show Failure data: {}",
            result
        );
        assert!(
            result.contains("expected 5 got 3"),
            "should show failure message: {}",
            result
        );
        assert!(
            result.contains("Tests summary:"),
            "should show summary: {}",
            result
        );
        assert!(
            !result.contains("[PASS]"),
            "should not show [PASS] lines: {}",
            result
        );
    }

    // ── Noise stripping ───────────────────────────────────────────────────────

    #[test]
    fn test_filter_snforge_test_strips_compilation_noise() {
        let input = r#"Blocking waiting for file lock on package cache
   Compiling snforge_scarb_plugin v0.55.0 (git+https://github.com/foundry-rs/starknet-foundry)
   Compiling falcon v0.1.0 (/path/Scarb.toml)
    Finished `release` profile [optimized] target(s) in 45s
Collected 5 test(s) from falcon package
Running 5 test(s) from src/
[PASS] falcon::test_a (gas: ~100)
Tests: 5 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 5 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            !result.contains("Compiling"),
            "should strip Compiling: {}",
            result
        );
        assert!(
            !result.contains("Blocking"),
            "should strip Blocking: {}",
            result
        );
        assert!(
            !result.contains("Finished"),
            "should strip Finished: {}",
            result
        );
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
    }

    #[test]
    fn test_filter_snforge_test_strips_pass_lines() {
        let input = r#"Collected 3 test(s) from falcon package
Running 3 test(s) from src/
[PASS] falcon::test_1 (gas: ~100)
[PASS] falcon::test_2 (gas: ~200)
[PASS] falcon::test_3 (gas: ~300)
Tests: 3 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 3 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            !result.contains("[PASS]"),
            "should strip [PASS] lines: {}",
            result
        );
        assert!(
            result.contains("✓ snforge test: 3 passed"),
            "should show compact summary: {}",
            result
        );
    }

    #[test]
    fn test_filter_snforge_test_strips_running_lines() {
        let input = r#"Collected 5 test(s) from falcon package
Running 3 test(s) from src/
[PASS] falcon::test_a (gas: ~100)
Running 2 test(s) from tests/
[PASS] falcon::integration::test_b (gas: ~200)
Tests: 5 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 5 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            !result.contains("Running "),
            "should strip Running lines: {}",
            result
        );
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
    }

    #[test]
    fn test_filter_snforge_test_strips_compiler_warnings() {
        let input = r#"   Compiling falcon v0.1.0 (/path/Scarb.toml)
warn[E0001]: Unused variable. Consider ignoring by prefixing with `_`.
 --> /home/user/packages/falcon/src/lib.cairo:42:9
    let unused_var = 5;
        ^^^^^^^^^^
    Finished `dev` profile target(s) in 10s
Collected 2 test(s) from falcon package
Running 2 test(s) from src/
[PASS] falcon::test_ok (gas: ~100)
Tests: 2 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 2 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            !result.contains("warn[E0001]"),
            "should strip compiler warnings: {}",
            result
        );
        assert!(
            !result.contains("Unused variable"),
            "should strip warning message: {}",
            result
        );
        assert!(
            !result.contains("-->"),
            "should strip code pointer: {}",
            result
        );
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
    }

    #[test]
    fn test_filter_snforge_test_strips_gas_report_tables() {
        let input = r#"Collected 2 test(s) from falcon package
Running 2 test(s) from src/
[PASS] falcon::test_a (gas: ~100)
[PASS] falcon::test_b (gas: ~200)
Tests: 2 passed, 0 failed, 0 ignored, 0 filtered out

╭────────────────────────────────────────╮
│ Calls gas report                       │
├──────────────────────┬─────────────────┤
│ Contract             │ Function        │
╞══════════════════════╪═════════════════╡
│ ERC20                │ transfer        │
├──────────────────────┼─────────────────┤
│                      │ min: 1234       │
╰──────────────────────┴─────────────────╯

Tests summary: 2 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            !result.contains('╭'),
            "should strip gas report table top: {}",
            result
        );
        assert!(
            !result.contains('╰'),
            "should strip gas report table bottom: {}",
            result
        );
        assert!(
            !result.contains('│'),
            "should strip gas report table borders: {}",
            result
        );
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
    }

    #[test]
    fn test_filter_snforge_test_strips_detailed_resources() {
        let input = r#"Collected 2 test(s) from falcon package
Running 2 test(s) from src/
[PASS] falcon::test_a (gas: ~100)
        sierra gas: 987654
        syscalls: call_contract: 2, deploy: 1
        builtins: range_check: 100
[PASS] falcon::test_b (gas: ~200)
        sierra gas: 123456
Tests: 2 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 2 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            !result.contains("sierra gas:"),
            "should strip sierra gas: {}",
            result
        );
        assert!(
            !result.contains("syscalls:"),
            "should strip syscalls: {}",
            result
        );
        assert!(
            !result.contains("builtins:"),
            "should strip builtins: {}",
            result
        );
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
    }

    // ── Edge cases ────────────────────────────────────────────────────────────

    #[test]
    fn test_filter_snforge_test_empty_input() {
        let result = filter_snforge_test("");
        // Should not panic and return empty or minimal output
        assert!(result.is_empty() || result.len() < 100);
    }

    // ── Token savings ─────────────────────────────────────────────────────────

    #[test]
    fn test_filter_snforge_test_token_savings() {
        // Realistic snforge output with many passing tests and compilation noise
        let input = r#"Blocking waiting for file lock on package cache
   Compiling snforge_scarb_plugin v0.55.0 (git+https://github.com/foundry-rs/starknet-foundry)
   Compiling falcon v0.1.0 (/path/Scarb.toml)
   Compiling falcon_account v0.1.0 (/path/Scarb.toml)
   Compiling falcon_old v0.1.0 (/path/Scarb.toml)
    Finished `release` profile [optimized] target(s) in 45s
Collected 10 test(s) from falcon package
Running 6 test(s) from src/
[PASS] falcon::ntt::test_ntt_forward (gas: ~123456)
[PASS] falcon::ntt::test_ntt_inverse (gas: ~234567)
[PASS] falcon::ntt::test_ntt_roundtrip (gas: ~345678)
[PASS] falcon::poly::test_poly_mul (gas: ~456789)
[PASS] falcon::poly::test_poly_add (gas: ~567890)
[PASS] falcon::poly::test_poly_sub (gas: ~678901)
Running 4 test(s) from tests/
[PASS] falcon::tests::test_sign_verify (gas: ~789012)
[PASS] falcon::tests::test_keygen (gas: ~890123)
[PASS] falcon::tests::test_hash_to_point (gas: ~901234)
[PASS] falcon::tests::test_compress (gas: ~112345)
Tests: 10 passed, 0 failed, 0 ignored, 0 filtered out

Collected 8 test(s) from falcon_account package
Running 8 test(s) from src/
[PASS] falcon_account::tests::test_validate_sig (gas: ~223456)
[PASS] falcon_account::tests::test_execute (gas: ~334567)
[PASS] falcon_account::tests::test_multicall (gas: ~445678)
[PASS] falcon_account::tests::test_upgrade (gas: ~556789)
[PASS] falcon_account::tests::test_declare (gas: ~667890)
[PASS] falcon_account::tests::test_deploy (gas: ~778901)
[PASS] falcon_account::tests::test_transfer (gas: ~889012)
[PASS] falcon_account::tests::test_approve (gas: ~990123)
Tests: 8 passed, 0 failed, 0 ignored, 0 filtered out

Collected 16 test(s) from falcon_old package
Running 16 test(s) from src/
[PASS] falcon_old::ntt::test_old_ntt_1 (gas: ~111111)
[PASS] falcon_old::ntt::test_old_ntt_2 (gas: ~222222)
[PASS] falcon_old::ntt::test_old_ntt_3 (gas: ~333333)
[PASS] falcon_old::ntt::test_old_ntt_4 (gas: ~444444)
[PASS] falcon_old::ntt::test_old_ntt_5 (gas: ~555555)
[PASS] falcon_old::ntt::test_old_ntt_6 (gas: ~666666)
[PASS] falcon_old::ntt::test_old_ntt_7 (gas: ~777777)
[PASS] falcon_old::ntt::test_old_ntt_8 (gas: ~888888)
[PASS] falcon_old::poly::test_old_poly_1 (gas: ~999999)
[PASS] falcon_old::poly::test_old_poly_2 (gas: ~101010)
[PASS] falcon_old::poly::test_old_poly_3 (gas: ~202020)
[PASS] falcon_old::poly::test_old_poly_4 (gas: ~303030)
[PASS] falcon_old::poly::test_old_poly_5 (gas: ~404040)
[PASS] falcon_old::poly::test_old_poly_6 (gas: ~505050)
[PASS] falcon_old::poly::test_old_poly_7 (gas: ~606060)
[PASS] falcon_old::poly::test_old_poly_8 (gas: ~707070)
Tests: 16 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 34 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let output = filter_snforge_test(input);
        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
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
