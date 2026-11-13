//! Passwords in the OS credential store: Windows Credential Manager, macOS
//! Keychain, or the Secret Service on Linux. The app never writes passwords to
//! its own files.

const SERVICE: &str = "gmail-labels-proto";

/// Saves the password for `email`, replacing any saved one.
pub fn save_password(email: &str, password: &str) -> Result<(), String> {
    keyring::Entry::new(SERVICE, email)
        .and_then(|entry| entry.set_password(password))
        .map_err(|e| format!("Could not save the password: {e}"))
}

/// The saved password for `email`, if there is one.
pub fn load_password(email: &str) -> Option<String> {
    keyring::Entry::new(SERVICE, email)
        .and_then(|entry| entry.get_password())
        .ok()
}

/// Deletes the saved password for `email`. Nothing saved counts as success.
pub fn forget_password(email: &str) -> Result<(), String> {
    match keyring::Entry::new(SERVICE, email).and_then(|entry| entry.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("Could not remove the saved password: {e}")),
    }
}
