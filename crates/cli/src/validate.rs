use agent_computer_definitions::{Format, MAX_DOCUMENT_BYTES, ValidationReport, validate_bytes};
use std::io::{self, Read};

pub fn run(args: &[&str]) -> u8 {
    let json = args.contains(&"--json");
    let options = parse_args(args);
    let (file, format) = match options {
        Some(options) => options,
        None => {
            emit(
                &ValidationReport::failure(
                    "usage",
                    "Usage: agent-computer validate <file|-> [--format yaml|json] [--json]",
                ),
                json,
            );
            return 2;
        }
    };
    let source: Box<dyn Read> = if file == "-" {
        Box::new(io::stdin())
    } else {
        // Bazel runs binaries from runfiles, but user input paths belong to the
        // original invocation directory. Installed binaries retain normal cwd semantics.
        let mut path = std::path::PathBuf::from(file);
        if path.is_relative()
            && let Some(directory) = std::env::var_os("BUILD_WORKING_DIRECTORY")
        {
            path = std::path::PathBuf::from(directory).join(path);
        }
        match std::fs::File::open(path) {
            Ok(file) => Box::new(file),
            Err(_) => {
                emit(
                    &ValidationReport::failure(
                        "input_unavailable",
                        "Unable to open the declaration file.",
                    ),
                    json,
                );
                return 2;
            }
        }
    };
    let mut input = Vec::new();
    if source
        .take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .is_err()
    {
        emit(
            &ValidationReport::failure("input_unavailable", "Unable to read the declaration."),
            json,
        );
        return 2;
    }
    match validate_bytes(&input, format) {
        Ok(definition) => {
            emit(definition.report(), json);
            0
        }
        Err(report) => {
            emit(&report, json);
            1
        }
    }
}

fn parse_args<'a>(args: &'a [&'a str]) -> Option<(&'a str, Format)> {
    let mut file = None;
    let mut format = None;
    let mut json_seen = false;
    let mut args = args.iter();
    while let Some(&arg) = args.next() {
        match arg {
            "--json" if !json_seen => json_seen = true,
            "--format" if format.is_none() => {
                format = Some(match *args.next()? {
                    "yaml" => Format::Yaml,
                    "json" => Format::Json,
                    _ => return None,
                });
            }
            _ if file.is_none() && (arg == "-" || !arg.starts_with('-')) => file = Some(arg),
            _ => return None,
        }
    }
    let file = file?;
    let inferred = if file.ends_with(".json") {
        Format::Json
    } else {
        Format::Yaml
    };
    Some((file, format.unwrap_or(inferred)))
}

fn emit(report: &ValidationReport, json: bool) {
    if json {
        // This report contains only bounded diagnostics and references, never source contents.
        println!(
            "{}",
            serde_json::to_string(report).expect("validation reports are JSON-serializable")
        );
    } else if report.valid {
        println!(
            "valid (static): {} resources; {}",
            report.resource_count,
            report.definition_digest.as_deref().unwrap_or("")
        );
        println!(
            "{} external references require server-side resolution and authorization.",
            report.external_references.len()
        );
    } else {
        for diagnostic in &report.diagnostics {
            eprintln!(
                "{}: {}: {}",
                diagnostic.path, diagnostic.code, diagnostic.message
            );
        }
        if report.diagnostics_truncated {
            eprintln!("Additional diagnostics omitted.");
        }
    }
}
