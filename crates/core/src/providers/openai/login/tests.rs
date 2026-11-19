use super::*;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use std::collections::HashMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Browser(tokio::sync::mpsc::UnboundedSender<Url>);
#[async_trait::async_trait]
impl ChatGptSignInUi for Browser {
    async fn wait_for_dismissal(&self, _: &str, url: &str) {
        self.0.send(Url::parse(url).unwrap()).unwrap();
        std::future::pending().await
    }
}

fn signed(claims: Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test".into());
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_der(include_bytes!("test-only-rsa.der")),
    )
    .unwrap()
}
fn claims(nonce: &str) -> Value {
    json!({"sub":"user","aud":"issued","iss":ISSUER,"exp":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()+300,"nonce":nonce})
}
fn keys() -> JwkSet {
    serde_json::from_str(include_str!("test-only-jwks.json")).unwrap()
}

#[test]
fn validates_signed_identity_and_pending_request() {
    let token = signed(claims("original"));
    assert!(verify(&token, &keys(), "issued", "original").is_ok());
    assert!(verify(&token, &keys(), "issued", "different").is_err());
    assert!(verify(&token, &keys(), "other", "original").is_err());
    for (key, value) in [
        ("iss", json!("wrong")),
        ("exp", json!(1)),
        ("sub", json!("")),
        ("azp", json!("other")),
    ] {
        let mut c = claims("original");
        c[key] = value;
        assert!(verify(&signed(c), &keys(), "issued", "original").is_err());
    }
    let mut parts = token.split('.').collect::<Vec<_>>();
    parts[2] = "invalid";
    assert!(verify(&parts.join("."), &keys(), "issued", "original").is_err());
}

#[tokio::test]
async fn browser_callback_exchanges_pkce_and_preserves_openrouter() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credentials.toml");
    tokio::fs::write(&file, "[openrouter]\napi_key='existing'\n")
        .await
        .unwrap();
    let (send, mut prompts) = tokio::sync::mpsc::unbounded_channel();
    let token_endpoint = format!("{}/token", server.uri());
    let jwks_endpoint = format!("{}/keys", server.uri());
    let mut coordinator = ChatGptSignIn::new(Arc::new(Browser(send)));
    coordinator.token_endpoint = token_endpoint.clone();
    coordinator.jwks_endpoint = jwks_endpoint;
    let provider =
        super::super::OpenAiProvider::new(file.clone(), Some(Arc::new(coordinator))).unwrap();
    let (tx, mut events) = tokio::sync::mpsc::channel(4);
    let responses = format!("{}/responses", server.uri());
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(wiremock::matchers::header("authorization", "Bearer usable"))
        .and(wiremock::matchers::body_partial_json(
            json!({"model":"sample","input":[{"role":"user","content":"resume this turn"}]}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let request = crate::providers::LlmRequest::new(
        "sample",
        vec![crate::providers::ChatMessage::user("resume this turn")],
    );
    let flow = provider.execute(&responses, request, &tx);
    let browser = async {
        let url = prompts.recv().await.unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["client_id"], DYNAMIC);
        assert_eq!(params["agent_name_hint"], "TAUQE");
        assert_eq!(params["code_challenge_method"], "S256");
        assert!(params["ext_agent_host_id"].starts_with("urn:uuid:"));
        let redirect = params["redirect_uri"].clone();
        let challenge = params["code_challenge"].clone();
        Mock::given(method("POST")).and(path("/token")).and(move |r:&wiremock::Request|{
            let mut form=Url::parse("https://example.invalid").unwrap();
            form.set_query(Some(std::str::from_utf8(&r.body).unwrap()));
            let values:HashMap<_,_>=form.query_pairs().into_owned().collect();
            values["client_id"]=="issued" && values["code"]=="code" && values["redirect_uri"]==redirect && values["resource"]==RESOURCE && URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256,values["code_verifier"].as_bytes()))==challenge
        }).respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"usable","refresh_token":"rotating","token_type":"Bearer","expires_in":3600,"scope":SCOPE,"id_token":signed(claims(&params["nonce"]))}))).expect(1).mount(&server).await;
        Mock::given(path("/keys"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(include_str!("test-only-jwks.json"), "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut redirect = Url::parse(&params["redirect_uri"]).unwrap();
        redirect.query_pairs_mut().extend_pairs([
            ("state", params["state"].as_str()),
            ("code", "code"),
            ("client_id", "issued"),
        ]);
        assert!(Client::new()
            .get(redirect)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(flow, browser)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(matches!(
        events.recv().await,
        Some(crate::providers::StreamEvent::Done)
    ));
    let saved: toml::Value =
        toml::from_str(&tokio::fs::read_to_string(&file).await.unwrap()).unwrap();
    assert_eq!(saved["openrouter"]["api_key"].as_str(), Some("existing"));
    assert_eq!(
        saved["openai"]["chatgpt"]["active_client_id"].as_str(),
        Some("issued")
    );
    assert_eq!(
        saved["openai"]["chatgpt"]["sessions"][0]["subject"].as_str(),
        Some("user")
    );
    assert_eq!(
        super::super::session::access_token(&file, &Client::new(), &token_endpoint)
            .await
            .unwrap(),
        "usable"
    );
    let registration = prepare(&file).unwrap();
    let returning = Attempt::new().await.unwrap().url(&registration).unwrap();
    let query: HashMap<_, _> = returning.query_pairs().collect();
    assert_eq!(query["client_id"], "issued");
    assert!(!query.contains_key("agent_name_hint"));
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .and(wiremock::matchers::body_string_contains("token=rotating"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    assert!(
        revoke_session(file.clone(), format!("{}/revoke", server.uri()))
            .await
            .unwrap()
    );
    let signed_out: toml::Value =
        toml::from_str(&tokio::fs::read_to_string(&file).await.unwrap()).unwrap();
    let record = &signed_out["openai"]["chatgpt"]["sessions"][0];
    for field in ["access_token", "refresh_token", "id_token"] {
        assert!(record.get(field).is_none());
    }
    assert_eq!(record["client_id"].as_str(), Some("issued"));
    assert_eq!(
        signed_out["openrouter"]["api_key"].as_str(),
        Some("existing")
    );
}

#[tokio::test]
async fn cancelling_shared_sign_in_wakes_every_waiter() {
    struct Dismiss(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl ChatGptSignInUi for Dismiss {
        async fn wait_for_dismissal(&self, _: &str, _: &str) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::task::yield_now().await;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credentials.toml");
    let ui = Arc::new(Dismiss(Default::default()));
    let coordinator = ChatGptSignIn::new(ui.clone());
    let client = Client::new();
    let (a, b) = tokio::join!(
        coordinator.authorize(&file, "one", &client),
        coordinator.authorize(&file, "two", &client)
    );
    assert!(!a.unwrap() && !b.unwrap());
    assert_eq!(ui.0.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(prepare(&file).unwrap().client_id.is_none());
}
