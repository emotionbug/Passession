//! The composer's only entry to the shell translation utility (ui-api.md §8). Keys are the
//! Flutter catalog keys (`packages/gui/resources/locales/{en,ko}.json`).

pub fn t(key: &str) -> String {
    crate::common::tr(key)
}

pub fn t_args(key: &str, args: &[(&str, &str)]) -> String {
    crate::common::tr_args(key, args)
}
