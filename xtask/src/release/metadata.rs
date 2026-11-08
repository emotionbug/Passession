use super::*;
use regex::Regex;
use std::{fs, path::Path};

struct Entry {
    version: String,
    date: String,
    body: String,
}
fn entries(text: &str) -> Result<Vec<Entry>> {
    let re = Regex::new(r"(?m)^##\s+\[?([^\]\s]+)\]?\s*-\s*(\d{4}-\d{2}-\d{2})\s*$")?;
    let headings: Vec<_> = re.captures_iter(text).collect();
    ensure!(!headings.is_empty(), "No release headings in CHANGELOG.md");
    Ok(headings
        .iter()
        .enumerate()
        .map(|(i, c)| Entry {
            version: c[1].into(),
            date: c[2].into(),
            body: text[c.get(0).unwrap().end()
                ..headings
                    .get(i + 1)
                    .map(|n| n.get(0).unwrap().start())
                    .unwrap_or(text.len())]
                .trim()
                .into(),
        })
        .collect())
}
fn changelog() -> Result<Vec<Entry>> {
    entries(&fs::read_to_string(common::root()?.join("CHANGELOG.md"))?)
}
fn options(
    args: &[String],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut values = std::collections::BTreeMap::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let value = if value_flags.contains(&arg.as_str()) {
            let v = iter
                .next()
                .with_context(|| format!("{arg} requires a value"))?;
            ensure!(!v.starts_with("--"), "{arg} requires a value");
            v.clone()
        } else if bool_flags.contains(&arg.as_str()) {
            "true".into()
        } else {
            bail!("Unknown option {arg}");
        };
        ensure!(
            values.insert(arg.clone(), value).is_none(),
            "Duplicate option {arg}"
        );
    }
    Ok(values)
}
pub(super) fn release_notes(v: &str) -> Result<String> {
    let entries = changelog()?;
    let entry = entries
        .iter()
        .find(|e| e.version == v)
        .context("No matching changelog section")?;
    let mentions = Regex::new(r"\[@([A-Za-z0-9-]+)\]\(https://github\.com/([A-Za-z0-9-]+)/?\)")?;
    let body = mentions.replace_all(&entry.body, |c: &regex::Captures<'_>| {
        if c[1].eq_ignore_ascii_case(&c[2]) {
            format!("@{}", &c[2])
        } else {
            c[0].to_owned()
        }
    });
    Ok(format!(
        "## {} - {}\n\n{}\n",
        entry.version, entry.date, body
    ))
}
pub(super) fn notes(args: &[String]) -> Result<()> {
    let opts = options(
        args,
        &["--tag", "--repo", "--output"],
        &["--publish", "--create-if-missing"],
    )?;
    if let Some(repo) = opts.get("--repo") {
        ensure!(repo == REPOSITORY, "Only {REPOSITORY} is supported");
    }
    let entries = changelog()?;
    let v = if let Some(tag) = opts.get("--tag") {
        tag_version(tag)?
    } else {
        version(&entries[0].version)?
    };
    let notes = release_notes(&v.to_string())?;
    if let Some(path) = opts.get("--output") {
        fs::write(path, &notes)?;
    }
    if !opts.contains_key("--publish") {
        print!("{notes}");
        return Ok(());
    }
    remote::repository()?;
    let tag = format!("v{v}");
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("notes.txt");
    fs::write(&path, notes)?;
    let release = match remote::lookup(&tag)? {
        Some(r) => r,
        None => {
            if !opts.contains_key("--create-if-missing") {
                eprintln!("Release {tag} does not exist; skipping notes synchronization.");
                return Ok(());
            }
            let mut args = vec![
                "release",
                "create",
                &tag,
                "--repo",
                REPOSITORY,
                "--title",
                &tag,
                "--notes-file",
                path.to_str().unwrap(),
                "--verify-tag",
                "--draft",
            ];
            if !v.pre.is_empty() {
                args.push("--prerelease");
            }
            let created = remote::gh(&args);
            match remote::lookup(&tag)? {
                Some(r) => r,
                None => {
                    created?;
                    bail!("Created release cannot be resolved");
                }
            }
        }
    };
    let id = release["id"].as_u64().context("Missing release ID")?;
    remote::gh(&[
        "api",
        "--method",
        "PATCH",
        &format!("repos/{REPOSITORY}/releases/{id}"),
        "-F",
        &format!("body=@{}", path.display()),
    ])?;
    Ok(())
}
fn next_version(current: &str, mode: &str) -> Result<String> {
    let mut v = version(current)?;
    match mode {
        "beta-next" => {
            ensure!(!v.pre.is_empty(), "beta-next requires a beta version");
            let n: u64 = v.pre.as_str().strip_prefix("beta.").unwrap().parse()?;
            v.pre = semver::Prerelease::new(&format!(
                "beta.{}",
                n.checked_add(1).context("Beta overflow")?
            ))?;
        }
        "promote" => {
            ensure!(!v.pre.is_empty(), "promote requires a beta version");
            v.pre = semver::Prerelease::EMPTY;
        }
        _ => {
            ensure!(
                v.pre.is_empty(),
                "Promote or advance the current beta before starting a new release line"
            );
            let base = mode.strip_prefix("beta-").unwrap_or(mode);
            match base {
                "patch" => v.patch = v.patch.checked_add(1).context("Patch overflow")?,
                "minor" => {
                    v.minor = v.minor.checked_add(1).context("Minor overflow")?;
                    v.patch = 0;
                }
                "major" => {
                    v.major = v.major.checked_add(1).context("Major overflow")?;
                    v.minor = 0;
                    v.patch = 0;
                }
                _ => bail!("Unknown version mode {mode}"),
            }
            if mode.starts_with("beta-") {
                v.pre = semver::Prerelease::new("beta.1")?;
            }
        }
    }
    Ok(v.to_string())
}
/// Edits release metadata only; never stages, commits, tags or pushes a release.
pub(super) fn set_version(args: &[String]) -> Result<()> {
    let opts = options(args, &["--mode", "--set"], &["--print"])?;
    let current = common::version()?;
    if opts.is_empty() || (opts.len() == 1 && opts.contains_key("--print")) {
        println!("{current}");
        return Ok(());
    }
    ensure!(
        opts.contains_key("--mode") != opts.contains_key("--set"),
        "version requires exactly one of --mode or --set"
    );
    let next = if let Some(value) = opts.get("--set") {
        version(value)?.to_string()
    } else {
        next_version(&current, &opts["--mode"])?
    };
    if opts.contains_key("--print") {
        println!("{next}");
        return Ok(());
    }
    let root = common::root()?;
    let manifest = root.join("Cargo.toml");
    let mut doc = fs::read_to_string(&manifest)?.parse::<toml_edit::DocumentMut>()?;
    ensure!(
        doc["workspace"]["package"]["version"].as_str() == Some(current.as_str()),
        "Workspace version missing"
    );
    doc["workspace"]["package"]["version"] = toml_edit::value(next.as_str());
    let lock_path = root.join("Cargo.lock");
    let mut lock = fs::read_to_string(&lock_path)?.parse::<toml_edit::DocumentMut>()?;
    let packages = lock["package"]
        .as_array_of_tables_mut()
        .context("Missing Cargo.lock packages")?;
    let mut count = 0;
    for package in packages.iter_mut() {
        if package.get("source").is_none()
            && package
                .get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.starts_with("paseo-"))
        {
            ensure!(
                package["version"].as_str() == Some(current.as_str()),
                "Local package version disagrees with workspace"
            );
            package["version"] = toml_edit::value(next.as_str());
            count += 1;
        }
    }
    ensure!(count > 0, "No native workspace packages in Cargo.lock");
    let build = build_number()?;
    // Resolve F-Droid content before writing any version file, so a missing
    // changelog cannot leave a version bump half-applied.
    let fdroid = fdroid_output(&root, &next, "en-US", build)?;
    fs::write(manifest, doc.to_string())?;
    fs::write(lock_path, lock.to_string())?;
    fs::write(
        root.join(common::VERSION_PROPERTIES),
        format!("versionCode={build}\n"),
    )?;
    if let Some((path, contents)) = fdroid {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, contents)?;
    }
    println!("{current} -> {next}; metadata updated without committing or tagging");
    Ok(())
}
/// `PASEO_BUILD_NUMBER`/`ANDROID_VERSION_CODE`, else the recorded versionCode.
fn build_number() -> Result<u64> {
    let number = common::build_number("android")?;
    ensure!(
        number > 13000 && number <= 2_100_000_000,
        "Android build number must exceed legacy floor 13000 and fit Android versionCode"
    );
    Ok(number)
}
fn plain(text: &str) -> String {
    let links = Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap();
    let text = links.replace_all(text, "$1");
    let attribution = Regex::new(r"\s*\([^()]*(?:#\d+|by @)[^()]*\)\s*$").unwrap();
    let text = attribution.replace(&text, "");
    let text = text.replace(['`', '*'], "");
    let emphasis = Regex::new(r"_([^_]+)_").unwrap();
    emphasis
        .replace_all(&text, "$1")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn fit(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.into();
    }
    if budget <= 3 {
        return String::new();
    }
    format!(
        "{}...",
        text.chars().take(budget - 3).collect::<String>().trim_end()
    )
}
fn fdroid_text(body: &str) -> String {
    const FOOTER: &str = "Full notes: https://github.com/emotionbug/Passession/releases";
    let budget = 500 - FOOTER.chars().count() - 3;
    let mut notes = Vec::new();
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    let mut heading = String::new();
    let mut bullets = Vec::new();
    let mut quote = Vec::new();
    let flush = |quote: &mut Vec<String>, notes: &mut Vec<String>| {
        if !quote.is_empty() {
            let mut paragraphs = Vec::new();
            let mut paragraph = Vec::new();
            for line in quote.drain(..) {
                if line.trim().is_empty() {
                    if !paragraph.is_empty() {
                        paragraphs.push(plain(&paragraph.join(" ")));
                        paragraph.clear();
                    }
                } else {
                    paragraph.push(line);
                }
            }
            if !paragraph.is_empty() {
                paragraphs.push(plain(&paragraph.join(" ")));
            }
            let note = paragraphs.join("\n");
            if !note.is_empty() {
                notes.push(note);
            }
        }
    };
    for line in body.lines() {
        let line = line.trim();
        if let Some(text) = line.strip_prefix('>') {
            quote.push(text.trim_start().into());
            continue;
        }
        flush(&mut quote, &mut notes);
        if let Some(title) = line.strip_prefix("### ") {
            if !bullets.is_empty() {
                sections.push((std::mem::take(&mut heading), std::mem::take(&mut bullets)));
            }
            heading = plain(title);
        } else if let Some(bullet) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            let text = plain(bullet);
            if !text.is_empty() {
                bullets.push(text);
            }
        } else {
            let text = plain(line);
            if !text.is_empty() {
                notes.push(text);
            }
        }
    }
    flush(&mut quote, &mut notes);
    if !bullets.is_empty() {
        sections.push((heading, bullets));
    }
    let mut note_text = String::new();
    for note in notes {
        let separator = if note_text.is_empty() { 0 } else { 2 };
        let remaining = budget.saturating_sub(note_text.chars().count() + separator);
        let fitted = fit(&note, remaining);
        if fitted.is_empty() {
            break;
        }
        if separator > 0 {
            note_text.push_str("\n\n");
        }
        note_text.push_str(&fitted);
    }
    let section_budget =
        budget.saturating_sub(note_text.chars().count() + if note_text.is_empty() { 0 } else { 2 });
    let mut candidates = Vec::new();
    for (heading, bullets) in &sections {
        if !candidates.is_empty() {
            candidates.push(String::new());
        }
        if !heading.is_empty() {
            candidates.push(heading.clone());
        }
        for bullet in bullets {
            candidates.push(format!("- {bullet}"));
        }
    }
    let mut kept = Vec::new();
    let mut length = 0;
    for line in candidates {
        let added = line.chars().count() + usize::from(!kept.is_empty());
        if length + added > section_budget {
            break;
        }
        length += added;
        kept.push(line);
    }
    while kept.last().is_some_and(|s| !s.starts_with("- ")) {
        kept.pop();
    }
    if kept.is_empty() && note_text.is_empty() {
        if let Some(bullet) = sections.first().and_then(|s| s.1.first()) {
            let text = fit(&format!("- {bullet}"), section_budget);
            if !text.is_empty() {
                kept.push(text);
            }
        }
    }
    let content = [note_text, kept.join("\n")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if content.is_empty() {
        format!("{FOOTER}\n")
    } else {
        format!("{content}\n\n{FOOTER}\n")
    }
}
fn fdroid_output(
    root: &Path,
    v: &str,
    locale: &str,
    build: u64,
) -> Result<Option<(PathBuf, String)>> {
    if !version(v)?.pre.is_empty() {
        return Ok(None);
    }
    ensure!(
        !locale.is_empty()
            && locale
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid locale"
    );
    let entries = entries(&fs::read_to_string(root.join("CHANGELOG.md"))?)?;
    let entry = entries
        .iter()
        .find(|e| e.version == v)
        .context("Add the release changelog before updating F-Droid metadata")?;
    let text = fdroid_text(&entry.body);
    ensure!(
        text.chars().count() <= 500,
        "F-Droid notes exceed 500 characters"
    );
    Ok(Some((
        root.join(format!(
            "fastlane/metadata/android/{locale}/changelogs/{build}.txt"
        )),
        text,
    )))
}
pub(super) fn fdroid(args: &[String]) -> Result<()> {
    let opts = options(args, &["--version", "--locale"], &["--check"])?;
    let v = opts.get("--version").cloned().unwrap_or(common::version()?);
    if let Some((path, content)) = fdroid_output(
        &common::root()?,
        &v,
        opts.get("--locale").map(String::as_str).unwrap_or("en-US"),
        build_number()?,
    )? {
        if opts.contains_key("--check") {
            ensure!(
                fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()),
                "Missing or stale F-Droid metadata: {}",
                path.display()
            );
        } else {
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, content)?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_transitions() {
        for (mode, expected) in [
            ("patch", "1.2.4"),
            ("minor", "1.3.0"),
            ("major", "2.0.0"),
            ("beta-minor", "1.3.0-beta.1"),
        ] {
            assert_eq!(next_version("1.2.3", mode).unwrap(), expected);
        }
        assert_eq!(
            next_version("1.2.3-beta.1", "beta-next").unwrap(),
            "1.2.3-beta.2"
        );
        assert_eq!(next_version("1.2.3-beta.2", "promote").unwrap(), "1.2.3");
        assert!(next_version("1.2.3-beta.1", "patch").is_err());
        assert!(next_version("1.2.3", "promote").is_err());
    }
    #[test]
    fn changelog_keeps_release_boundaries() {
        let parsed=entries("# Changes\n## [1.2.3] - 2026-01-01\n\n### Fixes\n- fixed\n\n## 1.2.2 - 2025-12-31\nold\n").unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].body, "### Fixes\n- fixed");
    }
    #[test]
    fn fdroid_prioritizes_warnings_and_bounds_unicode() {
        let text = fdroid_text(&format!(
            "### Fixes\n- {}\n\n> **Important**\n>\n> Reinstall manually.\n",
            "修正".repeat(600)
        ));
        assert!(text.starts_with("Important\nReinstall manually."));
        assert!(text.chars().count() <= 500);
        assert!(text.ends_with("Full notes: https://github.com/emotionbug/Passession/releases\n"));
        assert!(!text.contains("Fixes"));
    }
    #[test]
    fn fdroid_salvages_long_first_bullet() {
        let text = fdroid_text(&format!("### Fixes\n- {}", "a".repeat(800)));
        assert!(text.starts_with("- a"));
        assert!(text.contains("..."));
        assert_eq!(text.chars().count(), 500);
    }
    #[test]
    fn store_markup_is_removed() {
        assert_eq!(
            plain("**Fixed** [thing](https://example.com) (`#12` by @user)"),
            "Fixed thing"
        );
    }
}
