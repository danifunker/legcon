//! Passwords in the OS credential store (Credential Manager, Keychain, Secret Service), PLAN.md D28.
//! Local calls, not network work; the store may show its own unlock prompt.

use std::collections::HashMap;
use std::sync::Mutex;

/// The service name every legcon entry is filed under; the machine id is the account.
const SERVICE: &str = "legcon";

/// An in-memory store that replaces the OS one, for tests.
static MEMORY: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Names a JSON file that replaces the OS store, so UI flows never touch the real one.
pub const TEST_ENV: &str = "LEGCON_TEST_SECRETS";

fn test_file() -> Option<std::path::PathBuf> {
    std::env::var_os(TEST_ENV)
        .filter(|f| !f.is_empty())
        .map(Into::into)
}

fn read_file(f: &std::path::Path) -> HashMap<String, String> {
    std::fs::read_to_string(f)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_file(f: &std::path::Path, m: &HashMap<String, String>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(m).map_err(|e| e.to_string())?;
    std::fs::write(f, text).map_err(|e| format!("{}: {e}", f.display()))
}

/// Keep passwords in memory from now on instead of the OS store (tests only).
pub fn use_memory_store() {
    let mut m = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if m.is_none() {
        *m = Some(HashMap::new());
    }
}

fn entry(machine_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, machine_id).map_err(|e| store_error(&e))
}

fn store_error(e: &keyring::Error) -> String {
    format!("the {} could not be used: {e}", store_name())
}

/// What the store is called on this system, for messages and labels.
pub fn store_name() -> &'static str {
    if cfg!(windows) {
        "Windows Credential Manager"
    } else if cfg!(target_os = "macos") {
        "macOS Keychain"
    } else {
        "system keyring"
    }
}

/// The password kept for `machine_id`.
pub fn get(machine_id: &str) -> Result<String, String> {
    if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return m
            .get(machine_id)
            .cloned()
            .ok_or_else(|| format!("no password for {machine_id} in the {}", store_name()));
    }
    if let Some(f) = test_file() {
        return read_file(&f)
            .remove(machine_id)
            .ok_or_else(|| format!("no password for {machine_id} in the {}", store_name()));
    }
    entry(machine_id)?.get_password().map_err(|e| match e {
        keyring::Error::NoEntry => {
            format!("no password for {machine_id} in the {}", store_name())
        }
        e => store_error(&e),
    })
}

pub fn set(machine_id: &str, password: &str) -> Result<(), String> {
    if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.insert(machine_id.to_string(), password.to_string());
        return Ok(());
    }
    if let Some(f) = test_file() {
        let mut m = read_file(&f);
        m.insert(machine_id.to_string(), password.to_string());
        return write_file(&f, &m);
    }
    entry(machine_id)?
        .set_password(password)
        .map_err(|e| store_error(&e))
}

/// Forget the password; nothing to forget is not an error.
pub fn delete(machine_id: &str) -> Result<(), String> {
    if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(machine_id);
        return Ok(());
    }
    if let Some(f) = test_file() {
        let mut m = read_file(&f);
        m.remove(machine_id);
        return write_file(&f, &m);
    }
    match entry(machine_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(store_error(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_store_round_trips() {
        use_memory_store();
        set("test-secrets-a", "pw").unwrap();
        assert_eq!(get("test-secrets-a").unwrap(), "pw");
        delete("test-secrets-a").unwrap();
        assert!(get("test-secrets-a").is_err());
        delete("test-secrets-a").unwrap();
    }
}
