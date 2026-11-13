//! The only place settings code touches the common translation utility.
//!
//! Catalog keys (`settings.*`, `providerUsage.*`, …) come from the shared
//! `packages/gui/resources/locales/{en,ko}.json` catalogs through `crate::common`.
//! The Flutter port hardcoded a few English labels (host configuration, hub
//! access, daemon errors); those live in [`LOCAL`] with their Korean text under
//! the `settingsNative.` prefix so both locales stay complete.

/// The resolved catalog language (`preferences.language`, `system` through the OS locale).
pub fn is_korean() -> bool {
    crate::common::locale_tag() == "ko"
}

/// Translate `key`.
pub fn t(key: &str) -> String {
    if let Some(text) = local(key) {
        return text.to_string();
    }
    crate::common::tr(key)
}

/// Translate `key` with `{{name}}` arguments. A `count` argument selects the
/// catalog's `_one`/`_other` plural form in the common utility.
pub fn ta(key: &str, args: &[(&str, &str)]) -> String {
    if let Some(text) = local(key) {
        let mut out = text.to_string();
        for (name, value) in args {
            out = out.replace(&format!("{{{{{name}}}}}"), value);
        }
        return out;
    }
    crate::common::tr_args(key, args)
}

fn local(key: &str) -> Option<&'static str> {
    if !key.starts_with("settingsNative.") {
        return None;
    }
    let korean = is_korean();
    LOCAL.iter().find(|(k, _, _)| *k == key).map(|(_, en, ko)| if korean { *ko } else { *en })
}

/// (key, English, Korean)
const LOCAL: &[(&str, &str, &str)] = &[
    ("settingsNative.saving", "Saving…", "저장 중…"),
    ("settingsNative.required", "Required", "필수 항목입니다"),
    ("settingsNative.positiveInteger", "Enter a positive whole number", "양의 정수를 입력하세요"),
    ("settingsNative.useNameValue", "Use NAME=value", "NAME=value 형식을 사용하세요"),
    ("settingsNative.useKeyValue", "Use key=value", "key=value 형식을 사용하세요"),
    ("settingsNative.hostConfiguration", "Host configuration", "호스트 구성"),
    ("settingsNative.serverId", "Server ID", "서버 ID"),
    ("settingsNative.grantedPermissions", "Granted permissions: {{permissions}}", "부여된 권한: {{permissions}}"),
    ("settingsNative.readOnlyConfig", "This connection is read-only for daemon configuration.", "이 연결은 데몬 구성을 읽기만 할 수 있습니다."),
    ("settingsNative.noManagePermission", "This connection does not have daemon.manage permission.", "이 연결에는 daemon.manage 권한이 없습니다."),
    ("settingsNative.hostDiagnostics", "Host diagnostics", "호스트 진단"),
    ("settingsNative.relayEnabled", "Relay enabled", "릴레이 사용"),
    ("settingsNative.relayEnabledHint", "Relay lets paired devices connect from anywhere. Pairing traffic is end-to-end encrypted.", "릴레이를 사용하면 페어링한 기기가 어디에서나 연결할 수 있습니다. 페어링 트래픽은 종단 간 암호화됩니다."),
    ("settingsNative.relayOffTitle", "Turn off relay?", "릴레이를 끌까요?"),
    ("settingsNative.relayOffMessage", "Paired devices and relay connections will disconnect, including this app if it connects through the relay.", "페어링한 기기와 릴레이 연결이 끊어집니다. 이 앱이 릴레이로 연결되어 있다면 이 앱도 끊어집니다."),
    ("settingsNative.relayOffAction", "Turn off relay", "릴레이 끄기"),
    ("settingsNative.requireProtectedRelay", "Require protected relay security", "보호된 릴레이 보안 필수"),
    ("settingsNative.requireProtectedRelayHint", "Refuse relay clients that do not use protected security.", "보호된 보안을 사용하지 않는 릴레이 클라이언트를 거부합니다."),
    ("settingsNative.requireProtectedTitle", "Require protected relay security?", "보호된 릴레이 보안을 필수로 할까요?"),
    ("settingsNative.requireProtectedMessage", "Relay clients without protected security will disconnect and cannot reconnect until they are updated.", "보호된 보안을 지원하지 않는 릴레이 클라이언트는 연결이 끊어지며 업데이트하기 전까지 다시 연결할 수 없습니다."),
    ("settingsNative.requireProtectedAction", "Require", "필수로 설정"),
    ("settingsNative.permissionDenied", "Denied", "거부됨"),
    ("settingsNative.useConnection", "Use", "사용"),
    ("settingsNative.editConnection", "Edit connection", "연결 편집"),
    ("settingsNative.activeConnection", "Active", "사용 중"),
    ("settingsNative.noConnections", "No saved connections", "저장된 연결이 없습니다"),
    ("settingsNative.addConnection", "Add connection", "연결 추가"),
    ("settingsNative.addConnectionHint", "Reach this host another way: direct, relay or SSH.", "직접 연결, 릴레이 또는 SSH 등 다른 방법으로 이 호스트에 연결합니다."),
    ("settingsNative.showQrCode", "Show QR code", "QR 코드 표시"),
    ("settingsNative.scrollbackLines", "{{count}} lines", "{{count}}줄"),
    ("settingsNative.networkLimits", "Network and Git limits", "네트워크 및 Git 제한"),
    ("settingsNative.networkLimitsHint", "Allowed hostnames, trusted proxies, CORS origins and Git process limits.", "허용 호스트 이름, 신뢰할 수 있는 프록시, CORS 출처 및 Git 프로세스 제한입니다."),
    ("settingsNative.hostnames", "Allowed hostnames (one per line; * permits all)", "허용 호스트 이름 (한 줄에 하나, *는 모두 허용)"),
    ("settingsNative.trustedProxies", "Trusted proxies (one per line; * trusts all)", "신뢰할 수 있는 프록시 (한 줄에 하나, *는 모두 신뢰)"),
    ("settingsNative.corsOrigins", "Allowed CORS origins", "허용 CORS 출처"),
    ("settingsNative.gitRate", "Git processes per second", "초당 Git 프로세스 수"),
    ("settingsNative.gitConcurrency", "Concurrent Git processes", "동시 Git 프로세스 수"),
    ("settingsNative.catalogTimeout", "Catalog refresh timeout (ms)", "카탈로그 새로 고침 제한 시간 (ms)"),
    ("settingsNative.reloadConfig", "Reload configuration from disk", "디스크에서 구성 다시 불러오기"),
    ("settingsNative.reloadConfigHint", "Shows paths applied, restart-required and controlled by overrides.", "적용된 경로, 재시작이 필요한 경로, 재정의로 제어되는 경로를 보여 줍니다."),
    ("settingsNative.configReload", "Configuration reload", "구성 다시 불러오기"),
    ("settingsNative.reloadNoChanges", "Configuration reloaded. Nothing changed.", "구성을 다시 불러왔습니다. 변경된 내용이 없습니다."),
    ("settingsNative.reloadRestartRequired", "Requires a daemon restart", "데몬을 재시작해야 적용됨"),
    ("settingsNative.reloadOverrides", "Controlled by daemon launch overrides", "데몬 실행 재정의로 제어됨"),
    ("settingsNative.reloadApplied", "Applied", "적용됨"),
    ("settingsNative.providerConfig", "{{id}} configuration", "{{id}} 구성"),
    ("settingsNative.displayName", "Display name", "표시 이름"),
    ("settingsNative.commandLines", "Command and arguments (one per line)", "명령과 인수 (한 줄에 하나)"),
    ("settingsNative.envLines", "Environment variables (NAME=value, one per line)", "환경 변수 (NAME=value, 한 줄에 하나)"),
    ("settingsNative.paseoTools", "Enable Paseo tools for this provider", "이 프로바이더에 Paseo 도구 사용"),
    ("settingsNative.disabledTools", "Disabled Paseo tool IDs (one per line)", "비활성화할 Paseo 도구 ID (한 줄에 하나)"),
    ("settingsNative.archiveMerged", "Archive merged PR workspaces", "병합된 PR 워크스페이스 보관"),
    ("settingsNative.archiveMergedHint", "Automatically archive clean Paseo workspaces after their pull request is merged", "풀 리퀘스트가 병합되면 깨끗한 Paseo 워크스페이스를 자동으로 보관합니다"),
    ("settingsNative.terminalAgents", "Terminal agents", "터미널 에이전트"),
    ("settingsNative.terminalHooks", "Enable terminal agent hooks", "터미널 에이전트 훅 사용"),
    ("settingsNative.terminalHooksHint", "Get notifications and status from terminal agents. This installs hooks in your agent config files.", "터미널 에이전트의 알림과 상태를 받습니다. 에이전트 구성 파일에 훅을 설치합니다."),
    ("settingsNative.connectedHosts", "Daemon-connected hosts", "데몬 연결 호스트"),
    ("settingsNative.connectedHostsInfo", "Daemon-owned host routing registrations, not local app saved hosts.", "앱에 저장된 호스트가 아니라 데몬이 소유한 호스트 라우팅 등록입니다."),
    ("settingsNative.registerConnectedHost", "Register connected host", "연결 호스트 등록"),
    ("settingsNative.editConnectedHost", "Edit connected host", "연결 호스트 편집"),
    ("settingsNative.connectedHostsDescription", "These registrations belong to this daemon. They are separate from the app’s saved host connections.", "이 등록은 이 데몬에 속합니다. 앱에 저장된 호스트 연결과는 별개입니다."),
    ("settingsNative.label", "Label", "레이블"),
    ("settingsNative.connectionLink", "Connection link or endpoint", "연결 링크 또는 엔드포인트"),
    ("settingsNative.connectionLinkKeep", "Connection link or endpoint (blank preserves existing)", "연결 링크 또는 엔드포인트 (비워 두면 기존 값 유지)"),
    ("settingsNative.enabled", "Enabled", "사용"),
    ("settingsNative.replaceIdentity", "Explicitly replace registered daemon identity", "등록된 데몬 ID를 명시적으로 교체"),
    ("settingsNative.metadataLines", "Metadata (key=value, one per line)", "메타데이터 (key=value, 한 줄에 하나)"),
    ("settingsNative.removeConnectedHostTitle", "Remove connected host?", "연결 호스트를 제거할까요?"),
    ("settingsNative.removeConnectedHostMessage", "Remove this daemon-owned registration and its routes?", "데몬이 소유한 이 등록과 라우트를 제거할까요?"),
    ("settingsNative.noConnectedHosts", "No connected hosts", "연결 호스트 없음"),
    ("settingsNative.hubAccess", "Hub access & permissions", "허브 접근 및 권한"),
    ("settingsNative.relationship", "Relationship", "관계"),
    ("settingsNative.connectHub", "Connect hub", "허브 연결"),
    ("settingsNative.hubUrl", "Hub URL", "허브 URL"),
    ("settingsNative.registrationToken", "Registration token", "등록 토큰"),
    ("settingsNative.disconnect", "Disconnect", "연결 해제"),
    ("settingsNative.disconnectHubTitle", "Disconnect hub?", "허브 연결을 해제할까요?"),
    ("settingsNative.disconnectHubMessage", "Revoke this hub relationship?", "이 허브 관계를 취소할까요?"),
    ("settingsNative.startDaemon", "Start daemon", "데몬 시작"),
    ("settingsNative.updateDiagnostics", "Update diagnostics", "업데이트 진단"),
    ("settingsNative.identityChanged", "Daemon identity changed", "데몬 ID가 변경되었습니다"),
    ("settingsNative.replacementUnconfirmed", "Replacement worker could not be confirmed. Check daemon status and logs.", "교체 워커를 확인하지 못했습니다. 데몬 상태와 로그를 확인하세요."),
    ("settingsNative.restartAcknowledged", "Restart acknowledged: {{acknowledged}}. {{error}}", "재시작 확인됨: {{acknowledged}}. {{error}}"),
    ("settingsNative.packageFailed", "Package installation failed", "패키지 설치에 실패했습니다"),
    ("settingsNative.versionMismatch", "Expected installed version {{expected}}; observed worker {{observed}}.", "설치된 버전 {{expected}}을(를) 예상했지만 워커는 {{observed}}입니다."),
    ("settingsNative.packageUnconfirmed", "Package installed; replacement worker version was not confirmed. {{error}}", "패키지는 설치되었지만 교체 워커 버전을 확인하지 못했습니다. {{error}}"),
    ("settingsNative.unknownVersion", "unknown", "알 수 없음"),
    ("settingsNative.otherOperation", "Daemon answered for a different update operation.", "데몬이 다른 업데이트 작업에 대해 응답했습니다."),
    ("settingsNative.desktopUnavailable", "The native desktop host service has not initialized.", "네이티브 데스크톱 호스트 서비스가 초기화되지 않았습니다."),
    ("settingsNative.hopliteUpdate", "Update the host to add Hoplite.", "Hoplite를 추가하려면 호스트를 업데이트하세요."),
    ("settingsNative.profileRemoved", "This profile was removed on the host while the editor was open. Close the editor and create a new profile instead.", "편집기가 열려 있는 동안 호스트에서 이 프로필이 제거되었습니다. 편집기를 닫고 새 프로필을 만드세요."),
    ("settingsNative.usageNotReady", "Host connection is not ready", "호스트 연결이 준비되지 않았습니다"),
    ("settingsNative.usageUpdateHost", "Update the host to see usage.", "사용량을 보려면 호스트를 업데이트하세요."),
    ("settingsNative.usageIncomplete", "Some usage sources or sessions are unavailable. Received reports remain visible.", "일부 사용량 소스나 세션을 사용할 수 없습니다. 받은 보고서는 계속 표시됩니다."),
    ("settingsNative.invalidEndpoint", "Invalid endpoint — edit this connection", "잘못된 엔드포인트 — 이 연결을 편집하세요"),
    ("settingsNative.maintenance", "Daemon update in progress", "데몬 업데이트 진행 중"),
    ("settingsNative.statusOnline", "Online", "온라인"),
    ("settingsNative.statusConnecting", "Connecting", "연결 중"),
    ("settingsNative.statusError", "Error", "오류"),
    ("settingsNative.statusOffline", "Offline", "오프라인"),
];
