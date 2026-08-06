//! Per-request project attribution for the proxy.
//!
//! `headroom wrap` launches agents with an `X-Headroom-Project` header
//! naming the project directory. The proxy captures that header once per
//! request into a task-local variable, so the outcome funnel can attribute
//! savings to a project.

use std::collections::HashMap;

pub const PROJECT_HEADER: &str = "x-headroom-project";
pub const PROJECT_PATH_PREFIX: &str = "/p/";

thread_local! {
    static CURRENT_PROJECT: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
}

/// Sanitize a project name: printable ASCII only, length-capped.
pub fn sanitize_project_name(value: Option<&str>) -> Option<String> {
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    let sanitized: String = raw
        .chars()
        .filter(|c| *c >= '!' && *c <= '~')
        .take(200)
        .collect();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// Extract a sanitized project name from request headers, if present.
pub fn classify_project(headers: &HashMap<String, String>) -> Option<String> {
    let value = headers
        .get(PROJECT_HEADER)
        .or_else(|| headers.get("X-Headroom-Project"));
    sanitize_project_name(value.map(|s| s.as_str()))
}

/// Bind the active request's project for downstream outcome recording.
pub fn set_current_project(project: Option<&str>) {
    CURRENT_PROJECT.with(|p| {
        *p.borrow_mut() = sanitize_project_name(project);
    });
}

/// Project bound to the current request context, or `None`.
pub fn get_current_project() -> Option<String> {
    CURRENT_PROJECT.with(|p| p.borrow().clone())
}

/// Split `/p/<name>/rest` into `(name, /rest)`.
pub fn split_project_path(path: &str) -> (Option<String>, String) {
    if !path.starts_with(PROJECT_PATH_PREFIX) {
        return (None, path.to_string());
    }
    let remainder = &path[PROJECT_PATH_PREFIX.len()..];
    let (segment, rest) = match remainder.split_once('/') {
        Some((s, r)) => (s, Some(r)),
        None => (remainder, None),
    };
    let project = sanitize_project_name(Some(segment));
    if project.is_none() {
        return (None, path.to_string());
    }
    let stripped = match rest {
        Some(r) => format!("/{r}"),
        None => "/".to_string(),
    };
    (project, stripped)
}

/// Insert `/p/<name>` ahead of the path of a local proxy base URL.
pub fn with_project_prefix(base_url: &str, project: Option<&str>) -> String {
    let name = match sanitize_project_name(project) {
        Some(n) => n,
        None => return base_url.to_string(),
    };
    // Simple URL manipulation
    if let Some(pos) = base_url.find("://") {
        let scheme_and_rest = &base_url[..pos + 3];
        let after_scheme = &base_url[pos + 3..];
        let path_start = after_scheme
            .find('/')
            .map(|i| i + pos + 3)
            .unwrap_or(base_url.len());
        let path = &base_url[path_start..];
        let prefix = format!("{PROJECT_PATH_PREFIX}{name}");
        let new_path = format!("{prefix}{path}");
        if new_path.ends_with('/') {
            format!("{scheme_and_rest}{}", &new_path[..new_path.len() - 1])
        } else {
            format!("{scheme_and_rest}{new_path}")
        }
    } else {
        let prefix = format!("{PROJECT_PATH_PREFIX}{name}");
        format!("{prefix}{base_url}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_printable_ascii() {
        assert_eq!(
            sanitize_project_name(Some("my-project")),
            Some("my-project".to_string())
        );
    }

    #[test]
    fn sanitize_strips_non_printable() {
        assert_eq!(
            sanitize_project_name(Some("my\x00project")),
            Some("myproject".to_string())
        );
    }

    #[test]
    fn sanitize_empty_is_none() {
        assert!(sanitize_project_name(Some("")).is_none());
        assert!(sanitize_project_name(None).is_none());
    }

    #[test]
    fn classify_project_from_header() {
        let mut headers = HashMap::new();
        headers.insert("x-headroom-project".to_string(), "my-proj".to_string());
        assert_eq!(classify_project(&headers), Some("my-proj".to_string()));
    }

    #[test]
    fn classify_project_case_insensitive() {
        let mut headers = HashMap::new();
        headers.insert("X-Headroom-Project".to_string(), "proj".to_string());
        assert_eq!(classify_project(&headers), Some("proj".to_string()));
    }

    #[test]
    fn split_project_path_basic() {
        let (proj, rest) = split_project_path("/p/my-project/v1/messages");
        assert_eq!(proj.as_deref(), Some("my-project"));
        assert_eq!(rest, "/v1/messages");
    }

    #[test]
    fn split_project_path_no_prefix() {
        let (proj, rest) = split_project_path("/v1/messages");
        assert!(proj.is_none());
        assert_eq!(rest, "/v1/messages");
    }

    #[test]
    fn split_project_path_trailing_slash() {
        let (proj, rest) = split_project_path("/p/my-project/");
        assert_eq!(proj.as_deref(), Some("my-project"));
        assert_eq!(rest, "/");
    }

    #[test]
    fn with_project_prefix_basic() {
        let url = with_project_prefix("http://localhost:8787/v1/messages", Some("my-proj"));
        assert!(url.contains("/p/my-proj/"));
    }

    #[test]
    fn with_project_prefix_no_project() {
        let url = with_project_prefix("http://localhost:8787/v1/messages", None);
        assert_eq!(url, "http://localhost:8787/v1/messages");
    }
}
