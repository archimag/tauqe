//! Native public-client registration and loopback sign-in for ChatGPT plans.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::future::{BoxFuture, WeakShared};
use futures_util::FutureExt;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use reqwest::{Client, Url};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

use super::session::TOKEN_ENDPOINT;
use crate::config::update_credentials;

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";
const RESOURCE: &str = "https://api.openai.com/v1";
const DYNAMIC: &str = "dynamic_agent_client";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

/// Presents a browser sign-in invitation until dismissed. Dropping the future
/// closes the invitation, including when the model operation is cancelled.
#[async_trait::async_trait]
pub trait ChatGptSignInUi: Send + Sync {
    async fn wait_for_dismissal(&self, model: &str, authorization_url: &str);
}

type Pending = WeakShared<BoxFuture<'static, Result<bool, String>>>;

pub struct ChatGptSignIn {
    ui: Arc<dyn ChatGptSignInUi>,
    token_endpoint: String,
    jwks_endpoint: String,
    pending: std::sync::Mutex<std::collections::HashMap<PathBuf, Pending>>,
}

impl ChatGptSignIn {
    pub fn new(ui: Arc<dyn ChatGptSignInUi>) -> Self {
        Self {
            ui,
            token_endpoint: TOKEN_ENDPOINT.into(),
            jwks_endpoint: JWKS.into(),
            pending: Default::default(),
        }
    }
    pub(super) async fn authorize(
        &self,
        path: &Path,
        model: &str,
        client: &Client,
    ) -> Result<bool> {
        let future = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| anyhow!("Sign-in coordinator unavailable"))?;
            pending.retain(|_, entry| entry.upgrade().is_some());
            if let Some(active) = pending.get(path).and_then(Pending::upgrade) {
                active
            } else {
                let (path, model, client, ui) = (
                    path.to_owned(),
                    model.to_owned(),
                    client.clone(),
                    self.ui.clone(),
                );
                let key = path.clone();
                let token_endpoint = self.token_endpoint.clone();
                let jwks_endpoint = self.jwks_endpoint.clone();
                let future = async move {
                    tokio::time::timeout(
                        Duration::from_secs(300),
                        login(path, model, client, ui, &token_endpoint, &jwks_endpoint),
                    )
                    .await
                    .map_err(|_| "ChatGPT sign-in timed out".to_owned())?
                    .map_err(|err| err.to_string())
                }
                .boxed()
                .shared();
                if let Some(weak) = future.downgrade() {
                    pending.insert(key, weak);
                }
                future
            }
        };
        future.await.map_err(anyhow::Error::msg)
    }
}

struct Registration {
    host: String,
    client_id: Option<String>,
    subject: Option<String>,
}

fn section(doc: &mut DocumentMut) -> Result<&mut Table> {
    let openai = doc
        .entry("openai")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("Invalid openai credentials table")?;
    openai
        .entry("chatgpt")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("Invalid chatgpt credentials table")
}

fn prepare(path: &Path) -> Result<Registration> {
    update_credentials(path, |doc| {
        let table = section(doc)?;
        let host = table
            .entry("ext_agent_host_id")
            .or_insert(value(format!("urn:uuid:{}", uuid::Uuid::new_v4())))
            .as_str()
            .context("Invalid host identifier")?
            .to_owned();
        let client_id = table
            .get("active_client_id")
            .and_then(Item::as_str)
            .map(str::to_owned);
        let subject = client_id
            .as_ref()
            .and_then(|id| {
                table
                    .get("sessions")?
                    .as_array_of_tables()?
                    .iter()
                    .find(|s| s.get("client_id").and_then(Item::as_str) == Some(id))?
                    .get("subject")?
                    .as_str()
            })
            .map(str::to_owned);
        Ok(Registration {
            host,
            client_id,
            subject,
        })
    })
}

fn secret() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| anyhow!("System random source unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

struct Attempt {
    listener: TcpListener,
    redirect: Url,
    verifier: String,
    state: String,
    nonce: String,
}
impl Attempt {
    async fn new() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let redirect = Url::parse(&format!("http://{}/auth/callback", listener.local_addr()?))?;
        Ok(Self {
            listener,
            redirect,
            verifier: secret()?,
            state: secret()?,
            nonce: secret()?,
        })
    }
    fn url(&self, registration: &Registration) -> Result<Url> {
        let mut url = Url::parse(AUTHORIZE)?;
        let digest = ring::digest::digest(&ring::digest::SHA256, self.verifier.as_bytes());
        let mut query = url.query_pairs_mut();
        query.extend_pairs([
            (
                "client_id",
                registration.client_id.as_deref().unwrap_or(DYNAMIC),
            ),
            ("redirect_uri", self.redirect.as_str()),
            ("ext_agent_host_id", &registration.host),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("resource", RESOURCE),
            ("state", &self.state),
            ("nonce", &self.nonce),
            ("code_challenge_method", "S256"),
            ("code_challenge", &URL_SAFE_NO_PAD.encode(digest)),
        ]);
        if registration.client_id.is_none() {
            query.append_pair("agent_name_hint", "TAUQE");
        }
        drop(query);
        Ok(url)
    }
    async fn callback(&self, saved: Option<&str>) -> Result<(String, String)> {
        loop {
            let (mut socket, _) = self.listener.accept().await?;
            let Ok(Ok(Some(query))) =
                tokio::time::timeout(Duration::from_secs(2), self.read_callback(&mut socket)).await
            else {
                continue;
            };
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Type: text/plain\r\n\r\nReturn to TAUQE to finish signing in.").await;
            ensure!(!query.contains_key("error"), "ChatGPT sign-in was declined");
            let issued = query
                .get("client_id")
                .map(String::as_str)
                .or(saved)
                .filter(|id| !id.is_empty() && *id != DYNAMIC)
                .context("OpenAI did not return a registered client ID")?;
            ensure!(
                saved.is_none_or(|id| id == issued),
                "OpenAI returned a different client registration"
            );
            return Ok((
                query
                    .get("code")
                    .filter(|c| !c.is_empty())
                    .context("Missing authorization code")?
                    .clone(),
                issued.to_owned(),
            ));
        }
    }
    async fn read_callback(
        &self,
        socket: &mut TcpStream,
    ) -> Result<Option<std::collections::HashMap<String, String>>> {
        let mut line = String::new();
        BufReader::new(socket.take(16384))
            .read_line(&mut line)
            .await?;
        let mut parts = line.split_whitespace();
        if parts.next() != Some("GET") {
            return Ok(None);
        }
        let Some(target) = parts.next() else {
            return Ok(None);
        };
        let Some(query) = target.strip_prefix("/auth/callback?") else {
            return Ok(None);
        };
        let mut url = self.redirect.clone();
        url.set_query(Some(query));
        let mut fields = std::collections::HashMap::new();
        for (k, v) in url.query_pairs() {
            if fields.insert(k.into_owned(), v.into_owned()).is_some() {
                return Ok(None);
            }
        }
        Ok((fields.get("state") == Some(&self.state)).then_some(fields))
    }
}

#[derive(Deserialize)]
struct Grant {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_in: u64,
    token_type: String,
    scope: String,
}
#[derive(Deserialize)]
struct Claims {
    sub: String,
    nonce: String,
    email: Option<String>,
    azp: Option<String>,
    aud: serde_json::Value,
}

fn verify(token: &str, jwks: &JwkSet, id: &str, nonce: &str) -> Result<Claims> {
    let header = decode_header(token).context("Invalid OpenAI ID token header")?;
    ensure!(
        header.alg == Algorithm::RS256,
        "Unexpected OpenAI ID token algorithm"
    );
    let jwk = jwks
        .find(header.kid.as_deref().context("Missing ID token key ID")?)
        .context("Unknown OpenAI signing key")?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[id]);
    validation.set_issuer(&[ISSUER]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 0;
    validation.validate_nbf = true;
    let claims = decode::<Claims>(token, &DecodingKey::from_jwk(jwk)?, &validation)
        .context("Invalid OpenAI ID token")?
        .claims;
    ensure!(
        !claims.sub.is_empty()
            && claims.nonce == nonce
            && claims.azp.as_deref().is_none_or(|party| party == id)
            && (claims.aud.as_array().is_none_or(|aud| aud.len() <= 1)
                || claims.azp.as_deref() == Some(id)),
        "OpenAI sign-in identity mismatch"
    );
    Ok(claims)
}

fn save(path: &Path, reg: &Registration, id: &str, claims: Claims, grant: Grant) -> Result<()> {
    let expires = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .checked_add(grant.expires_in)
        .context("Invalid token expiry")?;
    update_credentials(path, |doc| {
        let table = section(doc)?;
        ensure!(
            table.get("active_client_id").and_then(Item::as_str) == reg.client_id.as_deref(),
            "Active ChatGPT registration changed during sign-in"
        );
        let entries = table
            .entry("sessions")
            .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
            .as_array_of_tables_mut()
            .context("Invalid sessions table")?;
        let mut record = Table::new();
        record.insert("client_id", value(id));
        record.insert("subject", value(&claims.sub));
        if let Some(email) = claims.email {
            record.insert("email", value(email));
        }
        record.insert("id_token", value(grant.id_token));
        record.insert("access_token", value(grant.access_token));
        record.insert("refresh_token", value(grant.refresh_token));
        record.insert(
            "scopes",
            value(grant.scope.split_whitespace().collect::<Array>()),
        );
        record.insert("expires_at", value(i64::try_from(expires)?));
        let index = entries
            .iter()
            .position(|entry| entry.get("client_id").and_then(Item::as_str) == Some(id));
        if let Some(existing) = index.and_then(|i| entries.get_mut(i)) {
            ensure!(
                existing
                    .get("subject")
                    .and_then(Item::as_str)
                    .is_none_or(|s| s == claims.sub),
                "Saved ChatGPT account differs from sign-in identity"
            );
            *existing = record;
        } else {
            entries.push(record);
        }
        table.insert("active_client_id", value(id));
        Ok(())
    })
}

async fn login(
    path: PathBuf,
    model: String,
    client: Client,
    ui: Arc<dyn ChatGptSignInUi>,
    token_endpoint: &str,
    jwks_endpoint: &str,
) -> Result<bool> {
    let file = path.clone();
    let reg = tokio::task::spawn_blocking(move || prepare(&file)).await??;
    let attempt = Attempt::new().await?;
    let url = attempt.url(&reg)?;
    let complete = async {
        let (code, id) = attempt.callback(reg.client_id.as_deref()).await?;
        let response = client
            .post(token_endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &id),
                ("code", &code),
                ("code_verifier", &attempt.verifier),
                ("redirect_uri", attempt.redirect.as_str()),
                ("resource", RESOURCE),
            ])
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "OpenAI code exchange returned {}",
            response.status()
        );
        let grant: Grant = response
            .json()
            .await
            .context("Invalid OpenAI token response")?;
        ensure!(
            grant.token_type.eq_ignore_ascii_case("bearer")
                && !grant.access_token.is_empty()
                && !grant.refresh_token.is_empty()
                && grant.expires_in > 0,
            "Incomplete OpenAI OAuth grant"
        );
        let jwks = client
            .get(jwks_endpoint)
            .send()
            .await?
            .error_for_status()?
            .json::<JwkSet>()
            .await?;
        let claims = verify(&grant.id_token, &jwks, &id, &attempt.nonce)?;
        ensure!(
            reg.subject.as_deref().is_none_or(|s| s == claims.sub),
            "Signed in to a different ChatGPT account"
        );
        if !grant
            .scope
            .split_whitespace()
            .any(|s| s == "chatgpt.tokens.use.direct")
        {
            bail!("Permission to use the ChatGPT plan was declined");
        }
        tokio::task::spawn_blocking(move || save(&path, &reg, &id, claims, grant)).await??;
        Ok(true)
    };
    tokio::select! {biased; _=ui.wait_for_dismissal(&model,url.as_str())=>Ok(false), result=complete=>result}
}

/// Clears only the selected registration's credentials and keeps its identity for reauthorization.
pub(crate) async fn sign_out(path: PathBuf) -> Result<bool> {
    revoke_session(
        path,
        "https://auth.openai.com/api/accounts/oauth/revoke".into(),
    )
    .await
}

async fn revoke_session(path: PathBuf, revocation_endpoint: String) -> Result<bool> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        if !path.try_exists()? {
            return Ok(true);
        }
        update_credentials(&path, |doc| {
            let table = section(doc)?;
            let Some(id) = table
                .get("active_client_id")
                .and_then(Item::as_str)
                .map(str::to_owned)
            else {
                return Ok(true);
            };
            let Some(record) = table
                .get_mut("sessions")
                .and_then(Item::as_array_of_tables_mut)
                .and_then(|entries| {
                    entries
                        .iter_mut()
                        .find(|e| e.get("client_id").and_then(Item::as_str) == Some(&id))
                })
            else {
                return Ok(true);
            };
            let revoked = if let Some(token) = record.get("refresh_token").and_then(Item::as_str) {
                let client = Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(10))
                    .build()?;
                runtime.block_on(async {
                    for attempt in 0..2 {
                        let result = client
                            .post(&revocation_endpoint)
                            .form(&[
                                ("token", token),
                                ("token_type_hint", "refresh_token"),
                                ("client_id", &id),
                            ])
                            .send()
                            .await;
                        if let Ok(response) = result {
                            if response.status().is_success() {
                                return true;
                            }
                            if !response.status().is_server_error() {
                                return false;
                            }
                        }
                        if attempt == 0 {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                    }
                    false
                })
            } else {
                true
            };
            for key in ["access_token", "refresh_token", "id_token", "expires_at"] {
                record.remove(key);
            }
            Ok(revoked)
        })
    })
    .await?
}

#[cfg(all(test, unix))]
mod tests;
