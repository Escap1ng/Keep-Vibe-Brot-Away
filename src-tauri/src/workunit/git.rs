//! Local Git evidence collection for a completed WorkUnit.
//!
//! Commands are executed directly without a shell, with prompts disabled and a
//! hard timeout. The result is a worktree-at-turn-end snapshot, not a claim that
//! every visible change was authored by the just-finished Agent turn.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use super::{ChangeScope, ChangeSet, FileChange};

const GIT_TIMEOUT: Duration = Duration::from_secs(3);

pub async fn collect(project_path: &str) -> ChangeSet {
    let root = match run_git(project_path, &["rev-parse", "--show-toplevel"]).await {
        Ok(output) => String::from_utf8_lossy(&output).trim().to_string(),
        Err(error) => {
            return ChangeSet {
                scope: ChangeScope::Unavailable,
                collection_error: Some(error),
                ..Default::default()
            };
        }
    };
    if root.is_empty() {
        return ChangeSet {
            scope: ChangeScope::Unavailable,
            collection_error: Some("Git 返回了空仓库根目录".into()),
            ..Default::default()
        };
    }

    let head = run_git(&root, &["rev-parse", "HEAD"])
        .await
        .ok()
        .map(|output| String::from_utf8_lossy(&output).trim().to_string())
        .filter(|value| !value.is_empty());
    let mut files = BTreeMap::<String, FileChange>::new();
    let mut errors = Vec::new();

    if let Some(base) = head.as_deref() {
        match run_git(
            &root,
            &[
                "diff",
                "--numstat",
                "--find-renames",
                "--no-ext-diff",
                "--no-textconv",
                base,
                "--",
            ],
        )
        .await
        {
            Ok(output) => merge_numstat(&mut files, &output),
            Err(error) => errors.push(error),
        }
    } else {
        for args in [
            &["diff", "--numstat", "--cached", "--"][..],
            &["diff", "--numstat", "--"][..],
        ] {
            match run_git(&root, args).await {
                Ok(output) => merge_numstat(&mut files, &output),
                Err(error) => errors.push(error),
            }
        }
    }

    match run_git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).await {
        Ok(output) => merge_untracked(&mut files, &output),
        Err(error) => errors.push(error),
    }

    let scope = if errors.is_empty() {
        ChangeScope::WorktreeAtTurnEnd
    } else if files.is_empty() {
        ChangeScope::Unavailable
    } else {
        ChangeScope::PartialWorktreeAtTurnEnd
    };
    ChangeSet {
        base_ref: head,
        // None means that the other side is the worktree, not an unknown commit.
        head_ref: None,
        files: files.into_values().collect(),
        scope,
        collection_error: (!errors.is_empty()).then(|| errors.join("; ")),
    }
}

async fn run_git(project_path: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("-C")
        .arg(project_path)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    let output = timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            format!(
                "Git 命令超时({}s):{}",
                GIT_TIMEOUT.as_secs(),
                args.join(" ")
            )
        })?
        .map_err(|error| format!("无法执行 Git:{error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail: String = stderr.trim().chars().take(240).collect();
        Err(format!(
            "Git 命令失败({}):{}",
            output.status.code().unwrap_or(-1),
            detail
        ))
    }
}

fn merge_numstat(files: &mut BTreeMap<String, FileChange>, output: &[u8]) {
    let text = String::from_utf8_lossy(output);
    for line in text.lines() {
        let mut parts = line.trim_end_matches('\r').splitn(3, '\t');
        let Some(additions) = parts.next() else {
            continue;
        };
        let Some(deletions) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next() else {
            continue;
        };
        if path.trim().is_empty() {
            continue;
        }
        let entry = files.entry(path.to_string()).or_insert_with(|| FileChange {
            path: path.to_string(),
            ..Default::default()
        });
        entry.additions = entry.additions.saturating_add(parse_count(additions));
        entry.deletions = entry.deletions.saturating_add(parse_count(deletions));
    }
}

fn merge_untracked(files: &mut BTreeMap<String, FileChange>, output: &[u8]) {
    let text = String::from_utf8_lossy(output);
    for path in text
        .split('\0')
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        files.entry(path.to_string()).or_insert_with(|| FileChange {
            path: path.to_string(),
            ..Default::default()
        });
    }
}

fn parse_count(value: &str) -> u64 {
    value.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numstat_and_keeps_unicode_paths() {
        let mut files = BTreeMap::new();
        merge_numstat(
            &mut files,
            "12\t3\tsrc/入口.rs\n-\t-\tassets/logo.bin\n".as_bytes(),
        );
        assert_eq!(files["src/入口.rs"].additions, 12);
        assert_eq!(files["src/入口.rs"].deletions, 3);
        assert_eq!(files["assets/logo.bin"].additions, 0);
    }

    #[test]
    fn merges_untracked_without_shell_parsing() {
        let mut files = BTreeMap::new();
        merge_untracked(&mut files, b"src/file with space.ts\0new.txt\0");
        assert!(files.contains_key("src/file with space.ts"));
        assert!(files.contains_key("new.txt"));
    }

    #[tokio::test]
    async fn non_repository_is_an_explicit_unavailable_state() {
        let temp_path = std::env::temp_dir().to_string_lossy().into_owned();
        let changes = collect(&temp_path).await;
        assert_eq!(changes.scope, ChangeScope::Unavailable);
        assert!(changes.collection_error.is_some());
    }
}
