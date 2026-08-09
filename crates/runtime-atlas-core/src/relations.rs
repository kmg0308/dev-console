use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::{ListeningPort, RuntimeContainer, RuntimeProcess};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_identity: String,
}

impl ProcessIdentity {
    pub fn is_valid(&self) -> bool {
        self.pid > 1 && !self.start_identity.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedProcess {
    pub identity: ProcessIdentity,
    pub name: String,
    pub cwd: Option<String>,
    pub ports: Vec<ListeningPort>,
}

impl ObservedProcess {
    pub fn from_runtime(process: RuntimeProcess, start_identity: String) -> Self {
        Self {
            identity: ProcessIdentity {
                pid: process.pid,
                start_identity,
            },
            name: process.name,
            cwd: process.cwd,
            ports: process.ports,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PathFlavor {
    MacOs,
    Windows,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRef {
    pub path: String,
    pub path_flavor: PathFlavor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSessionLink {
    pub session_id: Uuid,
    pub process_identity: ProcessIdentity,
    pub worktree_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProcessLink {
    pub process_identity: ProcessIdentity,
    pub worktree_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProcessRelationKind {
    ManagedSession,
    ObservedCwd,
    UserLinked,
    Unlinked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UnlinkedReason {
    CwdUnavailable,
    CwdOutsideRegisteredWorktrees,
    AmbiguousWorktree,
    InvalidProcessIdentity,
    ConflictingProcessObservations,
    ConflictingManagedSessions,
    ConflictingUserLinks,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessRelation {
    pub process_identity: ProcessIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    pub kind: ProcessRelationKind,
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unlinked_reason: Option<UnlinkedReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRelationGraph {
    pub processes: Vec<ObservedProcess>,
    pub process_relations: Vec<ProcessRelation>,
}

/// Builds one global process record and exactly one evidence-bearing relation per identity.
pub fn build_process_relations(
    observed: impl IntoIterator<Item = ObservedProcess>,
    worktrees: &[WorktreeRef],
    managed_sessions: &[ManagedSessionLink],
    user_links: &[UserProcessLink],
) -> RuntimeRelationGraph {
    let mut processes = BTreeMap::new();
    let mut conflicting_observations = BTreeSet::new();
    for process in observed {
        match processes.get(&process.identity) {
            None => {
                processes.insert(process.identity.clone(), process);
            }
            Some(existing) if existing != &process => {
                conflicting_observations.insert(process.identity.clone());
            }
            Some(_) => {}
        }
    }

    let process_relations = processes
        .values()
        .map(|process| {
            relate_process(
                process,
                worktrees,
                managed_sessions,
                user_links,
                conflicting_observations.contains(&process.identity),
            )
        })
        .collect();
    RuntimeRelationGraph {
        processes: processes.into_values().collect(),
        process_relations,
    }
}

fn relate_process(
    process: &ObservedProcess,
    worktrees: &[WorktreeRef],
    managed_sessions: &[ManagedSessionLink],
    user_links: &[UserProcessLink],
    conflicting_observation: bool,
) -> ProcessRelation {
    if !process.identity.is_valid() || process.identity.pid == 0 {
        return unlinked(process, UnlinkedReason::InvalidProcessIdentity);
    }
    if conflicting_observation {
        return unlinked(process, UnlinkedReason::ConflictingProcessObservations);
    }

    let sessions: Vec<_> = managed_sessions
        .iter()
        .filter(|session| session.process_identity == process.identity)
        .filter_map(|session| {
            resolve_exact_worktree(&session.worktree_path, worktrees)
                .map(|path| (session.session_id, path))
        })
        .collect();
    if sessions.len() == 1 {
        return linked(
            process,
            sessions[0].1.clone(),
            ProcessRelationKind::ManagedSession,
            Some(sessions[0].0),
            format!("managed session {}", sessions[0].0),
        );
    }
    if sessions.len() > 1 {
        return unlinked(process, UnlinkedReason::ConflictingManagedSessions);
    }

    let explicit: Vec<_> = user_links
        .iter()
        .filter(|link| link.process_identity == process.identity)
        .filter_map(|link| resolve_exact_worktree(&link.worktree_path, worktrees))
        .collect();
    if explicit.len() == 1 {
        return linked(
            process,
            explicit[0].clone(),
            ProcessRelationKind::UserLinked,
            None,
            format!(
                "saved link for process start identity {}",
                process.identity.start_identity
            ),
        );
    }
    if explicit.len() > 1 {
        return unlinked(process, UnlinkedReason::ConflictingUserLinks);
    }

    let Some(cwd) = process.cwd.as_deref() else {
        return unlinked(process, UnlinkedReason::CwdUnavailable);
    };
    match most_specific_worktree(cwd, worktrees) {
        WorktreeMatch::One(path) => linked(
            process,
            path,
            ProcessRelationKind::ObservedCwd,
            None,
            format!("observed cwd {cwd}"),
        ),
        WorktreeMatch::None => unlinked(process, UnlinkedReason::CwdOutsideRegisteredWorktrees),
        WorktreeMatch::Ambiguous => unlinked(process, UnlinkedReason::AmbiguousWorktree),
    }
}

fn linked(
    process: &ObservedProcess,
    worktree_path: String,
    kind: ProcessRelationKind,
    session_id: Option<Uuid>,
    evidence: String,
) -> ProcessRelation {
    ProcessRelation {
        process_identity: process.identity.clone(),
        worktree_path: Some(worktree_path),
        kind,
        evidence,
        session_id,
        unlinked_reason: None,
    }
}

fn unlinked(process: &ObservedProcess, reason: UnlinkedReason) -> ProcessRelation {
    ProcessRelation {
        process_identity: process.identity.clone(),
        worktree_path: None,
        kind: ProcessRelationKind::Unlinked,
        evidence: "no verified worktree relation".to_owned(),
        session_id: None,
        unlinked_reason: Some(reason),
    }
}

fn resolve_exact_worktree(path: &str, worktrees: &[WorktreeRef]) -> Option<String> {
    let matches: Vec<_> = worktrees
        .iter()
        .filter(|worktree| paths_equal(path, &worktree.path, worktree.path_flavor))
        .collect();
    (matches.len() == 1).then(|| matches[0].path.clone())
}

enum WorktreeMatch {
    None,
    One(String),
    Ambiguous,
}

fn most_specific_worktree(cwd: &str, worktrees: &[WorktreeRef]) -> WorktreeMatch {
    let mut matches: Vec<_> = worktrees
        .iter()
        .filter_map(|worktree| {
            let normalized = normalize_path(&worktree.path, worktree.path_flavor)?;
            is_same_or_descendant(cwd, &worktree.path, worktree.path_flavor)
                .then_some((normalized.len(), worktree.path.clone()))
        })
        .collect();
    matches.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    let Some((specificity, path)) = matches.first() else {
        return WorktreeMatch::None;
    };
    if matches.get(1).is_some_and(|next| next.0 == *specificity) {
        WorktreeMatch::Ambiguous
    } else {
        WorktreeMatch::One(path.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerWorktreeLink {
    pub worktree_path: String,
    pub mount_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedContainer {
    #[serde(flatten)]
    pub container: RuntimeContainer,
    pub worktree_links: Vec<ContainerWorktreeLink>,
}

/// Keeps each container global while preserving every direct mount-source edge.
pub fn relate_container_mounts(
    containers: &[RuntimeContainer],
    worktrees: &[WorktreeRef],
) -> Vec<RelatedContainer> {
    containers
        .iter()
        .cloned()
        .map(|container| {
            let mut worktree_links = Vec::new();
            for source in &container.mount_sources {
                for worktree in worktrees {
                    if is_same_or_descendant(source, &worktree.path, worktree.path_flavor) {
                        worktree_links.push(ContainerWorktreeLink {
                            worktree_path: worktree.path.clone(),
                            mount_source: source.clone(),
                        });
                    }
                }
            }
            worktree_links.sort_by(|left, right| {
                (&left.worktree_path, &left.mount_source)
                    .cmp(&(&right.worktree_path, &right.mount_source))
            });
            worktree_links.dedup();
            RelatedContainer {
                container,
                worktree_links,
            }
        })
        .collect()
}

pub fn paths_equal(left: &str, right: &str, flavor: PathFlavor) -> bool {
    normalize_path(left, flavor)
        .zip(normalize_path(right, flavor))
        .is_some_and(|(left, right)| left == right)
}

pub fn is_same_or_descendant(candidate: &str, root: &str, flavor: PathFlavor) -> bool {
    let Some(candidate) = normalize_path(candidate, flavor) else {
        return false;
    };
    let Some(root) = normalize_path(root, flavor) else {
        return false;
    };
    candidate == root || candidate.starts_with(&format!("{}/", root.trim_end_matches('/')))
}

/// Pure lexical normalization. Filesystem symlink resolution belongs at the OS adapter boundary.
pub fn normalize_path(path: &str, flavor: PathFlavor) -> Option<String> {
    match flavor {
        PathFlavor::MacOs => normalize_absolute(path, "/", false),
        PathFlavor::Windows => normalize_windows(path),
    }
}

fn normalize_windows(path: &str) -> Option<String> {
    let path = path.replace('\\', "/").to_lowercase();
    if path.starts_with("//./") {
        return None;
    }
    if let Some(rest) = path.strip_prefix("//?/unc/") {
        return normalize_unc(rest);
    }
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    if let Some(rest) = path.strip_prefix("//") {
        return normalize_unc(rest);
    }
    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'/' {
        return None;
    }
    normalize_absolute(&path[2..], &path[..2], true)
}

fn normalize_unc(path: &str) -> Option<String> {
    let mut components = path.split('/').filter(|part| !part.is_empty());
    let server = components.next()?;
    let share = components.next()?;
    normalize_components(
        format!("//{server}/{share}"),
        components.collect::<Vec<_>>(),
    )
}

fn normalize_absolute(path: &str, prefix: &str, prefix_needs_slash: bool) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let parts = path.split('/').filter(|part| !part.is_empty()).collect();
    let prefix = if prefix_needs_slash {
        format!("{prefix}/")
    } else {
        prefix.to_owned()
    };
    normalize_components(prefix, parts)
}

fn normalize_components(prefix: String, parts: Vec<&str>) -> Option<String> {
    let mut normalized = Vec::new();
    for part in parts {
        match part {
            "." => {}
            ".." => {
                normalized.pop()?;
            }
            _ => normalized.push(part),
        }
    }
    if normalized.is_empty() {
        Some(prefix)
    } else {
        Some(format!(
            "{}{separator}{}",
            prefix.trim_end_matches('/'),
            normalized.join("/"),
            separator = "/"
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminationSnapshot {
    pub process_identity: ProcessIdentity,
    pub name: String,
    pub cwd: Option<String>,
    pub ports: Vec<ListeningPort>,
    pub worktree_path: String,
    pub path_flavor: PathFlavor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TerminationSignal {
    Term,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminationPlan {
    pub process_identity: ProcessIdentity,
    pub signal: TerminationSignal,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TerminationPlanError {
    #[error("the displayed process is not a safe termination target")]
    InvalidTarget,
    #[error("the process identity or execution location changed")]
    ProcessChanged,
    #[error("the process no longer owns every displayed listening port")]
    NoLongerListening,
}

/// Revalidates a displayed snapshot and returns a normal termination request; it never sends a signal.
pub fn plan_termination(
    snapshot: &TerminationSnapshot,
    current: &ObservedProcess,
    runtime_atlas_identity: &ProcessIdentity,
) -> Result<TerminationPlan, TerminationPlanError> {
    let expected_cwd = snapshot
        .cwd
        .as_deref()
        .ok_or(TerminationPlanError::InvalidTarget)?;
    if !snapshot.process_identity.is_valid()
        || !runtime_atlas_identity.is_valid()
        || snapshot.ports.is_empty()
        || runtime_atlas_identity == &snapshot.process_identity
        || !is_same_or_descendant(expected_cwd, &snapshot.worktree_path, snapshot.path_flavor)
    {
        return Err(TerminationPlanError::InvalidTarget);
    }
    let current_cwd = current
        .cwd
        .as_deref()
        .ok_or(TerminationPlanError::ProcessChanged)?;
    if current.identity != snapshot.process_identity
        || current.name != snapshot.name
        || !paths_equal(current_cwd, expected_cwd, snapshot.path_flavor)
        || !is_same_or_descendant(current_cwd, &snapshot.worktree_path, snapshot.path_flavor)
    {
        return Err(TerminationPlanError::ProcessChanged);
    }
    let current_ports: BTreeSet<_> = current.ports.iter().collect();
    if !snapshot
        .ports
        .iter()
        .all(|port| current_ports.contains(port))
    {
        return Err(TerminationPlanError::NoLongerListening);
    }
    Ok(TerminationPlan {
        process_identity: snapshot.process_identity.clone(),
        signal: TerminationSignal::Term,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PublishedPort;

    fn identity(pid: u32, marker: &str) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_identity: marker.to_owned(),
        }
    }

    fn process(pid: u32, marker: &str, cwd: Option<&str>) -> ObservedProcess {
        ObservedProcess {
            identity: identity(pid, marker),
            name: "node".to_owned(),
            cwd: cwd.map(str::to_owned),
            ports: vec![ListeningPort {
                address: "*".to_owned(),
                port: 3000,
            }],
        }
    }

    #[test]
    fn normalizes_macos_windows_drive_and_unc_paths() {
        assert!(is_same_or_descendant(
            "/tmp/project/web",
            "/tmp/project",
            PathFlavor::MacOs
        ));
        assert!(!is_same_or_descendant(
            "/tmp/project-two",
            "/tmp/project",
            PathFlavor::MacOs
        ));
        assert!(paths_equal(
            r"C:\Dev\App\.\src\..",
            "c:/dev/app",
            PathFlavor::Windows
        ));
        assert!(is_same_or_descendant(
            r"\\Server\Share\Repo\Web",
            r"//server/share/repo",
            PathFlavor::Windows
        ));
        assert!(paths_equal(
            r"\\?\C:\Dev\App",
            r"c:\dev\app",
            PathFlavor::Windows
        ));
        assert!(paths_equal(
            r"\\?\UNC\Server\Share\Repo",
            r"\\server\share\repo",
            PathFlavor::Windows
        ));
    }

    #[test]
    fn builds_global_relations_from_verified_evidence_only() {
        let worktrees = vec![
            WorktreeRef {
                path: "/repo".to_owned(),
                path_flavor: PathFlavor::MacOs,
            },
            WorktreeRef {
                path: "/repo/apps/web".to_owned(),
                path_flavor: PathFlavor::MacOs,
            },
        ];
        let managed_id = Uuid::parse_str("00000000-0000-0000-0000-000000000010").unwrap();
        let managed = process(10, "start-10", None);
        let user = process(20, "start-20", None);
        let cwd = process(30, "start-30", Some("/repo/apps/web/src"));
        let unavailable = process(40, "start-40", None);
        let graph = build_process_relations(
            [managed.clone(), user.clone(), cwd, unavailable],
            &worktrees,
            &[ManagedSessionLink {
                session_id: managed_id,
                process_identity: managed.identity,
                worktree_path: "/repo".to_owned(),
            }],
            &[UserProcessLink {
                process_identity: user.identity,
                worktree_path: "/repo/apps/web".to_owned(),
            }],
        );
        assert_eq!(graph.processes.len(), 4);
        assert_eq!(
            graph.process_relations[0].kind,
            ProcessRelationKind::ManagedSession
        );
        assert_eq!(graph.process_relations[0].session_id, Some(managed_id));
        assert_eq!(
            graph.process_relations[1].kind,
            ProcessRelationKind::UserLinked
        );
        assert_eq!(
            graph.process_relations[2].worktree_path.as_deref(),
            Some("/repo/apps/web")
        );
        assert_eq!(
            graph.process_relations[3].unlinked_reason,
            Some(UnlinkedReason::CwdUnavailable)
        );
    }

    #[test]
    fn docker_mount_edges_are_verified_many_to_many() {
        let containers = vec![RuntimeContainer {
            id: "one".to_owned(),
            name: "web".to_owned(),
            image: "web".to_owned(),
            mount_sources: vec!["/repo/apps/web".to_owned()],
            ports: vec![PublishedPort {
                host_ip: "127.0.0.1".to_owned(),
                host_port: 3000,
                container_port: 3000,
                transport: "tcp".to_owned(),
            }],
        }];
        let worktrees = vec![
            WorktreeRef {
                path: "/repo".to_owned(),
                path_flavor: PathFlavor::MacOs,
            },
            WorktreeRef {
                path: "/repo/apps/web".to_owned(),
                path_flavor: PathFlavor::MacOs,
            },
        ];
        let containers = relate_container_mounts(&containers, &worktrees);
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].worktree_links.len(), 2);
        assert!(
            containers[0]
                .worktree_links
                .iter()
                .all(|link| link.mount_source == "/repo/apps/web")
        );
    }

    #[test]
    fn termination_plan_revalidates_identity_cwd_and_ports_and_only_returns_term() {
        let expected = process(42, "start-42", Some("/repo/web"));
        let snapshot = TerminationSnapshot {
            process_identity: expected.identity.clone(),
            name: expected.name.clone(),
            cwd: expected.cwd.clone(),
            ports: expected.ports.clone(),
            worktree_path: "/repo".to_owned(),
            path_flavor: PathFlavor::MacOs,
        };
        let runtime_atlas = identity(99, "runtime-atlas");
        assert_eq!(
            plan_termination(&snapshot, &expected, &runtime_atlas)
                .unwrap()
                .signal,
            TerminationSignal::Term
        );
        assert_eq!(
            plan_termination(&snapshot, &expected, &expected.identity),
            Err(TerminationPlanError::InvalidTarget)
        );

        let reused_pid = process(42, "later-start", Some("/repo/web"));
        assert_eq!(
            plan_termination(&snapshot, &reused_pid, &runtime_atlas),
            Err(TerminationPlanError::ProcessChanged)
        );
        let changed_ports = ObservedProcess {
            ports: vec![ListeningPort {
                address: "*".to_owned(),
                port: 4000,
            }],
            ..expected.clone()
        };
        assert_eq!(
            plan_termination(&snapshot, &changed_ports, &runtime_atlas),
            Err(TerminationPlanError::NoLongerListening)
        );
    }
}
