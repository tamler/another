//! Gate tests that can force-stop children or signal process groups.

use std::ffi::OsStr;

fn is_hosted_runner(actions: Option<&OsStr>, environment: Option<&OsStr>) -> bool {
    actions == Some(OsStr::new("true")) && environment == Some(OsStr::new("github-hosted"))
}

pub fn require_hosted_runner() {
    let actions = std::env::var_os("GITHUB_ACTIONS");
    let environment = std::env::var_os("RUNNER_ENVIRONMENT");
    assert!(
        is_hosted_runner(actions.as_deref(), environment.as_deref()),
        "this test may force-stop a child or signal a process group; run it only on a GitHub-hosted Actions runner"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_ci_guard_requires_both_exact_markers() {
        assert!(is_hosted_runner(
            Some(OsStr::new("true")),
            Some(OsStr::new("github-hosted"))
        ));
        assert!(!is_hosted_runner(None, None));
        assert!(!is_hosted_runner(Some(OsStr::new("true")), None));
        assert!(!is_hosted_runner(
            Some(OsStr::new("true")),
            Some(OsStr::new("self-hosted"))
        ));
        assert!(!is_hosted_runner(
            Some(OsStr::new("false")),
            Some(OsStr::new("github-hosted"))
        ));
    }
}
