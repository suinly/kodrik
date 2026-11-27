#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

impl Release {
    fn newer_than(self, current: &str) -> Option<String> {
        if self.draft || self.prerelease {
            return None;
        }
        let latest = stable_version(self.tag_name.strip_prefix('v')?)?;
        (latest > stable_version(current)?).then_some(self.tag_name)
    }
}

fn stable_version(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.split('.');
    let mut numbers = [0; 3];
    for number in &mut numbers {
        let part = parts.next()?;
        if (part.len() > 1 && part.starts_with('0')) || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *number = part.parse().ok()?;
    }
    parts.next().is_none().then_some(numbers)
}

pub(super) async fn check() -> Option<String> {
    reqwest::Client::builder()
        .user_agent("kodrik-tui")
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?
        .get("https://api.github.com/repos/suinly/kodrik/releases/latest")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<Release>()
        .await
        .ok()?
        .newer_than(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_newer_published_stable_releases_are_reported() {
        for (tag, draft, prerelease, expected) in [
            ("v0.10.0", false, false, Some("v0.10.0")),
            ("v1.0.0", false, false, Some("v1.0.0")),
            ("v0.9.9", false, false, Some("v0.9.9")),
            ("v0.9.8", false, false, None),
            ("v0.9.7", false, false, None),
            ("v1.0.0", true, false, None),
            ("v1.0.0", false, true, None),
            ("v1.0.0-rc.1", false, false, None),
            ("v1.0.0\n", false, false, None),
            ("v1.0.+0", false, false, None),
            ("v01.0.0", false, false, None),
        ] {
            let release = Release {
                tag_name: tag.into(),
                draft,
                prerelease,
            };
            assert_eq!(release.newer_than("0.9.8").as_deref(), expected, "{tag:?}");
        }
    }
}
