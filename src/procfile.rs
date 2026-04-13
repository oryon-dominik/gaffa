use std::collections::HashMap;
use std::time::Duration;

use regex::Regex;

use crate::constants::PROCESS_COLORS;
use crate::types::*;

/// Result of parsing a Procfile: process definitions, color assignments, and max name length.
pub struct ProcfileData {
    pub processes: HashMap<String, ProcessInfo>,
    pub colors: HashMap<String, colored::Color>,
    pub max_name_length: usize,
}

/// Parse a Procfile from its content string.
///
/// Returns process definitions with assigned colors, or an error if
/// the format is invalid or no valid processes are found.
///
/// # Panics
///
/// Panics if the regex pattern is invalid (should never happen with hardcoded pattern).
pub fn parse_procfile(procfile_path: &str) -> Result<ProcfileData> {
    let content =
        std::fs::read_to_string(procfile_path).map_err(|e| ProcessError::ProcfileRead {
            path: procfile_path.to_string(),
            source: e,
        })?;

    let re = Regex::new(r"^(\w+):\s+(.*)$").expect("Valid regex pattern");
    let mut processes = HashMap::new();
    let mut colors = HashMap::new();
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    let mut color_index = 0;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(caps) = re.captures(line) {
            let base_name = caps[1].to_string();
            let command = caps[2].to_string();

            // Handle duplicate names by appending a number
            let count = name_counts.entry(base_name.clone()).or_insert(0);
            *count += 1;

            let name = if *count == 1 {
                base_name.clone()
            } else {
                format!("{base_name}.{count}")
            };

            processes.insert(
                name.clone(),
                ProcessInfo {
                    command,
                    status: ProcessStatus::Stopped,
                    restart_count: 0,
                    last_restart: None,
                    stopped_at: None,
                    cumulative_runtime: Duration::ZERO,
                    exit_code: None,
                },
            );

            colors.insert(name, PROCESS_COLORS[color_index % PROCESS_COLORS.len()]);
            color_index += 1;
        } else {
            return Err(ProcessError::InvalidFormat {
                line: line.to_string(),
            });
        }
    }

    if processes.is_empty() {
        return Err(ProcessError::NoProcesses);
    }

    let max_name_length = processes.keys().map(|name| name.len()).max().unwrap_or(0);

    Ok(ProcfileData {
        processes,
        colors,
        max_name_length,
    })
}

/// Parse environment variables from a file.
pub async fn parse_env_file(path: &str, env_vars: &mut HashMap<String, String>) -> Result<()> {
    let contents =
        tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ProcessError::ProcfileRead {
                path: path.to_string(),
                source: e,
            })?;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            env_vars.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    Ok(())
}
