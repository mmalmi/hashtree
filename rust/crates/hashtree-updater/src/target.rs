use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateTarget {
    target: String,
    aliases: BTreeSet<String>,
}

impl UpdateTarget {
    pub fn new(target: impl Into<String>) -> Self {
        let target = normalize(&target.into());
        let aliases = aliases_for(&target);
        Self { target, aliases }
    }

    pub fn current() -> Self {
        Self::new(current_target())
    }

    pub fn as_str(&self) -> &str {
        &self.target
    }

    pub fn matches(&self, candidate: &str) -> bool {
        let candidate_aliases = aliases_for(&normalize(candidate));
        self.aliases
            .iter()
            .any(|alias| candidate_aliases.contains(alias))
    }
}

impl Default for UpdateTarget {
    fn default() -> Self {
        Self::current()
    }
}

fn normalize(target: &str) -> String {
    target.trim().to_ascii_lowercase()
}

fn aliases_for(target: &str) -> BTreeSet<String> {
    let mut aliases = BTreeSet::new();
    aliases.insert(target.to_string());

    let group: &[&str] = match target {
        "aarch64-apple-darwin"
        | "darwin-aarch64"
        | "darwin-arm64"
        | "macos-aarch64"
        | "macos-arm64" => &[
            "aarch64-apple-darwin",
            "darwin-aarch64",
            "darwin-arm64",
            "macos-aarch64",
            "macos-arm64",
        ],
        "x86_64-apple-darwin" | "darwin-x86_64" | "macos-x86_64" | "macos-x64" => &[
            "x86_64-apple-darwin",
            "darwin-x86_64",
            "macos-x86_64",
            "macos-x64",
        ],
        "x86_64-unknown-linux-gnu"
        | "x86_64-unknown-linux-musl"
        | "linux-x86_64"
        | "linux-x64" => &[
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "linux-x86_64",
            "linux-x64",
        ],
        "aarch64-unknown-linux-gnu"
        | "aarch64-unknown-linux-musl"
        | "linux-aarch64"
        | "linux-arm64" => &[
            "aarch64-unknown-linux-gnu",
            "aarch64-unknown-linux-musl",
            "linux-aarch64",
            "linux-arm64",
        ],
        "x86_64-pc-windows-msvc" | "windows-x86_64" | "windows-x64" => {
            &["x86_64-pc-windows-msvc", "windows-x86_64", "windows-x64"]
        }
        "aarch64-pc-windows-msvc" | "windows-aarch64" | "windows-arm64" => &[
            "aarch64-pc-windows-msvc",
            "windows-aarch64",
            "windows-arm64",
        ],
        _ => &[],
    };

    aliases.extend(group.iter().copied().map(String::from));
    aliases
}

pub(crate) fn current_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => "unknown",
    }
}
