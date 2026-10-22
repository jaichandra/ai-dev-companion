//! The real credential store plus the stateful wrapper around it (port of
//! `core/credentials.ts`): the 60-second read cache, the warn-once latch and
//! the undecryptable-store fallbacks (all in `token_cache`), built once and
//! exposed as plain methods. Everything in one process shares one instance,
//! which is also what makes the read cache safe to key by name alone.

use crate::credential_store::CredentialStore;
use crate::token_cache::{StoreError, TokenCache};
use std::path::PathBuf;
use std::sync::Mutex;

/// Where an API token for a site (or the MCP token) comes from.
pub trait TokenLookup: Send + Sync {
    fn get_token(&self, name: &str) -> Option<String>;
}

pub struct Credentials {
    cache: Mutex<TokenCache<CredentialStore>>,
}

impl Credentials {
    /// `file_path` is `credentials.enc`; `key_path` is `<stateDir>/credentials.key`.
    pub fn open(file_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Self {
        Self { cache: Mutex::new(TokenCache::new(CredentialStore::new(file_path, key_path))) }
    }

    /// `None` when nothing is saved or the store can't be decrypted (warned once).
    pub fn get_token(&self, name: &str) -> Option<String> {
        self.cache.lock().unwrap().get_token(name).unwrap_or(None)
    }

    /// `Some(value)` saves it, `None` clears it. Clearing against an undecryptable
    /// store is a no-op, not an error.
    pub fn set_token(&self, name: &str, value: Option<&str>) -> Result<(), StoreError> {
        self.cache.lock().unwrap().set_token(name, value)
    }

    /// Every saved credential name, or empty on an undecryptable store: what
    /// `public_settings` needs to compute `apiTokenSet` without ever risking a 500.
    pub fn list_token_names(&self) -> Vec<String> {
        self.cache.lock().unwrap().list_names().unwrap_or_default()
    }
}

impl TokenLookup for Credentials {
    fn get_token(&self, name: &str) -> Option<String> {
        Credentials::get_token(self, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &tempfile::TempDir) -> Credentials {
        Credentials::open(dir.path().join("credentials.enc"), dir.path().join("credentials.key"))
    }

    #[test]
    fn a_saved_token_reads_back_lists_by_name_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        assert_eq!(c.get_token("jira.apiToken"), None);
        c.set_token("jira.apiToken", Some("jira-tok")).unwrap();
        assert_eq!(c.get_token("jira.apiToken").as_deref(), Some("jira-tok"));
        assert_eq!(c.list_token_names(), ["jira.apiToken"]);
        c.set_token("jira.apiToken", None).unwrap();
        assert_eq!(c.get_token("jira.apiToken"), None, "a clear takes effect on the very next read");
        assert!(c.list_token_names().is_empty());
        assert!(c.set_token("not.allowed", Some("x")).is_err());
    }

    #[test]
    fn a_second_handle_on_the_same_files_sees_what_the_first_saved_and_the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        open(&dir).set_token("mcp.token", Some("m")).unwrap();
        assert_eq!(open(&dir).get_token("mcp.token").as_deref(), Some("m"));
        assert_eq!(std::fs::metadata(dir.path().join("credentials.enc")).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn an_undecryptable_store_reads_as_nothing_saved_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        open(&dir).set_token("jira.apiToken", Some("t")).unwrap();
        std::fs::write(dir.path().join("credentials.key"), [7u8; 32]).unwrap();
        let c = open(&dir);
        assert_eq!(c.get_token("jira.apiToken"), None);
        assert!(c.list_token_names().is_empty());
        assert!(c.set_token("jira.apiToken", None).is_ok(), "clearing is a no-op, not a throw");
    }
}
