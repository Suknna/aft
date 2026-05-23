use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

const MAX_INPUT_BYTES: usize = 1_048_576;

/// Input payload accepted by `bash_write`.
///
/// Two forms:
/// * `String` — verbatim bytes written to the PTY. Backward-compatible with the
///   v0.30 phase 1b shape; existing callers see no change.
/// * `Sequence` — array of items, each either a plain string (text bytes) or a
///   `{ "key": "<name>" }` object that expands to a known control-byte sequence
///   (ESC, arrows, Ctrl chords, function keys, …). Items are concatenated into
///   one atomic write so the PTY sees the whole sequence as one input chunk.
///
/// The agent never has to encode escape characters inside the `input` string —
/// they use named keys instead. The string form remains the right choice when
/// the agent wants to write literal `\u001b` characters (e.g. source code).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BashWriteInput {
    Text(String),
    Sequence(Vec<SequenceItem>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SequenceItem {
    Text(String),
    Key { key: String },
}

#[derive(Debug, Deserialize)]
pub struct BashWriteParams {
    pub task_id: String,
    pub input: BashWriteInput,
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

    let bytes = match expand_input(&params.input) {
        Ok(bytes) => bytes,
        Err(message) => {
            return Response::error(&req.id, "invalid_request", message);
        }
    };

    if bytes.len() > MAX_INPUT_BYTES {
        return Response::error(
            &req.id,
            "input_too_large",
            "bash_write input exceeds 1 MiB limit",
        );
    }

    match ctx
        .bash_background()
        .write_pty(&params.task_id, req.session(), &bytes)
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

fn expand_input(input: &BashWriteInput) -> Result<Vec<u8>, String> {
    match input {
        BashWriteInput::Text(s) => Ok(s.as_bytes().to_vec()),
        BashWriteInput::Sequence(items) => {
            let mut out: Vec<u8> = Vec::with_capacity(items.len() * 4);
            for item in items {
                match item {
                    SequenceItem::Text(s) => out.extend_from_slice(s.as_bytes()),
                    SequenceItem::Key { key } => {
                        let bytes = key_to_bytes(key).ok_or_else(|| {
                            format!(
                                "bash_write: unknown key '{key}'; allowed keys: {}",
                                allowed_keys_hint()
                            )
                        })?;
                        out.extend_from_slice(bytes);
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Map a named key to the byte sequence a terminal sends when that key is pressed.
///
/// Implementation notes:
/// * Names are lowercased and ASCII-only; case-insensitive matching is done by
///   lowercasing the caller-supplied name before lookup.
/// * Control chords `ctrl-a` through `ctrl-z` map programmatically to `0x01..=0x1a`
///   so we don't have to enumerate all 26.
/// * Function keys use the xterm sequence variant (DECFNK / linux-console hybrid)
///   that the vast majority of TUI programs accept.
/// * Arrow / nav keys use the "normal" cursor-key mode sequence (`ESC [ X`)
///   rather than application-keypad mode (`ESC O X`). Programs that toggle
///   application mode (vim with `:set keymodel`) handle both; the normal form
///   is the safer default.
fn key_to_bytes(name: &str) -> Option<&'static [u8]> {
    // Lowercased, hyphen-separated lookup. We allocate only when the input
    // is already non-canonical (rare on the hot path).
    let canonical: std::borrow::Cow<'_, str> = if name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit())
    {
        std::borrow::Cow::Borrowed(name)
    } else {
        std::borrow::Cow::Owned(name.to_ascii_lowercase())
    };

    static TABLE: &[(&str, &[u8])] = &[
        // Line / whitespace
        //
        // ENTER maps to CR (\r, 0x0D) — the byte a real terminal sends when
        // the user presses Enter. Cooked-mode programs (shells, REPLs) have
        // the line discipline translate CR→LF (`icrnl`), so `\r` works for
        // them too. Raw-mode TUIs (opencode TUI, vim insert mode, fzf, htop)
        // see `\r` directly and treat it as submit. LF was wrong for the
        // raw-mode case — opencode TUI would treat `\n` as multi-line input.
        ("enter", b"\r"),
        ("return", b"\r"),
        ("tab", b"\t"),
        ("space", b" "),
        ("backspace", b"\x7f"),
        // Escape
        ("esc", b"\x1b"),
        ("escape", b"\x1b"),
        // Arrows (normal cursor-key mode)
        ("up", b"\x1b[A"),
        ("down", b"\x1b[B"),
        ("right", b"\x1b[C"),
        ("left", b"\x1b[D"),
        // Navigation
        ("home", b"\x1b[H"),
        ("end", b"\x1b[F"),
        ("page-up", b"\x1b[5~"),
        ("page-down", b"\x1b[6~"),
        ("delete", b"\x1b[3~"),
        ("insert", b"\x1b[2~"),
        // Function keys (xterm-style)
        ("f1", b"\x1bOP"),
        ("f2", b"\x1bOQ"),
        ("f3", b"\x1bOR"),
        ("f4", b"\x1bOS"),
        ("f5", b"\x1b[15~"),
        ("f6", b"\x1b[17~"),
        ("f7", b"\x1b[18~"),
        ("f8", b"\x1b[19~"),
        ("f9", b"\x1b[20~"),
        ("f10", b"\x1b[21~"),
        ("f11", b"\x1b[23~"),
        ("f12", b"\x1b[24~"),
    ];

    if let Some((_, bytes)) = TABLE.iter().find(|(n, _)| *n == canonical.as_ref()) {
        return Some(bytes);
    }

    // Ctrl chords: ctrl-a → 0x01 … ctrl-z → 0x1a.
    if let Some(rest) = canonical.strip_prefix("ctrl-") {
        if rest.len() == 1 {
            let c = rest.chars().next().unwrap();
            if c.is_ascii_lowercase() {
                let byte = (c as u8) - b'a' + 1;
                return Some(CTRL_TABLE[byte as usize - 1]);
            }
        }
    }

    None
}

// Pre-materialized byte slices for ctrl-a..ctrl-z so key_to_bytes can return
// `&'static [u8]` without allocating.
static CTRL_TABLE: [&[u8]; 26] = [
    b"\x01", b"\x02", b"\x03", b"\x04", b"\x05", b"\x06", b"\x07", b"\x08", b"\x09", b"\x0a",
    b"\x0b", b"\x0c", b"\x0d", b"\x0e", b"\x0f", b"\x10", b"\x11", b"\x12", b"\x13", b"\x14",
    b"\x15", b"\x16", b"\x17", b"\x18", b"\x19", b"\x1a",
];

fn allowed_keys_hint() -> &'static str {
    "enter, return, tab, space, backspace, esc, escape, up, down, left, right, home, end, \
     page-up, page-down, delete, insert, f1..f12, ctrl-a..ctrl-z"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_form_passes_bytes_through_verbatim() {
        let input = BashWriteInput::Text("hello\n".into());
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, b"hello\n");
    }

    #[test]
    fn text_form_preserves_literal_escape_sequence_chars() {
        // Critical backward-compat case: the agent wants to write the literal
        // 6 characters \u001b (e.g. into source code), NOT an ESC byte.
        let input = BashWriteInput::Text(r"\u001b[31mred\u001b[0m".into());
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, br"\u001b[31mred\u001b[0m");
        // Sanity: byte count matches the literal char count
        // (6 + 4 + 3 + 6 + 3 = 22), not 8 (the JSON-decoded count) and not
        // some auto-stripped variant.
        assert_eq!(bytes.len(), 22);
    }

    #[test]
    fn sequence_form_expands_text_items() {
        let input = BashWriteInput::Sequence(vec![
            SequenceItem::Text("abc".into()),
            SequenceItem::Text("def".into()),
        ]);
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, b"abcdef");
    }

    #[test]
    fn sequence_form_expands_named_keys_to_byte_sequences() {
        let input = BashWriteInput::Sequence(vec![
            SequenceItem::Key { key: "esc".into() },
            SequenceItem::Key { key: "up".into() },
            SequenceItem::Key {
                key: "ctrl-c".into(),
            },
        ]);
        let bytes = expand_input(&input).unwrap();
        // ESC (\x1b) + arrow-up (\x1b[A) + ctrl-c (\x03)
        assert_eq!(bytes, b"\x1b\x1b[A\x03");
    }

    #[test]
    fn sequence_form_mixes_text_and_keys_in_order() {
        // The vim "type some text, exit insert, save+quit" idiom.
        let input = BashWriteInput::Sequence(vec![
            SequenceItem::Text("iHello".into()),
            SequenceItem::Key { key: "esc".into() },
            SequenceItem::Text(":wq".into()),
            SequenceItem::Key {
                key: "enter".into(),
            },
        ]);
        let bytes = expand_input(&input).unwrap();
        // ENTER maps to CR (\r) for raw-mode TUI compatibility, not LF (\n).
        // See key_to_bytes "Line / whitespace" docs for the rationale.
        assert_eq!(bytes, b"iHello\x1b:wq\r");
    }

    #[test]
    fn sequence_form_accepts_case_insensitive_key_names() {
        let input = BashWriteInput::Sequence(vec![
            SequenceItem::Key { key: "ESC".into() },
            SequenceItem::Key {
                key: "Ctrl-C".into(),
            },
        ]);
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, b"\x1b\x03");
    }

    #[test]
    fn sequence_form_unknown_key_returns_error_with_hint() {
        let input = BashWriteInput::Sequence(vec![SequenceItem::Key {
            key: "windows-key".into(),
        }]);
        let err = expand_input(&input).unwrap_err();
        assert!(err.contains("unknown key 'windows-key'"));
        assert!(err.contains("allowed keys:"));
    }

    #[test]
    fn ctrl_chord_table_covers_all_26_letters() {
        for (i, letter) in ('a'..='z').enumerate() {
            let name = format!("ctrl-{letter}");
            let bytes = key_to_bytes(&name).unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(bytes, &[(i as u8) + 1]);
        }
    }

    #[test]
    fn function_keys_use_documented_xterm_sequences() {
        assert_eq!(key_to_bytes("f1"), Some(b"\x1bOP".as_slice()));
        assert_eq!(key_to_bytes("f12"), Some(b"\x1b[24~".as_slice()));
    }

    #[test]
    fn empty_sequence_produces_zero_bytes() {
        let input = BashWriteInput::Sequence(vec![]);
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, b"");
    }

    #[test]
    fn arrows_use_normal_cursor_key_mode_sequence() {
        // ESC [ A/B/C/D form, not ESC O A/B/C/D (application mode).
        assert_eq!(key_to_bytes("up"), Some(b"\x1b[A".as_slice()));
        assert_eq!(key_to_bytes("down"), Some(b"\x1b[B".as_slice()));
        assert_eq!(key_to_bytes("right"), Some(b"\x1b[C".as_slice()));
        assert_eq!(key_to_bytes("left"), Some(b"\x1b[D".as_slice()));
    }
}
