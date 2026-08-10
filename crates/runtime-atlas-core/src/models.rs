use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::CONFIGURATION_SCHEMA_VERSION;
use crate::relations::UserProcessLink;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum AppLanguage {
    #[serde(rename = "ko")]
    Korean,
    #[default]
    #[serde(rename = "en")]
    English,
}

impl AppLanguage {
    pub fn preferred(language_identifiers: &[impl AsRef<str>]) -> Self {
        language_identifiers
            .first()
            .filter(|value| value.as_ref().to_ascii_lowercase().starts_with("ko"))
            .map_or(Self::English, |_| Self::Korean)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AvailabilityState {
    Available,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryAvailability {
    pub state: AvailabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl DiscoveryAvailability {
    pub fn available() -> Self {
        Self {
            state: AvailabilityState::Available,
            reason: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AtlasNoticeKind {
    Warning,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AtlasNotice {
    pub kind: AtlasNoticeKind,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryRegistration {
    pub id: Uuid,
    pub path: String,
    pub added_at: DateTime<Utc>,
}

impl RepositoryRegistration {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            path: path.into(),
            added_at: Utc::now(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAtlasConfiguration {
    pub schema_version: u32,
    pub repositories: Vec<RepositoryRegistration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_language: Option<AppLanguage>,
    pub custom_actions: Vec<CustomActionDefinition>,
    pub worktree_order_by_repository: BTreeMap<String, Vec<String>>,
    pub process_links: Vec<UserProcessLink>,
}

impl Default for RuntimeAtlasConfiguration {
    fn default() -> Self {
        Self {
            schema_version: CONFIGURATION_SCHEMA_VERSION,
            repositories: Vec::new(),
            app_language: None,
            custom_actions: Vec::new(),
            worktree_order_by_repository: BTreeMap::new(),
            process_links: Vec::new(),
        }
    }
}

impl<'de> Deserialize<'de> for RuntimeAtlasConfiguration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct StoredConfiguration {
            #[serde(default = "legacy_configuration_schema")]
            schema_version: u32,
            #[serde(default)]
            repositories: Vec<RepositoryRegistration>,
            #[serde(default)]
            app_language: Option<AppLanguage>,
            #[serde(default)]
            custom_actions: Vec<CustomActionDefinition>,
            #[serde(default)]
            worktree_order_by_repository: BTreeMap<String, Vec<String>>,
            #[serde(default)]
            process_links: Vec<UserProcessLink>,
        }

        let stored = StoredConfiguration::deserialize(deserializer)?;
        let worktree_order_by_repository =
            normalize_repository_keys(stored.worktree_order_by_repository);
        Ok(Self {
            schema_version: stored.schema_version,
            repositories: stored.repositories,
            app_language: stored.app_language,
            custom_actions: stored.custom_actions,
            worktree_order_by_repository,
            process_links: stored.process_links,
        })
    }
}

const fn legacy_configuration_schema() -> u32 {
    1
}

pub fn repository_uuid_key(id: Uuid) -> String {
    id.to_string().to_uppercase()
}

fn normalize_repository_keys(
    stored: BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    let mut normalized = BTreeMap::new();
    for (key, value) in &stored {
        if Uuid::parse_str(key).is_ok() && key == &key.to_uppercase() {
            normalized.insert(key.clone(), value.clone());
        }
    }
    for (key, value) in stored {
        let key = Uuid::parse_str(&key).map_or(key, repository_uuid_key);
        normalized.entry(key).or_insert(value);
    }
    normalized
}

pub fn worktree_order_key(branch: Option<&str>, detached: bool, sha: &str) -> String {
    match branch.filter(|branch| !detached && !branch.is_empty()) {
        Some(branch) => format!("branch:{branch}"),
        None => format!("detached:{sha}"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorktreeNavigationDirection {
    Next,
    Previous,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeNavigationSession {
    pub paths: Vec<String>,
    pub selected_index: usize,
}

impl WorktreeNavigationSession {
    pub fn selected_path(&self) -> Option<&str> {
        self.paths.get(self.selected_index).map(String::as_str)
    }
}

pub fn record_recent_worktree(path: &str, recent_paths: &[String]) -> Vec<String> {
    std::iter::once(path.to_owned())
        .chain(
            recent_paths
                .iter()
                .filter(|recent| *recent != path)
                .cloned(),
        )
        .take(20)
        .collect()
}

pub fn reconcile_recent_worktrees(
    recent_paths: &[String],
    available_paths: &[String],
) -> Vec<String> {
    let available = unique(available_paths);
    unique(recent_paths)
        .into_iter()
        .filter(|path| available.contains(path))
        .chain(
            available
                .iter()
                .filter(|path| !recent_paths.contains(path))
                .cloned(),
        )
        .take(20)
        .collect()
}

pub fn advance_worktree_navigation(
    available_paths: &[String],
    current_path: Option<&str>,
    recent_paths: &[String],
    session: Option<&WorktreeNavigationSession>,
    direction: WorktreeNavigationDirection,
) -> Option<WorktreeNavigationSession> {
    let available_paths = unique(available_paths);
    if available_paths.len() < 2 {
        return None;
    }

    let (paths, starting_index) = if let Some(session) = session {
        let paths: Vec<_> = session
            .paths
            .iter()
            .filter(|path| available_paths.contains(path))
            .cloned()
            .collect();
        if paths.len() < 2 {
            return None;
        }
        let starting_index = session
            .selected_path()
            .and_then(|selected| paths.iter().position(|path| path == selected))
            .unwrap_or(0);
        (paths, starting_index)
    } else {
        let reconciled = reconcile_recent_worktrees(recent_paths, &available_paths);
        if let Some(index) = current_path.and_then(|path| reconciled.iter().position(|p| p == path))
        {
            let current = reconciled[index].clone();
            (record_recent_worktree(&current, &reconciled), 0)
        } else {
            let starting_index = match direction {
                WorktreeNavigationDirection::Next => reconciled.len() - 1,
                WorktreeNavigationDirection::Previous => 0,
            };
            (reconciled, starting_index)
        }
    };

    let selected_index = match direction {
        WorktreeNavigationDirection::Next => (starting_index + 1) % paths.len(),
        WorktreeNavigationDirection::Previous => (starting_index + paths.len() - 1) % paths.len(),
    };
    Some(WorktreeNavigationSession {
        paths,
        selected_index,
    })
}

fn unique(values: &[String]) -> Vec<String> {
    values.iter().fold(Vec::new(), |mut result, value| {
        if !result.contains(value) {
            result.push(value.clone());
        }
        result
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CustomActionKind {
    Task,
    Session,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CustomActionRisk {
    Normal,
    Destructive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CustomActionWorkingDirectory {
    SelectedWorktree,
    RepositoryRoot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CustomActionInputKind {
    Text,
    Worktree,
    Flag,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomActionInputDefinition {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub kind: CustomActionInputKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag_argument: Option<String>,
    #[serde(default)]
    pub is_enabled_by_default: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomActionDefinition {
    pub id: Uuid,
    #[serde(rename = "repositoryID")]
    pub repository_id: Uuid,
    pub name: String,
    pub command_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_command_template: Option<String>,
    pub kind: CustomActionKind,
    pub risk: CustomActionRisk,
    pub working_directory: CustomActionWorkingDirectory,
    pub effects: Vec<String>,
    pub inputs: Vec<CustomActionInputDefinition>,
    pub detects_running_worktree_listener: bool,
}

impl CustomActionDefinition {
    pub fn new(
        repository_id: Uuid,
        name: impl Into<String>,
        command_template: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            repository_id,
            name: name.into(),
            command_template: command_template.into(),
            restart_command_template: None,
            kind: CustomActionKind::Task,
            risk: CustomActionRisk::Normal,
            working_directory: CustomActionWorkingDirectory::SelectedWorktree,
            effects: Vec::new(),
            inputs: Vec::new(),
            detects_running_worktree_listener: false,
        }
    }
}

impl<'de> Deserialize<'de> for CustomActionDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct StoredAction {
            id: Uuid,
            #[serde(rename = "repositoryID")]
            repository_id: Uuid,
            name: String,
            command_template: String,
            #[serde(default)]
            restart_command_template: Option<String>,
            kind: CustomActionKind,
            risk: CustomActionRisk,
            working_directory: CustomActionWorkingDirectory,
            #[serde(default)]
            effects: Vec<String>,
            #[serde(default)]
            inputs: Vec<CustomActionInputDefinition>,
            #[serde(default)]
            detects_running_worktree_listener: Option<bool>,
        }

        let stored = StoredAction::deserialize(deserializer)?;
        let detects_running_worktree_listener = stored.detects_running_worktree_listener.unwrap_or(
            stored.kind == CustomActionKind::Session
                && stored.working_directory == CustomActionWorkingDirectory::SelectedWorktree,
        );
        Ok(Self {
            id: stored.id,
            repository_id: stored.repository_id,
            name: stored.name,
            command_template: stored.command_template,
            restart_command_template: stored.restart_command_template,
            kind: stored.kind,
            risk: stored.risk,
            working_directory: stored.working_directory,
            effects: stored.effects,
            inputs: stored.inputs,
            detects_running_worktree_listener,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListeningPort {
    pub address: String,
    pub port: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProcess {
    pub pid: u32,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub ports: Vec<ListeningPort>,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedPort {
    #[serde(rename = "hostIP")]
    pub host_ip: String,
    pub host_port: u16,
    pub container_port: u16,
    pub transport: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub mount_sources: Vec<String>,
    pub ports: Vec<PublishedPort>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeStatus {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub detached: bool,
    pub sha: String,
    #[serde(rename = "shortSHA")]
    pub short_sha: String,
    pub dirty: bool,
    pub availability: AvailabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryStatus {
    pub id: Uuid,
    pub path: String,
    pub name: String,
    pub availability: AvailabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    pub worktrees: Vec<WorktreeStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_schemas_preserve_defaults_and_order() {
        let repository_id = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let fixtures = [
            (r#"{}"#, 1, None, 0, 0, 0),
            (
                r#"{"schemaVersion":2,"repositories":[],"appLanguage":"ko"}"#,
                2,
                Some(AppLanguage::Korean),
                0,
                0,
                0,
            ),
            (
                r#"{"schemaVersion":3,"repositories":[],"worktreeOrderByRepository":{"00000000-0000-0000-0000-000000000002":["branch:feature","branch:main"]}}"#,
                3,
                None,
                0,
                2,
                0,
            ),
            (
                r#"{"schemaVersion":4,"repositories":[],"customActions":[]}"#,
                4,
                None,
                0,
                0,
                0,
            ),
            (
                r#"{"schemaVersion":5,"repositories":[],"processLinks":[]}"#,
                5,
                None,
                0,
                0,
                0,
            ),
        ];

        for (json, schema, language, action_count, order_count, link_count) in fixtures {
            let configuration: RuntimeAtlasConfiguration = serde_json::from_str(json).unwrap();
            assert_eq!(configuration.schema_version, schema);
            assert_eq!(configuration.app_language, language);
            assert_eq!(configuration.custom_actions.len(), action_count);
            assert_eq!(
                configuration
                    .worktree_order_by_repository
                    .get(&repository_uuid_key(repository_id))
                    .map_or(0, Vec::len),
                order_count
            );
            assert_eq!(configuration.process_links.len(), link_count);
        }
    }

    #[test]
    fn schema_four_round_trip_preserves_array_order_and_legacy_action_defaults() {
        let json = r#"{
          "schemaVersion":4,
          "repositories":[
            {"id":"00000000-0000-0000-0000-000000000002","path":"/two","addedAt":"2026-08-09T08:00:00Z"},
            {"id":"00000000-0000-0000-0000-000000000001","path":"/one","addedAt":"2026-08-09T07:00:00Z"}
          ],
          "appLanguage":"en",
          "customActions":[{
            "id":"00000000-0000-0000-0000-000000000003",
            "repositoryID":"00000000-0000-0000-0000-000000000002",
            "name":"Server","commandTemplate":"npm run dev","kind":"session",
            "risk":"normal","workingDirectory":"selectedWorktree","effects":["listener"],"inputs":[]
          }],
          "worktreeOrderByRepository":{"00000000-0000-0000-0000-000000000002":["branch:feature","branch:main"]}
        }"#;
        let mut configuration: RuntimeAtlasConfiguration = serde_json::from_str(json).unwrap();
        assert_eq!(configuration.repositories[0].path, "/two");
        assert!(configuration.custom_actions[0].detects_running_worktree_listener);
        let mut second =
            CustomActionDefinition::new(configuration.repositories[0].id, "Build", "cargo build");
        second.id = Uuid::parse_str("00000000-0000-0000-0000-000000000004").unwrap();
        configuration.custom_actions.push(second);
        let encoded = serde_json::to_string(&configuration).unwrap();
        let decoded: RuntimeAtlasConfiguration = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, configuration);
        assert_eq!(
            decoded
                .custom_actions
                .iter()
                .map(|action| action.name.as_str())
                .collect::<Vec<_>>(),
            ["Server", "Build"]
        );
        assert_eq!(
            decoded
                .worktree_order_by_repository
                .values()
                .next()
                .unwrap(),
            &["branch:feature", "branch:main"]
        );
    }
}
