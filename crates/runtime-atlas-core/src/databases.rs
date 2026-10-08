use std::{path::Path, process::Command, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    command::{output, output_with_timeout},
    models::RuntimeContainer,
    relations::ProcessIdentity,
    sessions::process_identity,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseBinding {
    pub id: Uuid,
    pub kind: String,
    pub label: String,
    pub worktree_path: String,
    pub container_name: String,
    #[serde(rename = "ownerPID")]
    pub owner_pid: Option<u32>,
    #[serde(default)]
    pub owner_identity: Option<ProcessIdentity>,
    pub registered_at: DateTime<Utc>,
}

impl DatabaseBinding {
    pub fn is_valid(&self) -> bool {
        self.kind == "database"
            && !self.id.is_nil()
            && !self.label.trim().is_empty()
            && Path::new(&self.worktree_path).is_absolute()
            && valid_container_name(&self.container_name)
            && self.owner_pid.is_none_or(|pid| pid > 1)
            && self
                .owner_identity
                .as_ref()
                .is_none_or(|identity| identity.is_valid() && Some(identity.pid) == self.owner_pid)
    }

    pub fn retention_reason(&self) -> Option<&'static str> {
        let pid = self.owner_pid?;
        match process_identity(pid) {
            Ok(current) if self.owner_identity.as_ref() == Some(&current) => {
                Some("다른 실행이 공유 DB를 사용 중입니다.")
            }
            Ok(_) if self.owner_identity.is_none() => {
                Some("이전 DB 연결의 실행 정보를 확인할 수 없습니다.")
            }
            Ok(_) => None, // A different start identity proves that the recorded owner exited.
            Err(_) if process_absent(pid) => None,
            Err(_) => Some("DB 사용자 프로세스를 확인할 수 없습니다."),
        }
    }
}

pub fn valid_container_name(name: &str) -> bool {
    name.len() <= 255
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

#[cfg(target_os = "macos")]
fn process_absent(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only tests existence and never sends a termination signal.
    if (unsafe { libc::kill(pid, 0) }) == -1 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    // macOS may no longer expose proc_pidinfo for a zombie before its parent reaps it.
    let mut status = Command::new("/bin/ps");
    status.args(["-p", &pid.to_string(), "-o", "stat="]);
    output(&mut status).is_ok_and(|result| {
        result.status.success()
            && std::str::from_utf8(&result.stdout).is_ok_and(|state| state.trim().starts_with('Z'))
    })
}

#[cfg(windows)]
fn process_absent(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError},
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        unsafe { GetLastError() == ERROR_INVALID_PARAMETER }
    } else {
        unsafe { CloseHandle(handle) };
        false
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn process_absent(_pid: u32) -> bool {
    false
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseStatus {
    pub label: String,
    pub worktree_path: String,
    pub container_name: String,
    pub state: String,
}

pub fn database_statuses(
    records: &[DatabaseBinding],
    containers: &[RuntimeContainer],
    available: bool,
) -> Vec<DatabaseStatus> {
    let mut statuses = records
        .iter()
        .map(|record| DatabaseStatus {
            label: record.label.clone(),
            worktree_path: record.worktree_path.clone(),
            container_name: record.container_name.clone(),
            state: if !available {
                "unavailable"
            } else {
                containers
                    .iter()
                    .find(|container| container.name == record.container_name)
                    .map_or("missing", |container| {
                        if container.running {
                            "running"
                        } else {
                            "stopped"
                        }
                    })
            }
            .to_owned(),
        })
        .collect::<Vec<_>>();
    statuses.sort_by(|left, right| {
        (&left.worktree_path, &left.container_name, &left.label).cmp(&(
            &right.worktree_path,
            &right.container_name,
            &right.label,
        ))
    });
    statuses.dedup();
    statuses
}

#[derive(Debug, Eq, PartialEq)]
pub enum DatabaseStopOutcome {
    Stopped,
    Unconfirmed,
}

/// Only explicit PostgreSQL bindings on a local engine with persistent data can be stopped.
pub fn stop_idle_database(
    executable: &Path,
    expected: &RuntimeContainer,
) -> Result<DatabaseStopOutcome, String> {
    let mut command = Command::new(executable);
    command.args([
        "context",
        "inspect",
        "--format",
        "{{.Endpoints.docker.Host}}",
    ]);
    let context = checked_output(&mut command, "Docker 연결을 확인할 수 없습니다.")?;
    let endpoint = if std::env::var_os("DOCKER_CONTEXT").is_none() {
        std::env::var("DOCKER_HOST").unwrap_or(context)
    } else {
        context
    };
    if !endpoint.trim().starts_with("unix:///") && !endpoint.trim().starts_with("npipe:////./pipe/")
    {
        return Err("로컬 Docker 연결인지 확인할 수 없습니다.".to_owned());
    }
    let mut inspect = Command::new(executable);
    inspect.args(["inspect", &expected.id]);
    let details = checked_output(&mut inspect, "DB 컨테이너를 확인할 수 없습니다.")?;
    let details: Value =
        serde_json::from_str(&details).map_err(|_| "DB 컨테이너 정보를 해석할 수 없습니다.")?;
    verify_database_container(&details, expected)?;

    // Query all databases as the container's superuser; idle clients also keep the DB running.
    let mut clients = Command::new(executable);
    clients.args(["exec", &expected.id, "sh", "-c",
        "psql -X -v ON_ERROR_STOP=1 -U \"${POSTGRES_USER:-postgres}\" -d postgres -Atq -c \"SELECT CASE WHEN (SELECT rolsuper FROM pg_roles WHERE rolname = current_user) THEN (SELECT count(*)::text FROM pg_stat_activity WHERE backend_type = 'client backend' AND pid <> pg_backend_pid()) ELSE 'unknown' END;\""]);
    let count = checked_output(&mut clients, "DB 접속 현황을 확인할 수 없습니다.")?;
    if count.trim() != "0" {
        return Err("다른 DB 접속이 있거나 사용 여부를 확인할 수 없습니다.".to_owned());
    }
    let mut stop = Command::new(executable);
    // No SIGKILL fallback: a slow shutdown stays visible as running until Docker confirms it.
    stop.args([
        "stop",
        "--signal",
        "SIGTERM",
        "--timeout",
        "-1",
        &expected.id,
    ]);
    // Docker's daemon keeps the graceful stop request even if the CLI times out.
    let _ = output_with_timeout(&mut stop, Duration::from_secs(45));
    let mut state = Command::new(executable);
    state.args(["inspect", "--format", "{{.State.Running}}", &expected.id]);
    Ok(
        if checked_output(&mut state, "DB 중지 상태를 확인할 수 없습니다.")
            .is_ok_and(|value| value.trim() == "false")
        {
            DatabaseStopOutcome::Stopped
        } else {
            DatabaseStopOutcome::Unconfirmed
        },
    )
}

fn checked_output(command: &mut Command, failure: &str) -> Result<String, String> {
    let result = output(command).map_err(|_| failure.to_owned())?;
    if !result.status.success() {
        return Err(failure.to_owned());
    }
    String::from_utf8(result.stdout).map_err(|_| failure.to_owned())
}

fn verify_database_container(details: &Value, expected: &RuntimeContainer) -> Result<(), String> {
    let container = details
        .as_array()
        .filter(|items| items.len() == 1)
        .and_then(|items| items.first())
        .ok_or("DB 컨테이너 정보를 확인할 수 없습니다.")?;
    let image = container["Config"]["Image"].as_str().unwrap_or_default();
    let postgres = ["postgres", "library/postgres", "docker.io/library/postgres"]
        .iter()
        .any(|base| {
            image == *base
                || image
                    .strip_prefix(base)
                    .is_some_and(|suffix| suffix.starts_with(':') || suffix.starts_with('@'))
        });
    let data_path = container["Config"]["Env"]
        .as_array()
        .and_then(|env| {
            env.iter()
                .filter_map(Value::as_str)
                .find_map(|entry| entry.strip_prefix("PGDATA="))
        })
        .unwrap_or("/var/lib/postgresql/data");
    let persistent = container["Mounts"].as_array().is_some_and(|mounts| {
        mounts.iter().any(|mount| {
            matches!(mount["Type"].as_str(), Some("volume" | "bind"))
                && mount["Destination"].as_str().is_some_and(|destination| {
                    !destination.is_empty()
                        && (destination == data_path
                            || data_path
                                .starts_with(&format!("{}/", destination.trim_end_matches('/'))))
                })
        })
    });
    if container["Id"].as_str() != Some(&expected.id)
        || container["Name"]
            .as_str()
            .map(|name| name.trim_start_matches('/'))
            != Some(&expected.name)
        || container["State"]["Running"].as_bool() != Some(true)
        || !postgres
        || !persistent
    {
        return Err("DB 컨테이너의 동일성과 영구 데이터 저장을 확인할 수 없습니다.".to_owned());
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::storage::{DatabaseBindingStore, RuntimeAtlasPaths};
    use std::{fs, os::unix::fs::PermissionsExt};
    use tempfile::tempdir;

    fn binding(worktree: &str) -> DatabaseBinding {
        let owner = process_identity(std::process::id()).unwrap();
        DatabaseBinding {
            id: Uuid::new_v4(),
            kind: "database".into(),
            label: "fixture_db".into(),
            worktree_path: worktree.into(),
            container_name: "shared-db".into(),
            owner_pid: Some(owner.pid),
            owner_identity: Some(owner),
            registered_at: Utc::now(),
        }
    }

    fn container() -> RuntimeContainer {
        RuntimeContainer {
            id: "verified-db".into(),
            name: "shared-db".into(),
            image: "postgres:16".into(),
            running: true,
            mount_sources: Vec::new(),
            ports: Vec::new(),
        }
    }

    fn details() -> Value {
        serde_json::json!([{"Id":"verified-db", "Name":"/shared-db", "State":{"Running":true},
            "Config":{"Image":"postgres:16", "Env":["PGDATA=/var/lib/postgresql/data"]},
            "Mounts":[{"Type":"volume", "Destination":"/var/lib/postgresql/data", "Name":"preserved-data"}]}])
    }

    #[test]
    fn shared_owners_release_independently_and_keep_status_after_stop() {
        let temporary = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(temporary.path());
        let store = DatabaseBindingStore::new(&paths);
        let first = binding("/first-worktree");
        let second = binding("/second-worktree");
        store.link(first.clone()).unwrap();
        store.link(second.clone()).unwrap();
        store
            .unlink(&first.worktree_path, first.owner_identity.as_ref())
            .unwrap();
        store
            .with_records(|records| {
                assert_eq!(records.len(), 2);
                assert!(records[0].retention_reason().is_none());
                assert!(records[1].retention_reason().is_some());
                let mut db = container();
                assert!(
                    database_statuses(records, &[db.clone()], true)
                        .iter()
                        .all(|status| status.state == "running")
                );
                db.running = false;
                assert!(
                    database_statuses(records, &[db], true)
                        .iter()
                        .all(|status| status.state == "stopped")
                );
                assert!(
                    database_statuses(records, &[], false)
                        .iter()
                        .all(|status| status.state == "unavailable")
                );
            })
            .unwrap();
        store
            .unlink(&second.worktree_path, second.owner_identity.as_ref())
            .unwrap();
        store
            .with_records(|records| {
                assert!(
                    records
                        .iter()
                        .all(|record| record.retention_reason().is_none())
                )
            })
            .unwrap();
        let damaged = b"{broken";
        fs::write(temporary.path().join("runtime-bindings.json"), damaged).unwrap();
        assert!(store.link(first).is_err());
        assert!(store.with_records(|_| ()).is_err());
        assert_eq!(
            fs::read(temporary.path().join("runtime-bindings.json")).unwrap(),
            damaged
        );
    }

    #[test]
    fn legacy_live_pid_is_unknown_and_reused_identity_is_not_an_owner() {
        let mut record = binding("/worktree");
        record.owner_identity = None;
        assert!(record.retention_reason().is_some());
        record.owner_identity = Some(ProcessIdentity {
            pid: record.owner_pid.unwrap(),
            start_identity: "old-start".into(),
        });
        assert!(record.retention_reason().is_none());
        assert!(!valid_container_name("--all"));
        assert!(!valid_container_name("db;rm"));
    }

    #[test]
    fn unverified_identity_image_or_ephemeral_data_never_stops() {
        let expected = container();
        assert!(verify_database_container(&details(), &expected).is_ok());
        for (field, value) in [
            ("Id", serde_json::json!("replacement")),
            ("Mounts", serde_json::json!([])),
            ("Config", serde_json::json!({"Image":"unrelated:16"})),
        ] {
            let mut changed = details();
            changed[0][field] = value;
            assert!(verify_database_container(&changed, &expected).is_err());
        }
    }

    #[test]
    fn docker_stop_requires_zero_clients_and_reports_query_or_stop_failure() {
        let temporary = tempdir().unwrap();
        let executable = temporary.path().join("docker-fixture");
        let log = temporary.path().join("commands");
        for (clients, query_status, stop_status, running, delay, expected, stops) in [
            (
                "0",
                0,
                0,
                "false",
                0,
                Some(DatabaseStopOutcome::Stopped),
                true,
            ),
            ("1", 0, 0, "false", 0, None, false),
            ("unknown", 0, 0, "false", 0, None, false),
            ("0", 1, 0, "false", 0, None, false),
            (
                "0",
                0,
                1,
                "true",
                0,
                Some(DatabaseStopOutcome::Unconfirmed),
                true,
            ),
            (
                "0",
                0,
                1,
                "false",
                0,
                Some(DatabaseStopOutcome::Stopped),
                true,
            ),
            (
                "0",
                0,
                0,
                "unknown",
                0,
                Some(DatabaseStopOutcome::Unconfirmed),
                true,
            ),
            (
                "0",
                0,
                0,
                "false",
                11,
                Some(DatabaseStopOutcome::Stopped),
                true,
            ),
        ] {
            fs::write(&log, "").unwrap();
            fs::write(&executable, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\ncontext) echo unix:///fixture/docker.sock;;\ninspect) if [ \"$2\" = --format ]; then echo '{}'; else cat <<'JSON'\n{}\nJSON\nfi;;\nexec) echo '{}'; exit {};;\nstop) sleep {}; exit {};;\n*) exit 1;;\nesac\n", log.display(), running, details(), clients, query_status, delay, stop_status)).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(stop_idle_database(&executable, &container()).ok(), expected);
            let commands = fs::read_to_string(&log).unwrap();
            assert_eq!(
                commands.contains("stop --signal SIGTERM --timeout -1 verified-db"),
                stops
            );
            assert!(!commands.lines().any(|line| line.starts_with("rm ")
                || line.contains("prune")
                || line.contains("volume rm")));
        }
        fs::write(&executable, "#!/bin/sh\nexit 1\n").unwrap();
        assert!(stop_idle_database(&executable, &container()).is_err());
    }
}
