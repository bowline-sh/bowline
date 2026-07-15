use std::{collections::BTreeMap, error::Error, fmt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaseFoldPathCollision {
    pub(crate) existing: String,
    pub(crate) incoming: String,
}

impl fmt::Display for CaseFoldPathCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "case-only path collision between `{}` and `{}`",
            self.existing, self.incoming
        )
    }
}

impl Error for CaseFoldPathCollision {}

pub(crate) fn case_fold_path_component(component: &str) -> String {
    component.to_lowercase()
}

pub(crate) fn validate_case_folded_prefixes(
    path: &str,
    folded_paths: &mut BTreeMap<String, String>,
) -> Result<(), CaseFoldPathCollision> {
    let mut prefix = String::new();
    for component in path.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        let folded = case_fold_path_component(&prefix);
        if let Some(existing) = folded_paths.insert(folded, prefix.clone())
            && existing != prefix
        {
            return Err(CaseFoldPathCollision {
                existing,
                incoming: prefix,
            });
        }
    }
    Ok(())
}

pub(crate) fn is_secret_bearing_path(path: &str) -> bool {
    path.split('/').any(crate::policy::is_project_env_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_secret_bearing_paths() {
        assert!(is_secret_bearing_path(".env"));
        assert!(is_secret_bearing_path("apps/web/.env.local"));
        assert!(is_secret_bearing_path("service.env"));
        assert!(!is_secret_bearing_path("src/env_reader.rs"));
    }
}
