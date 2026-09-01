//! Workspace-aware command-line access to Lemmalog memory.
//!
//! The CLI is deliberately a one-shot boundary: resolve one workspace,
//! load its snapshot, perform one operation, save mutations, and exit.

use crate::{AgentMemory, MockExtractor};
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const EMBEDDED_SKILL: &str = include_str!("../skills/lemmalog/SKILL.md");

const USAGE: &str = "Usage:
  lemmalog                         start the interactive REPL
  lemmalog observe <path> <facts> [options]
  lemmalog search <path> <pattern> [options]
  lemmalog query <path> <goal> [options]
  lemmalog why <path> <fact> [options]
  lemmalog skill install [--path <skills-directory>]

Workspace and scope options:
  --workspace <path>               override .lemmalog workspace discovery
  --scope <repository|workspace>   observation scope (default: repository)
  --scope <all|repository|workspace>
                                   search/query/why visibility (default: all)

Other search options:
  --limit <positive-integer>       maximum result rows (default: 50)

Other observe options:
  --ts <unix-seconds>              fact validity/assertion time (default: now)
  --provenance <uri>               opaque evidence URI (repeatable)
  --captured-at <timestamp>        evidence capture time (default: now, RFC3339)

The target path may name any file or directory inside a Git repository.
Lemmalog uses an explicit workspace override, the nearest ancestor .lemmalog
marker, or the Git root fallback, in that order. Stores remain outside the
workspace under the user data directory. A marker contains id = \"stable-id\";
an override names that marker or its directory. A query goal may be one atom
or a temporary Datalog rule whose head is returned.
";

#[derive(Debug)]
struct Repository {
    root: PathBuf,
    identity: String,
}

#[derive(Debug)]
struct Workspace {
    root: PathBuf,
    identity: String,
    marked: bool,
    snapshot: PathBuf,
    evidence: PathBuf,
}

#[derive(Debug)]
struct Context {
    repository: Repository,
    workspace: Workspace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scope {
    All,
    Repository,
    Workspace,
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
        "search" => search(&args[1..]),
        "query" => query(&args[1..], false),
        "why" => why(&args[1..]),
        "skill" => skill(&args[1..]),
        other => Err(format!("unknown command {other:?}")),
    }
}

fn search(args: &[String]) -> Result<String, String> {
    let (path, pattern, workspace_override, scope, limit) = parse_search_args(args)?;
    let context = resolve_context(&path, workspace_override.as_deref())?;
    let repository_scope = scope_name(&context, Scope::Repository);
    let (selected_scopes, include_legacy) = match scope {
        Scope::All => (None, true),
        Scope::Workspace => (Some(["workspace".to_string()].into_iter().collect()), false),
        Scope::Repository => (
            Some(
                ["workspace".to_string(), repository_scope.clone()]
                    .into_iter()
                    .collect(),
            ),
            !context.workspace.marked,
        ),
    };
    let result = crate::search::search_snapshot(
        &context.workspace.snapshot,
        &pattern,
        limit,
        selected_scopes,
        repository_scope,
        include_legacy,
    )
    .map_err(|error| format!("search: {error}"))?;
    if result.rows.is_empty() {
        return Ok("(no answers)".to_string());
    }
    let mut output = result.rows.join("\n");
    if result.truncated {
        output.push_str(&format!("\ntruncated: limit ({limit} shown, more matched)"));
    }
    Ok(output)
}

fn skill(args: &[String]) -> Result<String, String> {
    if args.first().map(String::as_str) != Some("install") {
        return Err("skill requires install".to_string());
    }
    let mut skills_directory = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--path" => {
                index += 1;
                skills_directory = Some(PathBuf::from(
                    args.get(index)
                        .ok_or("--path requires a skills directory")?,
                ));
            }
            option => return Err(format!("unknown skill install option {option:?}")),
        }
        index += 1;
    }
    let skills_directory = match skills_directory {
        Some(path) => path,
        None => PathBuf::from(env::var_os("HOME").ok_or("HOME is not set")?)
            .join(".agents")
            .join("skills"),
    };
    let directory = skills_directory.join("lemmalog");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create skill directory {}: {error}", directory.display()))?;
    let destination = directory.join("SKILL.md");
    std::fs::write(&destination, EMBEDDED_SKILL)
        .map_err(|error| format!("install skill {}: {error}", destination.display()))?;
    Ok(format!("skill={}", destination.display()))
}

fn observe(args: &[String]) -> Result<String, String> {
    let mut positional = Vec::new();
    let mut ts = current_unix_seconds();
    let mut uris = Vec::new();
    let mut captured_at = None;
    let mut workspace_override = None;
    let mut scope = Scope::Repository;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" => {
                i += 1;
                workspace_override = Some(PathBuf::from(
                    args.get(i).ok_or("--workspace requires a path")?,
                ));
            }
            "--scope" => {
                i += 1;
                scope = parse_scope(args.get(i).ok_or("--scope requires a value")?, false)?;
            }
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
            other if other.starts_with('-') => {
                return Err(format!("unknown observe option {other:?}"));
            }
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    if positional.len() != 2 {
        return Err("observe requires <path> and one quoted facts argument".to_string());
    }
    if captured_at.is_some() && uris.is_empty() {
        return Err("--captured-at requires at least one --provenance URI".to_string());
    }

    let context = resolve_context(Path::new(&positional[0]), workspace_override.as_deref())?;
    let mut memory = load_memory(&context.workspace)?;
    let scope_name = scope_name(&context, scope);
    let (report, dropped) = if context.workspace.marked || scope == Scope::Workspace {
        memory.observe_scoped_extracted_with_provenance(&positional[1], ts, &scope_name, &uris)
    } else {
        memory.observe_extracted_with_provenance(&positional[1], ts, &uris)
    };
    memory.maintain(ts);
    if let Some(parent) = context.workspace.snapshot.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create data directory: {e}"))?;
    }
    memory
        .save(path_string(&context.workspace.snapshot).as_str())
        .map_err(|e| format!("save snapshot: {e}"))?;

    let captured_at = if uris.is_empty() {
        None
    } else {
        Some(captured_at.unwrap_or_else(current_rfc3339))
    };
    if let Some(captured_at) = captured_at {
        let capture_time = captured_at;
        let mut evidence = read_evidence(&context.workspace.evidence)?;
        for uri in &uris {
            evidence.insert(uri.clone(), capture_time.clone());
        }
        write_evidence(&context.workspace.evidence, &evidence)?;
    }

    let mut output = format!(
        "workspace={}\nworkspace_root={}\nrepository={}\nroot={}\nscope={}\nadded={} updated={} noop={} escalations={}\nsnapshot={}",
        context.workspace.identity,
        context.workspace.root.display(),
        context.repository.identity,
        context.repository.root.display(),
        scope_name,
        report.added,
        report.updated,
        report.noop,
        report.escalations.len(),
        context.workspace.snapshot.display()
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
    let (path, goal, workspace_override, scope) = parse_read_args(args, "query")?;
    let context = resolve_context(&path, workspace_override.as_deref())?;
    let mut memory = load_memory(&context.workspace)?;
    restrict_scope(&mut memory, &context, scope);
    let rows = if deep {
        memory.ask_deep(&goal).map_err(|e| format!("query: {e}"))?
    } else {
        ask_expression(&mut memory, &goal)?
    };
    Ok(if rows.is_empty() {
        "(no answers)".to_string()
    } else {
        rows.join("\n")
    })
}

fn why(args: &[String]) -> Result<String, String> {
    let (path, fact, workspace_override, scope) = parse_read_args(args, "why")?;
    let context = resolve_context(&path, workspace_override.as_deref())?;
    let mut memory = load_memory(&context.workspace)?;
    restrict_scope(&mut memory, &context, scope);
    let proof = memory.why(&fact);
    let evidence = read_evidence(&context.workspace.evidence)?;
    let mut output = proof;
    for (uri, captured_at) in evidence {
        if output.contains(&uri) {
            output.push_str(&format!("evidence: {uri} (captured_at={captured_at})\n"));
        }
    }
    Ok(output.trim_end().to_string())
}

fn resolve_context(path: &Path, workspace_override: Option<&Path>) -> Result<Context, String> {
    let path = path
        .canonicalize()
        .map_err(|e| format!("cannot resolve path {}: {e}", path.display()))?;
    let git_dir = if path.is_dir() {
        path.clone()
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
    let repository = Repository { root, identity };
    let marker = match workspace_override {
        Some(path) => Some(explicit_marker(path)?),
        None => find_marker(&path),
    };
    let data_root = data_root()?;
    let workspace = if let Some(marker) = marker {
        let id = read_workspace_id(&marker)?;
        let identity = format!("workspace:{id}");
        let key = stable_key(&identity);
        let directory = data_root.join("lemmalog").join("workspaces");
        Workspace {
            root: marker
                .parent()
                .expect("workspace marker has parent")
                .to_path_buf(),
            identity,
            marked: true,
            snapshot: directory.join(format!("{key}.snapshot")),
            evidence: directory.join(format!("{key}.evidence")),
        }
    } else {
        let key = stable_key(&repository.identity);
        let directory = data_root.join("lemmalog").join("repositories");
        Workspace {
            root: repository.root.clone(),
            identity: repository.identity.clone(),
            marked: false,
            snapshot: directory.join(format!("{key}.snapshot")),
            evidence: directory.join(format!("{key}.evidence")),
        }
    };
    Ok(Context {
        repository,
        workspace,
    })
}

fn load_memory(workspace: &Workspace) -> Result<AgentMemory<MockExtractor>, String> {
    if workspace.snapshot.exists() {
        AgentMemory::load(
            MockExtractor::new(0.9),
            path_string(&workspace.snapshot).as_str(),
        )
        .map_err(|e| format!("load snapshot: {e}"))
    } else {
        AgentMemory::new(MockExtractor::new(0.9), "").map_err(|e| format!("create memory: {e}"))
    }
}

fn parse_scope(value: &str, allow_all: bool) -> Result<Scope, String> {
    match value {
        "all" if allow_all => Ok(Scope::All),
        "repository" => Ok(Scope::Repository),
        "workspace" => Ok(Scope::Workspace),
        _ if allow_all => Err("--scope must be all, repository, or workspace".to_string()),
        _ => Err("--scope must be repository or workspace".to_string()),
    }
}

fn parse_read_args(
    args: &[String],
    command: &str,
) -> Result<(PathBuf, String, Option<PathBuf>, Scope), String> {
    let mut positional = Vec::new();
    let mut workspace = None;
    let mut scope = Scope::All;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                index += 1;
                workspace = Some(PathBuf::from(
                    args.get(index).ok_or("--workspace requires a path")?,
                ));
            }
            "--scope" => {
                index += 1;
                scope = parse_scope(args.get(index).ok_or("--scope requires a value")?, true)?;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown {command} option {option:?}"));
            }
            _ => positional.push(args[index].clone()),
        }
        index += 1;
    }
    if positional.len() != 2 {
        return Err(format!(
            "{command} requires <path> and one quoted {} argument",
            if command == "why" { "fact" } else { "goal" }
        ));
    }
    Ok((
        PathBuf::from(&positional[0]),
        positional.remove(1),
        workspace,
        scope,
    ))
}

fn parse_search_args(
    args: &[String],
) -> Result<(PathBuf, String, Option<PathBuf>, Scope, usize), String> {
    let mut positional = Vec::new();
    let mut workspace = None;
    let mut scope = Scope::All;
    let mut limit = 50usize;
    let mut options = true;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--" if options => options = false,
            "--workspace" if options => {
                index += 1;
                workspace = Some(PathBuf::from(
                    args.get(index).ok_or("--workspace requires a path")?,
                ));
            }
            "--scope" if options => {
                index += 1;
                scope = parse_scope(args.get(index).ok_or("--scope requires a value")?, true)?;
            }
            "--limit" if options => {
                index += 1;
                limit = args
                    .get(index)
                    .ok_or("--limit requires a positive integer")?
                    .parse()
                    .map_err(|_| "--limit requires a positive integer".to_string())?;
                if limit == 0 {
                    return Err("--limit requires a positive integer".to_string());
                }
            }
            option if options && option.starts_with('-') => {
                return Err(format!("unknown search option {option:?}"));
            }
            _ => positional.push(args[index].clone()),
        }
        index += 1;
    }
    if positional.len() != 2 {
        return Err("search requires <path> and one quoted pattern argument".to_string());
    }
    Ok((
        PathBuf::from(&positional[0]),
        positional.remove(1),
        workspace,
        scope,
        limit,
    ))
}

fn scope_name(context: &Context, scope: Scope) -> String {
    match scope {
        Scope::Workspace => "workspace".to_string(),
        Scope::Repository => format!("repository:{}", context.repository.identity),
        Scope::All => unreachable!("all is not an observation scope"),
    }
}

fn restrict_scope(memory: &mut AgentMemory<MockExtractor>, context: &Context, scope: Scope) {
    match scope {
        Scope::All => {}
        Scope::Workspace => {
            memory.retain_scopes(&["workspace".to_string()]);
            memory.remove_unscoped_facts();
        }
        Scope::Repository => {
            memory.retain_scopes(&[
                "workspace".to_string(),
                scope_name(context, Scope::Repository),
            ]);
            if context.workspace.marked {
                memory.remove_unscoped_facts();
            }
        }
    }
}

fn ask_expression(
    memory: &mut AgentMemory<MockExtractor>,
    expression: &str,
) -> Result<Vec<String>, String> {
    let Some((head, _)) = expression.split_once(":-") else {
        return memory.ask(expression).map_err(|e| format!("query: {e}"));
    };
    let head = head.trim();
    if head.is_empty() {
        return Err("query rule requires a head atom".to_string());
    }
    let rule = if expression.trim_end().ends_with('.') {
        expression.to_string()
    } else {
        format!("{expression}.")
    };
    memory
        .install_rules(&rule)
        .map_err(|e| format!("query rule: {e}"))?;
    memory.maintain(memory.engine.now);
    memory.ask(head).map_err(|e| format!("query: {e}"))
}

fn find_marker(path: &Path) -> Option<PathBuf> {
    let mut directory = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    loop {
        let marker = directory.join(".lemmalog");
        if marker.is_file() {
            return Some(marker);
        }
        if !directory.pop() {
            return None;
        }
    }
}

fn explicit_marker(path: &Path) -> Result<PathBuf, String> {
    let path = path
        .canonicalize()
        .map_err(|e| format!("cannot resolve workspace {}: {e}", path.display()))?;
    let marker = if path.is_dir() {
        path.join(".lemmalog")
    } else {
        path
    };
    if marker.file_name().and_then(|name| name.to_str()) != Some(".lemmalog") || !marker.is_file() {
        return Err(format!(
            "workspace override {} has no .lemmalog marker",
            marker.display()
        ));
    }
    Ok(marker)
}

fn read_workspace_id(marker: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(marker)
        .map_err(|e| format!("read workspace marker {}: {e}", marker.display()))?;
    let id = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .find_map(|line| {
            let value = line
                .strip_prefix("id")?
                .trim_start()
                .strip_prefix('=')?
                .trim();
            let value = value.strip_prefix('"')?.strip_suffix('"')?;
            Some(value.to_string())
        })
        .ok_or_else(|| {
            format!(
                "workspace marker {} requires id = \"...\"",
                marker.display()
            )
        })?;
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(format!(
            "workspace marker {} has invalid id {id:?}",
            marker.display()
        ));
    }
    Ok(id)
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
            stable_key("https://github.com/acme/payments"),
            stable_key("https://github.com/acme/payments")
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
