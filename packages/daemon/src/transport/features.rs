use serde_json::{Value, json};

pub fn features(config: &Value, hoplite: bool) -> Value {
    let mut result = serde_json::Map::new();
    for name in [
        "connectedHosts",
        "hostMultiplexing",
        "relayProtectedSecurity",
        "usageSources",
        "ownedSubscriptions",
        "agentRequestReceipts",
        "agentMessageReceipts",
        "workspaceRequestReceipts",
        "creationLifecycle",
        "hubAgentRpc",
        "backgroundWorkspaces",
        "directorySync",
        "workspaceSetupRun",
        "workspaceSetupObservation",
        "checkoutObservation",
        "workspaceSortTimes",
        "providersSnapshot",
        "providersSnapshotCwd",
        "checkoutForgeSetAutoMerge",
        "checkoutGithubSetAutoMerge",
        "githubCheckDetails",
        "forgeCheckDetails",
        "forgeSearch",
        "daemonConfigReload",
        "durableDaemonUpdates",
        "daemonUpdateCheck",
        "remoteDesktopAppUpdates",
        "daemonSelfUpdate",
        "pushTokenRevocation",
        "plugins",
        "pluginManagement",
        "pluginGitManagement",
        "pluginSourceInstallation",
        "pluginSourceUpdates",
        "pluginLogs",
        "pluginThemes",
        "pluginSettings",
        "pluginTimelineItems",
        "skillManagement",
        "terminal-restore-modes",
        "terminal-input-mode-replay",
        "terminal-size-ownership",
        "workspaceTerminals",
        "rewind",
        "agentTimelinePromptIndex",
        "agentHistorySearch",
        "checkoutRefresh",
        "workspaceMultiplicity",
        "projectRemove",
        "projectAdd",
        "projectList",
        "worktreeRestore",
        "workspaceRecovery",
        "workspaceFileEditing",
        "fileByteWrites",
        "providerUsageList",
        "agentDetach",
        "agentThinkingUpdate",
        "daemonDiagnostics",
        "agentForkContext",
        "agentForkContextCursor",
        "providerSubagents",
        "projectedSubagentTimeline",
        "providerSubagentNesting",
        "providerSubagentDelete",
        "providerTaskStop",
        "workspacePinning",
        "workspaceMarkUnread",
        "hubRelationship",
        "projectGithubClone",
        "workspaceGithubRepositorySearch",
        "projectCreateDirectory",
        "commitsList",
        "commitBaseClassification",
        "providerRemoval",
        "importSessionWorkspaceTarget",
        "importSessionSearch",
        "forgeProviders",
        "selectiveAgentTimeline",
        "explicitEventSubscriptions",
        "canonicalSubmittedPrompts",
        "agentMessageProvenance",
        "agentMessageBatching",
        "stableProjectIdentity",
        "workspaceScriptManagement",
        "projectCustomIcon",
        "fsEntryOps",
        "fsEntryDuplicate",
        "checkoutDiscardChanges",
        "checkoutDiffAll",
        "agentProfiles",
        "agentConfigApply",
        "workspaceLabels",
        "daemonStatusRpc",
        "gitLog",
        "gitSelectedCommit",
        "readOnlyAgentTimeline",
    ] {
        result.insert(name.to_owned(), Value::Bool(true));
    }
    result.insert("hopliteProvider".into(), json!(hoplite));
    if config["relayEnabledMutable"].as_bool().unwrap_or(true) {
        result.insert("relayConfig".into(), Value::Bool(true));
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn selected_commit_capability_is_advertised() {
        assert_eq!(
            super::features(&serde_json::json!({}), false)["gitSelectedCommit"],
            true
        );
    }
}
