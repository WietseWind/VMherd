//! API token secrets. Where a bookmark's secret lives ([`SecretSource`]):
//!
//! * the platform credential store via keyring-core: macOS Keychain, Windows Credential Manager,
//!   Linux Secret Service (GNOME Keyring, KWallet, KeePassXC); not every Linux system runs one;
//! * macOS: an existing Keychain item (read with the `security` tool);
//! * any platform: a command that prints the secret (`pass`, `secret-tool`, `op`, `bw`, ...);
//! * Linux fallback: a private file (mode 600) in the config directory, not encrypted.
//!
//! The Mac App Store build (feature `mas`) runs in the App Sandbox and starts no other programs:
//! it has only the Keychain store. The Keychain-item and command sources are compiled out; old
//! bookmarks that use them still load and get an explanation instead of a secret.

#[cfg(not(feature = "mas"))]
use std::io::Read as _;
use std::path::{Path, PathBuf};
#[cfg(not(feature = "mas"))]
use std::process::{Command, Stdio};
use std::sync::LazyLock;
#[cfg(not(feature = "mas"))]
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::config::{Bookmark, Config, SecretSource};

const SERVICE: &str = "VMherd";
/// Password managers may show an unlock prompt first.
#[cfg(not(feature = "mas"))]
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether this build can read a secret from outside VMherd (an existing Keychain item, a
/// command). The Mac App Store build cannot: the App Sandbox build starts no other programs.
pub const EXTERNAL_SOURCES: bool = !cfg!(feature = "mas");

/// Why a bookmark with a Keychain-item or command source gets no secret in the Mac App Store build.
#[cfg(feature = "mas")]
pub const NOT_IN_MAS: &str = "Reading the token secret from a Keychain item or a command is not available in the Mac \
                              App Store version; edit the cluster and save the token secret in the Keychain";

/// The name users know for the place where VMherd saves token secrets.
pub const STORE_NAME: &str = if cfg!(target_os = "macos") {
    "Keychain"
} else if cfg!(target_os = "windows") {
    "Windows Credential Manager"
} else {
    "Secret Service keyring"
};

static STORE: LazyLock<Result<(), String>> = LazyLock::new(|| {
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new();
    #[cfg(target_os = "windows")]
    let store = windows_native_keyring_store::Store::new();
    #[cfg(all(unix, not(target_os = "macos")))]
    let store = zbus_secret_service_keyring_store::Store::new();
    let store = store.map_err(|e| format!("The {STORE_NAME} is not available: {e}"))?;
    keyring_core::set_default_store(store);
    Ok(())
});

/// Whether the credential store can be used. On Linux this asks the Secret Service once (a
/// session bus with a keyring daemon is not a given); elsewhere the store is always there.
pub fn store_status() -> &'static Result<(), String> {
    static STATUS: LazyLock<Result<(), String>> = LazyLock::new(|| {
        STORE.clone()?;
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            return Ok(());
        }
        match keyring_core::Entry::new(SERVICE, "availability-check").and_then(|e| e.get_password()) {
            Ok(_) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("The {STORE_NAME} is not available: {e}")),
        }
    });
    &STATUS
}

fn entry(id: Uuid) -> Result<keyring_core::Entry, String> {
    STORE.clone()?;
    keyring_core::Entry::new(SERVICE, &id.to_string()).map_err(|e| e.to_string())
}

/// Read the secret of a bookmark (may block: Keychain prompt, password manager unlock).
pub fn get(bookmark: &Bookmark) -> Result<String, String> {
    match &bookmark.secret {
        SecretSource::Keyring => match entry(bookmark.id)?.get_password() {
            Ok(s) => Ok(s),
            Err(keyring_core::Error::NoEntry) => Err(format!(
                "No token secret saved in the {STORE_NAME} for this cluster. Edit the cluster and enter it."
            )),
            Err(e) => Err(format!("{STORE_NAME}: {e}")),
        },
        #[cfg(not(feature = "mas"))]
        SecretSource::MacKeychain { service, account } => mac_keychain(service, account.as_deref()),
        #[cfg(not(feature = "mas"))]
        SecretSource::Command { command } => run_command(command, COMMAND_TIMEOUT),
        #[cfg(feature = "mas")]
        SecretSource::MacKeychain { .. } | SecretSource::Command { .. } => Err(NOT_IN_MAS.into()),
        SecretSource::File => read_file(&file_path(bookmark.id)?),
    }
}

/// Save (replace) the secret of bookmark `id` where `source` says; sources that only read
/// (Keychain item, command) have nothing to save.
pub fn set(id: Uuid, source: &SecretSource, secret: &str) -> Result<(), String> {
    match source {
        SecretSource::Keyring => {
            store_status().clone()?;
            entry(id)?.set_password(secret).map_err(|e| format!("{STORE_NAME}: {e}"))
        }
        SecretSource::File => write_file(&file_path(id)?, secret),
        SecretSource::MacKeychain { .. } | SecretSource::Command { .. } => Ok(()),
    }
}

/// Forget whatever VMherd saved for bookmark `id` in `source` (nothing there is not an error).
pub fn delete(id: Uuid, source: &SecretSource) -> Result<(), String> {
    match source {
        SecretSource::Keyring => {
            if store_status().is_err() {
                return Ok(()); // nothing can have been saved there
            }
            match entry(id)?.delete_credential() {
                Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
                Err(e) => Err(format!("{STORE_NAME}: {e}")),
            }
        }
        SecretSource::File => match std::fs::remove_file(file_path(id)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Cannot remove the secret file: {e}")),
        },
        SecretSource::MacKeychain { .. } | SecretSource::Command { .. } => Ok(()),
    }
}

/// Whether VMherd holds a saved secret for `id` in `source` (for "leave empty to keep").
pub fn has_saved(id: Uuid, source: &SecretSource) -> bool {
    match source {
        SecretSource::Keyring => store_status().is_ok() && entry(id).is_ok_and(|e| e.get_password().is_ok()),
        SecretSource::File => file_path(id).is_ok_and(|p| p.is_file()),
        SecretSource::MacKeychain { .. } | SecretSource::Command { .. } => false,
    }
}

// ---------- private file ----------

fn file_path(id: Uuid) -> Result<PathBuf, String> {
    let config = Config::path().ok_or("No config directory on this system")?;
    let dir = config.parent().ok_or("No config directory on this system")?;
    Ok(dir.join("secrets").join(id.to_string()))
}

fn read_file(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => Ok(s.trim_end_matches(['\n', '\r']).to_owned()),
        Ok(_) => Err("The secret file is empty. Edit the cluster and enter the secret.".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("No token secret saved for this cluster. Edit the cluster and enter it.".into())
        }
        Err(e) => Err(format!("Cannot read the secret file: {e}")),
    }
}

fn write_file(path: &Path, secret: &str) -> Result<(), String> {
    let dir = path.parent().ok_or("Bad secret file path")?;
    create_private_dir(dir).map_err(|e| format!("Cannot create {}: {e}", crate::config::tilde(dir)))?;
    let tmp = path.with_extension("tmp");
    write_private(&tmp, secret.as_bytes()).map_err(|e| format!("Cannot write the secret file: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Cannot write the secret file: {e}"))
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

// ---------- command (not in the Mac App Store build) ----------

/// Run `command` through the shell; the first line it prints is the secret.
#[cfg(not(feature = "mas"))]
fn run_command(command: &str, timeout: Duration) -> Result<String, String> {
    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", command]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    };
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Cannot run the secret command: {e}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("The secret command did not finish within {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("Cannot run the secret command: {e}")),
        }
    };
    let (mut out, mut err) = (String::new(), String::new());
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    if !status.success() {
        let why = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(200).collect::<String>();
        let code = status.code().map_or_else(|| "a signal".to_owned(), |c| format!("exit code {c}"));
        return Err(if why.is_empty() {
            format!("The secret command failed ({code})")
        } else {
            format!("The secret command failed ({code}): {why}")
        });
    }
    match out.lines().next().map(|l| l.trim_end_matches('\r')) {
        Some(s) if !s.trim().is_empty() => Ok(s.to_owned()),
        _ => Err("The secret command printed nothing".into()),
    }
}

// ---------- macOS Keychain item (not in the Mac App Store build) ----------

#[cfg(all(target_os = "macos", not(feature = "mas")))]
fn mac_keychain(service: &str, account: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args(["find-generic-password", "-s", service]);
    if let Some(a) = account {
        cmd.args(["-a", a]);
    }
    let out = cmd.arg("-w").output().map_err(|e| format!("Cannot run the security tool: {e}"))?;
    if !out.status.success() {
        return Err(format!("Keychain item \"{service}\" not found or not readable"));
    }
    let secret = String::from_utf8(out.stdout).map_err(|_| "Keychain item is not UTF-8".to_owned())?;
    let secret = secret.trim_end_matches(['\n', '\r']).to_owned();
    if secret.is_empty() { Err(format!("Keychain item \"{service}\" is empty")) } else { Ok(secret) }
}

#[cfg(all(not(target_os = "macos"), not(feature = "mas")))]
fn mac_keychain(_service: &str, _account: Option<&str>) -> Result<String, String> {
    Err("Keychain items are only available on macOS".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(feature = "mas")))]
    #[test]
    fn command_source() {
        let t = Duration::from_secs(5);
        assert_eq!(run_command("printf 's3cret\\nmeta: x\\n'", t).unwrap(), "s3cret");
        assert_eq!(run_command("echo ' with spaces '", t).unwrap(), " with spaces ");
        let e = run_command("echo nope >&2; exit 3", t).unwrap_err();
        assert!(e.contains("exit code 3") && e.contains("nope"), "{e}");
        assert!(run_command("true", t).unwrap_err().contains("printed nothing"));
        assert!(run_command("sleep 5", Duration::from_millis(200)).unwrap_err().contains("did not finish"));
    }

    /// Touches the real credential store: saves, reads and deletes one entry. On Linux run it in a
    /// session with a Secret Service (tools/test-linux.sh does).
    #[test]
    #[ignore = "uses the real credential store"]
    fn credential_store_roundtrip() {
        if let Err(e) = store_status() {
            panic!("credential store not available: {e}");
        }
        let id = Uuid::new_v4();
        let b = Bookmark {
            id,
            name: "test".into(),
            url: "https://pve.example.com:8006".into(),
            token_id: "user@pve!vmherd".into(),
            secret: SecretSource::Keyring,
            pinned_sha256: None,
            grid: Vec::new(),
            nosync: Vec::new(),
        };
        set(id, &SecretSource::Keyring, "pw-123").unwrap();
        assert!(has_saved(id, &SecretSource::Keyring));
        assert_eq!(get(&b).unwrap(), "pw-123");
        delete(id, &SecretSource::Keyring).unwrap();
        assert!(get(&b).unwrap_err().contains("No token secret"));
    }

    /// Mac App Store build: old bookmarks with a Keychain-item or command source load, run nothing
    /// and explain what to do.
    #[cfg(feature = "mas")]
    #[test]
    fn mas_has_no_external_sources() {
        let mut b = Bookmark {
            id: Uuid::new_v4(),
            name: "old".into(),
            url: "https://pve.example.com:8006".into(),
            token_id: "user@pve!vmherd".into(),
            secret: SecretSource::Command { command: "echo s3cret".into() },
            pinned_sha256: None,
            grid: Vec::new(),
            nosync: Vec::new(),
        };
        assert_eq!(get(&b).unwrap_err(), NOT_IN_MAS);
        b.secret = SecretSource::MacKeychain { service: "vmherd-token".into(), account: None };
        assert_eq!(get(&b).unwrap_err(), NOT_IN_MAS);
    }

    /// Linux without a Secret Service: the store is reported unavailable, nothing panics, and the
    /// file source still works.
    #[test]
    #[ignore = "run on Linux without a session bus (tools/test-linux.sh does)"]
    fn without_secret_service() {
        let err = store_status().clone().unwrap_err();
        assert!(err.contains("not available"), "{err}");
        assert!(set(Uuid::new_v4(), &SecretSource::Keyring, "x").is_err());
        assert!(delete(Uuid::new_v4(), &SecretSource::Keyring).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn private_file_roundtrip() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("vmherd-secret-test-{}", std::process::id()));
        let path = dir.join("secrets").join("x");
        write_file(&path, "abc-123").unwrap();
        assert_eq!(read_file(&path).unwrap(), "abc-123");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        write_file(&path, "replaced").unwrap();
        assert_eq!(read_file(&path).unwrap(), "replaced");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(read_file(&path).unwrap_err().contains("No token secret"));
    }
}
