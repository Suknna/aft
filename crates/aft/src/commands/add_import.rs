//! Handler for the `add_import` command: add an import statement to a file.
//!
//! Analyzes existing imports, checks for duplicates, finds the correct
//! insertion point based on group and alphabetical ordering, and inserts
//! the new import with auto-backup and syntax validation.

use std::path::Path;

use crate::context::AppContext;
use crate::edit;
use crate::imports;
use crate::parser::{detect_language, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `add_import` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `module` (string, required) — the module path (e.g., "react", "./utils")
///   - `names` (array of strings, optional) — named imports (e.g., ["useState", "useEffect"])
///   - `default_import` (string, optional) — default import name (e.g., "React")
///   - `type_only` (bool, optional, default false) — whether this is a type-only import
///
/// Returns: `{ file, added, module, group, already_present?, syntax_valid?, backup_id? }`
pub fn handle_add_import(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_import: missing required param 'file'",
            );
        }
    };

    let module = match req.params.get("module").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_import: missing required param 'module'",
            );
        }
    };

    let names: Vec<String> = req
        .params
        .get("names")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let default_import = req
        .params
        .get("default_import")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let type_only = req
        .params
        .get("type_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Namespace import (`* as ns`) and whole-module alias — used by engines
    // that support them (ES namespace; Solidity namespace + whole-file alias).
    let namespace = req
        .params
        .get("namespace")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let alias = req
        .params
        .get("alias")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let modifiers: Vec<String> = req
        .params
        .get("modifiers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let import_kind = req
        .params
        .get("import_kind")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // --- Validate ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("add_import: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "unsupported_language",
                format!(
                    "add_import: unsupported file extension: {}",
                    path.extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("<none>")
                ),
            );
        }
    };

    if !imports::is_supported(lang) {
        return Response::error(
            &req.id,
            "unsupported_language",
            format!(
                "add_import: import management not yet supported for {:?}",
                lang
            ),
        );
    }

    // Must have at least one of: names, default_import, or neither (side-effect)
    // All combinations are valid.

    // --- Parse file and imports ---
    let (source, tree, block) = match imports::parse_file_imports(&path, lang) {
        Ok(result) => result,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Check for duplicates ---
    if imports::is_duplicate(&block, module, &names, default_import.as_deref(), type_only) {
        log::debug!("add_import: {} (already present)", file);
        return Response::success(
            &req.id,
            serde_json::json!({
                "file": file,
                "added": false,
                "module": module,
                "already_present": true,
            }),
        );
    }

    // --- Determine group ---
    let group = imports::classify_group(lang, module);

    // --- Try to merge into an existing same-module, same-kind named import ---
    //
    // When adding `{ baz }` to a file that already has `import { foo } from "lib"`,
    // the historical behavior inserted a second `import { baz } from "lib"` line
    // (TS/JS, Rust, Py). That produces duplicate imports the linter then complains
    // about, and the agent has to call `organize` afterwards to clean up.
    //
    // Instead, when the target module already has a value/type import statement of
    // the matching kind with named specifiers, merge `names` into that statement's
    // existing names (deduped + sorted) and replace its byte range. This only
    // applies to languages where named imports are a list inside one statement —
    // Go's "import (...)" block is handled separately by the insertion path.
    let target_kind = if type_only {
        imports::ImportKind::Type
    } else {
        imports::ImportKind::Value
    };
    let merge_target =
        if !names.is_empty() && default_import.is_none() && !matches!(lang, LangId::Go) {
            block.imports.iter().find(|imp| {
                imp.module_path == module
                    && imp.kind == target_kind
                    && imp.namespace_import.is_none()
                    && imp.default_import.is_none()
                    && !imp.names.is_empty()
            })
        } else {
            None
        };

    let (insert_offset, replace_end, insert_text, merged_into_existing) = if let Some(existing) =
        merge_target
    {
        // Build the merged named-import list: union of existing + new, sorted.
        let mut merged_names: Vec<String> = existing.names.clone();
        for name in &names {
            if !merged_names
                .iter()
                .any(|n| imports::specifier_matches(n, name))
            {
                merged_names.push(name.clone());
            }
        }
        // Sort for deterministic output (matches generate_import_line behavior).
        merged_names
            .sort_by(|a, b| imports::specifier_local_name(a).cmp(imports::specifier_local_name(b)));

        let merged_line = imports::generate_import_line(
            lang,
            &existing.module_path,
            &merged_names,
            None,
            type_only,
        );
        (
            existing.byte_range.start,
            existing.byte_range.end,
            merged_line,
            true,
        )
    } else {
        // Fall through to the original "find insertion point and insert a new
        // statement" behavior.
        let (insert_offset, needs_blank_before, needs_blank_after) =
            imports::find_insertion_point(&source, &block, group, module, type_only);

        // For Go, check if we're inserting into a grouped import block
        let import_line = if matches!(lang, LangId::Go) {
            let in_group = imports::go_has_grouped_import(&source, &tree).is_some();
            imports::generate_go_import_line_pub(module, default_import.as_deref(), in_group)
        } else {
            imports::generate_import(
                lang,
                &imports::ImportRequest {
                    module_path: module,
                    names: &names,
                    default_import: default_import.as_deref(),
                    namespace: namespace.as_deref(),
                    alias: alias.as_deref(),
                    type_only,
                    modifiers: &modifiers,
                    import_kind: import_kind.as_deref(),
                },
            )
        };

        // Build the text to insert
        let mut insert_text = String::new();
        if needs_blank_before {
            insert_text.push('\n');
        }
        insert_text.push_str(&import_line);
        insert_text.push('\n');
        if needs_blank_after {
            insert_text.push('\n');
        }
        (insert_offset, insert_offset, insert_text, false)
    };

    let _ = merged_into_existing;

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "add_import: pre-edit backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Insert (or replace, when merging) ---
    let new_source =
        match edit::replace_byte_range(&source, insert_offset, replace_end, &insert_text) {
            Ok(s) => s,
            Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
        };

    // --- Write, format, and validate ---
    let mut write_result =
        match edit::write_format_validate(&path, &new_source, &ctx.config(), &req.params) {
            Ok(r) => r,
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        };

    if let Ok(final_content) = std::fs::read_to_string(&path) {
        write_result.lsp_outcome = ctx.lsp_post_write(&path, &final_content, &req.params);
    }

    log::debug!("add_import: {}", file);

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "added": true,
        "module": module,
        "group": group.label(),
        "formatted": write_result.formatted,
    });

    if let Some(valid) = write_result.syntax_valid {
        result["syntax_valid"] = serde_json::json!(valid);
    }

    if let Some(ref reason) = write_result.format_skipped_reason {
        result["format_skipped_reason"] = serde_json::json!(reason);
    }

    if write_result.validate_requested {
        result["validation_errors"] = serde_json::json!(write_result.validation_errors);
    }
    if let Some(ref reason) = write_result.validate_skipped_reason {
        result["validate_skipped_reason"] = serde_json::json!(reason);
    }

    if let Some(ref id) = backup_id {
        result["backup_id"] = serde_json::json!(id);
    }

    write_result.append_lsp_diagnostics_to(&mut result);
    Response::success(&req.id, result)
}
