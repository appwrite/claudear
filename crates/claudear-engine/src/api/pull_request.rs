use url::Url;

const GITHUB_HOST: &str = "github.com";
const MAX_OWNER_CHARS: usize = 39;
const MAX_NAME_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PullRequestLocation {
    pub owner: String,
    pub name: String,
    pub number: i64,
}

impl PullRequestLocation {
    pub(super) fn parse(value: &str) -> Option<Self> {
        let parsed = Url::parse(value).ok()?;
        let is_plain_https_github = parsed.scheme() == "https"
            && parsed.host_str() == Some(GITHUB_HOST)
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.port().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none();
        if !is_plain_https_github {
            return None;
        }
        let segments: Vec<&str> = parsed.path_segments()?.collect();
        let [owner, name, "pull", number] = segments.as_slice() else {
            return None;
        };
        if !is_owner(owner) || !is_name(name) {
            return None;
        }
        let number = parse_number(number)?;
        Some(Self {
            owner: (*owner).to_string(),
            name: (*name).to_string(),
            number,
        })
    }

    pub(super) fn repository(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub(super) fn url(&self) -> String {
        format!(
            "https://{GITHUB_HOST}/{}/{}/pull/{}",
            self.owner, self.name, self.number
        )
    }

    pub(super) fn belongs_to(&self, repository: &str) -> bool {
        self.repository().eq_ignore_ascii_case(repository)
    }
}

fn is_owner(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_OWNER_CHARS
        && !segment.starts_with('-')
        && !segment.ends_with('-')
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn is_name(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_NAME_CHARS
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn parse_number(segment: &str) -> Option<i64> {
    if segment.is_empty() || segment.starts_with('0') {
        return None;
    }
    if !segment.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    segment.parse().ok().filter(|number| *number > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_canonical_github_pull_request_urls() {
        let location = PullRequestLocation::parse("https://github.com/appwrite/appwrite/pull/42")
            .expect("a canonical pull request url");
        assert_eq!(location.owner, "appwrite");
        assert_eq!(location.name, "appwrite");
        assert_eq!(location.number, 42);
        assert_eq!(location.repository(), "appwrite/appwrite");
        assert_eq!(
            location.url(),
            "https://github.com/appwrite/appwrite/pull/42"
        );
        assert!(PullRequestLocation::parse("https://GitHub.com/a-b/c.d_e/pull/1").is_some());
    }

    #[test]
    fn rejects_urls_that_only_look_like_github_pull_requests() {
        for value in [
            "",
            "not a url",
            "http://github.com/appwrite/appwrite/pull/1",
            "https://evil.example/github.com/appwrite/appwrite/pull/1",
            "https://github.com.evil.example/appwrite/appwrite/pull/1",
            "https://evil.example/appwrite/appwrite/pull/1",
            "https://user@github.com/appwrite/appwrite/pull/1",
            "https://user:secret@github.com/appwrite/appwrite/pull/1",
            "https://github.com:8443/appwrite/appwrite/pull/1",
            "https://github.com/appwrite/appwrite/pull/1?tab=files",
            "https://github.com/appwrite/appwrite/pull/1#discussion",
            "https://github.com/appwrite/appwrite/pull/1/",
            "https://github.com/appwrite/appwrite/pull/1/files",
            "https://github.com/appwrite/appwrite/issues/1",
            "https://github.com/appwrite/pull/1",
            "https://github.com/appwrite/appwrite/pull/0",
            "https://github.com/appwrite/appwrite/pull/007",
            "https://github.com/appwrite/appwrite/pull/-1",
            "https://github.com/appwrite/appwrite/pull/99999999999999999999",
            "https://github.com/-appwrite/appwrite/pull/1",
            "https://github.com/app_write/appwrite/pull/1",
            "https://github.com/appwrite/../pull/1",
            "https://github.com/appwrite/app%2Fwrite/pull/1",
            "javascript:alert(1)//github.com/appwrite/appwrite/pull/1",
        ] {
            assert_eq!(PullRequestLocation::parse(value), None, "{value}");
        }
    }

    #[test]
    fn matches_its_repository_ignoring_case() {
        let location =
            PullRequestLocation::parse("https://github.com/Appwrite/Appwrite/pull/3").expect("url");
        assert!(location.belongs_to("appwrite/appwrite"));
        assert!(!location.belongs_to("attacker/repo"));
        assert!(!location.belongs_to("appwrite/appwrite-fork"));
    }
}
