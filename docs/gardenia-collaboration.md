# Gardenia collaboration mode

Runner provides the live execution layer (Crew launch, PTYs, inboxes, and the
human-facing workspace). Gardenia provides the durable coordination layer
(agent identities, task claims, write sets, leases, and handoffs). Gardenia
mode connects the two without changing standard Runner missions.

## Start a mission

1. Create or select a Crew with exactly one lead and at least one worker.
2. Open **Start mission** and enter the goal.
3. Choose a working directory that is registered in
   `C:\Users\Haochen\gardenia-vault\projects.yaml`.
4. Enable **Gardenia collaboration** and start the mission.

Runner then:

- resolves the working directory to the longest matching Gardenia project;
- calls `gardenia-collab.ps1` to create one durable Gardenia session per slot;
- writes the mapping to the mission's `gardenia.json` sidecar;
- injects each slot's Gardenia identity and exact task commands into its first
  turn;
- tells the lead to create narrow, non-overlapping tasks and assign their IDs
  through the Runner inbox;
- tells every worker to claim its assigned task before editing and to complete
  or hand it off afterward.

The ordinary **Start mission** path is unchanged when the checkbox is off.

## Safety contract

- A Gardenia session's project-wide write set means the agent is eligible to
  claim work in that project; it is not permission to edit the whole project.
- Actual edit ownership comes from a claimed task with a narrow write set.
- Two concurrently claimed tasks cannot overlap.
- Shared integration files have one writer. Other agents review or test them.
- Task text, commands, messages, and logs must not contain secrets.

## Files and configuration

Defaults:

- Vault: `%USERPROFILE%\gardenia-vault`
- Collaboration script:
  `%USERPROFILE%\Desktop\gardenia-control-plane\scripts\gardenia-collab.ps1`
- PowerShell command: `pwsh.exe`

They can be overridden for development with `GARDENIA_VAULT`,
`GARDENIA_COLLAB_SCRIPT`, and `GARDENIA_PWSH` respectively.

The per-mission sidecar is stored beside `events.ndjson` and `roster.json` as
`gardenia.json`. Resetting the same mission reuses those durable Gardenia
sessions and re-injects their identities. If Crew membership changes, start a
new mission so every slot receives a fresh durable session.

Archiving a Gardenia mission closes all of its durable sessions. Runner refuses
the archive while any slot still owns a claimed task; complete or hand off that
task first, then archive again.

## Current boundary

Gardenia mode automates Crew launch, Gardenia session creation, prompt
injection, task decomposition, task assignment, and claim enforcement. Task
creation still happens through the lead agent after it interprets the mission
goal; this is intentional because the useful write-set partition depends on
the repository and the requested change.
