use std::collections::HashMap;
use std::path::Path;
use tokio::process::Command;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct DirectiveEntry {
    pub name: String,
    pub usage: String,
    pub description: String,
    pub applicable_protocols: Option<Vec<String>>,
    pub global_only: bool,
    pub subblock_link: Option<String>,
}

#[derive(Debug)]
pub struct DirectiveRegistry {
    pub sections: HashMap<String, Vec<DirectiveEntry>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ValidationDiagnostic {
    pub kind: String,
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ValidationResult {
    pub valid: bool,
    pub diagnostics: Vec<ValidationDiagnostic>,
}

#[derive(Debug, Deserialize)]
struct SerdeDiagnosticSpan {
    line: Option<usize>,
    column: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SerdeDiagnostic {
    kind: String,
    message: String,
    span: Option<SerdeDiagnosticSpan>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SerdeValidationResult {
    valid: bool,
    diagnostics: Vec<SerdeDiagnostic>,
}

#[derive(Debug, Deserialize)]
struct SerdeDirectiveEntry {
    name: String,
    usage: String,
    description: String,
    applicable_protocols: Option<Vec<String>>,
    global_only: bool,
    subblock_link: Option<String>,
}

impl DirectiveRegistry {
    pub async fn fetch(path_to_ferron: &Path) -> Self {
        let ferron_bin = path_to_ferron.join("ferron");
        let output = match Command::new(&ferron_bin).arg("directives").output().await {
            Ok(o) => o,
            Err(_) => return Self::empty(),
        };
        if !output.status.success() {
            return Self::empty();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Self::from_json(&stdout)
    }

    pub fn from_json(json_str: &str) -> Self {
        let parsed: HashMap<String, Vec<SerdeDirectiveEntry>> = match serde_json::from_str(json_str)
        {
            Ok(p) => p,
            Err(_) => return Self::empty(),
        };

        let mut sections = HashMap::new();

        for (section, entries) in parsed {
            let converted: Vec<DirectiveEntry> = entries
                .into_iter()
                .map(|e| DirectiveEntry {
                    name: e.name,
                    usage: e.usage,
                    description: e.description,
                    applicable_protocols: e.applicable_protocols,
                    global_only: e.global_only,
                    subblock_link: e.subblock_link,
                })
                .collect();

            sections.insert(section, converted);
        }

        Self { sections }
    }

    pub fn empty() -> Self {
        Self {
            sections: HashMap::new(),
        }
    }
}

pub async fn run_doctor(path_to_ferron: &Path, config_path: &str) -> ValidationResult {
    let ferron_bin = path_to_ferron.join("ferron");
    let output = match Command::new(&ferron_bin)
        .args([
            "doctor",
            "--json",
            "-c",
            config_path,
            "--config-adapter",
            "ferronconf", // Force configuration adapter to use ferronconf
        ])
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => {
            return ValidationResult {
                valid: true,
                diagnostics: Vec::new(),
            };
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Use .trim(), since stdout might have leading/trailing whitespace
    let parsed: SerdeValidationResult = match serde_json::from_str(stdout.trim()) {
        Ok(p) => p,
        Err(_) => {
            return ValidationResult {
                valid: output.status.success(),
                diagnostics: Vec::new(),
            };
        }
    };
    let diagnostics: Vec<ValidationDiagnostic> = parsed
        .diagnostics
        .into_iter()
        .map(|d| ValidationDiagnostic {
            kind: d.kind,
            message: d.message,
            line: d.span.as_ref().and_then(|s| s.line),
            column: d.span.as_ref().and_then(|s| s.column),
            scope: d.scope,
        })
        .collect();
    ValidationResult {
        valid: parsed.valid,
        diagnostics,
    }
}
