use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedGitWorktree {
    pub path: String,
    pub sha: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub prunable: bool,
    pub prunable_reason: Option<String>,
}

pub fn parse_worktree_porcelain(output: &str) -> Vec<ParsedGitWorktree> {
    let mut worktrees = Vec::new();
    let mut current: Option<ParsedGitWorktree> = None;

    let finish = |current: &mut Option<ParsedGitWorktree>, worktrees: &mut Vec<_>| {
        if let Some(worktree) = current.take().filter(|worktree| !worktree.path.is_empty()) {
            worktrees.push(worktree);
        }
    };

    for line in output.lines() {
        if line.is_empty() {
            finish(&mut current, &mut worktrees);
        } else if let Some(path) = line.strip_prefix("worktree ") {
            finish(&mut current, &mut worktrees);
            current = Some(ParsedGitWorktree {
                path: path.to_owned(),
                sha: String::new(),
                branch: None,
                detached: false,
                prunable: false,
                prunable_reason: None,
            });
        } else if let Some(worktree) = current.as_mut() {
            if let Some(sha) = line.strip_prefix("HEAD ") {
                worktree.sha = sha.to_owned();
            } else if let Some(branch) = line.strip_prefix("branch ") {
                worktree.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_owned(),
                );
            } else if line == "detached" {
                worktree.detached = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                worktree.prunable = true;
                worktree.prunable_reason = line
                    .strip_prefix("prunable ")
                    .filter(|reason| !reason.is_empty())
                    .map(str::to_owned);
            }
        }
    }
    finish(&mut current, &mut worktrees);
    worktrees
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorktreeAvailability {
    Available,
    Missing,
    GitUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedWorktree {
    pub worktree: ParsedGitWorktree,
    pub dirty: bool,
    pub availability: WorktreeAvailability,
}

/// Combines parsed Git output with the caller's filesystem and status-command evidence.
/// `status_porcelain` is `None` only when `git status --porcelain` failed.
pub fn inspect_parsed_worktree(
    worktree: ParsedGitWorktree,
    path_exists: bool,
    status_porcelain: Option<&str>,
) -> InspectedWorktree {
    let availability = if worktree.prunable || !path_exists {
        WorktreeAvailability::Missing
    } else if status_porcelain.is_none() {
        WorktreeAvailability::GitUnavailable
    } else {
        WorktreeAvailability::Available
    };
    InspectedWorktree {
        dirty: availability == WorktreeAvailability::Available
            && status_porcelain.is_some_and(is_dirty_status),
        worktree,
        availability,
    }
}

pub fn is_dirty_status(status_porcelain: &str) -> bool {
    status_porcelain.lines().any(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORCELAIN: &str = "worktree /tmp/main\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree /tmp/feature\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/feature/one\n\nworktree /tmp/detached\nHEAD 3333333333333333333333333333333333333333\ndetached\nprunable gitdir file points to non-existent location\n";

    #[test]
    fn parses_worktree_fixture_and_status_evidence() {
        let parsed = parse_worktree_porcelain(PORCELAIN);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(parsed[1].branch.as_deref(), Some("feature/one"));
        assert!(parsed[2].detached);
        assert_eq!(
            parsed[2].prunable_reason.as_deref(),
            Some("gitdir file points to non-existent location")
        );

        let dirty = inspect_parsed_worktree(parsed[0].clone(), true, Some("?? new.txt\n"));
        assert_eq!(dirty.availability, WorktreeAvailability::Available);
        assert!(dirty.dirty);

        let missing = inspect_parsed_worktree(parsed[2].clone(), false, None);
        assert_eq!(missing.availability, WorktreeAvailability::Missing);
        assert!(!missing.dirty);
    }
}
