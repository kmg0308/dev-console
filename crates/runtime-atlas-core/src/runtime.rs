use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::{ListeningPort, PublishedPort};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedListeningProcess {
    pub pid: u32,
    pub name: String,
    pub ports: Vec<ListeningPort>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LsofParseState {
    Complete,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsofParseIssue {
    pub line_number: usize,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsofListenerOutcome {
    pub state: LsofParseState,
    pub processes: Vec<ParsedListeningProcess>,
    pub issues: Vec<LsofParseIssue>,
}

pub fn parse_lsof_listeners(output: &str) -> LsofListenerOutcome {
    let mut records: BTreeMap<u32, ParsedListeningProcess> = BTreeMap::new();
    let mut process_lines = BTreeMap::new();
    let mut current_pid = None;
    let mut issues = Vec::new();

    for (line_number, line) in output
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
    {
        let mut characters = line.char_indices();
        let Some((_, field)) = characters.next() else {
            continue;
        };
        let value = &line[characters.next().map_or(line.len(), |(index, _)| index)..];
        match field {
            'p' => {
                current_pid = value.parse().ok().filter(|pid| *pid > 0);
                if let Some(pid) = current_pid {
                    process_lines.entry(pid).or_insert(line_number);
                    records
                        .entry(pid)
                        .or_insert_with(|| ParsedListeningProcess {
                            pid,
                            name: "Unknown".to_owned(),
                            ports: Vec::new(),
                        });
                } else {
                    issues.push(lsof_issue(line_number, "invalid process ID"));
                }
            }
            'c' => {
                if let Some(process) = current_pid.and_then(|pid| records.get_mut(&pid)) {
                    if value.is_empty() {
                        issues.push(lsof_issue(line_number, "process name is empty"));
                    } else {
                        process.name = value.to_owned();
                    }
                } else {
                    issues.push(lsof_issue(line_number, "process name has no process ID"));
                }
            }
            'n' => {
                match (
                    current_pid.and_then(|pid| records.get_mut(&pid)),
                    parse_lsof_port(value),
                ) {
                    (Some(process), Some(port)) if !process.ports.contains(&port) => {
                        process.ports.push(port);
                    }
                    (Some(_), Some(_)) => {}
                    (Some(_), None) => issues.push(lsof_issue(line_number, "invalid listener")),
                    (None, _) => issues.push(lsof_issue(line_number, "listener has no process ID")),
                }
            }
            _ => {}
        }
    }

    let processes = records
        .into_values()
        .filter_map(|mut process| {
            if process.ports.is_empty() {
                issues.push(lsof_issue(
                    process_lines.get(&process.pid).copied().unwrap_or(0),
                    format!("process {} has no valid listener", process.pid),
                ));
                return None;
            }
            process.ports.sort_by(|left, right| {
                (left.port, &left.address).cmp(&(right.port, &right.address))
            });
            Some(process)
        })
        .collect();
    LsofListenerOutcome {
        state: lsof_state(&issues),
        processes,
        issues,
    }
}

fn parse_lsof_port(raw: &str) -> Option<ListeningPort> {
    let value = raw
        .trim()
        .strip_prefix("TCP ")
        .unwrap_or(raw.trim())
        .strip_suffix(" (LISTEN)")
        .unwrap_or_else(|| raw.trim().strip_prefix("TCP ").unwrap_or(raw.trim()));
    let (address, port) = value.rsplit_once(':')?;
    let port = port.parse::<u16>().ok().filter(|port| *port > 0)?;
    Some(ListeningPort {
        address: if address.is_empty() { "*" } else { address }.to_owned(),
        port,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsofWorkingDirectoryOutcome {
    pub state: LsofParseState,
    pub directories: BTreeMap<u32, String>,
    pub issues: Vec<LsofParseIssue>,
}

pub fn parse_lsof_working_directories(output: &str) -> LsofWorkingDirectoryOutcome {
    let mut directories = BTreeMap::new();
    let mut current_pid = None;
    let mut current_field_is_cwd = false;
    let mut issues = Vec::new();

    for (line_number, line) in output
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
    {
        let mut characters = line.char_indices();
        let Some((_, field)) = characters.next() else {
            continue;
        };
        let value = &line[characters.next().map_or(line.len(), |(index, _)| index)..];
        match field {
            'p' => {
                current_pid = value.parse().ok().filter(|pid| *pid > 0);
                current_field_is_cwd = false;
                if current_pid.is_none() {
                    issues.push(lsof_issue(line_number, "invalid process ID"));
                }
            }
            'f' => current_field_is_cwd = value == "cwd",
            'n' if current_field_is_cwd => {
                if let Some(pid) = current_pid
                    && !value.is_empty()
                {
                    directories.insert(pid, value.to_owned());
                } else {
                    issues.push(lsof_issue(
                        line_number,
                        "cwd has no valid process ID or path",
                    ));
                }
            }
            _ => {}
        }
    }
    LsofWorkingDirectoryOutcome {
        state: lsof_state(&issues),
        directories,
        issues,
    }
}

fn lsof_issue(line_number: usize, reason: impl Into<String>) -> LsofParseIssue {
    LsofParseIssue {
        line_number: line_number + 1,
        reason: reason.into(),
    }
}

fn lsof_state(issues: &[LsofParseIssue]) -> LsofParseState {
    if issues.is_empty() {
        LsofParseState::Complete
    } else {
        LsofParseState::Partial
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub mount_sources: Vec<String>,
    pub ports: Vec<PublishedPort>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DockerInspectState {
    Complete,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerInspectIssue {
    pub object_index: usize,
    pub container_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerInspectOutcome {
    pub state: DockerInspectState,
    pub containers: Vec<ParsedContainer>,
    pub issues: Vec<DockerInspectIssue>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DockerInspectError {
    #[error("Docker inspect output is not valid JSON: {0}")]
    InvalidJson(String),
    #[error("Docker inspect output root is not an array")]
    InvalidRoot,
}

pub fn parse_docker_inspect(output: &str) -> Result<DockerInspectOutcome, DockerInspectError> {
    let root: Value = serde_json::from_str(output)
        .map_err(|error| DockerInspectError::InvalidJson(error.to_string()))?;
    let objects = root.as_array().ok_or(DockerInspectError::InvalidRoot)?;
    let mut containers = Vec::new();
    let mut issues = Vec::new();

    for (object_index, object) in objects.iter().enumerate() {
        let Some(object) = object.as_object() else {
            issues.push(issue(
                object_index,
                None,
                "container entry is not an object",
            ));
            continue;
        };
        let id = object
            .get("Id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let Some(id) = id else {
            issues.push(issue(object_index, None, "container ID is missing"));
            continue;
        };

        if object.get("Name").is_some_and(|value| !value.is_string()) {
            issues.push(issue(
                object_index,
                Some(id),
                "container name is not a string",
            ));
        }
        let raw_name = object
            .get("Name")
            .and_then(Value::as_str)
            .unwrap_or("Unnamed");
        let name = raw_name.strip_prefix('/').unwrap_or(raw_name).to_owned();
        if object.get("Config").is_some_and(|value| !value.is_object()) {
            issues.push(issue(
                object_index,
                Some(id),
                "container config is not an object",
            ));
        }
        let raw_image = object
            .get("Config")
            .and_then(Value::as_object)
            .and_then(|config| config.get("Image"));
        if raw_image.is_some_and(|value| !value.is_string()) {
            issues.push(issue(
                object_index,
                Some(id),
                "container image is not a string",
            ));
        }
        let image = raw_image
            .and_then(Value::as_str)
            .unwrap_or("Unknown image")
            .to_owned();
        let mut mount_sources = Vec::new();
        if let Some(mounts) = object.get("Mounts") {
            if let Some(mounts) = mounts.as_array() {
                for mount in mounts {
                    let source = mount
                        .as_object()
                        .and_then(|mount| mount.get("Source"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|source| !source.is_empty());
                    if let Some(source) = source {
                        mount_sources.push(source.to_owned());
                    } else {
                        issues.push(issue(object_index, Some(id), "invalid mount source"));
                    }
                }
            } else {
                issues.push(issue(
                    object_index,
                    Some(id),
                    "container mounts are not an array",
                ));
            }
        }
        mount_sources.sort();
        mount_sources.dedup();

        let mut ports = BTreeSet::new();
        let network_settings = object.get("NetworkSettings");
        if network_settings.is_some_and(|value| !value.is_object()) {
            issues.push(issue(
                object_index,
                Some(id),
                "network settings are not an object",
            ));
        }
        let raw_ports = network_settings
            .and_then(Value::as_object)
            .and_then(|network| network.get("Ports"));
        if raw_ports.is_some_and(|value| !value.is_object()) {
            issues.push(issue(object_index, Some(id), "port map is not an object"));
        }
        if let Some(port_map) = raw_ports.and_then(Value::as_object) {
            for (container_binding, bindings) in port_map {
                let (container_port, transport) = container_binding
                    .split_once('/')
                    .unwrap_or((container_binding, "tcp"));
                let Some(container_port) = parse_port_number(container_port) else {
                    issues.push(issue(
                        object_index,
                        Some(id),
                        format!("invalid container port {container_binding}"),
                    ));
                    continue;
                };
                let Some(bindings) = bindings.as_array() else {
                    if !bindings.is_null() {
                        issues.push(issue(
                            object_index,
                            Some(id),
                            format!("invalid port bindings for {container_binding}"),
                        ));
                    }
                    continue;
                };
                for binding in bindings {
                    let host_port = binding
                        .get("HostPort")
                        .and_then(Value::as_str)
                        .and_then(parse_port_number);
                    let Some(host_port) = host_port else {
                        issues.push(issue(
                            object_index,
                            Some(id),
                            format!("invalid host port for {container_binding}"),
                        ));
                        continue;
                    };
                    ports.insert((
                        host_port,
                        container_port,
                        binding
                            .get("HostIp")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        transport.to_owned(),
                    ));
                }
            }
        }

        containers.push(ParsedContainer {
            id: id.to_owned(),
            name,
            image,
            mount_sources,
            ports: ports
                .into_iter()
                .map(
                    |(host_port, container_port, host_ip, transport)| PublishedPort {
                        host_ip,
                        host_port,
                        container_port,
                        transport,
                    },
                )
                .collect(),
        });
    }
    containers.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    Ok(DockerInspectOutcome {
        state: if issues.is_empty() {
            DockerInspectState::Complete
        } else {
            DockerInspectState::Partial
        },
        containers,
        issues,
    })
}

fn parse_port_number(value: &str) -> Option<u16> {
    value.parse::<u16>().ok().filter(|port| *port > 0)
}

fn issue(
    object_index: usize,
    container_id: Option<&str>,
    reason: impl Into<String>,
) -> DockerInspectIssue {
    DockerInspectIssue {
        object_index,
        container_id: container_id.map(str::to_owned),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_ipv4_ipv6_and_cwd_fixture() {
        let listeners = parse_lsof_listeners(
            "p42\ncnode\nnTCP *:3000 (LISTEN)\nnTCP 127.0.0.1:4000 (LISTEN)\np84\ncpostgres\nnTCP [::1]:5432 (LISTEN)\n",
        );
        assert_eq!(listeners.state, LsofParseState::Complete);
        assert_eq!(listeners.processes.len(), 2);
        assert_eq!(listeners.processes[0].pid, 42);
        assert_eq!(listeners.processes[0].ports[0].port, 3000);
        assert_eq!(listeners.processes[1].ports[0].address, "[::1]");
        let directories =
            parse_lsof_working_directories("p42\nfcwd\nn/tmp/project\np84\nfcwd\nn/tmp/other\n");
        assert_eq!(directories.state, LsofParseState::Complete);
        assert_eq!(
            directories.directories,
            BTreeMap::from([
                (42, "/tmp/project".to_owned()),
                (84, "/tmp/other".to_owned())
            ])
        );

        let partial = parse_lsof_listeners("p42\ncnode\nnbad\npnope\n");
        assert_eq!(partial.state, LsofParseState::Partial);
        assert!(partial.processes.is_empty());
        assert!(!partial.issues.is_empty());
        assert_eq!(
            parse_lsof_working_directories("pnope\nfcwd\nn/tmp/project\n").state,
            LsofParseState::Partial
        );
    }

    #[test]
    fn parses_docker_mounts_ports_and_reports_partial_objects() {
        let fixture = r#"[
          {"Id":"abc123","Name":"/web","Config":{"Image":"example/web:latest"},
           "Mounts":[{"Source":"/tmp/project"}],
           "NetworkSettings":{"Ports":{"3000/tcp":[{"HostIp":"127.0.0.1","HostPort":"33000"}],"9229/tcp":null}}},
          {},
          {"Id":"def456","Name":"/api","Mounts":{},"NetworkSettings":{"Ports":{"bad/tcp":[{"HostPort":"3"}]}}}
        ]"#;
        let outcome = parse_docker_inspect(fixture).unwrap();
        assert_eq!(outcome.state, DockerInspectState::Partial);
        assert_eq!(outcome.containers.len(), 2);
        assert_eq!(outcome.containers[1].mount_sources, ["/tmp/project"]);
        assert_eq!(outcome.containers[1].ports[0].host_port, 33000);
        assert_eq!(outcome.issues.len(), 3);
        assert!(matches!(
            parse_docker_inspect("{"),
            Err(DockerInspectError::InvalidJson(_))
        ));
    }
}
