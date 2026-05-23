use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

const MAX_INPUT_BYTES: usize = 1_048_576;

#[derive(Debug, Deserialize)]
pub struct BashWriteParams {
    pub task_id: String,
    pub input: String,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashWriteParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_write: invalid params: {e}"),
            );
        }
    };

    if params.input.len() > MAX_INPUT_BYTES {
        return Response::error(
            &req.id,
            "input_too_large",
            "bash_write input exceeds 1 MiB limit",
        );
    }

    match ctx
        .bash_background()
        .write_pty(&params.task_id, req.session(), params.input.as_bytes())
    {
        Ok(bytes_written) => Response::success(&req.id, json!({ "bytes_written": bytes_written })),
        Err(code) if code == "task_not_found" => Response::error(
            &req.id,
            "task_not_found",
            format!("background task not found: {}", params.task_id),
        ),
        Err(code) if code == "task_not_pty" => Response::error(
            &req.id,
            "task_not_pty",
            format!("background task is not a PTY task: {}", params.task_id),
        ),
        Err(code) if code == "task_exited" => Response::error(
            &req.id,
            "task_exited",
            format!("PTY task is no longer running: {}", params.task_id),
        ),
        Err(message) => Response::error(&req.id, "write_failed", message),
    }
}
