//! Optional Gardenia collaboration integration for Runner missions.
//!
//! Gardenia remains the durable, agent-independent coordination layer while
//! Runner owns process launch and the live message bus.  A Gardenia-enabled
//! mission creates one Gardenia session per Crew slot, stores the mapping next
//! to the mission event log, and injects task-claim instructions into each
//! agent's first turn.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{Error, Result},
    model::{Mission, SlotWithRunner},
};

const SIDECAR_NAME: &str = "gardenia.json";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectRegistration {
    id: String,
    path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GardeniaMissionState {
    pub schema_version: u32,
    pub project_id: String,
    pub vault_path: String,
    pub collab_script: String,
    pub pwsh_command: String,
    pub slots: Vec<GardeniaSlotState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GardeniaSlotState {
    pub slot_handle: String,
    pub actor: String,
    pub session_id: String,
    pub session_path: String,
    pub session_sha256: String,
}

#[derive(Debug, Deserialize)]
struct CollabResult {
    path: String,
    sha256: String,
    object: CollabObject,
}

#[derive(Debug, Deserialize)]
struct CollabObject {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ActiveTaskClaim {
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct ActiveTask {
    id: String,
    claim: Option<ActiveTaskClaim>,
}

#[derive(Debug, Deserialize)]
struct SessionStatusView {
    status: String,
}

/// Create Gardenia sessions for every mission slot and persist their mapping.
///
/// This runs before PTY registration.  If any session creation or the sidecar
/// write fails, all sessions created so far are closed best-effort so the Vault
/// does not accumulate a half-started Runner mission.
pub fn prepare_mission(
    mission_dir: &Path,
    mission: &Mission,
    roster: &[SlotWithRunner],
) -> Result<GardeniaMissionState> {
    let cwd = mission.cwd.as_deref().ok_or_else(|| {
        Error::msg(
            "Gardenia mode requires a working directory so Runner can resolve the registered project",
        )
    })?;
    let vault_path = gardenia_vault_path()?;
    let project = resolve_project(&vault_path, cwd)?;
    let collab_script = gardenia_collab_script()?;
    if !collab_script.is_file() {
        return Err(Error::msg(format!(
            "Gardenia collaboration script not found: {}",
            collab_script.display()
        )));
    }
    let pwsh_command = gardenia_pwsh_command();
    let write_scope = format!("project://{}/", project.id);
    let mut slots = Vec::with_capacity(roster.len());

    for member in roster {
        let actor = actor_name(&member.runner.runtime, &member.slot.slot_handle);
        let args = vec![
            "-Action".to_string(),
            "session-start".to_string(),
            "-VaultPath".to_string(),
            vault_path.to_string_lossy().to_string(),
            "-ProjectId".to_string(),
            project.id.clone(),
            "-Actor".to_string(),
            actor.clone(),
            "-WriteSet".to_string(),
            write_scope.clone(),
        ];
        match invoke_collab(&pwsh_command, &collab_script, &args) {
            Ok(result) => slots.push(GardeniaSlotState {
                slot_handle: member.slot.slot_handle.clone(),
                actor,
                session_id: result.object.id,
                session_path: result.path,
                session_sha256: result.sha256,
            }),
            Err(error) => {
                close_sessions_best_effort(
                    &pwsh_command,
                    &collab_script,
                    &vault_path,
                    &project.id,
                    &slots,
                    "Runner mission setup rolled back",
                );
                return Err(error);
            }
        }
    }

    let state = GardeniaMissionState {
        schema_version: 1,
        project_id: project.id,
        vault_path: vault_path.to_string_lossy().to_string(),
        collab_script: collab_script.to_string_lossy().to_string(),
        pwsh_command,
        slots,
    };
    if let Err(error) = write_sidecar(mission_dir, &state) {
        close_sessions_best_effort(
            &state.pwsh_command,
            Path::new(&state.collab_script),
            Path::new(&state.vault_path),
            &state.project_id,
            &state.slots,
            "Runner mission sidecar write rolled back",
        );
        return Err(error);
    }
    Ok(state)
}

/// Load the optional Gardenia sidecar for reset/resume flows. Missions created
/// through the standard start command simply return `None`.
pub fn load_mission(mission_dir: &Path) -> Result<Option<GardeniaMissionState>> {
    let path = mission_dir.join(SIDECAR_NAME);
    if !path.is_file() {
        return Ok(None);
    }
    let state: GardeniaMissionState =
        serde_json::from_slice(&fs::read(&path)?).map_err(|error| {
            Error::msg(format!(
                "invalid Gardenia mission sidecar {}: {error}",
                path.display()
            ))
        })?;
    if state.schema_version != 1 {
        return Err(Error::msg(format!(
            "unsupported Gardenia mission sidecar schema {} in {}",
            state.schema_version,
            path.display()
        )));
    }
    Ok(Some(state))
}

/// Add the slot-specific Gardenia contract to a Runner-composed first turn.
pub fn append_first_turn(
    mut base: String,
    state: &GardeniaMissionState,
    slot_handle: &str,
    lead: bool,
) -> Result<String> {
    let slot = state
        .slots
        .iter()
        .find(|slot| slot.slot_handle == slot_handle)
        .ok_or_else(|| {
            Error::msg(format!(
                "Gardenia sidecar has no session for slot @{slot_handle}"
            ))
        })?;
    base.push_str("\n\n");
    base.push_str(&gardenia_prompt(state, slot, lead));
    Ok(base)
}

/// Close every Gardenia session created for a mission that did not finish
/// Runner's synchronous startup phase.  No task can have been assigned yet,
/// so the original session hashes are still valid.
pub fn rollback_mission(state: &GardeniaMissionState, summary: &str) {
    close_sessions_best_effort(
        &state.pwsh_command,
        Path::new(&state.collab_script),
        Path::new(&state.vault_path),
        &state.project_id,
        &state.slots,
        summary,
    );
}

/// Refuse terminal mission archive while one of its Gardenia sessions still
/// owns an active task. This check runs before Runner kills the PTYs so the
/// operator can ask the owning agent to complete or hand off the task.
pub fn ensure_sessions_releasable(state: &GardeniaMissionState) -> Result<()> {
    let active_tasks_dir = Path::new(&state.vault_path).join("tasks").join("active");
    if !active_tasks_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(active_tasks_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let task: ActiveTask = serde_json::from_slice(&fs::read(&path)?).map_err(|error| {
            Error::msg(format!(
                "invalid Gardenia active task {}: {error}",
                path.display()
            ))
        })?;
        let Some(claim) = task.claim else {
            continue;
        };
        if let Some(slot) = state
            .slots
            .iter()
            .find(|slot| slot.session_id == claim.session_id)
        {
            return Err(Error::msg(format!(
                "cannot archive Gardenia mission: task {} is still claimed by @{} (session {}); complete or hand off the task first",
                task.id, slot.slot_handle, slot.session_id
            )));
        }
    }
    Ok(())
}

/// Close all durable Gardenia sessions after their Runner mission becomes
/// terminal. Already-closed sessions are skipped, which makes retry safe after
/// a partial external failure.
pub fn close_mission_sessions(state: &GardeniaMissionState) -> Result<()> {
    for slot in &state.slots {
        let session: SessionStatusView = serde_json::from_slice(&fs::read(&slot.session_path)?)
            .map_err(|error| {
                Error::msg(format!(
                    "invalid Gardenia session {}: {error}",
                    slot.session_path
                ))
            })?;
        if session.status == "closed" {
            continue;
        }
        let args = vec![
            "-Action".to_string(),
            "session-close".to_string(),
            "-VaultPath".to_string(),
            state.vault_path.clone(),
            "-ProjectId".to_string(),
            state.project_id.clone(),
            "-SessionId".to_string(),
            slot.session_id.clone(),
            "-ExpectedHash".to_string(),
            slot.session_sha256.clone(),
            "-Summary".to_string(),
            "Runner mission archived".to_string(),
        ];
        invoke_collab(&state.pwsh_command, Path::new(&state.collab_script), &args)?;
    }
    Ok(())
}

fn gardenia_prompt(state: &GardeniaMissionState, slot: &GardeniaSlotState, lead: bool) -> String {
    let script = shell_quote(&state.collab_script);
    let vault = shell_quote(&state.vault_path);
    let project = shell_quote(&state.project_id);
    let actor = shell_quote(&slot.actor);
    let session = shell_quote(&slot.session_id);
    let prefix = format!(
        "{} -NoProfile -File {} -VaultPath {}",
        state.pwsh_command, script, vault
    );

    let mut out = format!(
        "== Gardenia collaboration (required) ==\n\
This Runner mission is registered as Gardenia project `{}`. Your durable Gardenia identity is actor `{}` / session `{}`. The session's project-wide write-set is only an eligibility boundary; it does not authorize edits until you claim a narrower task.\n\n",
        state.project_id, slot.actor, slot.session_id
    );

    if lead {
        out.push_str(
            "Before any agent edits files, decompose the mission into independently writable tasks. Give each task the narrowest practical `project://...` write-set; overlapping tasks must not be worked concurrently. Create each task with:\n",
        );
        out.push_str(&format!(
            "    {prefix} -Action task-create -ProjectId {project} -Actor {actor} -Objective '<subtask>' -AcceptanceCriteria '<done condition>' -WriteSet 'project://{}/<path>'\n",
            state.project_id
        ));
        out.push_str(
            "Read `object.id` and `sha256` from the JSON result, then assign the task ID and scope to exactly one worker with `runner msg post --to <handle> \"...\"`. The worker must claim it before editing. If you will edit too, create and claim a separate task with your own session. Use one writer for shared integration files, and ask idle workers to review or test instead of sharing a mutable object.\n\n",
        );
        out.push_str(&format!(
            "To claim your own task, first run `{prefix} -Action task-show -TaskId <task-id>`, then pass its latest `sha256` to:\n    {prefix} -Action task-claim -TaskId <task-id> -SessionId {session} -ExpectedHash <sha256>\n"
        ));
    } else {
        out.push_str(
            "Do not edit project files yet. Read your Runner inbox and wait for the lead to assign a Gardenia task ID and write-set. Verify the task, then claim it using the latest hash:\n",
        );
        out.push_str(&format!(
            "    {prefix} -Action task-show -TaskId <task-id>\n    {prefix} -Action task-claim -TaskId <task-id> -SessionId {session} -ExpectedHash <sha256>\n"
        ));
        out.push_str(
            "Only edit objects covered by the claimed task. If the requested work falls outside that scope, message the lead instead of expanding it silently.\n",
        );
    }

    out.push_str(&format!(
        "\nAfter finishing, read the task again for a fresh hash and either complete it:\n    {prefix} -Action task-complete -TaskId <task-id> -SessionId {session} -ExpectedHash <sha256> -Summary '<result and validation>'\nor hand it off with `-Action task-handoff` and the same identity/hash arguments. Report the outcome to the lead through Runner. Never expose secrets in task text, commands, logs, or messages."
    ));
    out
}

fn gardenia_vault_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("GARDENIA_VAULT") {
        return Ok(PathBuf::from(path));
    }
    let profile = std::env::var_os("USERPROFILE")
        .ok_or_else(|| Error::msg("USERPROFILE is not set; set GARDENIA_VAULT explicitly"))?;
    Ok(PathBuf::from(profile).join("gardenia-vault"))
}

fn gardenia_collab_script() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("GARDENIA_COLLAB_SCRIPT") {
        return Ok(PathBuf::from(path));
    }
    let profile = std::env::var_os("USERPROFILE").ok_or_else(|| {
        Error::msg("USERPROFILE is not set; set GARDENIA_COLLAB_SCRIPT explicitly")
    })?;
    Ok(PathBuf::from(profile)
        .join("Desktop")
        .join("gardenia-control-plane")
        .join("scripts")
        .join("gardenia-collab.ps1"))
}

fn gardenia_pwsh_command() -> String {
    std::env::var("GARDENIA_PWSH").unwrap_or_else(|_| "pwsh.exe".to_string())
}

fn resolve_project(vault_path: &Path, cwd: &str) -> Result<ProjectRegistration> {
    let registry_path = vault_path.join("projects.yaml");
    let raw = fs::read_to_string(&registry_path).map_err(|error| {
        Error::msg(format!(
            "cannot read Gardenia project registry {}: {error}",
            registry_path.display()
        ))
    })?;
    let cwd_normalized = normalize_path(cwd);
    let mut matches: Vec<ProjectRegistration> = parse_project_registry(&raw)
        .into_iter()
        .filter(|project| {
            let root = normalize_path(&project.path);
            cwd_normalized == root || cwd_normalized.starts_with(&(root + "/"))
        })
        .collect();
    matches.sort_by_key(|project| std::cmp::Reverse(normalize_path(&project.path).len()));
    matches.into_iter().next().ok_or_else(|| {
        Error::msg(format!(
            "working directory is not inside a project registered in {}: {cwd}",
            registry_path.display()
        ))
    })
}

fn parse_project_registry(raw: &str) -> Vec<ProjectRegistration> {
    let mut projects = Vec::new();
    let mut id: Option<String> = None;
    let mut path: Option<String> = None;

    let flush = |projects: &mut Vec<ProjectRegistration>,
                 id: &mut Option<String>,
                 path: &mut Option<String>| {
        if let (Some(id), Some(path)) = (id.take(), path.take()) {
            projects.push(ProjectRegistration { id, path });
        } else {
            *id = None;
            *path = None;
        }
    };

    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("- id:") {
            flush(&mut projects, &mut id, &mut path);
            id = Some(unquote(value.trim()).to_string());
        } else if id.is_some() {
            if let Some(value) = trimmed.strip_prefix("path:") {
                path = Some(unquote(value.trim()).to_string());
            }
        }
    }
    flush(&mut projects, &mut id, &mut path);
    projects
}

fn normalize_path(raw: &str) -> String {
    let mut value = unquote(raw.trim()).replace('\\', "/");
    if value
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("/mnt/"))
        && value.len() >= 7
    {
        let bytes = value.as_bytes();
        if bytes[5].is_ascii_alphabetic() && bytes[6] == b'/' {
            value = format!("{}:/{}", (bytes[5] as char), &value[7..]);
        }
    }
    while value.ends_with('/') && value.len() > 3 {
        value.pop();
    }
    value.to_lowercase()
}

fn unquote(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
        {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn actor_name(runtime: &str, handle: &str) -> String {
    fn clean(value: &str) -> String {
        let mut out = String::new();
        for ch in value.chars() {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                out.push(ch.to_ascii_lowercase());
            } else if !out.ends_with('-') {
                out.push('-');
            }
        }
        out.trim_matches('-').to_string()
    }
    let runtime = clean(runtime);
    let handle = clean(handle);
    format!(
        "runner.{}.{}",
        if runtime.is_empty() {
            "agent"
        } else {
            &runtime
        },
        if handle.is_empty() { "slot" } else { &handle }
    )
}

fn invoke_collab(command: &str, script: &Path, args: &[String]) -> Result<CollabResult> {
    let mut process = Command::new(command);
    process
        .arg("-NoProfile")
        .arg("-File")
        .arg(script)
        .args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        process.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = process.output().map_err(|error| {
        Error::msg(format!(
            "failed to launch Gardenia collaboration command {command}: {error}"
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(Error::msg(format!(
            "Gardenia collaboration command failed ({}): {}",
            output.status,
            if detail.is_empty() {
                "no diagnostic output"
            } else {
                &detail
            }
        )));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| {
        Error::msg(format!(
            "Gardenia collaboration command returned invalid JSON: {error}"
        ))
    })
}

fn close_sessions_best_effort(
    command: &str,
    script: &Path,
    vault_path: &Path,
    project_id: &str,
    slots: &[GardeniaSlotState],
    summary: &str,
) {
    for slot in slots.iter().rev() {
        let args = vec![
            "-Action".to_string(),
            "session-close".to_string(),
            "-VaultPath".to_string(),
            vault_path.to_string_lossy().to_string(),
            "-ProjectId".to_string(),
            project_id.to_string(),
            "-SessionId".to_string(),
            slot.session_id.clone(),
            "-ExpectedHash".to_string(),
            slot.session_sha256.clone(),
            "-Summary".to_string(),
            summary.to_string(),
        ];
        if let Err(error) = invoke_collab(command, script, &args) {
            log::warn!(
                "failed to roll back Gardenia session {}: {}",
                slot.session_id,
                error
            );
        }
    }
}

fn write_sidecar(mission_dir: &Path, state: &GardeniaMissionState) -> Result<()> {
    fs::create_dir_all(mission_dir)?;
    let target = mission_dir.join(SIDECAR_NAME);
    let mut tmp = tempfile::NamedTempFile::new_in(mission_dir)?;
    let json = serde_json::to_vec_pretty(state)?;
    tmp.write_all(&json)?;
    tmp.write_all(b"\n")?;
    tmp.flush()?;
    tmp.persist(&target)
        .map_err(|error| Error::Io(error.error))?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_and_resolves_longest_project_root() {
        let raw = r#"
projects:
  - id: parent
    path: C:\Users\Haochen\Desktop
  - id: child
    path: C:\Users\Haochen\Desktop\child
    knowledge_module: child
"#;
        let projects = parse_project_registry(raw);
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[1].id, "child");
        let cwd = normalize_path(r"C:\Users\Haochen\Desktop\child\src");
        let mut matching: Vec<_> = projects
            .into_iter()
            .filter(|project| {
                let root = normalize_path(&project.path);
                cwd == root || cwd.starts_with(&(root + "/"))
            })
            .collect();
        matching.sort_by_key(|project| std::cmp::Reverse(project.path.len()));
        assert_eq!(matching[0].id, "child");
    }

    #[test]
    fn normalizes_windows_and_wsl_paths_to_same_value() {
        assert_eq!(
            normalize_path(r"C:\Users\Haochen\Desktop\runner-windows\src"),
            normalize_path("/mnt/c/Users/Haochen/Desktop/runner-windows/src")
        );
        assert_eq!(normalize_path("项目/源码"), "项目/源码");
    }

    #[test]
    fn actor_names_are_valid_for_gardenia() {
        assert_eq!(
            actor_name("Claude Code", "Lead / One"),
            "runner.claude-code.lead-one"
        );
    }

    #[test]
    fn worker_prompt_requires_claim_before_edits() {
        let state = GardeniaMissionState {
            schema_version: 1,
            project_id: "runner-windows".into(),
            vault_path: r"C:\Users\Haochen\gardenia-vault".into(),
            collab_script:
                r"C:\Users\Haochen\Desktop\gardenia-control-plane\scripts\gardenia-collab.ps1"
                    .into(),
            pwsh_command: "pwsh.exe".into(),
            slots: vec![GardeniaSlotState {
                slot_handle: "impl".into(),
                actor: "runner.codex.impl".into(),
                session_id: "SES-test".into(),
                session_path: "session.json".into(),
                session_sha256: "a".repeat(64),
            }],
        };
        let prompt = append_first_turn("base".into(), &state, "impl", false).unwrap();
        assert!(prompt.contains("Do not edit project files yet"));
        assert!(prompt.contains("-Action task-claim"));
        assert!(prompt.contains("SES-test"));
        assert!(prompt.starts_with("base\n\n== Gardenia collaboration"));
    }

    #[test]
    fn lead_prompt_requires_disjoint_tasks_and_assignment() {
        let state = GardeniaMissionState {
            schema_version: 1,
            project_id: "runner-windows".into(),
            vault_path: "vault".into(),
            collab_script: "gardenia-collab.ps1".into(),
            pwsh_command: "pwsh.exe".into(),
            slots: vec![GardeniaSlotState {
                slot_handle: "lead".into(),
                actor: "runner.codex.lead".into(),
                session_id: "SES-lead".into(),
                session_path: "session.json".into(),
                session_sha256: "b".repeat(64),
            }],
        };
        let prompt = append_first_turn("base".into(), &state, "lead", true).unwrap();
        assert!(prompt.contains("independently writable tasks"));
        assert!(prompt.contains("overlapping tasks must not be worked concurrently"));
        assert!(prompt.contains("runner msg post --to <handle>"));
    }

    #[test]
    fn archive_preflight_rejects_claim_owned_by_mission_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let active = tmp.path().join("tasks").join("active");
        fs::create_dir_all(&active).unwrap();
        fs::write(
            active.join("TSK-test.json"),
            br#"{"id":"TSK-test","claim":{"session_id":"SES-worker"}}"#,
        )
        .unwrap();
        let state = GardeniaMissionState {
            schema_version: 1,
            project_id: "runner-windows".into(),
            vault_path: tmp.path().to_string_lossy().to_string(),
            collab_script: "gardenia-collab.ps1".into(),
            pwsh_command: "pwsh.exe".into(),
            slots: vec![GardeniaSlotState {
                slot_handle: "worker".into(),
                actor: "runner.codex.worker".into(),
                session_id: "SES-worker".into(),
                session_path: "session.json".into(),
                session_sha256: "c".repeat(64),
            }],
        };
        let error = ensure_sessions_releasable(&state).unwrap_err();
        assert!(error.to_string().contains("TSK-test"));
        assert!(error.to_string().contains("@worker"));
    }
}
