use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::{Command, Output};

use crate::command::output as command_output;
use crate::git::{WorktreeAvailability, inspect_parsed_worktree, parse_worktree_porcelain};
use crate::models::{
    AvailabilityState, RepositoryRegistration, RepositoryStatus, WorktreeStatus,
    repository_uuid_key, worktree_order_key,
};
use crate::storage::canonical_path;

const MISSING_REPOSITORY: &str = "Repository path is missing.";
const GIT_REPOSITORY_FAILED: &str = "Git could not inspect this repository.";
const GIT_REPOSITORY_TIMED_OUT: &str = "Git repository inspection timed out.";
const NOT_GIT_REPOSITORY: &str = "Path is not an available Git repository.";
const NO_WORKTREES: &str = "No Git worktrees were found.";
const MISSING_WORKTREE: &str = "Worktree path is missing.";
const GIT_WORKTREE_FAILED: &str = "Git could not inspect this worktree.";
const GIT_WORKTREE_TIMED_OUT: &str = "Git worktree inspection timed out.";

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

    let listing = match run_git(
        git_executable,
        &registration.path,
        &["worktree", "list", "--porcelain"],
    ) {
        Ok(listing) => listing,
        Err(error) => {
            let reason = if error.kind() == std::io::ErrorKind::TimedOut {
                GIT_REPOSITORY_TIMED_OUT
            } else {
                GIT_REPOSITORY_FAILED
            };
            return unavailable_repository(registration, name, reason);
        }
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
            let mut timed_out = false;
            let status = path_exists
                .then(|| {
                    match run_git(
                        git_executable,
                        &path,
                        &["status", "--porcelain", "--untracked-files=normal"],
                    ) {
                        Ok(output) if output.status.success() => Some(output),
                        Err(error) => {
                            timed_out = error.kind() == std::io::ErrorKind::TimedOut;
                            None
                        }
                        Ok(_) => None,
                    }
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
                    Some(if timed_out {
                        GIT_WORKTREE_TIMED_OUT.to_owned()
                    } else {
                        GIT_WORKTREE_FAILED.to_owned()
                    }),
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
        sort_worktrees(&mut worktrees, preferred_order);
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

pub fn expand_worktree_order(
    paths: &[String],
    worktrees: &[WorktreeStatus],
) -> Option<Vec<String>> {
    if paths.len() != worktrees.len() {
        return None;
    }
    let by_path: HashMap<_, _> = worktrees
        .iter()
        .map(|worktree| (worktree.path.as_str(), worktree))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut expanded = Vec::with_capacity(paths.len() * 2);
    for path in paths {
        let worktree = by_path.get(path.as_str())?;
        if !seen.insert(path) {
            return None;
        }
        expanded.push(path.clone());
        expanded.push(worktree_order_key(
            worktree.branch.as_deref(),
            worktree.detached,
            &worktree.sha,
        ));
    }
    Some(expanded)
}

fn sort_worktrees(worktrees: &mut [WorktreeStatus], preferred_order: &[String]) {
    let mut ranks = HashMap::new();
    for (rank, key) in preferred_order.iter().enumerate() {
        ranks.entry(key.as_str()).or_insert(rank);
    }
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

fn run_git(git_executable: &Path, directory: &str, arguments: &[&str]) -> std::io::Result<Output> {
    let mut command = Command::new(git_executable);
    command
        .args([
            "--no-optional-locks",
            "-c",
            "core.quotePath=false",
            "-C",
            directory,
        ])
        .args(arguments);
    command_output(&mut command)
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

    fn worktree(path: &str, branch: Option<&str>, sha: &str) -> WorktreeStatus {
        WorktreeStatus {
            path: path.to_owned(),
            branch: branch.map(str::to_owned),
            detached: branch.is_none(),
            sha: sha.to_owned(),
            short_sha: sha.chars().take(7).collect(),
            dirty: false,
            availability: AvailabilityState::Available,
            unavailable_reason: None,
        }
    }

    #[test]
    fn exact_order_distinguishes_equal_detached_heads_and_stable_keys_restore_moved_paths() {
        let original = vec![
            worktree("/detached-one", None, "1234567890abcdef"),
            worktree("/detached-two", None, "1234567890abcdef"),
            worktree("/main", Some("main"), "1234567890abcdef"),
        ];
        let requested = vec![
            "/detached-two".to_owned(),
            "/detached-one".to_owned(),
            "/main".to_owned(),
        ];
        let expanded = expand_worktree_order(&requested, &original).unwrap();
        assert_eq!(
            expanded,
            [
                "/detached-two",
                "detached:1234567890abcdef",
                "/detached-one",
                "detached:1234567890abcdef",
                "/main",
                "branch:main",
            ]
        );

        let mut current = original.clone();
        sort_worktrees(&mut current, &expanded);
        assert_eq!(
            current
                .iter()
                .map(|worktree| worktree.path.as_str())
                .collect::<Vec<_>>(),
            requested.iter().map(String::as_str).collect::<Vec<_>>()
        );

        current[0].path = "/detached-two-moved".to_owned();
        sort_worktrees(&mut current, &expanded);
        assert_eq!(
            current
                .iter()
                .map(|worktree| worktree.path.as_str())
                .collect::<Vec<_>>(),
            ["/detached-two-moved", "/detached-one", "/main"]
        );

        sort_worktrees(
            &mut current,
            &[
                "branch:main".to_owned(),
                "detached:1234567890abcdef".to_owned(),
            ],
        );
        assert_eq!(current[0].path, "/main");
        assert!(expand_worktree_order(&requested[..2], &original).is_none());
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
