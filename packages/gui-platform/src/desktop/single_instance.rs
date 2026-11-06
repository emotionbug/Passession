//! One primary process per user session. Later launches forward their deep
//! link / project path to it and exit (Linux: session-bus name
//! `sh.paseo.sessions.desktop`; Windows: `Local\sh.paseo.sessions.desktop`
//! mutex + WM_COPYDATA; macOS: LaunchServices plus the GetURL Apple Event).

use crate::Bootstrap;

pub(crate) fn bootstrap(argv: &[String]) -> Bootstrap {
    super::os::bootstrap(argv)
}
