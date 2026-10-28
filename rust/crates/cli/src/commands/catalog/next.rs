//! `pay catalog next` — print the first failing check on a PAY.md as one brief, or `green`.

use std::fmt;
use std::path::{Path, PathBuf};

use pay_core::skills::build::parse_frontmatter;
use pay_core::skills::openapi::effective_endpoints_relative_to;
use pay_types::registry::ProviderFrontmatter;
use serde_yml::Value;

const FORMAT_BOUNDS: [(&str, usize, usize); 3] = [
    ("summary", 24, 63),
    ("description", 64, 255),
    ("use_case", 32, 255),
];

#[derive(clap::Args)]
pub struct NextCommand {
    /// Provider `PAY.md` to check.
    pub path: PathBuf,

    /// Earlier copy of the listing; every endpoint it declares must remain.
    #[arg(long)]
    pub before: Option<PathBuf>,
}

impl NextCommand {
    pub fn run(self) -> pay_core::Result<()> {
        match next_brief(&self.path, self.before.as_deref())? {
            Some(brief) => print!("{brief}"),
            None => println!("green"),
        }
        Ok(())
    }
}

struct Brief {
    check: &'static str,
    rule: String,
    current: String,
    replacement: String,
}

impl fmt::Display for Brief {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "check: {}", self.check)?;
        writeln!(f, "rule: {}", self.rule)?;
        writeln!(f, "current: {}", self.current)?;
        writeln!(f, "replacement: {}", self.replacement)
    }
}

fn next_brief(path: &Path, before: Option<&Path>) -> pay_core::Result<Option<Brief>> {
    let yaml = read_frontmatter(path)?;
    let value: Value = serde_yml::from_str(&yaml)
        .map_err(|e| pay_core::Error::Config(format!("{}: {e}", path.display())))?;
    if let Some(brief) = format_brief(&value) {
        return Ok(Some(brief));
    }
    let Some(before) = before else {
        return Ok(None);
    };
    let held = endpoint_pairs(path, &yaml)?;
    let dropped = endpoint_pairs(before, &read_frontmatter(before)?)?
        .into_iter()
        .find(|pair| !held.contains(pair));
    Ok(dropped.map(|(method, path)| {
        let pair = format!("{method} {path}");
        Brief {
            check: "declared_less",
            rule: format!("--before declares {pair}"),
            replacement: format!("restore {pair}"),
            current: pair,
        }
    }))
}

fn format_brief(yaml: &Value) -> Option<Brief> {
    FORMAT_BOUNDS.iter().find_map(|&(field, low, high)| {
        let text = yaml.get(field).and_then(Value::as_str).unwrap_or("");
        let n = text.chars().count();
        (!(low..=high).contains(&n)).then(|| Brief {
            check: "format",
            rule: format!("{field} is {n} characters"),
            current: text.to_string(),
            replacement: format!("a {field} of {low}-{high} characters"),
        })
    })
}

fn read_frontmatter(path: &Path) -> pay_core::Result<String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| pay_core::Error::Config(format!("read {}: {e}", path.display())))?;
    Ok(parse_frontmatter(&text)?.0)
}

fn endpoint_pairs(path: &Path, yaml: &str) -> pay_core::Result<Vec<(String, String)>> {
    let spec: ProviderFrontmatter = serde_yml::from_str(yaml)
        .map_err(|e| pay_core::Error::Config(format!("{}: {e}", path.display())))?;
    Ok(effective_endpoints_relative_to(&spec, path.parent())?
        .into_iter()
        .map(|r| (r.spec.method.to_ascii_uppercase(), r.spec.path))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str = "Paid data feed for one call";
    const PAID: &str = "[{method: GET, path: /paid, description: Fetch the paid resource}]";

    fn listing(
        summary: &str,
        description: &str,
        use_case: Option<&str>,
        endpoints: &str,
    ) -> String {
        let use_case = use_case.map_or(String::new(), |u| format!("use_case: {u}\n"));
        format!(
            "---\nname: paid\nsummary: {summary}\ndescription: {description}\n{use_case}\
             category: data\nservice_url: https://paid.example.com\nendpoints: {endpoints}\n---\n"
        )
    }

    fn valid(summary: &str, endpoints: &str) -> String {
        listing(summary, &"d".repeat(64), Some(&"u".repeat(32)), endpoints)
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn brief(path: &Path, before: Option<&Path>) -> String {
        next_brief(path, before).unwrap().unwrap().to_string()
    }

    fn is_green(path: &Path, before: Option<&Path>) -> bool {
        next_brief(path, before).unwrap().is_none()
    }

    #[test]
    fn format_fail_names_summary() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid("Pays per call", PAID));
        assert_eq!(
            brief(&path, None),
            "check: format\nrule: summary is 13 characters\ncurrent: Pays per call\n\
             replacement: a summary of 24-63 characters\n"
        );
    }

    #[test]
    fn format_checks_description_then_use_case() {
        let dir = tempfile::tempdir().unwrap();
        let short = write(
            dir.path(),
            "short.md",
            &listing(SUMMARY, "Too short", None, PAID),
        );
        assert!(
            brief(&short, None).starts_with("check: format\nrule: description is 9 characters\n")
        );

        let absent = write(
            dir.path(),
            "absent.md",
            &listing(SUMMARY, &"d".repeat(64), None, PAID),
        );
        assert_eq!(
            brief(&absent, None),
            "check: format\nrule: use_case is 0 characters\ncurrent: \n\
             replacement: a use_case of 32-255 characters\n"
        );
    }

    #[test]
    fn format_counts_code_points() {
        let dir = tempfile::tempdir().unwrap();
        let wide = write(dir.path(), "wide.md", &valid(&"é".repeat(24), PAID));
        assert!(is_green(&wide, None));

        let long = write(dir.path(), "long.md", &valid(&"é".repeat(64), PAID));
        assert!(brief(&long, None).starts_with("check: format\nrule: summary is 64 characters\n"));

        let under = write(dir.path(), "under.md", &valid(&"a".repeat(23), PAID));
        assert!(brief(&under, None).starts_with("check: format\nrule: summary is 23 characters\n"));

        let at = write(dir.path(), "at.md", &valid(&"a".repeat(24), PAID));
        assert!(is_green(&at, None));
    }

    #[test]
    fn declared_less_names_dropped_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let before = write(dir.path(), "no-usdc.md", &valid(SUMMARY, PAID));
        let path = write(dir.path(), "PAY.md", &valid(SUMMARY, "[]"));
        assert_eq!(
            brief(&path, Some(&before)),
            "check: declared_less\nrule: --before declares GET /paid\ncurrent: GET /paid\n\
             replacement: restore GET /paid\n"
        );

        let faulty = write(dir.path(), "faulty.md", &valid("Pays per call", "[]"));
        assert!(brief(&faulty, Some(&before)).starts_with("check: format\n"));
    }

    #[test]
    fn valid_listing_is_green() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid(SUMMARY, PAID));
        assert!(is_green(&path, None));

        let before = write(dir.path(), "before.md", &valid(SUMMARY, PAID));
        assert!(is_green(&path, Some(&before)));
    }

    #[test]
    fn unreadable_listing_is_err() {
        let dir = tempfile::tempdir().unwrap();
        assert!(next_brief(&dir.path().join("missing.md"), None).is_err());

        let open = write(dir.path(), "open.md", "---\nname: paid\nsummary: Paid\n");
        assert!(next_brief(&open, None).is_err());
    }
}
