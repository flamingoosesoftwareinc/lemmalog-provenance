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
fn cli_installs_embedded_skill_to_default_and_custom_directories() {
    let root = temp_root("skill-install");
    let installed = cli()
        .env("HOME", &root)
        .args(["skill", "install"])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let default_destination = root.join(".agents/skills/lemmalog/SKILL.md");
    assert_eq!(
        String::from_utf8(installed.stdout).unwrap().trim(),
        format!("skill={}", default_destination.display())
    );
    assert_eq!(
        std::fs::read_to_string(&default_destination).unwrap(),
        include_str!("../skills/lemmalog/SKILL.md")
    );

    let custom = root.join("custom-skills");
    let updated = cli()
        .env("HOME", root.join("unused-home"))
        .args(["skill", "install", "--path", custom.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(updated.status.success());
    assert_eq!(
        std::fs::read_to_string(custom.join("lemmalog/SKILL.md")).unwrap(),
        include_str!("../skills/lemmalog/SKILL.md")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cli_persists_and_explains_repository_facts_from_outside_checkout() {
    let root = temp_root("persistence");
    let repo = repository(&root, "git@github.com:example/persistence.git");
    let outside = root.join("outside");
    let data = root.join("data");
    std::fs::create_dir(&outside).unwrap();
    let provenance = "https://github.com/example/persistence/blob/abc123/src/lib.rs#L1";

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

#[test]
fn marked_workspace_joins_repositories_and_preserves_scope() {
    let root = temp_root("workspace");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join(".lemmalog"), "id = \"billing-platform\"\n").unwrap();
    let first = repository(&workspace.join("first"), "git@github.com:example/first.git");
    let second = repository(
        &workspace.join("second"),
        "git@github.com:example/second.git",
    );
    let data = root.join("data");

    let observations = [
        (&first, "service_a --emits--> payment_created", "repository"),
        (
            &second,
            "payment_created --handled_by--> service_b",
            "repository",
        ),
        (
            &first,
            "payment_created --means--> settled_payment",
            "workspace",
        ),
        (&first, "alice --works_at--> acme", "repository"),
        (&second, "alice --works_at--> globex", "repository"),
    ];
    for (repo, fact, scope) in observations {
        let output = cli()
            .env("XDG_DATA_HOME", &data)
            .args([
                "observe",
                repo.to_str().unwrap(),
                fact,
                "--scope",
                scope,
                "--ts",
                "100",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let joined = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "query",
            first.to_str().unwrap(),
            "flow(X) :- current(\"service_a\", \"emits\", E), current(E, \"handled_by\", X)",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(joined.stdout).unwrap().trim(),
        "X=service_b"
    );

    let repository_fact = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "query",
            first.to_str().unwrap(),
            "current(\"alice\", \"works_at\", O)",
            "--scope",
            "repository",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(repository_fact.stdout).unwrap().trim(),
        "O=acme"
    );

    let shared_fact = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "query",
            second.to_str().unwrap(),
            "current(\"payment_created\", \"means\", O)",
            "--scope",
            "repository",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(shared_fact.stdout).unwrap().trim(),
        "O=settled_payment"
    );

    let hidden_repository_fact = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "query",
            first.to_str().unwrap(),
            "current(\"payment_created\", \"handled_by\", O)",
            "--scope",
            "repository",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(hidden_repository_fact.stdout)
            .unwrap()
            .trim(),
        "(no answers)"
    );

    let provenance = "https://github.com/example/first/blob/abc123/src/lib.rs#L1";
    let cited = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "observe",
            first.to_str().unwrap(),
            "service_a --owns--> payment_ledger",
            "--provenance",
            provenance,
            "--captured-at",
            "2026-08-30T10:00:00Z",
            "--ts",
            "100",
        ])
        .output()
        .unwrap();
    assert!(cited.status.success());
    let explained = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "why",
            second.to_str().unwrap(),
            "current(service_a, owns, payment_ledger)",
        ])
        .output()
        .unwrap();
    let explanation = String::from_utf8(explained.stdout).unwrap();
    assert!(explanation.lines().any(|line| {
        line == format!("evidence: {provenance} (captured_at=2026-08-30T10:00:00Z)")
    }));
    let cited_scope = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "query",
            second.to_str().unwrap(),
            "scoped_current(S, \"service_a\", \"owns\", \"payment_ledger\")",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(cited_scope.stdout).unwrap().trim(),
        "S=repository:git@github.com:example/first.git"
    );

    let snapshots = std::fs::read_dir(data.join("lemmalog/workspaces"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.path().extension().and_then(|value| value.to_str()) == Some("snapshot")
        })
        .count();
    assert_eq!(snapshots, 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_workspace_override_and_nearest_marker_are_deterministic() {
    let root = temp_root("workspace-resolution");
    let outer = root.join("outer");
    let nested = outer.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(outer.join(".lemmalog"), "id = \"outer\"\n").unwrap();
    std::fs::write(nested.join(".lemmalog"), "id = \"nested\"\n").unwrap();
    let nested_repo = repository(&nested.join("service"), "git@github.com:example/nested.git");
    let outside_repo = repository(&root.join("outside"), "git@github.com:example/outside.git");
    let data = root.join("data");

    let nested_observation = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "observe",
            nested_repo.to_str().unwrap(),
            "nested_service --belongs_to--> nested_workspace",
            "--ts",
            "100",
        ])
        .output()
        .unwrap();
    assert!(nested_observation.status.success());

    let override_observation = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "observe",
            outside_repo.to_str().unwrap(),
            "outside_service --belongs_to--> outer_workspace",
            "--workspace",
            outer.to_str().unwrap(),
            "--ts",
            "100",
        ])
        .output()
        .unwrap();
    assert!(override_observation.status.success());

    let nested_isolated = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "query",
            nested_repo.to_str().unwrap(),
            "current(\"outside_service\", \"belongs_to\", O)",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(nested_isolated.stdout).unwrap().trim(),
        "(no answers)"
    );

    let override_visible = cli()
        .env("XDG_DATA_HOME", &data)
        .args([
            "query",
            outside_repo.to_str().unwrap(),
            "current(\"outside_service\", \"belongs_to\", O)",
            "--workspace",
            outer.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(override_visible.stdout).unwrap().trim(),
        "O=outer_workspace"
    );
    std::fs::remove_dir_all(root).unwrap();
}
