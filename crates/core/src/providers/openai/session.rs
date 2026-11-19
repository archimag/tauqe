use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::update_credentials;
use crate::models::ProviderKind;
use crate::providers::ProviderError;
use reqwest::Client;
use serde::Deserialize;
use toml_edit::{value, Array};

pub(super) const TOKEN_ENDPOINT: &str = "https://auth.openai.com/api/accounts/oauth/token";

#[derive(Default, Deserialize)]
#[serde(default)]
struct CredentialFile {
    openai: OpenAiCredentials,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct OpenAiCredentials {
    chatgpt: ChatGptCredentials,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct ChatGptCredentials {
    active_client_id: Option<String>,
    sessions: Vec<Session>,
}
#[derive(Deserialize)]
struct Session {
    client_id: String,
    #[serde(default)]
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: u64,
    scopes: Vec<String>,
}

fn auth() -> ProviderError {
    ProviderError::AuthenticationRequired(ProviderKind::OpenAI)
}

fn now() -> Result<u64, ProviderError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.as_secs())
        .map_err(|e| ProviderError::Request(e.into()))
}

fn active(text: &str) -> Result<Session, ProviderError> {
    let file: CredentialFile = toml::from_str(text)
        .map_err(|_| ProviderError::Request(anyhow::anyhow!("Invalid OpenAI credentials file")))?;
    let credentials = file.openai.chatgpt;
    let id = credentials.active_client_id.ok_or_else(auth)?;
    credentials
        .sessions
        .into_iter()
        .find(|s| s.client_id == id && s.scopes.iter().any(|s| s == "chatgpt.tokens.use.direct"))
        .ok_or_else(auth)
}

#[derive(Deserialize)]
struct TokenGrant {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    scope: Option<String>,
    token_type: String,
    id_token: Option<String>,
}

async fn refresh_grant(
    client: &Client,
    endpoint: &str,
    client_id: &str,
    refresh: &str,
) -> Result<Option<TokenGrant>, ProviderError> {
    let response = client
        .post(endpoint)
        .timeout(Duration::from_secs(30))
        .form(&[
            ("grant_type", "refresh_token"),
            ("resource", "https://api.openai.com/v1"),
            ("client_id", client_id),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .map_err(|e| ProviderError::Transient(e.into()))?;
    let status = response.status();
    if !status.is_success() {
        let error = response
            .json::<serde_json::Value>()
            .await
            .unwrap_or_default();
        let code = error["error"]
            .as_str()
            .or_else(|| error["error"]["code"].as_str());
        return match code {
            Some(
                "invalid_grant"
                | "invalid_refresh_token"
                | "token_expired"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
                | "refresh_token_reused",
            ) => Ok(None),
            _ if status.is_server_error() || status.as_u16() == 429 => Err(
                ProviderError::Transient(anyhow::anyhow!("OpenAI token refresh returned {status}")),
            ),
            _ => Err(ProviderError::Request(anyhow::anyhow!(
                "OpenAI token refresh returned {status}"
            ))),
        };
    }
    response
        .json::<TokenGrant>()
        .await
        .map(Some)
        .map_err(|_| ProviderError::Request(anyhow::anyhow!("Invalid OpenAI token response")))
}

pub(super) async fn access_token(
    path: &Path,
    client: &Client,
    endpoint: &str,
) -> Result<String, ProviderError> {
    let text = tokio::fs::read_to_string(path).await.map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            auth()
        } else {
            e.into()
        }
    })?;
    let session = active(&text)?;
    if session.expires_at > now()?.saturating_add(30) && !session.access_token.is_empty() {
        return Ok(session.access_token);
    }
    let path = path.to_owned();
    let client = client.clone();
    let endpoint = endpoint.to_owned();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        update_credentials(&path, |document| {
            let current = active(&document.to_string())?;
            if current.client_id != session.client_id {
                return Err(auth());
            }
            if current.expires_at > now()?.saturating_add(30) && !current.access_token.is_empty() {
                return Ok(Some(current.access_token));
            }
            let refresh = current
                .refresh_token
                .filter(|t| !t.is_empty())
                .ok_or_else(auth)?;
            let grant = runtime.block_on(refresh_grant(
                &client,
                &endpoint,
                &current.client_id,
                &refresh,
            ))?;

            let table = document["openai"]["chatgpt"]["sessions"]
                .as_array_of_tables_mut()
                .and_then(|entries| {
                    entries.iter_mut().find(|t| {
                        t.get("client_id").and_then(|v| v.as_str()) == Some(&current.client_id)
                    })
                })
                .ok_or_else(auth)?;
            let Some(grant) = grant else {
                table.remove("refresh_token");
                table.remove("access_token");
                return Ok(None);
            };
            if grant.access_token.is_empty()
                || grant.refresh_token.is_empty()
                || grant.expires_in == 0
                || !grant.token_type.eq_ignore_ascii_case("bearer")
            {
                return Err(ProviderError::Request(anyhow::anyhow!(
                    "Incomplete OpenAI token response"
                )));
            }
            let scopes = grant.scope.map_or(current.scopes, |s| {
                s.split_whitespace().map(str::to_owned).collect()
            });
            let permitted = scopes.iter().any(|s| s == "chatgpt.tokens.use.direct");
            table.insert("scopes", value(scopes.into_iter().collect::<Array>()));
            table.insert("access_token", value(&grant.access_token));
            table.insert("refresh_token", value(grant.refresh_token));
            if let Some(id_token) = grant.id_token {
                table.insert("id_token", value(id_token));
            }
            let expiry = i64::try_from(now()?.saturating_add(grant.expires_in))
                .map_err(|e| ProviderError::Request(e.into()))?;
            table.insert("expires_at", value(expiry));
            Ok(permitted.then_some(grant.access_token))
        })
    })
    .await
    .map_err(|e| ProviderError::Request(e.into()))?
    .and_then(|token| token.ok_or_else(auth))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn concurrent_expired_sessions_rotate_the_refresh_token_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("credentials.toml");
        tokio::fs::write(
            &file,
            r#"
[openrouter]
api_key = "preserve"
[openai.chatgpt]
active_client_id = "client"
[[openai.chatgpt.sessions]]
client_id = "client"
access_token = "expired"
refresh_token = "old"
expires_at = 1
scopes = ["chatgpt.tokens.use.direct"]
"#,
        )
        .await
        .unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(body_string_contains("refresh_token=old"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token":"renewed","refresh_token":"rotated","token_type":"Bearer","expires_in":3600})))
            .expect(1).mount(&server).await;
        let client = Client::new();
        let url = server.uri();
        let (a, b) = tokio::join!(
            access_token(&file, &client, &url),
            access_token(&file, &client, &url)
        );
        assert_eq!(a.unwrap(), "renewed");
        assert_eq!(b.unwrap(), "renewed");
        let text = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(
            active(&text).unwrap().refresh_token.as_deref(),
            Some("rotated")
        );
        assert_eq!(
            text.parse::<toml::Value>().unwrap()["openrouter"]["api_key"].as_str(),
            Some("preserve")
        );
    }
}
