use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("lemmalog-cli-{name}-{nonce}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git command failed: {args:?}");
}

fn repository(root: &Path, remote: &str) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn example() {}\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["remote", "add", "origin", remote]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lemmalog"))
}

#[test]
fn cli_persists_and_explains_repository_facts_from_outside_checkout() {
    let root = temp_root("persistence");
    let repo = repository(&root, "git@github.com:example/persistence.git");
    let outside = root.join("outside");
    let data = root.join("data");
    std::fs::create_dir(&outside).unwrap();
    let provenance = "github://example/persistence/abc123/src/lib.rs#L1-L1";

    let observed = cli()
        .current_dir(&outside)
        .env("XDG_DATA_HOME", &data)
        .args([
            "observe",
            repo.join("src/lib.rs").to_str().unwrap(),
            "alice --works_at--> acme",
            "--ts",
            "100",
            "--provenance",
            provenance,
            "--captured-at",
            "2026-08-29T19:42:00Z",
        ])
        .output()
        .unwrap();
    assert!(
        observed.status.success(),
        "{}",
        String::from_utf8_lossy(&observed.stderr)
    );
    let observed_text = String::from_utf8(observed.stdout).unwrap();
    assert!(observed_text
        .lines()
        .any(|line| line == "added=1 updated=0 noop=0 escalations=0"));

    let queried = cli()
        .current_dir(&outside)
        .env("XDG_DATA_HOME", &data)
        .args([
            "query",
            repo.to_str().unwrap(),
            "current(\"alice\", \"works_at\", O)",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(queried.stdout).unwrap().trim(), "O=acme");

    let explained = cli()
        .current_dir(&outside)
        .env("XDG_DATA_HOME", &data)
        .args([
            "why",
            repo.to_str().unwrap(),
            "current(alice, works_at, acme)",
        ])
        .output()
        .unwrap();
    let explanation = String::from_utf8(explained.stdout).unwrap();
    assert!(explanation.lines().any(|line| {
        line == format!("evidence: {provenance} (captured_at=2026-08-29T19:42:00Z)")
    }));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cli_keeps_repository_snapshots_isolated() {
    let root = temp_root("isolation");
    let first = repository(&root.join("first"), "git@github.com:example/first.git");
    let second = repository(&root.join("second"), "git@github.com:example/second.git");
    let data = root.join("data");

    for repo in [&first, &second] {
        let observed = cli()
            .env("XDG_DATA_HOME", &data)
            .args([
                "observe",
                repo.to_str().unwrap(),
                "alice --works_at--> acme",
                "--ts",
                "100",
            ])
            .output()
            .unwrap();
        assert!(
            observed.status.success(),
            "{}",
            String::from_utf8_lossy(&observed.stderr)
        );
    }

    let other_fact = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "observe",
            first.to_str().unwrap(),
            "alice --likes--> tea",
            "--ts",
            "101",
        ])
        .output()
        .unwrap();
    assert!(other_fact.status.success());

    let isolated = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "query",
            second.to_str().unwrap(),
            "current(\"alice\", \"likes\", O)",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(isolated.stdout).unwrap().trim(),
        "(no answers)"
    );

    std::fs::remove_dir_all(root).unwrap();
}
