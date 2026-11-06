//! The timeline's only link to the common translation utility. Keys are the
//! Flutter catalog keys (`packages/flutter_app/assets/locales/{en,ko}.json`).

pub fn t(key: &str) -> String {
    crate::common::tr(key)
}

pub fn ta(key: &str, args: &[(&str, &str)]) -> String {
    crate::common::tr_args(key, args)
}
