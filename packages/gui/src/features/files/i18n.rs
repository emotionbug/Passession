//! Files translation boundary. Catalog keys go through the shell's `common::tr`; copy the
//! original app hard-coded in English (editor settings, transfer dialog, HTML card) is kept here
//! with its Korean translation, selected by `common::locale_tag()`.

use crate::common;

pub fn t(key: &str) -> String {
    common::tr(key)
}

pub fn ta(key: &str, args: &[(&str, &str)]) -> String {
    common::tr_args(key, args)
}

/// Original English copy (`l("Editor settings")`) in the current language.
pub fn l(english: &'static str) -> &'static str {
    if common::locale_tag() != "ko" {
        return english;
    }
    KO.iter().find(|(en, _)| *en == english).map(|(_, ko)| *ko).unwrap_or(english)
}

/// `l` with `{name}` placeholders replaced.
pub fn lf(english: &'static str, args: &[(&str, &str)]) -> String {
    let mut text = l(english).to_owned();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

const KO: &[(&str, &str)] = &[
    ("Added to chat draft", "채팅 초안에 추가했습니다"),
    ("File reference added to chat; unsaved edits are not uploaded", "채팅에 파일 참조를 추가했습니다. 저장하지 않은 편집은 업로드되지 않습니다"),
    ("Rename {name}", "{name} 이름 바꾸기"),
    ("Save / Download", "저장 / 다운로드"),
    ("Open a downloaded copy", "다운로드한 사본 열기"),
    ("HTML previews open in your system browser.", "HTML 미리보기는 시스템 브라우저에서 열립니다."),
    ("Relative workspace assets are served locally; scripts are disabled.", "상대 경로의 워크스페이스 에셋은 로컬에서 제공되며 스크립트는 비활성화됩니다."),
    ("Open HTML preview", "HTML 미리보기 열기"),
    ("Preview HTML in system browser", "시스템 브라우저에서 HTML 미리보기"),
    ("Find / Replace", "찾기 / 바꾸기"),
    ("Go to line", "줄로 이동"),
    ("Go to line:column", "줄:열로 이동"),
    ("Editor settings", "편집기 설정"),
    ("Save a copy / Download", "사본 저장 / 다운로드"),
    ("Open downloaded copy externally", "다운로드한 사본을 외부 앱에서 열기"),
    ("Open in editor / file manager", "편집기 / 파일 관리자에서 열기"),
    ("Open with", "다음으로 열기"),
    ("Add selected lines to chat", "선택한 줄을 채팅에 추가"),
    ("Match case", "대소문자 구분"),
    ("Word wrap", "자동 줄 바꿈"),
    ("Syntax highlighting", "구문 강조"),
    ("Code font size", "코드 글꼴 크기"),
    ("Smaller", "작게"),
    ("Larger", "크게"),
    ("Downloading {name}", "{name} 다운로드 중"),
    ("{received} / {total} bytes", "{received} / {total} 바이트"),
    ("Cancel", "취소"),
    ("Saved to {path}", "{path}에 저장했습니다"),
    ("Copied", "복사했습니다"),
    ("Directory navigation", "디렉터리 탐색"),
    ("Parent directory", "상위 디렉터리"),
    ("Forward", "앞으로"),
    ("Go to directory…", "디렉터리로 이동…"),
    ("Remote directory", "원격 디렉터리"),
    ("Use this directory", "이 디렉터리 사용"),
    ("Choose a remote directory", "원격 디렉터리 선택"),
    ("Local desktop host is not available", "로컬 데스크톱 호스트를 사용할 수 없습니다"),
    ("This file belongs to a different host. Open a downloaded copy instead.", "이 파일은 다른 호스트에 있습니다. 대신 다운로드한 사본을 여세요."),
    ("This link scheme is not allowed: {scheme}", "허용되지 않는 링크 스킴입니다: {scheme}"),
    ("Unsupported image URI", "지원하지 않는 이미지 URI"),
    ("Host is disconnected", "호스트 연결이 끊어졌습니다"),
    ("Update the host to edit files.", "파일을 편집하려면 호스트를 업데이트하세요."),
    ("No application can open {name}", "{name}을(를) 열 수 있는 앱이 없습니다"),
    ("The system browser could not open the HTML preview", "시스템 브라우저에서 HTML 미리보기를 열 수 없습니다"),
    ("Directory listing unavailable", "디렉터리 목록을 사용할 수 없습니다"),
    ("Back", "뒤로"),
    ("Cut", "잘라내기"),
    ("Paste", "붙여넣기"),
    ("Platform services are not available", "플랫폼 서비스를 사용할 수 없습니다"),
    ("Remote save was not acknowledged; local edits are preserved", "원격 저장이 확인되지 않았습니다. 로컬 편집은 보존됩니다"),
];
