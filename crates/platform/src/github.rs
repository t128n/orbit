use std::str::FromStr;

/// One GitHub installation's canonical web origin and derived API endpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubHost {
    origin: url::Url,
    kind: GitHubHostKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHubHostKind {
    Dotcom,
    Ghec,
    Ghes,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "must be an HTTPS origin without credentials, path, query or fragment; ghe.com requires exactly one subdomain"
)]
pub struct InvalidGitHubHost;

impl Default for GitHubHost {
    fn default() -> Self {
        "https://github.com".parse().expect("default GitHub origin")
    }
}

impl FromStr for GitHubHost {
    type Err = InvalidGitHubHost;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut origin = url::Url::parse(value).map_err(|_| InvalidGitHubHost)?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || value.contains('@')
            || value.contains('\\')
        {
            return Err(InvalidGitHubHost);
        }
        // Reject path syntax even if URL normalization would collapse it to '/'.
        let authority_and_path = value.split_once("://").ok_or(InvalidGitHubHost)?.1;
        if authority_and_path
            .find('/')
            .is_some_and(|i| &authority_and_path[i..] != "/")
        {
            return Err(InvalidGitHubHost);
        }
        let normalized_host = origin
            .host_str()
            .ok_or(InvalidGitHubHost)?
            .trim_end_matches('.')
            .to_owned();
        if normalized_host.is_empty() {
            return Err(InvalidGitHubHost);
        }
        origin
            .set_host(Some(&normalized_host))
            .map_err(|_| InvalidGitHubHost)?;
        let host = origin.host_str().ok_or(InvalidGitHubHost)?;
        let kind = if host == "github.com" {
            GitHubHostKind::Dotcom
        } else if host == "ghe.com" {
            return Err(InvalidGitHubHost);
        } else if let Some(prefix) = host.strip_suffix(".ghe.com") {
            if prefix.is_empty() || prefix.contains('.') {
                return Err(InvalidGitHubHost);
            }
            GitHubHostKind::Ghec
        } else {
            GitHubHostKind::Ghes
        };
        Ok(Self { origin, kind })
    }
}

impl GitHubHost {
    pub fn kind(&self) -> GitHubHostKind {
        self.kind
    }
    /// Canonical origin without a trailing slash.
    pub fn web_origin(&self) -> &str {
        self.origin.as_str().trim_end_matches('/')
    }
    /// REST API base URL, with a trailing slash for joining relative API paths.
    pub fn rest_base(&self) -> String {
        match self.kind {
            GitHubHostKind::Dotcom => "https://api.github.com/".to_owned(),
            GitHubHostKind::Ghec => format!(
                "https://api.{}{}/",
                self.origin.host_str().unwrap(),
                self.port_suffix()
            ),
            GitHubHostKind::Ghes => format!("{}/api/v3/", self.web_origin()),
        }
    }
    pub fn graphql_url(&self) -> String {
        match self.kind {
            GitHubHostKind::Ghes => format!("{}/api/graphql", self.web_origin()),
            _ => format!("{}graphql", self.rest_base()),
        }
    }
    fn port_suffix(&self) -> String {
        self.origin
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default()
    }
    fn web_path(&self, segments: &[&str]) -> String {
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .expect("HTTPS origin is a base URL")
            .clear()
            .extend(segments.iter().copied());
        url.into()
    }
    pub fn oauth_authorize_url(&self) -> String {
        self.web_path(&["login", "oauth", "authorize"])
    }
    pub fn oauth_token_url(&self) -> String {
        self.web_path(&["login", "oauth", "access_token"])
    }
    pub fn manifest_url(&self, organization: Option<&str>) -> String {
        match organization {
            Some(org) => self.web_path(&["organizations", org, "settings", "apps", "new"]),
            None => self.web_path(&["settings", "apps", "new"]),
        }
    }
    pub fn installation_url(&self, slug: &str) -> String {
        self.web_path(&["apps", slug, "installations", "new"])
    }
    pub fn issue_url(&self, owner: &str, repo: &str, number: i64) -> String {
        self.web_path(&[owner, repo, "issues", &number.to_string()])
    }
    pub fn pull_url(&self, owner: &str, repo: &str, number: i64) -> String {
        self.web_path(&[owner, repo, "pull", &number.to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hosts_and_endpoints() {
        for (input, origin, kind, rest, graphql) in [
            (
                "https://GITHUB.com:443/",
                "https://github.com",
                GitHubHostKind::Dotcom,
                "https://api.github.com/",
                "https://api.github.com/graphql",
            ),
            (
                "https://Acme.GHE.com/",
                "https://acme.ghe.com",
                GitHubHostKind::Ghec,
                "https://api.acme.ghe.com/",
                "https://api.acme.ghe.com/graphql",
            ),
            (
                "https://acme.ghe.com:8443",
                "https://acme.ghe.com:8443",
                GitHubHostKind::Ghec,
                "https://api.acme.ghe.com:8443/",
                "https://api.acme.ghe.com:8443/graphql",
            ),
            (
                "https://git.example:8443",
                "https://git.example:8443",
                GitHubHostKind::Ghes,
                "https://git.example:8443/api/v3/",
                "https://git.example:8443/api/graphql",
            ),
            (
                "https://git.example.",
                "https://git.example",
                GitHubHostKind::Ghes,
                "https://git.example/api/v3/",
                "https://git.example/api/graphql",
            ),
        ] {
            let host: GitHubHost = input.parse().unwrap();
            assert_eq!(host.web_origin(), origin);
            assert_eq!(host.kind(), kind);
            assert_eq!(host.rest_base(), rest);
            assert_eq!(host.graphql_url(), graphql);
            assert_eq!(
                host.oauth_authorize_url(),
                format!("{origin}/login/oauth/authorize")
            );
            assert_eq!(
                host.oauth_token_url(),
                format!("{origin}/login/oauth/access_token")
            );
            assert_eq!(
                host.manifest_url(None),
                format!("{origin}/settings/apps/new")
            );
            assert_eq!(
                host.manifest_url(Some("org")),
                format!("{origin}/organizations/org/settings/apps/new")
            );
            assert_eq!(
                host.installation_url("app"),
                format!("{origin}/apps/app/installations/new")
            );
            assert_eq!(
                host.issue_url("owner", "repo", 42),
                format!("{origin}/owner/repo/issues/42")
            );
            assert_eq!(
                host.pull_url("owner", "repo", 42),
                format!("{origin}/owner/repo/pull/42")
            );
            assert_eq!(
                url::Url::parse(&host.rest_base())
                    .unwrap()
                    .join("user")
                    .unwrap()
                    .as_str(),
                format!("{rest}user")
            );
        }
        assert_eq!(GitHubHost::default().web_origin(), "https://github.com");
        let host = GitHubHost::default();
        assert_eq!(
            host.issue_url("a/b", "c d", 7),
            "https://github.com/a%2Fb/c%20d/issues/7"
        );
        assert_eq!(
            host.manifest_url(Some("a/b")),
            "https://github.com/organizations/a%2Fb/settings/apps/new"
        );
    }

    #[test]
    fn rejects_invalid_origins() {
        for invalid in [
            "",
            "github.com",
            "//github.com",
            "ftp://github.com",
            "https://",
            "http://github.com",
            "https://u@github.com",
            "https://u:p@github.com",
            "https://@github.com",
            "https://github.com/x",
            "https://github.com//",
            "https://github.com/.",
            "https://github.com/%2e%2e",
            "https://github.com/?",
            "https://github.com/#",
            "https://ghe.com",
            "https://ghe.com.",
            "https://a.b.ghe.com",
            "https://a.b.ghe.com.",
            "https://github.com\\evil",
            "https://github.com/a/..",
        ] {
            assert!(invalid.parse::<GitHubHost>().is_err(), "{invalid}");
        }
    }
}
