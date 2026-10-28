use librespot::core::authentication::Credentials;
use librespot::core::cache::Cache;
use librespot::core::config::SessionConfig;
use librespot::core::session::Session;
use librespot::oauth::{OAuthClient, OAuthClientBuilder, OAuthToken};
use librespot::playback::audio_backend::find;
use librespot::playback::config::{AudioFormat, Bitrate, PlayerConfig};
use librespot::playback::mixer::softmixer::SoftMixer;
use librespot::playback::mixer::{Mixer, MixerConfig};
use librespot::playback::player::Player;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use stupefinity_protocol::AuthState;
use tracing::{info, warn};

static OAUTH_SCOPES: &[&str] = &[
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-public",
    "playlist-modify-private",
    "user-library-read",
    "user-library-modify",
    // Needed for `product` in /v1/me, which tells Free and Premium apart.
    "user-read-private",
    "streaming",
];

const REDIRECT_URI: &str = "http://127.0.0.1:8888/login";

/// The client ID of the user's own Spotify app (developer.spotify.com). There is no shared
/// default: Spotify's terms want one per app, and a shared one is rate-limited by everyone.
static CLIENT_ID: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Whether `id` looks like a Spotify client ID (32 hex digits).
pub fn valid_client_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// Sets (or with `None` or an invalid ID, clears) the client ID used to log in.
pub fn set_client_id(client_id: Option<String>) {
    let id = client_id
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| valid_client_id(s));
    *CLIENT_ID.write().unwrap_or_else(|e| e.into_inner()) = id;
}

pub fn has_client_id() -> bool {
    client_id().is_some()
}

fn client_id() -> Option<String> {
    CLIENT_ID.read().unwrap_or_else(|e| e.into_inner()).clone()
}
const TOKEN_FILE: &str = "token.json";

/// Refresh the access token when it has less than this much life left.
const REFRESH_MARGIN_SECS: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: String,
    pub display_name: Option<String>,
    /// "premium", "free" or "open"; empty if Spotify leaves it out (treated as Free).
    #[serde(default)]
    pub product: String,
    pub country: Option<String>,
}

/// OAuth tokens as persisted in `token.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// Unix timestamp (seconds) at which `access_token` expires; 0 if unknown.
    #[serde(default)]
    pub expires_at: u64,
}

impl TokenSet {
    fn from_oauth(token: OAuthToken) -> Self {
        let remaining = token.expires_at.saturating_duration_since(Instant::now());
        Self {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: unix_now() + remaining.as_secs(),
        }
    }

    fn expires_soon(&self) -> bool {
        self.expires_at != 0 && unix_now() + REFRESH_MARGIN_SECS >= self.expires_at
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A fully established login, built without holding the `SessionManager` lock.
pub struct Account {
    pub tokens: TokenSet,
    pub profile: UserProfile,
    pub session: Option<Session>,
    pub player: Option<Arc<Player>>,
    pub mixer: Option<SoftMixer>,
}

pub struct SessionManager {
    pub session: Option<Session>,
    pub player: Option<Arc<Player>>,
    pub mixer: Option<SoftMixer>,
    pub access_token: String,
    pub is_premium: bool,
    pub user_id: String,
    pub display_name: String,
    pub is_authenticated: bool,
    tokens: TokenSet,
    cache_dir: PathBuf,
}

impl SessionManager {
    /// Initialize session manager.
    /// Attempts silent login (refreshing the token if needed) when cached credentials exist.
    /// Otherwise starts unauthenticated so login can be triggered on demand later.
    pub async fn new() -> anyhow::Result<Self> {
        let cache_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Stupefinity")
            .join("cache");
        std::fs::create_dir_all(&cache_dir)?;
        info!("Using cache directory: {:?}", cache_dir);

        let mut mgr = Self {
            session: None,
            player: None,
            mixer: None,
            access_token: String::new(),
            is_premium: false,
            user_id: String::new(),
            display_name: String::new(),
            is_authenticated: false,
            tokens: TokenSet::default(),
            cache_dir,
        };

        if let Some(tokens) = Self::load_tokens(&mgr.cache_dir) {
            match Self::establish(&mgr.cache_dir, tokens).await {
                Ok(account) => {
                    info!("Silent login successful for: {}", account.profile.id);
                    mgr.set_account(account);
                }
                Err(e) => warn!("Cached login failed ({e}); a fresh login is required."),
            }
        }

        if !mgr.is_authenticated {
            info!("Stupefinity daemon started in unauthenticated mode. Ready for on-demand login.");
        }

        Ok(mgr)
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.cache_dir.clone()
    }

    /// Runs the browser PKCE login flow and establishes the account.
    /// Static so the caller does not hold the manager lock while the user is in the browser.
    pub async fn interactive_login(cache_dir: PathBuf) -> anyhow::Result<Account> {
        info!("Initiating browser PKCE login at {REDIRECT_URI}...");
        let token = Self::oauth_client(true)?
            .get_access_token_async()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get Spotify OAuth access token: {e}"))?;
        Self::establish(&cache_dir, TokenSet::from_oauth(token)).await
    }

    /// Installs an established account as the active login.
    pub fn set_account(&mut self, account: Account) {
        let Account {
            tokens,
            profile,
            session,
            player,
            mixer,
        } = account;
        self.is_premium = profile.product.eq_ignore_ascii_case("premium");
        self.display_name = profile.display_name.unwrap_or_else(|| profile.id.clone());
        self.user_id = profile.id;
        self.access_token = tokens.access_token.clone();
        self.tokens = tokens;
        self.session = session;
        self.player = player;
        self.mixer = mixer;
        self.is_authenticated = true;

        info!(
            "Logged in as: {} (Display: '{}'), tier: {}",
            self.user_id,
            self.display_name,
            if self.is_premium {
                "Spotify Premium"
            } else {
                "Spotify Free"
            }
        );
    }

    /// Clears cached credentials and resets session state.
    pub fn logout(&mut self) {
        let _ = std::fs::remove_file(self.cache_dir.join(TOKEN_FILE));
        // Saved by librespot's cache for Premium sessions.
        let _ = std::fs::remove_file(self.cache_dir.join("credentials.json"));
        self.mixer = None;
        self.player = None;
        self.session = None;
        self.access_token.clear();
        self.tokens = TokenSet::default();
        self.is_premium = false;
        self.user_id.clear();
        self.display_name.clear();
        self.is_authenticated = false;
        info!("Logged out successfully.");
    }

    /// Returns the refresh token when the access token is about to expire.
    pub fn refresh_due(&self) -> Option<String> {
        (self.is_authenticated
            && !self.tokens.refresh_token.is_empty()
            && self.tokens.expires_soon())
        .then(|| self.tokens.refresh_token.clone())
    }

    /// Exchanges a refresh token for new tokens. Static so it runs without the manager lock.
    pub async fn refresh(refresh_token: &str) -> anyhow::Result<TokenSet> {
        let token = Self::oauth_client(false)?
            .refresh_token_async(refresh_token)
            .await
            .map_err(|e| anyhow::anyhow!("Token refresh failed: {e}"))?;
        let mut tokens = TokenSet::from_oauth(token);
        // Spotify may omit a new refresh token, in which case the old one stays valid.
        if tokens.refresh_token.is_empty() {
            tokens.refresh_token = refresh_token.to_string();
        }
        Ok(tokens)
    }

    /// Stores refreshed tokens (ignored if the user logged out in the meantime).
    pub fn update_tokens(&mut self, tokens: TokenSet) {
        if !self.is_authenticated {
            return;
        }
        Self::save_tokens(&self.cache_dir, &tokens);
        self.access_token = tokens.access_token.clone();
        self.tokens = tokens;
        info!("Spotify access token refreshed.");
    }

    /// Returns the current protocol AuthState enum.
    pub fn auth_state(&self) -> AuthState {
        if self.is_authenticated {
            AuthState::Authenticated {
                user_id: self.user_id.clone(),
                display_name: self.display_name.clone(),
                is_premium: self.is_premium,
            }
        } else {
            AuthState::Unauthenticated
        }
    }

    /// Validates (refreshing if needed) the tokens, fetches the profile, persists the tokens
    /// and, for Premium accounts only, connects a native librespot session.
    async fn establish(cache_dir: &Path, mut tokens: TokenSet) -> anyhow::Result<Account> {
        if tokens.expires_soon() && !tokens.refresh_token.is_empty() {
            tokens = Self::refresh(&tokens.refresh_token).await?;
        }

        let http_client = Client::builder().build()?;
        let profile = match Self::fetch_profile(&http_client, &tokens.access_token).await {
            Ok(p) => p,
            // Expiry unknown (old cache format) or token revoked early: try one refresh.
            Err(e) if is_unauthorized(&e) && !tokens.refresh_token.is_empty() => {
                tokens = Self::refresh(&tokens.refresh_token).await?;
                Self::fetch_profile(&http_client, &tokens.access_token).await?
            }
            Err(e) => return Err(e),
        };
        Self::save_tokens(cache_dir, &tokens);

        // librespot exits the process on non-Premium accounts, so only connect for Premium.
        let mut session = None;
        let mut player = None;
        let mut mixer = None;
        if profile.product.eq_ignore_ascii_case("premium") {
            info!("Connecting native librespot session for Spotify Premium...");
            let cache = Cache::new(Some(cache_dir), None, None, None)?;
            let s = Session::new(SessionConfig::default(), Some(cache));
            match s
                .connect(
                    Credentials::with_access_token(tokens.access_token.clone()),
                    true,
                )
                .await
            {
                Ok(_) => {
                    info!("Native librespot session connected.");
                    let m = SoftMixer::open(MixerConfig::default())?;
                    let volume_getter = m.get_soft_volume();
                    let config = PlayerConfig {
                        bitrate: Bitrate::Bitrate320,
                        ..PlayerConfig::default()
                    };
                    let eq = crate::eq::shared();
                    let p = Player::new(config, s.clone(), volume_getter, move || {
                        let builder = find(None).expect("No audio sink backend available");
                        Box::new(crate::eq::EqSink::new(builder(None, AudioFormat::F32), eq))
                    });
                    player = Some(p);
                    session = Some(s);
                    mixer = Some(m);
                }
                Err(e) => warn!("Native librespot session connection failed ({e})."),
            }
        }

        Ok(Account {
            tokens,
            profile,
            session,
            player,
            mixer,
        })
    }

    fn load_tokens(cache_dir: &Path) -> Option<TokenSet> {
        let content = std::fs::read_to_string(cache_dir.join(TOKEN_FILE)).ok()?;
        serde_json::from_str::<TokenSet>(&content)
            .ok()
            .filter(|t| !t.access_token.is_empty())
    }

    fn save_tokens(cache_dir: &Path, tokens: &TokenSet) {
        match serde_json::to_string(tokens) {
            Ok(json) => {
                if let Err(e) = std::fs::write(cache_dir.join(TOKEN_FILE), json) {
                    warn!("Failed to persist Spotify tokens: {e}");
                }
            }
            Err(e) => warn!("Failed to serialize Spotify tokens: {e}"),
        }
    }

    /// Fetches the user profile from Spotify Web API.
    async fn fetch_profile(client: &Client, access_token: &str) -> anyhow::Result<UserProfile> {
        if access_token.is_empty() {
            return Err(anyhow::anyhow!("Empty access token"));
        }

        let resp = crate::api::send_spotify(
            client
                .get("https://api.spotify.com/v1/me")
                .bearer_auth(access_token)
                .timeout(Duration::from_secs(10)),
        )
        .await?;
        Ok(resp.json().await?)
    }

    fn oauth_client(open_browser: bool) -> anyhow::Result<OAuthClient> {
        let client_id =
            client_id().ok_or_else(|| anyhow::anyhow!("No Spotify client ID is set"))?;
        let mut builder = OAuthClientBuilder::new(
            &client_id,
            REDIRECT_URI,
            OAUTH_SCOPES.to_vec(),
        )
        .with_custom_message("<h1>Logged in to Stupefinity</h1><p>You can close this tab and return to Stupefinity.</p>");
        if open_browser {
            builder = builder.open_in_browser();
        }
        builder
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build OAuth client: {e}"))
    }
}

/// True when `e` is an HTTP 401 (the access token is expired or revoked).
fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        == Some(reqwest::StatusCode::UNAUTHORIZED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_client_id_format() {
        assert!(valid_client_id("0123456789abcdefABCDEF0123456789"));
        assert!(!valid_client_id("0123456789abcdef"));
        assert!(!valid_client_id("0123456789abcdefg123456789abcdef"));
        assert!(!valid_client_id(""));
    }
}
