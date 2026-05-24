use crate::bash_background::watches::WatchPattern;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct BashNotifyParams {
    task_id: String,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    regex: Option<String>,
    #[serde(default = "default_once")]
    once: bool,
}

#[derive(Debug, Deserialize)]
struct BashUnnotifyParams {
    task_id: String,
    watch_id: String,
}

fn default_once() -> bool {
    true
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashNotifyParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_notify: invalid params: {e}"),
            );
        }
    };

    let pattern = if let Some(regex) = params.regex.as_deref() {
        match WatchPattern::regex(regex) {
            Ok(pattern) => pattern,
            Err(e) => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!("bash_notify: invalid regex: {e}"),
                );
            }
        }
    } else if let Some(pattern) = params.pattern {
        WatchPattern::Substring(pattern)
    } else {
        return Response::error(
            &req.id,
            "invalid_request",
            "bash_notify: missing pattern or regex",
        );
    };

    match ctx
        .bash_background()
        .register_watch(params.task_id.clone(), pattern, params.once)
    {
        Ok(watch_id) => Response::success(&req.id, json!({ "watch_id": watch_id })),
        Err("too_many_watches") => Response::error(
            &req.id,
            "too_many_watches",
            format!("Too many active watches on task {} (max 8)", params.task_id),
        ),
        Err(error) => Response::error(&req.id, "invalid_request", error),
    }
}

pub fn handle_unnotify(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashUnnotifyParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_unnotify: invalid params: {e}"),
            );
        }
    };
    ctx.bash_background()
        .unregister_watch(&params.task_id, &params.watch_id);
    Response::success(&req.id, json!({ "unregistered": true }))
}
