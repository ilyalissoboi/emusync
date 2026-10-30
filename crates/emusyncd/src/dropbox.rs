//! Dropbox for the CLI (DESIGN §30.6): signing in, and the refresh token, which is kept in the
//! system's credential store (§24), never in the data directory's database.

use emusync_remote::dropbox::oauth::{self, SignInError};
use emusync_remote::{
    CredentialProvider, Credentials, DropboxConfig, DropboxRemote, Remote, RemoteError, StaticCredentials,
};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use zeroize::Zeroizing;

/// What `emusync remote add dropbox` means.
pub const DEFAULT_URI: &str = "dropbox:/saves";

/// Overrides the stored refresh token, for scripts and `watch --env-file`.
pub const TOKEN_ENV: &str = "EMUSYNC_DROPBOX_TOKEN";

/// The loopback redirect registered for the emusync app (DESIGN §30.4). Dropbox matches redirect
/// URIs exactly, so the port is fixed; if it is taken, sign-in falls back to pasting the code.
const REDIRECT_URI: &str = "http://127.0.0.1:53682/dropbox";

/// How long the browser has to come back.
const BROWSER_WAIT: Duration = Duration::from_secs(300);

/// The credential store's service name for the CLI's secrets.
const SERVICE: &str = "emusync";

/// `dropbox` alone stands for the default folder.
pub fn expand_uri(uri: &str) -> String {
    if uri == "dropbox" {
        DEFAULT_URI.to_string()
    } else {
        uri.to_string()
    }
}

pub fn is_dropbox(uri: &str) -> bool {
    uri.starts_with("dropbox:")
}

/// The secret's name: one per data directory, so two data directories can use two accounts.
fn secret_name(data_dir: &Path) -> String {
    let path = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let hash = blake3::hash(path.to_string_lossy().as_bytes()).to_hex();
    format!("dropbox-refresh-token-{}", &hash[..12])
}

/// The account signed in for this data directory: `<id>\t<email>`. Not secret.
fn account_file(data_dir: &Path) -> PathBuf {
    data_dir.join("dropbox-account")
}

/// The stored refresh token: `EMUSYNC_DROPBOX_TOKEN`, else the credential store's.
fn stored_token(data_dir: &Path) -> Result<Option<Zeroizing<String>>, RemoteError> {
    if let Some(t) = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty()) {
        return Ok(Some(Zeroizing::new(t)));
    }
    emusync_core::secrets::get(SERVICE, &secret_name(data_dir), Some(data_dir))
        .map_err(|e| RemoteError::Auth(format!("the credential store can't be read: {e}")))
}

pub fn is_signed_in(data_dir: &Path) -> bool {
    matches!(stored_token(data_dir), Ok(Some(_)))
}

/// The email address of the account signed in, for `emusync status`.
pub fn account_email(data_dir: &Path) -> Option<String> {
    let s = std::fs::read_to_string(account_file(data_dir)).ok()?;
    s.trim_end().split_once('\t').map(|(_, e)| e.to_string()).filter(|e| !e.is_empty())
}

/// The refresh token for a Dropbox remote, read when the remote first needs it.
pub struct TokenProvider {
    data_dir: PathBuf,
}

impl TokenProvider {
    pub fn new(data_dir: &Path) -> Self {
        Self { data_dir: data_dir.to_path_buf() }
    }
}

impl CredentialProvider for TokenProvider {
    fn credentials(&self) -> Result<Credentials, RemoteError> {
        match stored_token(&self.data_dir)? {
            Some(t) => Ok(Credentials::OAuthRefresh(t.to_string())),
            None => Err(RemoteError::Auth("not signed in to Dropbox (run `emusync remote login`)".into())),
        }
    }
}

/// Opens `url` in the default browser, without waiting for the browser: some launchers only
/// return when it exits. False if the launcher is missing or fails within 2 s.
fn open_browser(url: &str) -> bool {
    let child = if cfg!(windows) {
        // `start` would split the URL at its `&`s.
        std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    let Ok(mut child) = child else { return false };
    for _ in 0..20 {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return false,
        }
    }
    true
}

fn read_code() -> Result<String, Box<dyn std::error::Error>> {
    eprint!("Code: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Checks a remote reached with a new sign-in, before its token is stored.
pub type Verify<'a> = &'a dyn Fn(&dyn Remote) -> Result<(), Box<dyn std::error::Error>>;

/// Signs in to Dropbox for `uri` and stores the refresh token for this data directory. With
/// `no_browser`, or when the browser can't be used, the user pastes the code Dropbox shows.
/// `verify` sees the remote as the new sign-in reaches it, and can refuse it (another account's
/// repository): the token is then not stored, so nothing ever syncs with it.
pub fn login(uri: &str, data_dir: &Path, no_browser: bool, verify: Verify) -> Result<(), Box<dyn std::error::Error>> {
    let signed = sign_in(uri, no_browser)?;
    let creds = Credentials::OAuthRefresh(signed.refresh_token.clone());
    let remote = DropboxRemote::new(DropboxConfig::parse(uri)?, Box::new(StaticCredentials(creds)))?;
    verify(&remote)?;
    std::fs::create_dir_all(data_dir)?;
    emusync_core::secrets::set(SERVICE, &secret_name(data_dir), &signed.refresh_token, Some(data_dir))?;
    std::fs::write(account_file(data_dir), format!("{}\t{}\n", signed.account_id, signed.email))?;
    let who = if signed.email.is_empty() { signed.account_id.clone() } else { signed.email.clone() };
    println!("signed in to Dropbox as {who}");
    Ok(())
}

fn sign_in(uri: &str, no_browser: bool) -> Result<oauth::SignIn, Box<dyn std::error::Error>> {
    if !no_browser {
        let start = oauth::start(uri, REDIRECT_URI)?;
        // Listening before the browser opens, so a fast redirect can't find the port closed.
        match oauth::listen(&start.pending) {
            Ok(listener) if open_browser(&start.url) => {
                eprintln!("Sign in to Dropbox in the browser window that opened. Waiting…");
                let redirect = listener.wait(BROWSER_WAIT)?;
                return Ok(oauth::finish(&start.pending, &redirect)?);
            }
            Ok(_) => eprintln!("no browser could be opened; sign in with a code instead"),
            Err(SignInError::PortInUse(port)) => eprintln!("port {port} is in use; sign in with a code instead"),
            Err(e) => return Err(e.into()),
        }
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("(reading the code from standard input)");
    }
    let start = oauth::start(uri, "")?;
    eprintln!("Open this page, sign in to Dropbox, allow emusync, and paste the code it shows:\n\n  {}\n", start.url);
    let code = read_code()?;
    Ok(oauth::finish(&start.pending, &code)?)
}

/// Signs out: revokes the refresh token and deletes it from this device. `EMUSYNC_DROPBOX_TOKEN`
/// is left alone (it isn't ours to delete).
pub fn logout(uri: &str, data_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let name = secret_name(data_dir);
    match emusync_core::secrets::get(SERVICE, &name, Some(data_dir))? {
        Some(token) => {
            if let Err(e) = oauth::revoke(uri, &token) {
                eprintln!("warning: Dropbox couldn't be told ({e}); the token is deleted from this device anyway");
            }
            emusync_core::secrets::delete(SERVICE, &name, Some(data_dir))?;
            let _ = std::fs::remove_file(account_file(data_dir));
            println!("signed out of Dropbox on this device");
        }
        None => println!("not signed in to Dropbox on this device"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_and_names() {
        assert_eq!(expand_uri("dropbox"), "dropbox:/saves");
        assert_eq!(expand_uri("dropbox:/x"), "dropbox:/x");
        assert!(is_dropbox("dropbox:/x") && !is_dropbox("s3://b"));
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (na, nb) = (secret_name(a.path()), secret_name(b.path()));
        assert_ne!(na, nb);
        assert_eq!(na, secret_name(a.path()));
        assert!(na.len() <= 64 && na.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'));
    }

    #[test]
    fn account_email_from_the_file() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(account_email(d.path()), None);
        std::fs::write(account_file(d.path()), "dbid:x\tme@example.com\n").unwrap();
        assert_eq!(account_email(d.path()).as_deref(), Some("me@example.com"));
        std::fs::write(account_file(d.path()), "dbid:x\t\n").unwrap();
        assert_eq!(account_email(d.path()), None);
    }
}
