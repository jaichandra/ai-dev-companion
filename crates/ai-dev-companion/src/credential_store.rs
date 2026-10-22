//! An encrypted-at-rest map of the API tokens the service holds for the
//! browser extension: Jira, Jenkins, Bitbucket, the MCP token, the Slack token
//! and a distribution's own ([`allowed_names`]); nothing else may be stored here
//! (port of `core/credential-store.js`).
//!
//! The file (`credentials.enc`) holds one AES-256-GCM ciphertext of the whole
//! `{name: value}` map, never individual values, so there's nothing to leak by
//! reading its bytes without the key. The key lives in a separate file; this
//! module only knows it by path.
//!
//! The on-disk format is exactly the Node build's, so an existing install's
//! saved tokens still decrypt: a JSON envelope
//! `{"v":1,"alg":"aes-256-gcm","iv":b64,"tag":b64,"data":b64}` (a 12-byte IV, a
//! 16-byte tag kept apart from the ciphertext) and a raw 32-byte key file.

use crate::environment;
use crate::token_cache::{CredentialBackend, StoreError};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

const ALG: &str = "aes-256-gcm";
const IV_LENGTH: usize = 12;
const TAG_LENGTH: usize = 16;
const KEY_LENGTH: usize = 32;

/// Shown whenever the ciphertext can't be turned back into the credential map:
/// wrong or missing key, a tampered file, or corruption. There's no way to tell
/// those apart from the outside, and no partial recovery: the only safe
/// recommendation is to re-enter the tokens.
pub const UNDECRYPTABLE_MESSAGE: &str = "Saved credentials can't be decrypted \u{2014} re-enter your tokens in \u{2728} \u{2192} \u{2699} Settings.";

/// The only credential names this store will read or write: the profile's site
/// tokens, then the fixed ones. Anything else is a programming error in the
/// caller, not a user-facing case.
pub fn allowed_names() -> Vec<String> {
    let mut names = environment::current().site_credential_names();
    names.extend(["mcp.token", "slack.token"].map(String::from));
    names.extend(environment::current().extra_credential_names());
    names
}

#[derive(Debug)]
pub enum CredentialError {
    /// `code: "unknown-name"`.
    UnknownName(String),
    /// `code: "undecryptable"`.
    Undecryptable,
    Io(io::Error),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::UnknownName(m) => write!(f, "{m}"),
            CredentialError::Undecryptable => write!(f, "{UNDECRYPTABLE_MESSAGE}"),
            CredentialError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CredentialError {}

impl From<io::Error> for CredentialError {
    fn from(e: io::Error) -> Self {
        CredentialError::Io(e)
    }
}

impl From<CredentialError> for StoreError {
    fn from(e: CredentialError) -> Self {
        match e {
            CredentialError::Undecryptable => StoreError::Undecryptable(UNDECRYPTABLE_MESSAGE.to_string()),
            other => StoreError::Other(other.to_string()),
        }
    }
}

fn assert_allowed_name(name: &str) -> Result<(), CredentialError> {
    let allowed = allowed_names();
    if allowed.iter().any(|n| n == name) {
        return Ok(());
    }
    Err(CredentialError::UnknownName(format!("\"{name}\" isn't a known credential name. Allowed: {}.", allowed.join(", "))))
}

/// Reads `key_path`, or `None` if it doesn't exist yet. Any other read failure
/// (permissions, a directory where a file is expected) is not swallowed.
fn load_key_if_exists(key_path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(key_path) {
        Ok(k) => Ok(Some(k)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

fn write_new_private_file(path: &Path, contents: &[u8], exclusive: bool) -> io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true);
    if exclusive {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(contents)
}

/// Loads the key, creating a fresh 32-byte one on first use. The directory is
/// created 0700 if missing and the key file itself 0600, exclusively, so two
/// processes racing to create it can't stomp on each other: the loser just
/// re-reads what the winner wrote.
fn load_or_create_key(key_path: &Path) -> io::Result<Vec<u8>> {
    if let Some(existing) = load_key_if_exists(key_path)? {
        return Ok(existing);
    }
    if let Some(dir) = key_path.parent() {
        create_private_dir(dir)?;
    }
    let mut key = vec![0u8; KEY_LENGTH];
    getrandom::fill(&mut key).map_err(|e| io::Error::other(e.to_string()))?;
    match write_new_private_file(key_path, &key, true) {
        Ok(()) => Ok(key),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => fs::read(key_path),
        Err(e) => Err(e),
    }
}

/// Reads and parses `file_path`, or `None` if it doesn't exist yet: a missing
/// file is the normal "nothing saved" case, not an error. A file that exists
/// but isn't valid JSON is corruption, same as any other undecryptable file.
fn read_envelope(file_path: &Path) -> Result<Option<Value>, CredentialError> {
    let raw = match fs::read(file_path) {
        Ok(r) => String::from_utf8_lossy(&r).into_owned(),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    serde_json::from_str(&raw).map(Some).map_err(|_| CredentialError::Undecryptable)
}

/// Decrypts an envelope back into the `{name: value}` map. Any failure along
/// the way (a malformed envelope, a wrong key, a tampered iv/tag/data) is
/// folded into one error: there's no safe way to distinguish them from outside,
/// and no partial map is ever returned.
fn decrypt_envelope(envelope: &Value, key: &[u8]) -> Result<Map<String, Value>, CredentialError> {
    let bad = || CredentialError::Undecryptable;
    let field = |k: &str| envelope.get(k).and_then(Value::as_str);
    if envelope.get("v").and_then(Value::as_f64) != Some(1.0) || field("alg") != Some(ALG) {
        return Err(bad());
    }
    let (Some(iv), Some(tag), Some(data)) = (field("iv"), field("tag"), field("data")) else { return Err(bad()) };
    let decode = |s: &str| STANDARD.decode(s).map_err(|_| bad());
    let (iv, tag, mut data) = (decode(iv)?, decode(tag)?, decode(data)?);
    if iv.len() != IV_LENGTH || tag.len() != TAG_LENGTH || key.len() != KEY_LENGTH {
        return Err(bad());
    }
    data.extend_from_slice(&tag);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| bad())?;
    let plain = cipher.decrypt(Nonce::from_slice(&iv), Payload { msg: &data, aad: &[] }).map_err(|_| bad())?;
    match serde_json::from_slice::<Value>(&plain) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(bad()),
    }
}

/// The full `{name: value}` map as currently saved, or empty if nothing is
/// saved yet. An error if the file exists but can't be turned back into a map.
/// `get`, `list` and `remove` only ever read, so they all fail straight from
/// here; see [`load_map_or_empty`] for the one place (`set`) that recovers.
fn load_map(file_path: &Path, key_path: &Path) -> Result<Map<String, Value>, CredentialError> {
    let Some(envelope) = read_envelope(file_path)? else { return Ok(Map::new()) };
    let Some(key) = load_key_if_exists(key_path)? else { return Err(CredentialError::Undecryptable) };
    decrypt_envelope(&envelope, &key)
}

/// [`load_map`], but an undecryptable file is treated as an empty map. Used
/// only by `set`: the old values behind an undecryptable file are gone no
/// matter what `set` does (no key can recover them), and the error message tells
/// the user to re-enter their tokens in Settings, which would be a lie if that
/// very re-entry also refused. So `set` on an undecryptable file quietly starts
/// fresh and atomically replaces it. Any other error still propagates.
fn load_map_or_empty(file_path: &Path, key_path: &Path) -> Result<Map<String, Value>, CredentialError> {
    match load_map(file_path, key_path) {
        Err(CredentialError::Undecryptable) => Ok(Map::new()),
        other => other,
    }
}

/// Encrypts `map` under a fresh random IV and writes it atomically (temp file in
/// the same directory, then rename) so a crash or a concurrent reader never sees
/// a half-written file. Always 0600: these are secrets.
fn write_map(map: &Map<String, Value>, file_path: &Path, key: &[u8]) -> Result<(), CredentialError> {
    let mut iv = [0u8; IV_LENGTH];
    getrandom::fill(&mut iv).map_err(|e| io::Error::other(e.to_string()))?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| io::Error::other(e.to_string()))?;
    let plain = serde_json::to_vec(map).map_err(|e| io::Error::other(e.to_string()))?;
    let mut sealed = cipher.encrypt(Nonce::from_slice(&iv), Payload { msg: &plain, aad: &[] }).map_err(|e| io::Error::other(e.to_string()))?;
    let tag = sealed.split_off(sealed.len() - TAG_LENGTH);
    let envelope = json!({ "v": 1, "alg": ALG, "iv": STANDARD.encode(iv), "tag": STANDARD.encode(tag), "data": STANDARD.encode(sealed) });
    let dir = file_path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let base = file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let tmp = dir.join(format!(".{base}.{}.{millis}.tmp", std::process::id()));
    let result = write_new_private_file(&tmp, envelope.to_string().as_bytes(), false).and_then(|()| fs::rename(&tmp, file_path));
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// `get`/`set`/`remove`/`list` over an AES-256-GCM-encrypted `{name: value}` map,
/// restricted to [`allowed_names`].
#[derive(Debug, Clone)]
pub struct CredentialStore {
    file_path: PathBuf,
    key_path: PathBuf,
}

impl CredentialStore {
    pub fn new(file_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Self {
        CredentialStore { file_path: file_path.into(), key_path: key_path.into() }
    }

    pub fn get_value(&self, name: &str) -> Result<Option<Value>, CredentialError> {
        assert_allowed_name(name)?;
        Ok(load_map(&self.file_path, &self.key_path)?.get(name).cloned())
    }

    pub fn get_value_text(&self, name: &str) -> Result<Option<String>, CredentialError> {
        Ok(self.get_value(name)?.and_then(|v| v.as_str().map(str::to_string)))
    }

    pub fn set_value(&self, name: &str, value: &str) -> Result<(), CredentialError> {
        assert_allowed_name(name)?;
        let mut map = load_map_or_empty(&self.file_path, &self.key_path)?;
        map.insert(name.to_string(), Value::String(value.to_string()));
        let key = load_or_create_key(&self.key_path)?;
        write_map(&map, &self.file_path, &key)
    }

    pub fn remove_value(&self, name: &str) -> Result<(), CredentialError> {
        assert_allowed_name(name)?;
        let mut map = load_map(&self.file_path, &self.key_path)?;
        if map.shift_remove(name).is_none() {
            return Ok(());
        }
        let key = load_or_create_key(&self.key_path)?;
        write_map(&map, &self.file_path, &key)
    }

    pub fn list_names(&self) -> Result<Vec<String>, CredentialError> {
        Ok(load_map(&self.file_path, &self.key_path)?.keys().cloned().collect())
    }
}

impl CredentialBackend for CredentialStore {
    fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
        Ok(self.get_value_text(name)?)
    }

    fn set(&self, name: &str, value: &str) -> Result<(), StoreError> {
        Ok(self.set_value(name, value)?)
    }

    fn remove(&self, name: &str) -> Result<(), StoreError> {
        Ok(self.remove_value(name)?)
    }

    fn list(&self) -> Result<Vec<String>, StoreError> {
        Ok(self.list_names()?)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _dir: tempfile::TempDir,
        file: PathBuf,
        key: PathBuf,
        store: CredentialStore,
    }

    fn make() -> Fixture {
        let dir = tempfile::Builder::new().prefix("credential-store-test-").tempdir().unwrap();
        let file = dir.path().join("credentials.enc");
        let key = dir.path().join("keys").join("credentials.key");
        let store = CredentialStore::new(&file, &key);
        Fixture { _dir: dir, file, key, store }
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn envelope(f: &Fixture) -> Value {
        serde_json::from_str(&fs::read_to_string(&f.file).unwrap()).unwrap()
    }

    fn undecryptable(r: Result<impl std::fmt::Debug, CredentialError>) {
        match r {
            Err(CredentialError::Undecryptable) => {}
            other => panic!("expected undecryptable, got {other:?}"),
        }
    }

    #[test]
    fn get_and_list_on_a_missing_file_are_empty_and_create_no_key() {
        let f = make();
        assert_eq!(f.store.get_value_text("jira.apiToken").unwrap(), None);
        assert!(f.store.list_names().unwrap().is_empty());
        assert!(!f.key.exists());
    }

    #[test]
    fn set_then_get_round_trips_across_store_instances_and_keeps_names_independent() {
        let f = make();
        f.store.set_value("jira.apiToken", "secret-123").unwrap();
        assert_eq!(f.store.get_value_text("jira.apiToken").unwrap().as_deref(), Some("secret-123"));
        let second = CredentialStore::new(&f.file, &f.key);
        second.set_value("slack.token", "xoxb-abc").unwrap();
        assert_eq!(f.store.get_value_text("slack.token").unwrap().as_deref(), Some("xoxb-abc"));
        f.store.set_value("jenkins.apiToken", "b").unwrap();
        let mut names = f.store.list_names().unwrap();
        names.sort();
        assert_eq!(names, ["jenkins.apiToken", "jira.apiToken", "slack.token"]);
        assert_eq!(f.store.get_value_text("jira.apiToken").unwrap().as_deref(), Some("secret-123"));
        assert!(f.store.list_names().unwrap().iter().all(|n| n != "secret-123"), "list returns names only, never values");
    }

    #[test]
    fn every_write_uses_a_fresh_iv_so_the_ciphertext_differs_for_the_same_value() {
        let f = make();
        f.store.set_value("mcp.token", "same-value").unwrap();
        let first = envelope(&f);
        f.store.set_value("mcp.token", "same-value").unwrap();
        let second = envelope(&f);
        assert_ne!(first["iv"], second["iv"]);
        assert_ne!(first["data"], second["data"]);
        assert_eq!((second["v"].clone(), second["alg"].clone()), (json!(1), json!("aes-256-gcm")));
        let keys: Vec<&str> = second.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["v", "alg", "iv", "tag", "data"], "the envelope's field order is the Node build's");
        assert_eq!(STANDARD.decode(second["iv"].as_str().unwrap()).unwrap().len(), 12);
        assert_eq!(STANDARD.decode(second["tag"].as_str().unwrap()).unwrap().len(), 16);
    }

    #[test]
    fn remove_deletes_a_name_and_is_a_harmless_no_op_for_a_missing_file_or_an_unset_name() {
        let f = make();
        f.store.remove_value("jira.apiToken").unwrap();
        assert!(!f.file.exists());
        f.store.set_value("jira.apiToken", "a").unwrap();
        f.store.set_value("jenkins.apiToken", "b").unwrap();
        f.store.remove_value("github.apiToken").unwrap();
        f.store.remove_value("jira.apiToken").unwrap();
        assert_eq!(f.store.get_value_text("jira.apiToken").unwrap(), None);
        assert_eq!(f.store.get_value_text("jenkins.apiToken").unwrap().as_deref(), Some("b"));
        assert_eq!(f.store.list_names().unwrap(), ["jenkins.apiToken"]);
    }

    #[test]
    fn an_unknown_name_is_refused_for_get_set_and_remove_and_the_allowed_names_are_the_profiles_plus_the_fixed_ones() {
        let f = make();
        for r in [f.store.get_value("not.a.thing").map(|_| ()), f.store.set_value("not.a.thing", "x"), f.store.remove_value("not.a.thing")] {
            match r {
                Err(CredentialError::UnknownName(m)) => assert!(m.starts_with("\"not.a.thing\" isn't a known credential name. Allowed: ")),
                other => panic!("{other:?}"),
            }
        }
        let mut want: Vec<String> = environment::current().sites.iter().map(|s| format!("{}.apiToken", s.id)).collect();
        want.extend(["mcp.token", "slack.token"].map(String::from));
        assert_eq!(allowed_names(), want);
    }

    fn tamper(f: &Fixture, field: &str) {
        let mut e = envelope(f);
        let mut bytes = STANDARD.decode(e[field].as_str().unwrap()).unwrap();
        bytes[0] ^= 0xff;
        e[field] = json!(STANDARD.encode(bytes));
        fs::write(&f.file, e.to_string()).unwrap();
    }

    #[test]
    fn a_tampered_data_tag_or_iv_field_is_undecryptable_with_the_standard_message() {
        for field in ["data", "tag", "iv"] {
            let f = make();
            f.store.set_value("jira.apiToken", "secret").unwrap();
            tamper(&f, field);
            undecryptable(f.store.get_value("jira.apiToken"));
            assert_eq!(CredentialError::Undecryptable.to_string(), UNDECRYPTABLE_MESSAGE);
        }
    }

    #[test]
    fn a_wrong_key_a_missing_key_or_a_wrong_length_key_is_undecryptable() {
        let f = make();
        f.store.set_value("jira.apiToken", "secret").unwrap();
        fs::write(&f.key, [7u8; 32]).unwrap();
        undecryptable(f.store.get_value("jira.apiToken"));
        fs::write(&f.key, [7u8; 31]).unwrap();
        undecryptable(f.store.get_value("jira.apiToken"));
        fs::remove_file(&f.key).unwrap();
        undecryptable(f.store.get_value("jira.apiToken"));
    }

    #[test]
    fn corrupt_or_malformed_files_are_undecryptable_too() {
        let f = make();
        f.store.set_value("jira.apiToken", "secret").unwrap();
        for text in ["{not json", "[]", "null", "{}", &json!({ "v": 2, "alg": "aes-256-gcm", "iv": "", "tag": "", "data": "" }).to_string(), &json!({ "v": 1, "alg": "aes-128-gcm", "iv": "", "tag": "", "data": "" }).to_string()] {
            fs::write(&f.file, text).unwrap();
            undecryptable(f.store.list_names());
        }
        let e = {
            f.store.set_value("jira.apiToken", "secret").unwrap();
            envelope(&f)
        };
        let mut no_iv = e.clone();
        no_iv.as_object_mut().unwrap().remove("iv");
        fs::write(&f.file, no_iv.to_string()).unwrap();
        undecryptable(f.store.list_names());
        let mut bad_b64 = e;
        bad_b64["data"] = json!("!!!not base64!!!");
        fs::write(&f.file, bad_b64.to_string()).unwrap();
        undecryptable(f.store.list_names());
    }

    #[test]
    fn set_recovers_from_a_wrong_or_missing_key_by_starting_fresh() {
        let f = make();
        f.store.set_value("jira.apiToken", "old-secret").unwrap();
        fs::write(&f.key, [9u8; 32]).unwrap();
        f.store.set_value("jenkins.apiToken", "new-secret").unwrap();
        assert_eq!(f.store.get_value_text("jenkins.apiToken").unwrap().as_deref(), Some("new-secret"));
        assert_eq!(f.store.get_value_text("jira.apiToken").unwrap(), None);
        assert_eq!(f.store.list_names().unwrap(), ["jenkins.apiToken"]);

        let g = make();
        g.store.set_value("jira.apiToken", "old-secret").unwrap();
        fs::remove_file(&g.key).unwrap();
        g.store.set_value("jenkins.apiToken", "new-secret").unwrap();
        assert_eq!(fs::read(&g.key).unwrap().len(), 32);
        assert_eq!(g.store.get_value_text("jenkins.apiToken").unwrap().as_deref(), Some("new-secret"));
        assert_eq!(g.store.get_value_text("jira.apiToken").unwrap(), None);
    }

    #[test]
    fn remove_and_list_on_an_undecryptable_file_still_fail_never_returning_partial_data() {
        let f = make();
        f.store.set_value("jira.apiToken", "secret").unwrap();
        f.store.set_value("jenkins.apiToken", "other").unwrap();
        fs::write(&f.key, [9u8; 32]).unwrap();
        undecryptable(f.store.remove_value("jira.apiToken"));
        undecryptable(f.store.list_names());
        undecryptable(f.store.get_value("jenkins.apiToken"));
    }

    #[test]
    fn the_files_are_created_0600_and_the_keys_directory_0700_and_the_key_is_made_on_first_set_and_reused() {
        let f = make();
        assert!(!f.key.exists());
        f.store.set_value("jira.apiToken", "secret").unwrap();
        assert_eq!((mode(&f.file), mode(&f.key), mode(f.key.parent().unwrap())), (0o600, 0o600, 0o700));
        let first = fs::read(&f.key).unwrap();
        assert_eq!(first.len(), 32);
        f.store.set_value("jenkins.apiToken", "b").unwrap();
        assert_eq!(fs::read(&f.key).unwrap(), first, "the key is reused, not regenerated");
        let leftovers: Vec<_> = fs::read_dir(f.file.parent().unwrap()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
        assert!(leftovers.is_empty(), "no temp files are left behind");
    }

    #[test]
    fn it_plugs_into_the_token_cache_as_its_backend() {
        use crate::token_cache::TokenCache;
        let f = make();
        let mut cache = TokenCache::new(f.store.clone());
        cache.set_token("jira.apiToken", Some("via-cache")).unwrap();
        assert_eq!(cache.get_token("jira.apiToken").unwrap().as_deref(), Some("via-cache"));
        assert_eq!(cache.list_names().unwrap(), ["jira.apiToken"]);
        cache.set_token("jira.apiToken", None).unwrap();
        assert_eq!(cache.get_token("jira.apiToken").unwrap(), None);
        assert!(matches!(cache.get_token("bogus"), Err(StoreError::Other(m)) if m.contains("isn't a known credential name")));
        fs::write(&f.file, "{broken").unwrap();
        let mut fresh = TokenCache::with(f.store.clone(), || 0, |_| {});
        assert_eq!(fresh.get_token("jira.apiToken").unwrap(), None, "undecryptable reads as nothing saved");
    }

    // ---- compatibility with the file the Node build wrote (a fixture) ----

    #[test]
    fn a_file_the_node_build_wrote_decrypts_here() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/credentials");
        let (file, key) = (dir.path().join("credentials.enc"), dir.path().join("credentials.key"));
        fs::copy(fixtures.join("credentials.enc"), &file).unwrap();
        fs::copy(fixtures.join("credentials.key"), &key).unwrap();
        let store = CredentialStore::new(&file, &key);
        assert_eq!(store.get_value_text("jira.apiToken").unwrap().as_deref(), Some("tok-jira \u{2728} \u{e9}"));
        assert_eq!(store.get_value_text("mcp.token").unwrap().as_deref(), Some("mcp-tok"));
        assert_eq!(store.list_names().unwrap(), ["jira.apiToken", "mcp.token"], "insertion order survives, and the removed name is gone");
        // And it can be updated in place.
        store.set_value("slack.token", "xoxb").unwrap();
        assert_eq!(store.get_value_text("jira.apiToken").unwrap().as_deref(), Some("tok-jira \u{2728} \u{e9}"));
    }
}
