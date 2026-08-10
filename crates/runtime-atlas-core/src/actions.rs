use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::Serialize;
use thiserror::Error;

use crate::models::{
    AvailabilityState, CustomActionDefinition, CustomActionInputKind, CustomActionKind,
    CustomActionWorkingDirectory, RepositoryStatus, WorktreeStatus,
};
use crate::storage::canonical_path;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CustomActionError {
    #[error("Command name must be 1-60 characters.")]
    InvalidName,
    #[error("Command is invalid: {0}")]
    InvalidTemplate(String),
    #[error("Input is invalid: {0}")]
    InvalidInput(String),
    #[error("A value is required for {{{{{0}}}}}.")]
    MissingValue(String),
    #[error("The selected worktree is not registered: {0}")]
    InvalidWorktree(String),
}

pub type ActionResult<T> = Result<T, CustomActionError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomActionPlan {
    pub executable: String,
    pub arguments: Vec<String>,
    pub current_directory: String,
    pub display_command: String,
}

pub fn initial_action_values(
    action: &CustomActionDefinition,
    selected_worktree: &str,
) -> BTreeMap<String, String> {
    action
        .inputs
        .iter()
        .map(|input| {
            let value = match input.kind {
                CustomActionInputKind::Text => String::new(),
                CustomActionInputKind::Worktree => selected_worktree.to_owned(),
                CustomActionInputKind::Flag => input.is_enabled_by_default.to_string(),
            };
            (input.key.clone(), value)
        })
        .collect()
}

pub fn validate_custom_action(action: &CustomActionDefinition) -> ActionResult<()> {
    let name = action.name.trim();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(CustomActionError::InvalidName);
    }
    if action
        .effects
        .iter()
        .map(String::as_str)
        .chain([
            action.name.as_str(),
            action.command_template.as_str(),
            action.restart_command_template.as_deref().unwrap_or(""),
        ])
        .chain(action.inputs.iter().flat_map(|input| {
            [
                input.key.as_str(),
                input.label.as_str(),
                input.flag_argument.as_deref().unwrap_or(""),
            ]
        }))
        .any(contains_sensitive_content)
    {
        return invalid_input("credentials and URLs must not be stored in a command");
    }
    if action.restart_command_template.is_some() && action.kind != CustomActionKind::Session {
        return invalid_input("a restart command requires a keep-running command");
    }
    if action.detects_running_worktree_listener
        && (action.kind != CustomActionKind::Session
            || action.working_directory != CustomActionWorkingDirectory::SelectedWorktree)
    {
        return invalid_input(
            "open-port running detection is only available for keep-running commands",
        );
    }

    let mut keys = HashSet::new();
    for input in &action.inputs {
        if !keys.insert(input.key.as_str()) {
            return invalid_input("input keys must be unique");
        }
        let mut characters = input.key.chars();
        if input.key.len() > 32
            || !characters
                .next()
                .is_some_and(|value| value.is_ascii_alphabetic())
            || !characters.all(|value| value.is_ascii_alphanumeric() || value == '_')
        {
            return invalid_input("input keys must use letters, numbers, or underscores");
        }
        let label = input.label.trim();
        if label.is_empty() || label.chars().count() > 60 {
            return invalid_input("input labels must be 1-60 characters");
        }
        if input.kind == CustomActionInputKind::Flag {
            let flag = input.flag_argument.as_deref().unwrap_or("");
            if flag.is_empty() || flag.chars().count() > 100 || tokenize(flag)?.len() != 1 {
                return invalid_input("a checkbox argument must be one argument");
            }
        }
    }
    validate_template(&action.command_template, &keys)?;
    if let Some(template) = &action.restart_command_template {
        validate_template(template, &keys)?;
    }
    Ok(())
}

pub fn plan_custom_action(
    action: &CustomActionDefinition,
    values: &BTreeMap<String, String>,
    repository: &RepositoryStatus,
    selected_worktree: &WorktreeStatus,
) -> ActionResult<CustomActionPlan> {
    validate_custom_action(action)?;
    plan(
        action,
        &action.command_template,
        values,
        repository,
        selected_worktree,
    )
}

pub fn plan_custom_action_restart(
    action: &CustomActionDefinition,
    values: &BTreeMap<String, String>,
    repository: &RepositoryStatus,
    selected_worktree: &WorktreeStatus,
) -> ActionResult<CustomActionPlan> {
    validate_custom_action(action)?;
    let template = action
        .restart_command_template
        .as_deref()
        .ok_or_else(|| invalid_template_error("enter a restart command"))?;
    plan(action, template, values, repository, selected_worktree)
}

fn plan(
    action: &CustomActionDefinition,
    template: &str,
    values: &BTreeMap<String, String>,
    repository: &RepositoryStatus,
    selected_worktree: &WorktreeStatus,
) -> ActionResult<CustomActionPlan> {
    if action.repository_id != repository.id
        || repository.availability != AvailabilityState::Available
        || !Path::new(&repository.path).is_absolute()
    {
        return invalid_input("the action repository is not available");
    }

    let allowed: HashSet<_> = repository
        .worktrees
        .iter()
        .filter(|worktree| worktree.availability == AvailabilityState::Available)
        .map(|worktree| canonical_path(Path::new(&worktree.path)))
        .collect();
    let selected = canonical_path(Path::new(&selected_worktree.path));
    if selected_worktree.availability != AvailabilityState::Available
        || !Path::new(&selected_worktree.path).is_absolute()
        || !allowed.contains(&selected)
    {
        return Err(CustomActionError::InvalidWorktree(
            selected_worktree.path.clone(),
        ));
    }

    let mut expanded = Vec::new();
    for token in tokenize(template)? {
        let Some(key) = placeholder(&token) else {
            expanded.push(token);
            continue;
        };
        let input = action
            .inputs
            .iter()
            .find(|input| input.key == key)
            .ok_or_else(|| invalid_template_error(format!("unknown input {{{{{key}}}}}")))?;
        let raw = values.get(key).map(String::as_str).unwrap_or("");
        match input.kind {
            CustomActionInputKind::Flag => {
                if raw == "true" {
                    expanded.push(input.flag_argument.clone().ok_or_else(|| {
                        CustomActionError::InvalidInput(
                            "a checkbox argument must be one argument".into(),
                        )
                    })?);
                }
            }
            CustomActionInputKind::Worktree => {
                let path = canonical_path(Path::new(raw));
                if !Path::new(raw).is_absolute() || !allowed.contains(&path) {
                    return Err(CustomActionError::InvalidWorktree(raw.into()));
                }
                expanded.push(path);
            }
            CustomActionInputKind::Text => {
                if raw.is_empty() {
                    return Err(CustomActionError::MissingValue(key.into()));
                }
                if raw.chars().count() > 500
                    || raw.contains(['\0', '\n', '\r'])
                    || raw.contains("$(")
                    || raw.contains('`')
                    || contains_sensitive_content(raw)
                {
                    return invalid_input(format!("{{{{{key}}}}} contains an unsupported value"));
                }
                expanded.push(raw.into());
            }
        }
    }
    validate_direct_invocation(&expanded)?;
    let executable = expanded[0].clone();
    let current_directory = match action.working_directory {
        CustomActionWorkingDirectory::SelectedWorktree => selected,
        CustomActionWorkingDirectory::RepositoryRoot => canonical_path(Path::new(&repository.path)),
    };
    Ok(CustomActionPlan {
        executable,
        arguments: expanded[1..].to_vec(),
        current_directory,
        display_command: expanded
            .iter()
            .map(|token| display_token(token))
            .collect::<Vec<_>>()
            .join(" "),
    })
}

fn validate_template(template: &str, input_keys: &HashSet<&str>) -> ActionResult<()> {
    if template.is_empty() || template.chars().count() > 500 {
        return invalid_template("use 1-500 characters");
    }
    let tokens = tokenize(template)?;
    validate_direct_invocation(&tokens)?;
    for token in tokens {
        if token.contains("{{") || token.contains("}}") {
            let Some(key) = placeholder(&token) else {
                return invalid_template(
                    "placeholders must be complete arguments such as {{target}}",
                );
            };
            if !input_keys.contains(key) {
                return invalid_template(format!("unknown input {{{{{key}}}}}"));
            }
        }
    }
    Ok(())
}

fn validate_direct_invocation(tokens: &[String]) -> ActionResult<()> {
    let Some(executable) = tokens.first() else {
        return invalid_template("enter an executable");
    };
    let executable = executable_name(executable);
    if matches!(executable.as_str(), "env" | "env.exe") {
        return invalid_template("environment injection is not supported");
    }
    for pair in tokens.windows(2) {
        let executable = executable_name(&pair[0]);
        let option = pair[1].to_ascii_lowercase();
        let command_mode = match executable.as_str() {
            "sh" | "bash" | "dash" | "fish" | "zsh" => {
                option.starts_with('-') && option.chars().skip(1).any(|value| value == 'c')
            }
            "cmd" | "cmd.exe" => matches!(option.as_str(), "/c" | "/k" | "-c" | "-k"),
            "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => matches!(
                option.as_str(),
                "-c" | "-command" | "-commandwithargs" | "-ec" | "-encodedcommand"
            ),
            _ => false,
        };
        if command_mode {
            return invalid_template("shell command strings are not supported");
        }
    }
    Ok(())
}

fn executable_name(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase()
}

fn placeholder(token: &str) -> Option<&str> {
    token
        .strip_prefix("{{")
        .and_then(|value| value.strip_suffix("}}"))
        .filter(|value| !value.is_empty() && !value.contains(['{', '}']))
}

fn tokenize(value: &str) -> ActionResult<Vec<String>> {
    if value.contains("$(") || value.contains('`') {
        return invalid_template("shell expansion is not supported");
    }
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            let escapes_next = characters.peek().is_some_and(|next| match quote {
                Some('\'') => *next == '\'',
                Some('"') => matches!(*next, '"' | '\\'),
                _ => {
                    next.is_whitespace()
                        || matches!(*next, '"' | '\'' | '\\' | ';' | '|' | '<' | '>' | '&')
                }
            });
            if escapes_next {
                escaped = true;
            } else {
                current.push(character);
            }
        } else if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                current.push(character);
            }
        } else if character == '"' || character == '\'' {
            quote = Some(character);
        } else if matches!(character, ';' | '|' | '<' | '>' | '&') {
            return invalid_template(
                "pipes, redirects, chaining, and background operators are not supported",
            );
        } else if character.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if quote.is_some() || escaped {
        return invalid_template("a quote or escape is incomplete");
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn display_token(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_./:@%+=,-".contains(character))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn contains_sensitive_content(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("://") {
        return true;
    }
    if lower
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| matches!(pair[0], "bearer" | "basic") && !pair[1].is_empty())
    {
        return true;
    }
    const SENSITIVE: [&str; 10] = [
        "password",
        "passwd",
        "token",
        "secret",
        "api-key",
        "api_key",
        "apikey",
        "authorization",
        "cookie",
        "session",
    ];
    SENSITIVE.iter().any(|fragment| {
        lower.match_indices(fragment).any(|(index, _)| {
            let before = lower[..index].chars().next_back();
            let after = lower[index + fragment.len()..].trim_start();
            !before.is_some_and(|value| value.is_ascii_alphanumeric() || value == '_')
                && (after.starts_with(':') || after.starts_with('='))
        })
    })
}

/// Removes credentials and URLs from untrusted child-process output before it reaches the UI.
pub fn sanitize_output(value: &str) -> String {
    value
        .split_inclusive('\n')
        .map(sanitize_output_line)
        .collect()
}

fn sanitize_output_line(line: &str) -> String {
    let (content, newline) = line
        .strip_suffix('\n')
        .map_or((line, ""), |line| (line, "\n"));
    let trimmed = content.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    for header in [
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "x-api-key",
    ] {
        if lower
            .strip_prefix(header)
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
        {
            let indent = &content[..content.len() - trimmed.len()];
            return format!("{indent}{header}: <redacted>{newline}");
        }
    }

    let content = redact_assignments(content);
    let mut words = Vec::new();
    let mut position = 0;
    for word in content.split_whitespace() {
        let start = content[position..].find(word).unwrap_or(0) + position;
        words.push((start, start + word.len(), word));
        position = start + word.len();
    }
    let mut output = String::with_capacity(content.len());
    let mut copied = 0;
    let mut redact_next = false;
    for (start, end, word) in words {
        output.push_str(&content[copied..start]);
        let lower = word.to_ascii_lowercase();
        if redact_next || lower.contains("://") {
            output.push_str(if lower.contains("://") {
                "<redacted-url>"
            } else {
                "<redacted>"
            });
            redact_next = false;
        } else if matches!(lower.as_str(), "bearer" | "basic") {
            output.push_str(word);
            redact_next = true;
        } else {
            output.push_str(word);
        }
        copied = end;
    }
    output.push_str(&content[copied..]);
    output.push_str(newline);
    output
}

fn redact_assignments(value: &str) -> String {
    const KEYS: [&str; 10] = [
        "password",
        "passwd",
        "token",
        "secret",
        "api-key",
        "api_key",
        "apikey",
        "authorization",
        "cookie",
        "session",
    ];
    let lower = value.to_ascii_lowercase();
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        let match_at = KEYS
            .iter()
            .flat_map(|key| {
                lower[cursor..]
                    .match_indices(key)
                    .map(move |(at, _)| (at, key))
            })
            .filter_map(|(at, key)| {
                let start = cursor + at;
                let boundary = start == 0
                    || !lower[..start]
                        .chars()
                        .next_back()
                        .is_some_and(|value| value.is_ascii_alphanumeric() || value == '_');
                let mut separator = start + key.len();
                while lower
                    .as_bytes()
                    .get(separator)
                    .is_some_and(u8::is_ascii_whitespace)
                {
                    separator += 1;
                }
                (boundary
                    && lower
                        .as_bytes()
                        .get(separator)
                        .is_some_and(|value| matches!(*value, b':' | b'=')))
                .then_some((start, separator))
            })
            .min_by_key(|(start, _)| *start);
        let Some((_start, separator)) = match_at else {
            output.push_str(&value[cursor..]);
            break;
        };
        let mut secret = separator + 1;
        while value
            .as_bytes()
            .get(secret)
            .is_some_and(u8::is_ascii_whitespace)
        {
            secret += 1;
        }
        let end = value[secret..]
            .find(char::is_whitespace)
            .map_or(value.len(), |length| secret + length);
        output.push_str(&value[cursor..secret]);
        if secret < end {
            output.push_str("<redacted>");
        }
        cursor = end.max(secret);
        if cursor == value.len() {
            break;
        }
    }
    output
}

fn invalid_template<T>(message: impl Into<String>) -> ActionResult<T> {
    Err(invalid_template_error(message))
}

fn invalid_template_error(message: impl Into<String>) -> CustomActionError {
    CustomActionError::InvalidTemplate(message.into())
}

fn invalid_input<T>(message: impl Into<String>) -> ActionResult<T> {
    Err(CustomActionError::InvalidInput(message.into()))
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::models::{CustomActionInputDefinition, CustomActionRisk};

    fn fixture() -> (CustomActionDefinition, RepositoryStatus, WorktreeStatus) {
        let id = Uuid::new_v4();
        #[cfg(windows)]
        let (repository_path, worktree_path) =
            (r"C:\tmp\runtime atlas", r"C:\tmp\runtime atlas\worktree");
        #[cfg(not(windows))]
        let (repository_path, worktree_path) =
            ("/tmp/runtime atlas", "/tmp/runtime atlas/worktree");
        let selected = WorktreeStatus {
            path: worktree_path.into(),
            branch: Some("main".into()),
            detached: false,
            sha: "0123456789".into(),
            short_sha: "0123456".into(),
            dirty: false,
            availability: AvailabilityState::Available,
            unavailable_reason: None,
        };
        let repository = RepositoryStatus {
            id,
            path: repository_path.into(),
            name: "runtime atlas".into(),
            availability: AvailabilityState::Available,
            unavailable_reason: None,
            worktrees: vec![selected.clone()],
        };
        let mut action = CustomActionDefinition::new(
            id,
            "Remove worktree",
            "npm run worktree:remove -- {{target}} {{deleteBranch}}",
        );
        action.risk = CustomActionRisk::Destructive;
        action.working_directory = CustomActionWorkingDirectory::RepositoryRoot;
        action.effects = vec!["Deletes the selected worktree".into()];
        action.inputs = vec![
            CustomActionInputDefinition {
                id: Uuid::new_v4(),
                key: "target".into(),
                label: "Worktree".into(),
                kind: CustomActionInputKind::Worktree,
                flag_argument: None,
                is_enabled_by_default: false,
            },
            CustomActionInputDefinition {
                id: Uuid::new_v4(),
                key: "deleteBranch".into(),
                label: "Delete branch".into(),
                kind: CustomActionInputKind::Flag,
                flag_argument: Some("--delete-branch".into()),
                is_enabled_by_default: true,
            },
        ];
        (action, repository, selected)
    }

    #[test]
    fn plans_direct_arguments_and_restart_from_registered_worktrees() {
        let (mut action, repository, selected) = fixture();
        let values = initial_action_values(&action, &selected.path);
        let plan = plan_custom_action(&action, &values, &repository, &selected).unwrap();
        assert_eq!(plan.executable, "npm");
        assert_eq!(
            plan.arguments,
            [
                "run",
                "worktree:remove",
                "--",
                selected.path.as_str(),
                "--delete-branch"
            ]
        );
        assert_eq!(
            plan.current_directory,
            canonical_path(Path::new(&repository.path))
        );
        assert!(
            plan.display_command
                .contains(&format!("'{}'", selected.path))
        );

        action.kind = CustomActionKind::Session;
        action.restart_command_template = Some("npm run dev:restart".into());
        assert_eq!(
            plan_custom_action_restart(&action, &BTreeMap::new(), &repository, &selected)
                .unwrap()
                .arguments,
            ["run", "dev:restart"]
        );
    }

    #[test]
    fn rejects_shells_injections_and_unregistered_paths_before_execution() {
        let (mut action, repository, selected) = fixture();
        for unsafe_template in [
            "npm run dev && touch /tmp/no",
            "echo $(whoami)",
            "/bin/sh -c 'touch /tmp/no'",
            r"C:\Windows\System32\cmd.exe /C whoami",
            "powershell -EncodedCommand ZQBjAGgAbwA=",
            "/usr/bin/env DEBUG=1 npm run dev",
        ] {
            action.command_template = unsafe_template.into();
            assert!(
                validate_custom_action(&action).is_err(),
                "{unsafe_template}"
            );
        }

        action.command_template = "npm run remove -- {{target}}".into();
        action.inputs.truncate(1);
        #[cfg(windows)]
        let unregistered = r"C:\tmp\not-registered";
        #[cfg(not(windows))]
        let unregistered = "/tmp/not-registered";
        let values = BTreeMap::from([("target".into(), unregistered.into())]);
        assert!(matches!(
            plan_custom_action(&action, &values, &repository, &selected),
            Err(CustomActionError::InvalidWorktree(_))
        ));

        action.command_template = "{{target}} -c whoami".into();
        action.inputs[0].kind = CustomActionInputKind::Text;
        let values = BTreeMap::from([("target".into(), "/bin/sh".into())]);
        assert!(matches!(
            plan_custom_action(&action, &values, &repository, &selected),
            Err(CustomActionError::InvalidTemplate(_))
        ));

        let values = BTreeMap::from([("target".into(), "token=do-not-pass".into())]);
        assert!(matches!(
            plan_custom_action(&action, &values, &repository, &selected),
            Err(CustomActionError::InvalidInput(_))
        ));
    }

    #[test]
    fn sanitizes_child_output_credentials_and_urls() {
        let output = sanitize_output(
            "Authorization: Bearer private\nurl=https://user:pass@example.invalid/x token = secret\nBasic c2VjcmV0\n",
        );
        assert!(
            ["private", "user:pass", "secret", "c2VjcmV0"]
                .iter()
                .all(|secret| !output.contains(secret))
        );
        assert!(output.contains("authorization: <redacted>"));
        assert!(output.contains("<redacted-url>"));
        assert!(output.contains("token = <redacted>"));
    }
}
