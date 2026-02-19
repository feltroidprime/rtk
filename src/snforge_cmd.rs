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

/// Returns true if the line starts a gas report ASCII table
fn is_gas_table_start(line: &str) -> bool {
    line.trim().starts_with('╭')
}

/// Returns true if the line ends a gas report ASCII table
fn is_gas_table_end(line: &str) -> bool {
    line.trim().starts_with('╰')
}

/// Compress a [PASS] line with extended gas format (l1_gas/l2_gas).
/// Returns None if it's the basic `(gas: ~N)` format (should be stripped by caller).
///
/// Input:  "[PASS] test::name (l1_gas: ~0, l1_data_gas: ~0, l2_gas: ~809044)"
/// Output: Some("[PASS] test::name (l2:~809044)")
fn compress_pass_gas_line(line: &str) -> Option<String> {
    // Only handle extended format with l1_gas/l2_gas
    if !line.contains("l1_gas:") && !line.contains("l2_gas:") {
        return None;
    }

    let paren_start = line.rfind('(')?;
    let paren_end = line.rfind(')')?;
    if paren_end <= paren_start {
        return None;
    }

    let name_part = line[..paren_start].trim();
    let gas_content = &line[paren_start + 1..paren_end];

    let mut fields = Vec::new();
    for field in gas_content.split(',') {
        let field = field.trim();
        if field.is_empty() || field.ends_with("~0") {
            continue; // skip zero values
        }
        // Abbreviate field names
        let compressed = if let Some(rest) = field.strip_prefix("l1_data_gas: ") {
            format!("l1d:{}", rest)
        } else if let Some(rest) = field.strip_prefix("l1_gas: ") {
            format!("l1:{}", rest)
        } else if let Some(rest) = field.strip_prefix("l2_gas: ") {
            format!("l2:{}", rest)
        } else {
            field.to_string()
        };
        fields.push(compressed);
    }

    if fields.is_empty() {
        // All gas values are zero — show test name without gas
        Some(name_part.to_string())
    } else {
        Some(format!("{} ({})", name_part, fields.join(" ")))
    }
}

/// Compress a syscalls/builtins/l2_l1_message_sizes line.
/// Returns None if the value is empty `()` or blank.
///
/// Input:  "        syscalls: call_contract: 2, deploy: 1"
/// Output: Some("  syscalls:call_contract:2,deploy:1")
fn compress_resource_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let (prefix, rest) = if let Some(r) = trimmed.strip_prefix("syscalls:") {
        ("syscalls", r.trim())
    } else if let Some(r) = trimmed.strip_prefix("builtins:") {
        ("builtins", r.trim())
    } else if let Some(r) = trimmed.strip_prefix("l2_l1_message_sizes:") {
        ("l2_msg", r.trim())
    } else {
        return None;
    };

    // Empty or empty parens → strip
    if rest.is_empty() || rest == "()" {
        return None;
    }

    // Compact: remove spaces around colons and commas
    let compact = rest
        .trim_matches(|c| c == '(' || c == ')')
        .replace(": ", ":")
        .replace(", ", ",");

    if compact.is_empty() {
        None
    } else {
        Some(format!("  {}:{}", prefix, compact))
    }
}

/// Parse a gas report ASCII table and return compressed single-line entries.
///
/// Input table:
///   │ Calls gas report │
///   │ Contract │ Function │
///   │ ERC20    │ transfer │
///   │          │ min: 1234 │
///   │          │ max: 5678 │
///
/// Output: ["ERC20::transfer: min:1234 max:5678 mean:2345 median:2100"]
fn compress_gas_table(table_lines: &[String]) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    let mut current_contract = String::new();
    let mut current_function = String::new();
    let mut stats: Vec<String> = Vec::new();
    let mut past_header = false;

    for line in table_lines {
        let trimmed = line.trim();

        // Only process │ content lines; skip pure border lines (╭╰├╞─ etc.)
        if !trimmed.starts_with('│') {
            continue;
        }

        // Split by │ preserving empty segments to detect two-column structure.
        // A line like "│         │ min: 1234 │" splits to ["", "         ", " min: 1234 ", ""]
        // so raw_cells[1] = left cell, raw_cells[2] = right cell.
        let raw: Vec<&str> = trimmed.split('│').collect();
        if raw.len() < 3 {
            // Only a title cell or malformed — skip
            continue;
        }

        let left = raw[1].trim();
        let right = raw[2].trim();

        // Column header row
        if left == "Contract" {
            past_header = true;
            continue;
        }
        // Title rows (single meaningful cell, no two-column data)
        if !past_header {
            continue;
        }

        if !left.is_empty() {
            // New contract::function row — flush previous entry
            if !current_function.is_empty() && !stats.is_empty() {
                entries.push(format!(
                    "{}::{}: {}",
                    current_contract,
                    current_function,
                    stats.join(" ")
                ));
                stats.clear();
            }
            current_contract = left.to_string();
            if !right.is_empty() {
                current_function = right.to_string();
            }
        } else if !right.is_empty() {
            // Stat row: left empty, right has value like "min: 1234"
            let stat = right.replace(": ", ":");
            stats.push(stat);
        }
    }

    // Flush last entry
    if !current_function.is_empty() && !stats.is_empty() {
        entries.push(format!(
            "{}::{}: {}",
            current_contract,
            current_function,
            stats.join(" ")
        ));
    }

    entries
}

/// Filter snforge test output.
///
/// Basic mode (`snforge test`):
///   - Strip [PASS]/[IGNORED]/Running/Compiled lines → compact `✓` summary
///
/// Detailed resources mode (`--detailed-resources`):
///   - [PASS] lines with l1_gas/l2_gas → compressed: `[PASS] test (l2:~809044)`
///   - `sierra gas:` lines → stripped (redundant with inline gas in [PASS])
///   - `syscalls:` non-empty → compressed: `  syscalls:call_contract:2,deploy:1`
///   - `builtins:` non-empty → compressed
///
/// Gas report mode (`--gas-report`):
///   - [PASS] lines with l1_gas/l2_gas → compressed
///   - Gas report ASCII tables (╭╰│) → parsed into compact `Contract::Fn: min:N max:N`
///   - "No contract gas usage data..." → stripped
///
/// Always:
///   - [FAIL] lines + Failure data: blocks → preserved
///   - `Tests: ...` / `Tests summary: ...` → preserved
pub fn filter_snforge_test(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let n = lines.len();

    // Output collectors
    let mut detail_lines: Vec<String> = Vec::new(); // compressed [PASS] + resources
    let mut gas_report_entries: Vec<String> = Vec::new(); // compressed gas tables
    let mut failures: Vec<String> = Vec::new();
    let mut per_package_summaries: Vec<String> = Vec::new();
    let mut global_summary: Option<String> = None;
    let mut package_count = 0usize;

    // State machine
    let mut in_failure_data = false;
    let mut current_failure_data: Vec<String> = Vec::new();
    let mut last_fail_line: Option<String> = None;
    let mut skip_warn_block = false;
    let mut in_gas_table = false;
    let mut gas_table_lines: Vec<String> = Vec::new();

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

        // --- Compiler warning blocks ---
        if line.starts_with("warn[") || line.starts_with("warn: ") {
            skip_warn_block = true;
            i += 1;
            continue;
        }
        if skip_warn_block {
            if trimmed.is_empty() {
                skip_warn_block = false;
                i += 1;
                continue;
            }
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
            skip_warn_block = false;
        }

        // --- Gas report table: collect from ╭ to ╰, then compress ---
        if is_gas_table_start(line) {
            in_gas_table = true;
            gas_table_lines.clear();
            gas_table_lines.push(line.to_string());
            i += 1;
            continue;
        }
        if in_gas_table {
            gas_table_lines.push(line.to_string());
            if is_gas_table_end(line) {
                let compressed = compress_gas_table(&gas_table_lines);
                gas_report_entries.extend(compressed);
                in_gas_table = false;
                gas_table_lines.clear();
            }
            i += 1;
            continue;
        }

        // --- sierra gas: — strip (redundant: l2_gas is already in [PASS] line inline) ---
        if trimmed.starts_with("sierra gas:") {
            i += 1;
            continue;
        }

        // --- syscalls: / builtins: / l2_l1_message_sizes: — compress if non-empty ---
        if trimmed.starts_with("syscalls:")
            || trimmed.starts_with("builtins:")
            || trimmed.starts_with("l2_l1_message_sizes:")
        {
            if let Some(compressed) = compress_resource_line(line) {
                detail_lines.push(compressed);
            }
            i += 1;
            continue;
        }

        // --- "No contract gas usage data" — strip (noise from --gas-report) ---
        if trimmed.starts_with("No contract gas usage data") {
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
            i += 1;
            continue;
        }

        // --- [PASS] lines ---
        if trimmed.starts_with("[PASS]") {
            // Flush any open failure data block
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

            // Extended gas format (l1_gas/l2_gas) → compress and show
            // Basic format (gas: ~N) → strip (success handled via compact summary)
            if let Some(compressed) = compress_pass_gas_line(trimmed) {
                detail_lines.push(compressed);
            }
            i += 1;
            continue;
        }

        // --- [IGNORED] lines: strip ---
        if trimmed.starts_with("[IGNORED]") {
            i += 1;
            continue;
        }

        // --- [FAIL] lines: always keep ---
        if trimmed.starts_with("[FAIL]") {
            // Flush previous failure data
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
            current_failure_data.push(line.to_string());
            i += 1;
            continue;
        }

        // --- Tests summary: (global) ---
        if trimmed.starts_with("Tests summary:") {
            global_summary = Some(line.to_string());
            i += 1;
            continue;
        }

        // --- Tests: N passed, ... (per-package) ---
        if trimmed.starts_with("Tests:") && trimmed.contains("passed") {
            per_package_summaries.push(line.to_string());
            i += 1;
            continue;
        }

        i += 1;
    }

    // Flush trailing failure data
    if in_failure_data && !current_failure_data.is_empty() {
        if let Some(fail_line) = last_fail_line.take() {
            let mut block = vec![fail_line];
            block.append(&mut current_failure_data);
            failures.push(block.join("\n"));
        } else {
            failures.push(current_failure_data.join("\n"));
        }
    } else if let Some(fail_line) = last_fail_line {
        failures.push(fail_line);
    }

    // --- Build compact summary string ---
    let compact_summary = build_compact_summary(
        global_summary.as_deref(),
        &per_package_summaries,
        package_count,
    );

    // --- No failures ---
    if failures.is_empty() {
        let has_extra = !detail_lines.is_empty() || !gas_report_entries.is_empty();

        if !has_extra {
            // Basic mode: compact single line
            return compact_summary.unwrap_or_default();
        }

        // Extended mode: show compressed details + compact summary
        let mut result = String::new();
        for line in &detail_lines {
            result.push_str(line);
            result.push('\n');
        }
        if !gas_report_entries.is_empty() {
            result.push_str("Gas report:\n");
            for entry in &gas_report_entries {
                result.push_str("  ");
                result.push_str(entry);
                result.push('\n');
            }
        }
        if let Some(ref summary) = compact_summary {
            result.push_str(summary);
            result.push('\n');
        }
        return result.trim().to_string();
    }

    // --- Failures present ---
    let mut result = String::new();

    for failure in &failures {
        result.push_str(failure);
        result.push_str("\n\n");
    }

    if !gas_report_entries.is_empty() {
        result.push_str("Gas report:\n");
        for entry in &gas_report_entries {
            result.push_str("  ");
            result.push_str(entry);
            result.push('\n');
        }
        result.push('\n');
    }

    for line in &per_package_summaries {
        result.push_str(line);
        result.push('\n');
    }
    if let Some(ref summary) = global_summary {
        result.push_str(summary);
        result.push('\n');
    }

    result.trim().to_string()
}

/// Build compact `✓ snforge test: N passed (K packages)` from summary lines.
fn build_compact_summary(
    global: Option<&str>,
    per_pkg: &[String],
    package_count: usize,
) -> Option<String> {
    if let Some(summary) = global {
        if let Some(counts) = SnforgeTestCounts::parse(summary) {
            if counts.failed == 0 {
                return Some(counts.format_compact(package_count.max(1)));
            }
        }
    }

    if !per_pkg.is_empty() {
        let mut total = SnforgeTestCounts {
            passed: 0,
            failed: 0,
            ignored: 0,
            filtered_out: 0,
        };
        let mut all_parsed = true;
        for line in per_pkg {
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
            return Some(total.format_compact(package_count.max(per_pkg.len())));
        }
    }

    None
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

    // ── Gas report compression ────────────────────────────────────────────────

    #[test]
    fn test_filter_snforge_test_compresses_gas_report_tables() {
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
│                      │ max: 5678       │
│                      │ mean: 2345      │
│                      │ median: 2100    │
╰──────────────────────┴─────────────────╯

Tests summary: 2 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        // No raw ASCII art
        assert!(
            !result.contains('╭'),
            "should strip gas report table top border: {}",
            result
        );
        assert!(
            !result.contains('╰'),
            "should strip gas report table bottom border: {}",
            result
        );
        assert!(
            !result.contains('│'),
            "should strip gas report table borders: {}",
            result
        );
        // Compressed contract data is shown
        assert!(
            result.contains("ERC20::transfer"),
            "should show contract::function: {}",
            result
        );
        assert!(
            result.contains("min:1234"),
            "should show min stat: {}",
            result
        );
        assert!(
            result.contains("max:5678"),
            "should show max stat: {}",
            result
        );
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
    }

    // ── Detailed resources compression ────────────────────────────────────────

    #[test]
    fn test_filter_snforge_test_compresses_extended_pass_gas() {
        // Real format from `snforge test --detailed-resources`
        let input = r#"Collected 2 test(s) from falcon package
Running 2 test(s) from tests/
[PASS] falcon::test_hash_to_point (l1_gas: ~0, l1_data_gas: ~0, l2_gas: ~809044)
        sierra gas: 809044
        syscalls: ()

[PASS] falcon::test_packing (l1_gas: ~0, l1_data_gas: ~0, l2_gas: ~7330020)
        sierra gas: 7330020
        syscalls: ()

Tests: 2 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 2 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        // Test names are shown (compressed [PASS] kept)
        assert!(
            result.contains("test_hash_to_point"),
            "should show test name: {}",
            result
        );
        assert!(
            result.contains("test_packing"),
            "should show test name: {}",
            result
        );
        // l2 gas shown compressed
        assert!(
            result.contains("l2:~809044"),
            "should compress l2 gas: {}",
            result
        );
        // Zero l1 values stripped
        assert!(
            !result.contains("l1_gas:"),
            "should strip zero l1_gas: {}",
            result
        );
        assert!(
            !result.contains("l1_data_gas:"),
            "should strip zero l1_data_gas: {}",
            result
        );
        // sierra gas stripped (redundant)
        assert!(
            !result.contains("sierra gas:"),
            "should strip redundant sierra gas: {}",
            result
        );
        // Empty syscalls stripped
        assert!(
            !result.contains("syscalls:"),
            "should strip empty syscalls: {}",
            result
        );
        // Compact summary still shown
        assert!(
            result.contains("✓ snforge test"),
            "should show success: {}",
            result
        );
    }

    #[test]
    fn test_filter_snforge_test_compresses_non_empty_syscalls() {
        let input = r#"Collected 1 test(s) from falcon package
Running 1 test(s) from tests/
[PASS] falcon::test_contract_call (l1_gas: ~0, l1_data_gas: ~0, l2_gas: ~200000)
        sierra gas: 200000
        syscalls: call_contract: 2, deploy: 1
        builtins: range_check: 100

Tests: 1 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 1 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        // Non-empty syscalls compressed
        assert!(
            result.contains("syscalls:call_contract:2,deploy:1"),
            "should compress non-empty syscalls: {}",
            result
        );
        // Non-empty builtins compressed
        assert!(
            result.contains("builtins:range_check:100"),
            "should compress non-empty builtins: {}",
            result
        );
        // sierra gas still stripped
        assert!(
            !result.contains("sierra gas:"),
            "should strip sierra gas: {}",
            result
        );
    }

    #[test]
    fn test_filter_snforge_test_strips_no_contract_gas_message() {
        // Format from `snforge test --gas-report` when no contract calls
        let input = r#"Collected 3 test(s) from falcon package
Running 3 test(s) from src/
[PASS] falcon::test_1 (l1_gas: ~0, l1_data_gas: ~0, l2_gas: ~161320)
No contract gas usage data to display, no contract calls made.

[PASS] falcon::test_2 (l1_gas: ~0, l1_data_gas: ~0, l2_gas: ~428100)
No contract gas usage data to display, no contract calls made.

[PASS] falcon::test_3 (l1_gas: ~0, l1_data_gas: ~0, l2_gas: ~1058800)
No contract gas usage data to display, no contract calls made.

Tests: 3 passed, 0 failed, 0 ignored, 0 filtered out

Tests summary: 3 passed, 0 failed, 0 ignored, 0 filtered out
"#;
        let result = filter_snforge_test(input);
        assert!(
            !result.contains("No contract gas usage data"),
            "should strip no-contract-gas message: {}",
            result
        );
        assert!(
            result.contains("l2:~161320"),
            "should compress l2 gas: {}",
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
