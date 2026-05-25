use std::path::{Path, PathBuf};

use crate::context::AppContext;
use crate::error::AftError;
use crate::protocol::{RawRequest, Response};

/// Handle a `trace_to_symbol` request.
///
/// Traces forward from one symbol to another symbol using breadth-first search,
/// returning the first (shortest) resolved call path from origin to target.
///
/// Expects:
/// - `file` (string, required) — path to the source file containing the FROM symbol
/// - `symbol` (string, required) — name of the FROM symbol
/// - `toSymbol` (string, required) — name of the TO symbol
/// - `toFile` (string, optional) — file containing the TO symbol, required when ambiguous
/// - `depth` (number, optional, default 10, max 16) — maximum forward BFS depth
///
/// Returns `TraceToSymbolResult` with fields: `path` (array of hops or null),
/// `complete`, and `reason` when no path is returned.
pub fn handle_trace_to_symbol(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to_symbol: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to_symbol: missing required param 'symbol'",
            );
        }
    };

    let to_symbol = match req.params.get("toSymbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to_symbol: missing required param 'toSymbol'",
            );
        }
    };

    let depth = req
        .params
        .get("depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .min(16) as usize;

    let mut cg_ref = ctx.callgraph().borrow_mut();
    let graph = match cg_ref.as_mut() {
        Some(g) => g,
        None => {
            return Response::error(
                &req.id,
                "not_configured",
                "trace_to_symbol: project not configured — send 'configure' first",
            );
        }
    };

    let file_path = match validate_callgraph_path(req, ctx, file) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let to_file_path = match req.params.get("toFile").and_then(|v| v.as_str()) {
        Some(to_file) => match validate_callgraph_path(req, ctx, to_file) {
            Ok(path) => Some(path),
            Err(resp) => return resp,
        },
        None => None,
    };

    let symbol = match graph.resolve_symbol_query(&file_path, symbol) {
        Ok(symbol) => symbol,
        Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
    };

    let max_files = ctx.config().max_callgraph_files;

    if to_file_path.is_none() {
        match graph.trace_to_symbol_candidates(to_symbol, max_files) {
            Ok(candidates) if candidates.len() > 1 => {
                let candidates_json = serde_json::to_value(&candidates).unwrap_or_default();
                return Response::error_with_data(
                    &req.id,
                    "ambiguous_target",
                    format!(
                        "trace_to_symbol: target symbol '{}' exists in multiple files; pass 'toFile' to disambiguate",
                        to_symbol
                    ),
                    serde_json::json!({ "candidates": candidates_json }),
                );
            }
            Ok(_) => {}
            Err(err @ AftError::ProjectTooLarge { .. }) => {
                return Response::error(&req.id, "project_too_large", format!("{}", err));
            }
            Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
        }
    }

    match graph.trace_to_symbol(
        &file_path,
        &symbol,
        to_symbol,
        to_file_path.as_deref(),
        depth,
        max_files,
    ) {
        Ok(result) => {
            let result_json = serde_json::to_value(&result).unwrap_or_default();
            Response::success(&req.id, result_json)
        }
        Err(err @ AftError::ProjectTooLarge { .. }) => {
            Response::error(&req.id, "project_too_large", format!("{}", err))
        }
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}

fn validate_callgraph_path(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
) -> Result<PathBuf, Response> {
    let file_path = ctx.validate_path(&req.id, Path::new(file))?;

    let project_root = ctx.config().project_root.clone();
    if let Some(project_root) = project_root {
        let canonical_root = std::fs::canonicalize(&project_root).unwrap_or(project_root.clone());
        let input_for_resolution = if file_path.is_relative() {
            project_root.join(&file_path)
        } else {
            file_path.clone()
        };
        let canonical_input =
            std::fs::canonicalize(&input_for_resolution).unwrap_or(input_for_resolution);
        if !canonical_input.starts_with(&canonical_root) {
            return Err(Response::error(
                &req.id,
                "path_outside_project_root",
                format!(
                    "Callgraph operations require paths inside project_root. Got: {} (project_root: {})",
                    file_path.display(),
                    project_root.display(),
                ),
            ));
        }
    }

    Ok(file_path)
}
