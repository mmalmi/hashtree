//! Git references (branches, tags, HEAD)
//!
//! Refs are named pointers to commits.

use super::object::ObjectId;
use super::{Error, Result};

/// A git reference
#[derive(Debug, Clone)]
pub enum Ref {
    /// Direct reference to an object
    Direct(ObjectId),
    /// Symbolic reference to another ref (e.g., HEAD -> refs/heads/main)
    Symbolic(String),
}

/// Validate a ref name according to git rules
pub fn validate_ref_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidRefName("empty ref name".into()));
    }

    if name.starts_with('/') || name.ends_with('/') {
        return Err(Error::InvalidRefName("cannot start or end with /".into()));
    }

    if name.contains("//") {
        return Err(Error::InvalidRefName("cannot contain //".into()));
    }

    if name.contains("..") {
        return Err(Error::InvalidRefName("cannot contain ..".into()));
    }

    for c in name.chars() {
        if c.is_control()
            || c == ' '
            || c == '~'
            || c == '^'
            || c == ':'
            || c == '?'
            || c == '*'
            || c == '['
        {
            return Err(Error::InvalidRefName(format!("invalid character: {:?}", c)));
        }
    }

    if name.ends_with(".lock") {
        return Err(Error::InvalidRefName("cannot end with .lock".into()));
    }

    if name.contains("@{") {
        return Err(Error::InvalidRefName("cannot contain @{".into()));
    }

    if name == "@" {
        return Err(Error::InvalidRefName("cannot be @".into()));
    }

    if name.ends_with('.') {
        return Err(Error::InvalidRefName("cannot end with .".into()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_ref_names() {
        assert!(validate_ref_name("refs/heads/main").is_ok());
        assert!(validate_ref_name("refs/heads/feature/test").is_ok());
        assert!(validate_ref_name("refs/tags/v1.0.0").is_ok());
        assert!(validate_ref_name("HEAD").is_ok());
    }

    #[test]
    fn test_invalid_ref_names() {
        assert!(validate_ref_name("").is_err());
        assert!(validate_ref_name("/refs/heads/main").is_err());
        assert!(validate_ref_name("refs/heads/main/").is_err());
        assert!(validate_ref_name("refs//heads").is_err());
        assert!(validate_ref_name("refs/heads/..").is_err());
        assert!(validate_ref_name("refs/heads/test.lock").is_err());
        assert!(validate_ref_name("refs/heads/te st").is_err());
    }
}
