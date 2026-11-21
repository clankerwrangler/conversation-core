//! Provider sign-in, credential storage, and renewal.
//!
//! Credentials live in one file (`--auth <file>`, default
//! `~/.local/share/conversation-core/auth.json`), in a Pi-compatible `auth.json` format.
//! The file is readable only by its owner:
//!
//! ```json
//! {"openai-codex": {"type": "oauth", "access": "...", "refresh": "...", "expires": 0, "accountId": "..."},
//!  "grok-cli": {"type": "oauth", "access": "...", "refresh": "...", "expires": 0, "tokenEndpoint": "...", "baseUrl": "..."},
//!  "openrouter": {"type": "api_key", "key": "..."}}
//! ```
//!
//! Renewal. Providers rotate refresh tokens: renewing makes the old refresh token invalid.
//! Many sessions share one file, so a renewal holds `<file>.lock` (an flock, released by
//! the OS if the process dies) while it re-reads the file, renews only if the stored token
//! is still the old one, and writes the result. A session that waited finds the new token
//! and does not renew again. Modeled in `specification/CredentialRefresh.tla`.

use base64::Engine;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CODEX: &str = "openai-codex";
pub const GROK: &str = "grok-cli";
pub const OPENROUTER: &str = "openrouter";

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_AUTH: &str = "https://auth.openai.com";
const CODEX_REDIRECT: &str = "http://localhost:1455/auth/callback";
const CODEX_SCOPE: &str = "openid profile email offline_access";
const GROK_ISSUER: &str = "https://auth.x.ai";
const GROK_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const GROK_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const GROK_REDIRECT: &str = "http://127.0.0.1:56122/callback";
const GROK_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
const OPENROUTER_AUTH: &str = "https://openrouter.ai/auth";
const OPENROUTER_KEYS: &str = "https://openrouter.ai/api/v1/auth/keys";
const OPENROUTER_REDIRECT: &str = "http://localhost:3000/openrouter/callback";
/// Renew this long before expiry (the grok-cli add-on's skew).
const RENEW_EARLY_MS: u64 = 120_000;
const TIMEOUT: Duration = Duration::from_secs(30);

/// Sign-ins are data (they cannot be fetched again without signing in).
pub fn default_path() -> Result<PathBuf, &'static str> {
    crate::dirs::data()
        .map(|dir| dir.join("auth.json"))
        .ok_or("cannot determine credentials path: set XDG_DATA_HOME or HOME to an absolute path, or pass --auth")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Replace the file atomically, owner-only.
fn write(path: &Path, all: &Map<String, Value>) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|_| "auth_not_saved")?;
    let tmp = dir.join(format!(
        ".{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("auth"),
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|_| "auth_not_saved")?;
    use std::io::Write;
    file.write_all(Value::Object(all.clone()).to_string().as_bytes())
        .map_err(|_| "auth_not_saved")?;
    file.sync_all().map_err(|_| "auth_not_saved")?;
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp, path).map_err(|_| "auth_not_saved".to_string())
}

/// Run `f` holding `<file>.lock`.
fn locked<T>(path: &Path, f: impl FnOnce() -> T) -> Result<T, String> {
    use std::os::unix::io::AsRawFd;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|_| "auth_lock_failed")?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("json.lock"))
        .map_err(|_| "auth_lock_failed")?;
    // SAFETY: flock on a file descriptor this function owns; released when `lock` drops.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err("auth_lock_failed".into());
    }
    let out = f();
    drop(lock);
    Ok(out)
}

/// Save one provider's credentials.
pub fn save(path: &Path, provider: &str, credential: Value) -> Result<(), String> {
    locked(path, || {
        let mut all = read(path);
        all.insert(provider.to_string(), credential);
        write(path, &all)
    })?
}

pub fn remove(path: &Path, provider: &str) -> Result<bool, String> {
    locked(path, || {
        let mut all = read(path);
        let had = all.remove(provider).is_some();
        if had {
            write(path, &all)?;
        }
        Ok(had)
    })?
}

/// Which providers are signed in, and until when (never the secrets).
pub fn status(path: &Path) -> Value {
    let all = read(path);
    Value::Array(
        all.iter()
            .map(|(provider, c)| {
                json!({"provider": provider, "type": c.get("type"), "expires": c.get("expires"),
                    "accountId": c.get("accountId")})
            })
            .collect(),
    )
}

fn auth_config() -> ureq::config::Config {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
}

fn agent() -> ureq::Agent {
    auth_config().into()
}

/// POST and read JSON back: (status, body).
fn post(
    url: &str,
    form: Option<&[(&str, &str)]>,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    post_with(agent(), url, form, body).map_err(|_| "sign_in_unreachable".to_string())
}

fn post_with(
    agent: ureq::Agent,
    url: &str,
    form: Option<&[(&str, &str)]>,
    body: Option<&Value>,
) -> Result<(u16, Value), ureq::Error> {
    let call = agent.post(url).header("accept", "application/json");
    let mut response = match (form, body) {
        (Some(form), _) => call.send_form(form.iter().copied()),
        (None, Some(body)) => call
            .header("content-type", "application/json")
            .send(body.to_string().as_str()),
        (None, None) => call.send_empty(),
    }?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

fn error_code(body: &Value) -> String {
    let e = body.get("error");
    e.and_then(|e| e.get("code"))
        .or(e)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The ChatGPT account id in a Codex access token.
fn codex_account(access: &str) -> Option<String> {
    crate::provider::codex_account(access)
}

/// A token response as stored credentials for the provider.
fn credential_from(provider: &str, body: &Value, token_endpoint: &str) -> Result<Value, String> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("sign_in_no_access_token")?;
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_f64)
        .unwrap_or(3600.0);
    let mut c = json!({"type": "oauth", "access": access, "refresh": refresh,
        "expires": now_ms() + (expires_in * 1000.0) as u64});
    match provider {
        CODEX => {
            c["accountId"] = json!(codex_account(access).ok_or("sign_in_no_account_id")?);
        }
        GROK => {
            c["expires"] =
                json!((now_ms() + (expires_in * 1000.0) as u64).saturating_sub(RENEW_EARLY_MS));
            c["tokenEndpoint"] = json!(token_endpoint);
            c["baseUrl"] = json!(GROK_BASE);
        }
        _ => {}
    }
    Ok(c)
}

/// OpenAI's sign-in server (`CONVERSATION_CORE_CODEX_AUTH` replaces it, for tests).
fn codex_auth() -> String {
    std::env::var("CONVERSATION_CORE_CODEX_AUTH").unwrap_or_else(|_| CODEX_AUTH.to_string())
}

fn codex_token_url() -> String {
    format!("{}/oauth/token", codex_auth())
}

/// xAI's sign-in endpoints, from its OIDC discovery (only x.ai hosts are accepted).
fn grok_endpoints() -> Result<(String, String, Option<String>), String> {
    let mut response = agent()
        .get(&format!("{GROK_ISSUER}/.well-known/openid-configuration"))
        .header("accept", "application/json")
        .call()
        .map_err(|_| "sign_in_unreachable")?;
    let body: Value =
        serde_json::from_str(&response.body_mut().read_to_string().unwrap_or_default())
            .map_err(|_| "sign_in_invalid")?;
    let ok = |field: &str| -> Option<String> {
        let url = body.get(field)?.as_str()?;
        let host = url
            .strip_prefix("https://")?
            .split(['/', ':'])
            .next()?
            .to_ascii_lowercase();
        (host == "x.ai" || host.ends_with(".x.ai")).then(|| url.to_string())
    };
    Ok((
        ok("authorization_endpoint").ok_or("sign_in_invalid")?,
        ok("token_endpoint").ok_or("sign_in_invalid")?,
        ok("device_authorization_endpoint"),
    ))
}

#[derive(Debug)]
pub enum CredentialError {
    /// The renewal transport never became available to send the OAuth POST.
    RenewalUnavailable,
    Other(String),
}

impl CredentialError {
    /// Preserve the diagnostic used when the caller cannot switch providers.
    pub fn code(&self) -> &str {
        match self {
            Self::RenewalUnavailable => "sign_in_unreachable",
            Self::Other(code) => code,
        }
    }
}

impl From<String> for CredentialError {
    fn from(code: String) -> Self {
        Self::Other(code)
    }
}

/// A renewal gets a fresh agent. Once any transport has been handed to the HTTP client,
/// a subsequent error cannot establish that the POST was unsent (including on a redirect).
fn renewal_agent() -> (ureq::Agent, Arc<AtomicBool>) {
    use ureq::unversioned::{
        resolver::DefaultResolver,
        transport::{ConnectionDetails, Connector, DefaultConnector, Transport},
    };

    #[derive(Debug)]
    struct MarkConnected(Arc<AtomicBool>);
    impl<T: Transport> Connector<T> for MarkConnected {
        type Out = T;
        fn connect(
            &self,
            _: &ConnectionDetails,
            chained: Option<T>,
        ) -> Result<Option<T>, ureq::Error> {
            if chained.is_some() {
                self.0.store(true, Ordering::Relaxed);
            }
            Ok(chained)
        }
    }

    let connected = Arc::new(AtomicBool::new(false));
    let connector = DefaultConnector::default().chain(MarkConnected(connected.clone()));
    (
        ureq::Agent::with_parts(auth_config(), connector, DefaultResolver::default()),
        connected,
    )
}

/// Renew one provider's credentials (under the lock: see the module notes).
fn renew(provider: &str, c: &Value) -> Result<Value, CredentialError> {
    let refresh = c
        .get("refresh")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CredentialError::Other("sign_in_again".into()))?;
    let (url, client) = match provider {
        CODEX => (codex_token_url(), CODEX_CLIENT_ID),
        GROK => (
            c.get("tokenEndpoint")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(format!("{GROK_ISSUER}/oauth2/token")),
            GROK_CLIENT_ID,
        ),
        _ => return Ok(c.clone()),
    };
    let (agent, connected) = renewal_agent();
    let (status, body) = post_with(
        agent,
        &url,
        Some(&[
            ("grant_type", "refresh_token"),
            ("client_id", client),
            ("refresh_token", refresh),
        ]),
        None,
    )
    .map_err(|err| {
        if !connected.load(Ordering::Relaxed)
            && (crate::provider::connect_unavailable(&err)
                || matches!(err, ureq::Error::Timeout(ureq::Timeout::Global)))
        {
            CredentialError::RenewalUnavailable
        } else {
            CredentialError::Other("sign_in_unreachable".into())
        }
    })?;
    if status != 200 {
        // A refused refresh token cannot be fixed by trying again: sign in again.
        return Err(if matches!(status, 400 | 401 | 403) {
            "sign_in_again".into()
        } else {
            format!("renewal_http_{status}")
        }
        .into());
    }
    let mut renewed = credential_from(provider, &body, &url)?;
    if renewed["refresh"] == "" {
        renewed["refresh"] = json!(refresh);
    }
    Ok(renewed)
}

fn expiring(c: &Value) -> bool {
    c.get("type").and_then(Value::as_str) == Some("oauth")
        && c.get("expires")
            .and_then(Value::as_u64)
            .is_some_and(|e| e <= now_ms() + RENEW_EARLY_MS)
}

/// What a request needs for a provider, renewing first when the token is about to expire:
/// None when the core holds nothing for it (the runner is asked instead).
pub fn credentials(path: &Path, provider: &str) -> Result<Option<Value>, CredentialError> {
    let Some(stored) = read(path).get(provider).cloned() else {
        return Ok(None);
    };
    let current = if expiring(&stored) {
        locked(path, || -> Result<Value, CredentialError> {
            // Another session may have renewed while this one waited for the lock.
            let mut all = read(path);
            let latest = all.get(provider).cloned().unwrap_or(stored.clone());
            if !expiring(&latest) {
                return Ok(latest);
            }
            let renewed = renew(provider, &latest)?;
            all.insert(provider.to_string(), renewed.clone());
            write(path, &all)?;
            Ok(renewed)
        })??
    } else {
        stored
    };
    let token = current
        .get("access")
        .or_else(|| current.get("key"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut out = json!({"token": token});
    if let Some(a) = current.get("accountId") {
        out["accountId"] = a.clone();
    }
    if provider == GROK {
        out["baseUrl"] = current.get("baseUrl").cloned().unwrap_or(json!(GROK_BASE));
    }
    Ok(Some(out))
}

fn pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    random(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn random(out: &mut [u8]) {
    use std::io::Read;
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(out))
        .is_err()
    {
        let seed = format!("{}{:?}", now_ms(), std::thread::current().id());
        out.copy_from_slice(&Sha256::digest(seed.as_bytes())[..out.len().min(32)]);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(b) => {
                    out.push(b);
                    i += 3;
                    continue;
                }
                None => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `code` (and `state`) from a pasted redirect URL, query string or bare code.
pub fn parse_pasted(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    let query = value.split_once('?').map(|(_, q)| q).unwrap_or(value);
    let query = query.split('#').next().unwrap_or(query);
    if query.contains("code=") {
        let get = |key: &str| {
            query.split('&').find_map(|p| {
                p.split_once('=')
                    .filter(|(k, _)| *k == key)
                    .map(|(_, v)| decode(v))
            })
        };
        return (get("code"), get("state"));
    }
    if let Some((code, state)) = value.split_once('#') {
        return (Some(code.to_string()), Some(state.to_string()));
    }
    (Some(value.to_string()), None)
}

/// One step of a sign-in, reported to whoever started it.
pub enum Step {
    /// Open this page and enter the code (device sign-in).
    DeviceCode {
        url: String,
        code: String,
        expires_in: u64,
    },
    /// Open this page, sign in, then paste the address it ends on (browser sign-in).
    OpenAndPaste { url: String },
}

/// Sign in to a provider. `method`: "device", "browser" or "key" (OpenRouter API key, given
/// as `secret`). `show` reports each step; `paste` waits for the pasted address. Saves the
/// credentials and returns what was stored about them (no secrets).
pub fn login(
    path: &Path,
    provider: &str,
    method: &str,
    secret: Option<&str>,
    show: &dyn Fn(Step),
    paste: &dyn Fn() -> Option<String>,
) -> Result<Value, String> {
    let credential = match (provider, method) {
        (OPENROUTER, "key") => {
            let key = secret
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .ok_or("sign_in_needs_key")?;
            json!({"type": "api_key", "key": key})
        }
        (CODEX, "device") => codex_device(show)?,
        (CODEX, "browser") => codex_browser(show, paste)?,
        (GROK, "device") => grok_device(show)?,
        (GROK, "browser") => grok_browser(show, paste)?,
        (OPENROUTER, "browser") => openrouter_browser(show, paste)?,
        _ => return Err("sign_in_unsupported".into()),
    };
    save(path, provider, credential.clone())?;
    Ok(
        json!({"provider": provider, "type": credential.get("type"), "expires": credential.get("expires"),
        "accountId": credential.get("accountId")}),
    )
}

/// Poll every `interval` seconds until `poll` returns Some, up to `expires` seconds;
/// `poll` returns Err to stop. RFC 8628: a slow_down adds 5 seconds.
fn poll_device(
    mut interval: u64,
    expires: u64,
    mut poll: impl FnMut() -> Result<Option<Result<Value, ()>>, String>,
) -> Result<Value, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(expires);
    interval = interval.max(1);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(interval));
        match poll()? {
            Some(Ok(v)) => return Ok(v),
            Some(Err(())) => interval += 5,
            None => {}
        }
    }
    Err("sign_in_timed_out".into())
}

/// OpenAI's device sign-in for Codex.
fn codex_device(show: &dyn Fn(Step)) -> Result<Value, String> {
    let auth = codex_auth();
    let (status, body) = post(
        &format!("{auth}/api/accounts/deviceauth/usercode"),
        None,
        Some(&json!({"client_id": CODEX_CLIENT_ID})),
    )?;
    if status != 200 {
        return Err(format!("sign_in_http_{status}"));
    }
    let id = body
        .get("device_auth_id")
        .and_then(Value::as_str)
        .ok_or("sign_in_invalid")?
        .to_string();
    let code = body
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or("sign_in_invalid")?
        .to_string();
    let interval = match body.get("interval") {
        Some(Value::String(s)) => s.trim().parse().unwrap_or(5),
        Some(v) => v.as_u64().unwrap_or(5),
        None => 5,
    };
    show(Step::DeviceCode {
        url: format!("{auth}/codex/device"),
        code: code.clone(),
        expires_in: 900,
    });
    let grant = poll_device(interval, 900, || {
        let (status, body) = post(
            &format!("{auth}/api/accounts/deviceauth/token"),
            None,
            Some(&json!({"device_auth_id": id, "user_code": code})),
        )?;
        if status == 200 {
            return Ok(Some(Ok(body)));
        }
        match (status, error_code(&body).as_str()) {
            (403 | 404, _) | (_, "deviceauth_authorization_pending") => Ok(None),
            (_, "slow_down") => Ok(Some(Err(()))),
            _ => Err(format!("sign_in_http_{status}")),
        }
    })?;
    let code = grant
        .get("authorization_code")
        .and_then(Value::as_str)
        .ok_or("sign_in_invalid")?;
    let verifier = grant
        .get("code_verifier")
        .and_then(Value::as_str)
        .ok_or("sign_in_invalid")?;
    codex_exchange(code, verifier, &format!("{auth}/deviceauth/callback"))
}

fn codex_exchange(code: &str, verifier: &str, redirect: &str) -> Result<Value, String> {
    let url = codex_token_url();
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", CODEX_CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect),
    ];
    let (status, body) = post(&url, Some(&form), None)?;
    if status != 200 {
        return Err(format!("sign_in_http_{status}"));
    }
    credential_from(CODEX, &body, &url)
}

/// OpenAI's browser sign-in, finished by pasting the address the
/// browser ends on (http://localhost:1455/auth/callback?code=...).
fn codex_browser(show: &dyn Fn(Step), paste: &dyn Fn() -> Option<String>) -> Result<Value, String> {
    let (verifier, challenge) = pkce();
    let mut state = [0u8; 16];
    random(&mut state);
    let state = hex(&state);
    let url = format!(
        "{}/oauth/authorize?{}",
        codex_auth(),
        query(&[
            ("response_type", "code"),
            ("client_id", CODEX_CLIENT_ID),
            ("redirect_uri", CODEX_REDIRECT),
            ("scope", CODEX_SCOPE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "pi")
        ])
    );
    show(Step::OpenAndPaste { url });
    let (code, got_state) = parse_pasted(&paste().ok_or("sign_in_cancelled")?);
    if got_state.is_some_and(|s| s != state) {
        return Err("sign_in_state_mismatch".into());
    }
    codex_exchange(&code.ok_or("sign_in_no_code")?, &verifier, CODEX_REDIRECT)
}

/// xAI's device sign-in for the Grok CLI proxy (the grok-cli add-on's loginWithDeviceCode).
fn grok_device(show: &dyn Fn(Step)) -> Result<Value, String> {
    let (_, token_url, device_url) = grok_endpoints()?;
    let device_url = device_url.ok_or("sign_in_unsupported")?;
    let (status, body) = post(
        &device_url,
        Some(&[("client_id", GROK_CLIENT_ID), ("scope", GROK_SCOPE)]),
        None,
    )?;
    if status != 200 {
        return Err(format!("sign_in_http_{status}"));
    }
    let device = body
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or("sign_in_invalid")?
        .to_string();
    let code = body
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or("sign_in_invalid")?
        .to_string();
    let url = body
        .get("verification_uri_complete")
        .or_else(|| body.get("verification_uri"))
        .and_then(Value::as_str)
        .ok_or("sign_in_invalid")?
        .to_string();
    let interval = body.get("interval").and_then(Value::as_u64).unwrap_or(5);
    let expires = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(1800);
    show(Step::DeviceCode {
        url,
        code,
        expires_in: expires,
    });
    let grant = poll_device(interval, expires, || {
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", GROK_CLIENT_ID),
            ("device_code", device.as_str()),
        ];
        let (status, body) = post(&token_url, Some(&form), None)?;
        if status == 200 {
            return Ok(Some(Ok(body)));
        }
        match error_code(&body).as_str() {
            "authorization_pending" => Ok(None),
            "slow_down" => Ok(Some(Err(()))),
            _ => Err(format!("sign_in_http_{status}")),
        }
    })?;
    credential_from(GROK, &grant, &token_url)
}

/// xAI's browser sign-in, finished by pasting the address the browser ends on.
fn grok_browser(show: &dyn Fn(Step), paste: &dyn Fn() -> Option<String>) -> Result<Value, String> {
    let (auth_url, token_url, _) = grok_endpoints()?;
    let (verifier, challenge) = pkce();
    let (mut state, mut nonce) = ([0u8; 16], [0u8; 16]);
    random(&mut state);
    random(&mut nonce);
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let (state, nonce) = (b64(&state), b64(&nonce));
    let url = format!(
        "{auth_url}?{}",
        query(&[
            ("response_type", "code"),
            ("client_id", GROK_CLIENT_ID),
            ("redirect_uri", GROK_REDIRECT),
            ("scope", GROK_SCOPE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("nonce", &nonce),
            ("plan", "generic"),
            ("referrer", "pi-grok-cli")
        ])
    );
    show(Step::OpenAndPaste { url });
    let (code, got_state) = parse_pasted(&paste().ok_or("sign_in_cancelled")?);
    if got_state.is_some_and(|s| s != state) {
        return Err("sign_in_state_mismatch".into());
    }
    let code = code.ok_or("sign_in_no_code")?;
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", GROK_CLIENT_ID),
        ("code", code.as_str()),
        ("redirect_uri", GROK_REDIRECT),
        ("code_verifier", verifier.as_str()),
    ];
    let (status, body) = post(&token_url, Some(&form), None)?;
    if status != 200 {
        return Err(format!("sign_in_http_{status}"));
    }
    credential_from(GROK, &body, &token_url)
}

/// OpenRouter's sign-in: it ends with a lasting API key.
fn openrouter_browser(
    show: &dyn Fn(Step),
    paste: &dyn Fn() -> Option<String>,
) -> Result<Value, String> {
    let (verifier, challenge) = pkce();
    let url = format!(
        "{OPENROUTER_AUTH}?{}",
        query(&[
            ("callback_url", OPENROUTER_REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256")
        ])
    );
    show(Step::OpenAndPaste { url });
    let code = parse_pasted(&paste().ok_or("sign_in_cancelled")?)
        .0
        .ok_or("sign_in_no_code")?;
    let (status, body) = post(
        OPENROUTER_KEYS,
        None,
        Some(&json!({"code": code, "code_verifier": verifier, "code_challenge_method": "S256"})),
    )?;
    if status != 200 {
        return Err(format!("sign_in_http_{status}"));
    }
    let key = body
        .get("key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
        .ok_or("sign_in_invalid")?;
    Ok(json!({"type": "api_key", "key": key}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("core-auth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("auth.json")
    }

    #[test]
    fn pasted_addresses_and_codes() {
        assert_eq!(
            parse_pasted("http://localhost:1455/auth/callback?code=ab%2Fc&state=xyz"),
            (Some("ab/c".into()), Some("xyz".into()))
        );
        assert_eq!(
            parse_pasted("code=abc&state=s"),
            (Some("abc".into()), Some("s".into()))
        );
        assert_eq!(
            parse_pasted("abc#s"),
            (Some("abc".into()), Some("s".into()))
        );
        assert_eq!(parse_pasted("  abc  "), (Some("abc".into()), None));
        assert_eq!(parse_pasted(""), (None, None));
    }

    #[test]
    fn stored_credentials_are_owner_only_and_never_reported() {
        use std::os::unix::fs::PermissionsExt;
        let path = file("store");
        let got = login(
            &path,
            OPENROUTER,
            "key",
            Some("sk-or-secret"),
            &|_| {},
            &|| None,
        )
        .unwrap();
        assert!(!got.to_string().contains("secret"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            credentials(&path, OPENROUTER).unwrap().unwrap()["token"],
            "sk-or-secret"
        );
        assert!(!status(&path).to_string().contains("secret"));
        assert!(
            credentials(&path, CODEX).unwrap().is_none(),
            "nothing stored: the runner is asked"
        );
        assert!(remove(&path, OPENROUTER).unwrap());
        assert!(credentials(&path, OPENROUTER).unwrap().is_none());
    }

    /// A Codex-shaped access token (a JWT carrying the account id).
    fn jwt(n: u32) -> String {
        let claims = json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct"}, "n": n});
        format!(
            "h.{}.s",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    #[test]
    fn sessions_renewing_at_once_use_the_refresh_token_once() {
        use std::io::{BufRead, BufReader, Read, Write};
        // A token server that, like OpenAI's, rotates the refresh token: an old one is refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = std::sync::Arc::new(std::sync::Mutex::new((1u32, 0u32)));
        let state = served.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let form = String::from_utf8_lossy(&body).to_string();
                let mut s = state.lock().unwrap();
                let (status, reply) = if form.contains(&format!("refresh_token=r{}", s.0)) {
                    s.0 += 1;
                    s.1 += 1;
                    (
                        200,
                        json!({"access_token": jwt(s.0), "refresh_token": format!("r{}", s.0), "expires_in": 3600}),
                    )
                } else {
                    (400, json!({"error": "invalid_grant"}))
                };
                let text = reply.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        unsafe { std::env::set_var("CONVERSATION_CORE_CODEX_AUTH", &base) };
        let path = file("race");
        save(&path, CODEX, json!({"type": "oauth", "access": jwt(1), "refresh": "r1", "expires": 0, "accountId": "acct"})).unwrap();
        let sessions: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || credentials(&path, CODEX))
            })
            .collect();
        let tokens: Vec<Value> = sessions
            .into_iter()
            .map(|t| t.join().unwrap().unwrap().unwrap()["token"].clone())
            .collect();
        assert!(
            tokens.iter().all(|t| *t == json!(jwt(2))),
            "every session got the one renewed token"
        );
        assert_eq!(
            served.lock().unwrap().1,
            1,
            "the refresh token was used once"
        );
        let stored = read(&path);
        assert_eq!(stored[CODEX]["refresh"], "r2");
    }

    #[test]
    fn a_token_still_valid_is_not_renewed() {
        let path = file("valid");
        save(&path, GROK, json!({"type": "oauth", "access": "a1", "refresh": "r1", "expires": now_ms() + 3_600_000, "baseUrl": GROK_BASE})).unwrap();
        let got = credentials(&path, GROK).unwrap().unwrap();
        assert_eq!(
            (got["token"].clone(), got["baseUrl"].clone()),
            (json!("a1"), json!(GROK_BASE))
        );
    }
}
