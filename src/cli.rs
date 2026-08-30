//! Repository-scoped command-line access to Lemmalog memory.
//!
//! The CLI is deliberately a one-shot boundary: resolve one repository,
//! load its snapshot, perform one operation, save mutations, and exit.

use crate::{AgentMemory, MockExtractor};
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const USAGE: &str = "Usage:
  lemmalog                         start the interactive REPL
  lemmalog observe <path> <facts> [options]
  lemmalog query <path> <goal>
  lemmalog why <path> <fact>

Options for observe:
  --ts <unix-seconds>              fact validity/assertion time (default: now)
  --provenance <uri>               opaque evidence URI (repeatable)
  --captured-at <timestamp>        evidence capture time (default: now, RFC3339)

The path may name any file or directory inside a Git repository. The
repository snapshot is stored outside the checkout under the user data
directory, keyed by its Git remote (or canonical root for an unremoted repo).
";

#[derive(Debug)]
struct Repository {
    root: PathBuf,
    identity: String,
    snapshot: PathBuf,
    evidence: PathBuf,
}

pub fn run(args: impl IntoIterator<Item = String>) -> i32 {
    match command(args.into_iter().collect()) {
        Ok(output) => {
            if !output.is_empty() {
                println!("{output}");
            }
            0
        }
        Err(error) => {
            eprintln!("lemmalog: {error}");
            eprintln!("{USAGE}");
            2
        }
    }
}

fn command(args: Vec<String>) -> Result<String, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err("missing command".to_string());
    };
    match command {
        "help" | "--help" | "-h" => Ok(USAGE.to_string()),
        "observe" => observe(&args[1..]),
        "query" => query(&args[1..], false),
        "why" => why(&args[1..]),
        other => Err(format!("unknown command {other:?}")),
    }
}

fn observe(args: &[String]) -> Result<String, String> {
    if args.len() < 2 {
        return Err("observe requires <path> and one quoted facts argument".to_string());
    }
    let repo = repository(Path::new(&args[0]))?;
    let facts = &args[1];
    let mut ts = current_unix_seconds();
    let mut uris = Vec::new();
    let mut captured_at = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--ts" => {
                i += 1;
                ts = args
                    .get(i)
                    .ok_or("--ts requires an integer")?
                    .parse()
                    .map_err(|_| "--ts requires an integer".to_string())?;
            }
            "--provenance" => {
                i += 1;
                let uri = args.get(i).ok_or("--provenance requires a URI")?;
                if uri.contains(['\t', '\n', '\r']) {
                    return Err("provenance URI cannot contain whitespace".to_string());
                }
                uris.push(uri.clone());
            }
            "--captured-at" => {
                i += 1;
                let value = args.get(i).ok_or("--captured-at requires a timestamp")?;
                if value.contains(['\t', '\n', '\r']) {
                    return Err("capture timestamp cannot contain whitespace".to_string());
                }
                captured_at = Some(value.clone());
            }
            other => return Err(format!("unknown observe option {other:?}")),
        }
        i += 1;
    }
    if captured_at.is_some() && uris.is_empty() {
        return Err("--captured-at requires at least one --provenance URI".to_string());
    }

    let mut memory = load_memory(&repo)?;
    let (report, dropped) = memory.observe_extracted_with_provenance(facts, ts, &uris);
    memory.maintain(ts);
    if let Some(parent) = repo.snapshot.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create data directory: {e}"))?;
    }
    memory
        .save(path_string(&repo.snapshot).as_str())
        .map_err(|e| format!("save snapshot: {e}"))?;

    let captured_at = if uris.is_empty() {
        None
    } else {
        Some(captured_at.unwrap_or_else(current_rfc3339))
    };
    if let Some(captured_at) = captured_at {
        let capture_time = captured_at;
        let mut evidence = read_evidence(&repo.evidence)?;
        for uri in &uris {
            evidence.insert(uri.clone(), capture_time.clone());
        }
        write_evidence(&repo.evidence, &evidence)?;
    }

    let mut output = format!(
        "repository={}\nroot={}\nadded={} updated={} noop={} escalations={}\nsnapshot={}",
        repo.identity,
        repo.root.display(),
        report.added,
        report.updated,
        report.noop,
        report.escalations.len(),
        repo.snapshot.display()
    );
    for escalation in report.escalations {
        output.push_str(&format!("\nescalation: {escalation}"));
    }
    for (line, reason) in dropped {
        output.push_str(&format!("\ndropped `{line}` — {reason}"));
    }
    Ok(output)
}

fn query(args: &[String], deep: bool) -> Result<String, String> {
    if args.len() != 2 {
        return Err("query requires <path> and one quoted goal argument".to_string());
    }
    let repo = repository(Path::new(&args[0]))?;
    let mut memory = load_memory(&repo)?;
    let rows = if deep {
        memory
            .ask_deep(&args[1])
            .map_err(|e| format!("query: {e}"))?
    } else {
        memory.ask(&args[1]).map_err(|e| format!("query: {e}"))?
    };
    Ok(if rows.is_empty() {
        "(no answers)".to_string()
    } else {
        rows.join("\n")
    })
}

fn why(args: &[String]) -> Result<String, String> {
    if args.len() != 2 {
        return Err("why requires <path> and one quoted fact argument".to_string());
    }
    let repo = repository(Path::new(&args[0]))?;
    let memory = load_memory(&repo)?;
    let proof = memory.why(&args[1]);
    let evidence = read_evidence(&repo.evidence)?;
    let mut output = proof;
    for (uri, captured_at) in evidence {
        if output.contains(&uri) {
            output.push_str(&format!("evidence: {uri} (captured_at={captured_at})\n"));
        }
    }
    Ok(output.trim_end().to_string())
}

fn repository(path: &Path) -> Result<Repository, String> {
    let path = path
        .canonicalize()
        .map_err(|e| format!("cannot resolve path {}: {e}", path.display()))?;
    let git_dir = if path.is_dir() {
        path
    } else {
        path.parent()
            .ok_or_else(|| format!("path {} has no parent", path.display()))?
            .to_path_buf()
    };
    let root = git(&git_dir, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim());
    let identity = git(&root, &["remote", "get-url", "origin"])
        .ok()
        .map(|remote| remote.trim().to_string())
        .filter(|remote| !remote.is_empty())
        .unwrap_or_else(|| root.display().to_string());
    let data_root = data_root()?;
    let key = stable_key(&identity);
    let directory = data_root.join("lemmalog").join("repositories");
    Ok(Repository {
        root,
        identity,
        snapshot: directory.join(format!("{key}.snapshot")),
        evidence: directory.join(format!("{key}.evidence")),
    })
}

fn load_memory(repo: &Repository) -> Result<AgentMemory<MockExtractor>, String> {
    if repo.snapshot.exists() {
        AgentMemory::load(
            MockExtractor::new(0.9),
            path_string(&repo.snapshot).as_str(),
        )
        .map_err(|e| format!("load snapshot: {e}"))
    } else {
        AgentMemory::new(MockExtractor::new(0.9), "").map_err(|e| format!("create memory: {e}"))
    }
}

fn data_root() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local").join("share"))
}

fn git(directory: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .map_err(|e| format!("run git: {e}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if error.is_empty() {
            format!("git command failed: {args:?}")
        } else {
            error
        });
    }
    String::from_utf8(output.stdout).map_err(|e| format!("git output is not UTF-8: {e}"))
}

fn stable_key(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn read_evidence(path: &Path) -> Result<BTreeMap<String, String>, String> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("read evidence: {e}"))?;
    let mut entries = BTreeMap::new();
    for line in text.lines() {
        let Some((uri, captured_at)) = line.split_once('\t') else {
            return Err(format!("malformed evidence record in {}", path.display()));
        };
        entries.insert(uri.to_string(), captured_at.to_string());
    }
    Ok(entries)
}

fn write_evidence(path: &Path, entries: &BTreeMap<String, String>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create data directory: {e}"))?;
    }
    let mut text = String::new();
    for (uri, captured_at) in entries {
        text.push_str(uri);
        text.push('\t');
        text.push_str(captured_at);
        text.push('\n');
    }
    std::fs::write(path, text).map_err(|e| format!("write evidence: {e}"))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn current_rfc3339() -> String {
    let seconds = current_unix_seconds();
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;

    // Civil-date conversion from days since 1970-01-01.
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }).div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_prime = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * month_prime + 2).div_euclid(5) + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::{current_rfc3339, current_unix_seconds, stable_key};

    #[test]
    fn repository_keys_are_stable() {
        assert_eq!(
            stable_key("github://acme/payments"),
            stable_key("github://acme/payments")
        );
        assert_ne!(stable_key("a"), stable_key("b"));
    }

    #[test]
    fn current_time_is_unix_time() {
        assert!(current_unix_seconds() > 1_000_000_000);
    }

    #[test]
    fn current_time_is_rfc3339() {
        let timestamp = current_rfc3339();
        assert_eq!(timestamp.len(), 20);
        assert_eq!(&timestamp[10..11], "T");
        assert_eq!(&timestamp[19..20], "Z");
    }
}
