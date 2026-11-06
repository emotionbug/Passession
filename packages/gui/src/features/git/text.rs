//! Localized copy for the git feature. Keys are the Flutter catalog keys
//! (`assets/locales/{en,ko}.json`); lookup, `{{name}}` interpolation and
//! `_one`/`_other` plural selection belong to the shell's translation utility.

use super::host;

/// A catalog key with its interpolation arguments. Policies return these so
/// copy is resolved in the reader's locale at draw time.
#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    pub key: String,
    pub args: Vec<(&'static str, String)>,
}

impl Msg {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            args: Vec::new(),
        }
    }

    pub fn arg(mut self, name: &'static str, value: impl ToString) -> Self {
        self.args.push((name, value.to_string()));
        self
    }
}

/// Resolves `key` with `args`; a numeric `count` argument selects the plural form.
pub fn tr(key: &str, args: &[(&'static str, String)]) -> String {
    host::translate(key, args)
}

pub fn t(key: &str) -> String {
    host::translate(key, &[])
}

pub fn msg(message: &Msg) -> String {
    host::translate(&message.key, &message.args)
}

/// `t(key, { context: changeRequestContext })`: GitLab uses the `_mr`
/// variant of a key when the catalog has one.
pub fn tr_forge(key: &str, merge_request: bool, args: &[(&'static str, String)]) -> String {
    if merge_request {
        let variant = format!("{key}_mr");
        let text = host::translate(&variant, args);
        if text != variant {
            return text;
        }
    }
    host::translate(key, args)
}
