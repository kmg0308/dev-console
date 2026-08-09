use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::{Command, Output};

use crate::git::{WorktreeAvailability, inspect_parsed_worktree, parse_worktree_porcelain};
use crate::models::{
    AvailabilityState, RepositoryRegistration, RepositoryStatus, WorktreeStatus,
    repository_uuid_key, worktree_order_key,
};
use crate::storage::canonical_path;

const MISSING_REPOSITORY: &str = "Repository path is missing.";
const GIT_REPOSITORY_FAILED: &str = "Git could not inspect this repository.";
const NOT_GIT_REPOSITORY: &str = "Path is not an available Git repository.";
const NO_WORKTREES: &str = "No Git worktrees were found.";
const MISSING_WORKTREE: &str = "Worktree path is missing.";
const GIT_WORKTREE_FAILED: &str = "Git could not inspect this worktree.";

/// Inspects only the registered paths, in their stored order, using the caller-provided Git.
pub fn inspect_repositories(
    registrations: &[RepositoryRegistration],
    worktree_order_by_repository: &BTreeMap<String, Vec<String>>,
    git_executable: &Path,
) -> Vec<RepositoryStatus> {
    registrations
        .iter()
        .map(|registration| {
            inspect_repository(registration, worktree_order_by_repository, git_executable)
        })
        .collect()
}

fn inspect_repository(
    registration: &RepositoryRegistration,
    worktree_order_by_repository: &BTreeMap<String, Vec<String>>,
    git_executable: &Path,
) -> RepositoryStatus {
    let path = Path::new(&registration.path);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&registration.path)
        .to_owned();
    if !path.is_dir() {
        return unavailable_repository(registration, name, MISSING_REPOSITORY);
    }

    let Ok(listing) = run_git(
        git_executable,
        &registration.path,
        &["worktree", "list", "--porcelain"],
    ) else {
        return unavailable_repository(registration, name, GIT_REPOSITORY_FAILED);
    };
    if !listing.status.success() {
        return unavailable_repository(registration, name, NOT_GIT_REPOSITORY);
    }
    let Ok(listing) = String::from_utf8(listing.stdout) else {
        return unavailable_repository(registration, name, GIT_REPOSITORY_FAILED);
    };
    let parsed = parse_worktree_porcelain(&listing);
    if parsed.is_empty() {
        return unavailable_repository(registration, name, NO_WORKTREES);
    }

    let mut worktrees: Vec<_> = parsed
        .into_iter()
        .map(|parsed| {
            let path = canonical_path(Path::new(&parsed.path));
            let path_exists = Path::new(&path).is_dir();
            let status = path_exists
                .then(|| {
                    run_git(
                        git_executable,
                        &path,
                        &["status", "--porcelain", "--untracked-files=normal"],
                    )
                    .ok()
                    .filter(|output| output.status.success())
                })
                .flatten();
            let inspected = inspect_parsed_worktree(
                parsed,
                path_exists,
                status
                    .as_ref()
                    .map(|output| String::from_utf8_lossy(&output.stdout))
                    .as_deref(),
            );
            let (availability, unavailable_reason) = match inspected.availability {
                WorktreeAvailability::Available => (AvailabilityState::Available, None),
                WorktreeAvailability::Missing => (
                    AvailabilityState::Unavailable,
                    Some(MISSING_WORKTREE.to_owned()),
                ),
                WorktreeAvailability::GitUnavailable => (
                    AvailabilityState::Unavailable,
                    Some(GIT_WORKTREE_FAILED.to_owned()),
                ),
            };
            WorktreeStatus {
                path,
                branch: inspected.worktree.branch,
                detached: inspected.worktree.detached,
                short_sha: inspected.worktree.sha.chars().take(7).collect(),
                sha: inspected.worktree.sha,
                dirty: inspected.dirty,
                availability,
                unavailable_reason,
            }
        })
        .collect();

    let preferred_order = worktree_order_by_repository
        .get(&repository_uuid_key(registration.id))
        .or_else(|| worktree_order_by_repository.get(&registration.id.to_string()));
    if let Some(preferred_order) = preferred_order {
        let ranks: HashMap<_, _> = preferred_order
            .iter()
            .enumerate()
            .map(|(rank, key)| (key.as_str(), rank))
            .collect();
        worktrees.sort_by_key(|worktree| {
            ranks
                .get(worktree.path.as_str())
                .or_else(|| {
                    ranks.get(
                        worktree_order_key(
                            worktree.branch.as_deref(),
                            worktree.detached,
                            &worktree.sha,
                        )
                        .as_str(),
                    )
                })
                .copied()
                .unwrap_or(usize::MAX)
        });
    }

    RepositoryStatus {
        id: registration.id,
        path: registration.path.clone(),
        name,
        availability: AvailabilityState::Available,
        unavailable_reason: None,
        worktrees,
    }
}

fn run_git(git_executable: &Path, directory: &str, arguments: &[&str]) -> std::io::Result<Output> {
    Command::new(git_executable)
        .args([
            "--no-optional-locks",
            "-c",
            "core.quotePath=false",
            "-C",
            directory,
        ])
        .args(arguments)
        .output()
}

fn unavailable_repository(
    registration: &RepositoryRegistration,
    name: String,
    reason: &str,
) -> RepositoryStatus {
    RepositoryStatus {
        id: registration.id,
        path: registration.path.clone(),
        name,
        availability: AvailabilityState::Unavailable,
        unavailable_reason: Some(reason.to_owned()),
        worktrees: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::Utc;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;

    fn git(directory: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?} failed");
    }

    fn registration(id: Uuid, path: &Path) -> RepositoryRegistration {
        RepositoryRegistration {
            id,
            path: path.to_string_lossy().into_owned(),
            added_at: Utc::now(),
        }
    }

    #[test]
    fn inspects_temp_repositories_with_order_and_partial_failures() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repository");
        fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "--initial-branch=main"]);
        git(&repository, &["config", "user.name", "Runtime Atlas Test"]);
        git(
            &repository,
            &["config", "user.email", "runtime-atlas@example.invalid"],
        );
        fs::write(repository.join("README.md"), "main\n").unwrap();
        git(&repository, &["add", "README.md"]);
        git(
            &repository,
            &[
                "-c",
                "commit.gpgSign=false",
                "commit",
                "--no-verify",
                "-m",
                "initial",
            ],
        );

        let nested = repository.join("nested").join("feature");
        git(
            &repository,
            &["worktree", "add", "-b", "feature", nested.to_str().unwrap()],
        );
        let detached_one = temporary.path().join("detached-one");
        let detached_two = temporary.path().join("detached-two");
        for path in [&detached_one, &detached_two] {
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    path.to_str().unwrap(),
                    "HEAD",
                ],
            );
        }
        fs::write(repository.join("dirty.txt"), "dirty\n").unwrap();

        let repository_id = Uuid::parse_str("00000000-0000-0000-0000-00000000000a").unwrap();
        let missing_id = Uuid::parse_str("00000000-0000-0000-0000-00000000000b").unwrap();
        let plain_id = Uuid::parse_str("00000000-0000-0000-0000-00000000000c").unwrap();
        let missing = temporary.path().join("missing");
        let plain = temporary.path().join("plain");
        fs::create_dir(&plain).unwrap();

        let registrations = vec![
            registration(repository_id, &repository),
            registration(missing_id, &missing),
            registration(plain_id, &plain),
        ];
        let order = BTreeMap::from([(
            repository_uuid_key(repository_id),
            vec!["branch:feature".to_owned(), "branch:main".to_owned()],
        )]);
        let statuses = inspect_repositories(&registrations, &order, Path::new("git"));

        assert_eq!(
            statuses.iter().map(|status| status.id).collect::<Vec<_>>(),
            vec![repository_id, missing_id, plain_id]
        );
        assert_eq!(statuses[0].availability, AvailabilityState::Available);
        assert_eq!(statuses[0].worktrees.len(), 4);
        assert_eq!(statuses[0].worktrees[0].branch.as_deref(), Some("feature"));
        assert_eq!(statuses[0].worktrees[1].branch.as_deref(), Some("main"));
        assert!(statuses[0].worktrees[1].dirty);
        assert_ne!(statuses[0].worktrees[0].path, statuses[0].worktrees[1].path);
        assert_eq!(
            statuses[1].unavailable_reason.as_deref(),
            Some(MISSING_REPOSITORY)
        );
        assert_eq!(
            statuses[2].unavailable_reason.as_deref(),
            Some(NOT_GIT_REPOSITORY)
        );

        let exact_order = vec![
            canonical_path(&detached_two),
            canonical_path(&detached_one),
            canonical_path(&nested),
            canonical_path(&repository),
        ];
        let exact = inspect_repositories(
            &registrations[..1],
            &BTreeMap::from([(repository_uuid_key(repository_id), exact_order.clone())]),
            Path::new("git"),
        );
        assert_eq!(
            exact[0]
                .worktrees
                .iter()
                .map(|worktree| worktree.path.clone())
                .collect::<Vec<_>>(),
            exact_order
        );
        assert!(exact[0].worktrees[0].detached && exact[0].worktrees[1].detached);
        assert_eq!(exact[0].worktrees[0].sha, exact[0].worktrees[1].sha);

        fs::remove_dir_all(&nested).unwrap();
        let missing_worktree =
            inspect_repositories(&registrations[..1], &BTreeMap::new(), Path::new("git"));
        assert!(missing_worktree[0].worktrees.iter().any(|worktree| {
            worktree.availability == AvailabilityState::Unavailable
                && worktree.unavailable_reason.as_deref() == Some(MISSING_WORKTREE)
        }));

        let unavailable_git = inspect_repositories(
            &registrations[..1],
            &BTreeMap::new(),
            &temporary.path().join("not-git"),
        );
        assert_eq!(
            unavailable_git[0].unavailable_reason.as_deref(),
            Some(GIT_REPOSITORY_FAILED)
        );
    }
}
