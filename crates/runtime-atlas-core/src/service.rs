use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::STATUS_SCHEMA_VERSION;
use std::path::Path;

use crate::models::{
    AppLanguage, AtlasNotice, AtlasNoticeKind, CustomActionDefinition, DiscoveryAvailability,
    RepositoryStatus, RuntimeAtlasConfiguration, RuntimeContainer,
};
use crate::observe::{DockerObservation, ProcessObservation};
use crate::relations::{
    ManagedSessionLink, ObservedProcess, PathFlavor, ProcessRelation, RelatedContainer,
    UserProcessLink, WorktreeRef, build_process_relations, relate_container_mounts,
};
use crate::repository::inspect_repositories;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySnapshotInput {
    pub repository: RepositoryStatus,
    pub path_flavor: PathFlavor,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionRunPhase {
    Pending,
    Running,
    Restarting,
    Stopping,
    Succeeded,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRun {
    #[serde(rename = "actionID")]
    pub action_id: Uuid,
    pub worktree_path: String,
    pub phase: ActionRunPhase,
    pub output: String,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub managed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeAtlasSnapshotInput {
    pub generated_at: DateTime<Utc>,
    pub language: AppLanguage,
    pub process_discovery: DiscoveryAvailability,
    pub docker_discovery: DiscoveryAvailability,
    pub notices: Vec<AtlasNotice>,
    pub repositories: Vec<RepositorySnapshotInput>,
    pub observed_processes: Vec<ObservedProcess>,
    pub managed_sessions: Vec<ManagedSessionLink>,
    pub user_links: Vec<UserProcessLink>,
    pub containers: Vec<RuntimeContainer>,
    pub actions: Vec<CustomActionDefinition>,
    pub action_runs: Vec<ActionRun>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAtlasSnapshot {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub language: AppLanguage,
    pub process_discovery: DiscoveryAvailability,
    pub docker_discovery: DiscoveryAvailability,
    pub notices: Vec<AtlasNotice>,
    pub repositories: Vec<RepositoryStatus>,
    pub processes: Vec<ObservedProcess>,
    pub relations: Vec<ProcessRelation>,
    pub containers: Vec<RelatedContainer>,
    pub actions: Vec<CustomActionDefinition>,
    pub action_runs: Vec<ActionRun>,
}

pub fn build_observed_snapshot(
    configuration: RuntimeAtlasConfiguration,
    recovery_notice: Option<String>,
    default_language: AppLanguage,
    git_executable: &Path,
    path_flavor: PathFlavor,
    process_observation: ProcessObservation,
    docker_observation: DockerObservation,
) -> RuntimeAtlasSnapshot {
    let mut notices = process_observation.notices;
    notices.extend(docker_observation.notices);
    if let Some(message) = recovery_notice {
        notices.push(AtlasNotice {
            kind: AtlasNoticeKind::Error,
            message,
        });
    }
    let repositories = inspect_repositories(
        &configuration.repositories,
        &configuration.worktree_order_by_repository,
        git_executable,
    )
    .into_iter()
    .map(|repository| RepositorySnapshotInput {
        repository,
        path_flavor,
    })
    .collect();

    build_snapshot(RuntimeAtlasSnapshotInput {
        generated_at: Utc::now(),
        language: configuration.app_language.unwrap_or(default_language),
        process_discovery: process_observation.availability,
        docker_discovery: docker_observation.availability,
        notices,
        repositories,
        observed_processes: process_observation.processes,
        managed_sessions: Vec::new(),
        user_links: configuration.process_links,
        containers: docker_observation.containers,
        actions: configuration.custom_actions,
        action_runs: Vec::new(),
    })
}

/// Composes caller-verified observations without performing filesystem or process discovery.
pub fn build_snapshot(input: RuntimeAtlasSnapshotInput) -> RuntimeAtlasSnapshot {
    let worktrees: Vec<_> = input
        .repositories
        .iter()
        .flat_map(|input| {
            input
                .repository
                .worktrees
                .iter()
                .map(|worktree| WorktreeRef {
                    path: worktree.path.clone(),
                    path_flavor: input.path_flavor,
                })
        })
        .collect();
    let graph = build_process_relations(
        input.observed_processes,
        &worktrees,
        &input.managed_sessions,
        &input.user_links,
    );
    let containers = relate_container_mounts(&input.containers, &worktrees);

    let mut snapshot = RuntimeAtlasSnapshot {
        schema_version: STATUS_SCHEMA_VERSION,
        generated_at: input.generated_at,
        language: input.language,
        process_discovery: input.process_discovery,
        docker_discovery: input.docker_discovery,
        notices: input.notices,
        repositories: input
            .repositories
            .into_iter()
            .map(|input| input.repository)
            .collect(),
        processes: graph.processes,
        relations: graph.process_relations,
        containers,
        actions: input.actions,
        action_runs: input.action_runs,
    };
    add_external_action_runs(&mut snapshot);
    snapshot
}

fn add_external_action_runs(snapshot: &mut RuntimeAtlasSnapshot) {
    for action in &snapshot.actions {
        if action.kind != crate::models::CustomActionKind::Session
            || !action.detects_running_worktree_listener
        {
            continue;
        }
        let Some(repository) = snapshot
            .repositories
            .iter()
            .find(|repository| repository.id == action.repository_id)
        else {
            continue;
        };
        for worktree in repository
            .worktrees
            .iter()
            .filter(|worktree| worktree.availability == crate::models::AvailabilityState::Available)
        {
            let already_running = snapshot.action_runs.iter().any(|run| {
                run.action_id == action.id
                    && run.worktree_path == worktree.path
                    && matches!(
                        run.phase,
                        ActionRunPhase::Pending
                            | ActionRunPhase::Running
                            | ActionRunPhase::Restarting
                            | ActionRunPhase::Stopping
                    )
            });
            let listener = snapshot.relations.iter().any(|relation| {
                relation.worktree_path.as_deref() == Some(&worktree.path)
                    && snapshot.processes.iter().any(|process| {
                        process.identity == relation.process_identity && !process.ports.is_empty()
                    })
            });
            if listener && !already_running {
                snapshot.action_runs.push(ActionRun {
                    action_id: action.id,
                    worktree_path: worktree.path.clone(),
                    phase: ActionRunPhase::Running,
                    output: String::new(),
                    exit_code: None,
                    managed: false,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AvailabilityState, CustomActionDefinition, CustomActionKind, ListeningPort, PublishedPort,
        WorktreeStatus,
    };
    use crate::relations::{ProcessIdentity, ProcessRelationKind, UnlinkedReason};

    fn worktree(path: &str, branch: &str) -> WorktreeStatus {
        WorktreeStatus {
            path: path.to_owned(),
            branch: Some(branch.to_owned()),
            detached: false,
            sha: "1234567890abcdef".to_owned(),
            short_sha: "1234567".to_owned(),
            dirty: false,
            availability: AvailabilityState::Available,
            unavailable_reason: None,
        }
    }

    fn process(pid: u32, cwd: Option<&str>) -> ObservedProcess {
        ObservedProcess {
            identity: ProcessIdentity {
                pid,
                start_identity: format!("started-{pid}"),
            },
            name: "node".to_owned(),
            cwd: cwd.map(str::to_owned),
            ports: vec![ListeningPort {
                address: "127.0.0.1".to_owned(),
                port: 3000,
            }],
        }
    }

    #[test]
    fn composes_ui_snapshot_from_verified_facts() {
        let repository_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let action_id = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let session_id = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();
        let root = "/repo";
        let nested = "/repo/.worktrees/feature";
        let repositories = vec![RepositorySnapshotInput {
            repository: RepositoryStatus {
                id: repository_id,
                path: root.to_owned(),
                name: "repo".to_owned(),
                availability: AvailabilityState::Available,
                unavailable_reason: None,
                worktrees: vec![worktree(root, "main"), worktree(nested, "feature")],
            },
            path_flavor: PathFlavor::MacOs,
        }];
        let nested_process = process(42, Some("/repo/.worktrees/feature/web"));
        let external_process = process(43, None);
        let managed_process = process(44, None);
        let user_linked_process = process(45, None);
        let mut action = CustomActionDefinition::new(repository_id, "Dev server", "npm run dev");
        action.id = action_id;

        let snapshot = build_snapshot(RuntimeAtlasSnapshotInput {
            generated_at: DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            language: AppLanguage::Korean,
            process_discovery: DiscoveryAvailability::available(),
            docker_discovery: DiscoveryAvailability::available(),
            notices: Vec::new(),
            repositories,
            observed_processes: vec![
                external_process.clone(),
                nested_process.clone(),
                managed_process.clone(),
                user_linked_process.clone(),
            ],
            managed_sessions: vec![ManagedSessionLink {
                session_id,
                process_identity: managed_process.identity.clone(),
                worktree_path: root.to_owned(),
            }],
            user_links: vec![UserProcessLink {
                process_identity: user_linked_process.identity.clone(),
                worktree_path: nested.to_owned(),
            }],
            containers: vec![RuntimeContainer {
                id: "container-1".to_owned(),
                name: "web".to_owned(),
                image: "web:latest".to_owned(),
                mount_sources: vec![nested.to_owned()],
                ports: vec![PublishedPort {
                    host_ip: "127.0.0.1".to_owned(),
                    host_port: 8080,
                    container_port: 80,
                    transport: "tcp".to_owned(),
                }],
            }],
            actions: vec![action],
            action_runs: vec![ActionRun {
                action_id,
                worktree_path: nested.to_owned(),
                phase: ActionRunPhase::Running,
                output: "ready".to_owned(),
                exit_code: None,
                managed: true,
            }],
        });

        assert_eq!(snapshot.schema_version, STATUS_SCHEMA_VERSION);
        assert_eq!(snapshot.schema_version, 2);
        assert_eq!(snapshot.language, AppLanguage::Korean);
        assert_eq!(snapshot.processes.len(), 4);
        let relation = |pid| {
            snapshot
                .relations
                .iter()
                .find(|relation| relation.process_identity.pid == pid)
                .unwrap()
        };
        assert_eq!(relation(42).kind, ProcessRelationKind::ObservedCwd);
        assert_eq!(relation(42).worktree_path.as_deref(), Some(nested));
        assert_eq!(relation(43).kind, ProcessRelationKind::Unlinked);
        assert_eq!(
            relation(43).unlinked_reason,
            Some(UnlinkedReason::CwdUnavailable)
        );
        assert_eq!(relation(44).kind, ProcessRelationKind::ManagedSession);
        assert_eq!(relation(44).session_id, Some(session_id));
        assert_eq!(relation(45).kind, ProcessRelationKind::UserLinked);
        assert_eq!(snapshot.containers[0].worktree_links.len(), 2);
        assert_eq!(snapshot.actions[0].id, action_id);
        assert_eq!(snapshot.action_runs[0].phase, ActionRunPhase::Running);

        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(json["generatedAt"], "2026-08-09T00:00:00Z");
        assert_eq!(json["language"], "ko");
        assert_eq!(
            json["processes"][0]["identity"]["startIdentity"],
            "started-42"
        );
        assert_eq!(
            json["actions"][0]["repositoryID"],
            repository_id.to_string()
        );
        assert_eq!(json["actionRuns"][0]["exitCode"], serde_json::Value::Null);
    }

    #[test]
    fn marks_only_verified_external_worktree_listeners_as_unmanaged_runs() {
        let repository_id = Uuid::new_v4();
        let root = "/repo";
        let listener = process(42, Some("/repo/web"));
        let mut action = CustomActionDefinition::new(repository_id, "Dev", "npm run dev");
        action.kind = CustomActionKind::Session;
        action.detects_running_worktree_listener = true;
        let snapshot = build_snapshot(RuntimeAtlasSnapshotInput {
            generated_at: Utc::now(),
            language: AppLanguage::English,
            process_discovery: DiscoveryAvailability::available(),
            docker_discovery: DiscoveryAvailability::available(),
            notices: Vec::new(),
            repositories: vec![RepositorySnapshotInput {
                repository: RepositoryStatus {
                    id: repository_id,
                    path: root.into(),
                    name: "repo".into(),
                    availability: AvailabilityState::Available,
                    unavailable_reason: None,
                    worktrees: vec![worktree(root, "main")],
                },
                path_flavor: PathFlavor::MacOs,
            }],
            observed_processes: vec![listener],
            managed_sessions: Vec::new(),
            user_links: Vec::new(),
            containers: Vec::new(),
            actions: vec![action.clone()],
            action_runs: Vec::new(),
        });
        assert_eq!(snapshot.action_runs.len(), 1);
        assert_eq!(snapshot.action_runs[0].action_id, action.id);
        assert!(!snapshot.action_runs[0].managed);
    }
}
