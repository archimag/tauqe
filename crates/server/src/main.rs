use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use workbench_protocol::{
    methods, InitializeResult, Request, RequestId, Response, ResponseError, PROTOCOL_VERSION,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    workbench_core::init();

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(err) => {
                let err_resp = Response {
                    id: RequestId::Number(0),
                    result: None,
                    error: Some(ResponseError {
                        code: "PARSE_ERROR".to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                };
                let resp_str = serde_json::to_string(&err_resp)? + "\n";
                stdout.write_all(resp_str.as_bytes()).await?;
                stdout.flush().await?;
                continue;
            }
        };

        let response = handle_request(request).await;
        let resp_str = serde_json::to_string(&response)? + "\n";
        stdout.write_all(resp_str.as_bytes()).await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn handle_request(req: Request) -> Response {
    match req.method.as_str() {
        methods::CLIENT_INITIALIZE => {
            let repo_state = workbench_core::git::get_repository_state(None);
            let result = InitializeResult {
                protocol_version: PROTOCOL_VERSION.to_string(),
                server_name: "workbench-server".to_string(),
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                repository: Some(repo_state),
            };
            Response {
                id: req.id,
                result: Some(serde_json::to_value(result).unwrap()),
                error: None,
            }
        }
        methods::REPOSITORY_GET_STATE => {
            let repo_state = workbench_core::git::get_repository_state(None);
            Response {
                id: req.id,
                result: Some(serde_json::to_value(repo_state).unwrap()),
                error: None,
            }
        }
        _ => Response {
            id: req.id,
            result: None,
            error: Some(ResponseError {
                code: "METHOD_NOT_FOUND".to_string(),
                message: format!("Method '{}' not found", req.method),
                data: None,
            }),
        },
    }
}
