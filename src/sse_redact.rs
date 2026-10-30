// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! What a client receives from a provider's event stream.
//!
//! A provider can send an error inside a stream that began with `200`, and its
//! message can name the deployment's cloud project, region and resource path.
//! The relay therefore forwards only events it can positively classify as safe,
//! and fails closed on everything else:
//!
//! - **Parsing follows the SSE specification**: a line ends at CR, LF or
//!   CRLF; an event's `data:` lines are joined with `\n`; a blank line ends
//!   the event. Classifying single lines would let an error split over two
//!   `data:` lines through.
//! - **Only `data:` and `event:` lines are kept.** Comments, `id:`, `retry:`
//!   and unknown fields are dropped (neither provider's documented streams use
//!   `id:` or `retry:`, and a comment is only a keepalive).
//! - **A safe event** has no data, or data that is `[DONE]` (OpenAI), or one
//!   JSON object that parses strictly and is not an error: no top-level `error`
//!   that is not `null`, and (Anthropic) no `"type": "error"`. Text in any
//!   other field of a safe event, such as generated output, is relayed. Its
//!   kept lines are forwarded with `\n` endings, and raw U+2028, U+2029 and
//!   U+0085 inside it are escaped, so a client that also splits lines on those
//!   (some older SDK versions do) reads the same event.
//! - **Anything else ends the client's stream**: an error event, an event that
//!   cannot be classified (plain text, `NaN`, an array, invalid UTF-8), an
//!   unknown event name, a repeated `event:` line, or an event over
//!   [`MAX_EVENT_BYTES`]. The client gets one fixed error event and nothing
//!   more. The relay keeps reading and metering the provider's stream to its
//!   end even if the client then disconnects, so what is charged does not
//!   change.
//!
//! The meters see the provider's original bytes before this runs; nothing here
//! affects metering.

use bytes::Bytes;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::json;

use crate::provider::{RefusalDetail, ResponseMeta, clean_token};

/// The largest event the relay will hold before forwarding it, in bytes of
/// its lines. Larger than any event the relay is expected to see, and the
/// bound on what it holds per stream; an event over it ends the client's
/// stream.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Event names Anthropic documents for Messages streaming
/// (platform.claude.com/docs/en/build-with-claude/streaming). Tool use,
/// thinking, citations and server tools use these names, with their variants
/// inside the data. Anthropic says new event types may be added: an unknown
/// name ends the stream (fail closed) and is logged with its name.
const ANTHROPIC_EVENTS: [&str; 8] = [
    "message_start",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
    "ping",
    "error",
];

/// Anthropic error types that are passed on inside a stream: they describe the
/// shared upstream's load or latency, not the caller or the deployment's
/// account. Every other type becomes `api_error`.
const ANTHROPIC_KEPT_ERROR_TYPES: [&str; 3] =
    ["overloaded_error", "rate_limit_error", "timeout_error"];

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Which protocol the stream speaks, which decides what an error looks like
/// and what the client's replacement looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamSurface {
    Anthropic,
    OpenAi,
}

/// Why the client's stream was ended early, for the log and `stream settled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The provider sent an error event.
    ErrorEvent,
    /// An event could not be classified as safe.
    Unclassifiable,
    /// An event name outside the protocol's known set.
    UnknownEventName,
    /// An event over [`MAX_EVENT_BYTES`].
    OverLimit,
}

impl Ended {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ErrorEvent => "error_event",
            Self::Unclassifiable => "unclassifiable",
            Self::UnknownEventName => "unknown_event_name",
            Self::OverLimit => "over_limit",
        }
    }
}

/// The parts of an event's JSON data that classification reads. Unknown
/// fields are ignored; a repeated `type` or `error` key is rejected, so parsers
/// that disagree about duplicates cannot be played against each other.
/// `Option` maps both a missing key and JSON `null` to `None`, so "an error"
/// means a top-level `error` that is present and not `null`.
#[derive(Deserialize)]
struct Probe {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    error: Option<IgnoredAny>,
}

enum Verdict {
    /// Forward the event; `end_marker` says nothing after it counts.
    Safe {
        end_marker: bool,
    },
    End(Ended),
}

/// Filters one provider stream for one client. See the module documentation.
pub struct SseRedactor {
    surface: StreamSurface,
    request_id: String,
    provider: &'static str,
    model: String,

    /// The current, unterminated line.
    line: Vec<u8>,
    /// The previous byte was CR, so an LF now is part of the same line end.
    after_cr: bool,
    first_line: bool,

    /// The current event's kept lines, each ending in `\n`.
    kept: Vec<u8>,
    /// The current event's data, joined as the SSE specification says.
    data: Vec<u8>,
    has_data: bool,
    /// The event's `event:` name. A second `event:` line makes the event
    /// unsafe (clients take the last one, and earlier ones would be forwarded
    /// unchecked).
    name: Option<String>,
    /// An `event:` line whose value was not a plain token.
    bad_name: bool,
    /// Bytes of every line of the current event so far.
    event_bytes: usize,

    /// A comment (an upstream keepalive) was dropped since the last check.
    keepalive_due: bool,
    /// Why the client's stream was ended, once it has been.
    ended: Option<Ended>,
    /// The provider's end marker was forwarded; nothing after it counts.
    finished: bool,
}

impl SseRedactor {
    #[must_use]
    pub fn new(
        surface: StreamSurface,
        request_id: impl Into<String>,
        provider: &'static str,
        model: impl Into<String>,
    ) -> Self {
        Self {
            surface,
            request_id: request_id.into(),
            provider,
            model: model.into(),
            line: Vec::new(),
            after_cr: false,
            first_line: true,
            kept: Vec::new(),
            data: Vec::new(),
            has_data: false,
            name: None,
            bad_name: false,
            event_bytes: 0,
            keepalive_due: false,
            ended: None,
            finished: false,
        }
    }

    /// Why the client's stream was ended early, if it was.
    #[must_use]
    pub fn ended(&self) -> Option<Ended> {
        self.ended
    }

    /// Whether an upstream keepalive was dropped and no output has been
    /// produced since (output is traffic enough).
    pub fn take_keepalive(&mut self) -> bool {
        std::mem::take(&mut self.keepalive_due)
    }

    /// The client's stream is over: ended early, or the end marker was sent.
    /// Nothing more is forwarded.
    #[must_use]
    pub fn silent(&self) -> bool {
        self.ended.is_some() || self.finished
    }

    /// Feed provider bytes; returns what may be forwarded to the client: every
    /// event completed by these bytes that is safe, or the one fixed error event
    /// that ends the client's stream. An incomplete event is held.
    pub fn push(&mut self, chunk: &[u8]) -> Bytes {
        let mut out = Vec::new();
        for &b in chunk {
            if self.silent() {
                break;
            }
            if std::mem::take(&mut self.after_cr) && b == b'\n' {
                continue;
            }
            match b {
                b'\r' => {
                    self.end_line(&mut out);
                    self.after_cr = true;
                }
                b'\n' => self.end_line(&mut out),
                _ => {
                    self.line.push(b);
                    // The line's terminator counts too.
                    if self.event_bytes + self.line.len() + 1 > MAX_EVENT_BYTES {
                        self.end_stream(&mut out, Ended::OverLimit);
                    }
                }
            }
        }
        if !out.is_empty() {
            // Output was produced, so the client is seeing traffic anyway.
            self.keepalive_due = false;
        }
        Bytes::from(out)
    }

    /// The provider's stream ended (its end marker was metered, or it closed).
    /// The held event, including an unterminated last line, is classified as
    /// usual and, if safe, forwarded with its blank line added, so the client
    /// still dispatches it (an unterminated `data: [DONE]` included).
    pub fn end(&mut self) -> Bytes {
        let mut out = Vec::new();
        if !self.silent() {
            self.after_cr = false;
            if !self.line.is_empty() {
                self.end_line(&mut out);
            }
            if !self.silent() {
                self.end_event(&mut out);
            }
        }
        Bytes::from(out)
    }

    fn end_line(&mut self, out: &mut Vec<u8>) {
        let mut line = std::mem::take(&mut self.line);
        if std::mem::take(&mut self.first_line) && line.starts_with(BOM) {
            line.drain(..BOM.len());
        }
        if line.is_empty() {
            self.end_event(out);
            return;
        }
        self.event_bytes += line.len() + 1;
        if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            if self.has_data {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(value);
            self.has_data = true;
            self.keep(&line);
        } else if let Some(value) = line.strip_prefix(b"event:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            // A second `event:` line: the documented streams have one per event,
            // and every kept line is forwarded, so an earlier name would reach
            // the client unchecked.
            if self.name.is_some() {
                self.bad_name = true;
            }
            match std::str::from_utf8(value)
                .ok()
                .and_then(|v| clean_token(v, 64))
            {
                Some(name) => self.name = Some(name),
                None => self.bad_name = true,
            }
            self.keep(&line);
        } else if line.starts_with(b":") {
            // A comment: an upstream keepalive. Dropped; the relay may send its
            // own fixed one in its place.
            self.keepalive_due = true;
        }
        // `id:`, `retry:` and unknown fields are dropped.
    }

    fn keep(&mut self, line: &[u8]) {
        // In a safe event, raw U+2028, U+2029 and U+0085 can only sit inside a
        // JSON string (event names are ASCII tokens, `[DONE]` is matched
        // without them, and anywhere else in JSON they fail the parse), where
        // the escaped form means the same; escaped, no client can split a line
        // on them.
        let mut rest = line;
        while !rest.is_empty() {
            let (escape, len): (&[u8], usize) = if rest.starts_with(b"\xE2\x80\xA8") {
                (&b"\\u2028"[..], 3)
            } else if rest.starts_with(b"\xE2\x80\xA9") {
                (&b"\\u2029"[..], 3)
            } else if rest.starts_with(b"\xC2\x85") {
                (&b"\\u0085"[..], 2)
            } else {
                (&rest[..1], 1)
            };
            self.kept.extend_from_slice(escape);
            rest = &rest[len..];
        }
        self.kept.push(b'\n');
    }

    fn end_event(&mut self, out: &mut Vec<u8>) {
        if !self.kept.is_empty() {
            match self.classify() {
                Verdict::Safe { end_marker } => {
                    out.extend_from_slice(&self.kept);
                    out.push(b'\n');
                    self.finished = end_marker;
                }
                Verdict::End(why) => self.end_stream(out, why),
            }
        }
        self.kept.clear();
        self.data.clear();
        self.has_data = false;
        self.name = None;
        self.bad_name = false;
        self.event_bytes = 0;
    }

    fn classify(&self) -> Verdict {
        if self.bad_name {
            return Verdict::End(Ended::Unclassifiable);
        }
        if let Some(name) = self.name.as_deref() {
            if name == "error" {
                return Verdict::End(Ended::ErrorEvent);
            }
            // Neither OpenAI nor Vertex names its events, and a name changes
            // how an OpenAI client dispatches.
            if self.surface == StreamSurface::OpenAi || !ANTHROPIC_EVENTS.contains(&name) {
                return Verdict::End(Ended::UnknownEventName);
            }
        }
        if !self.has_data {
            return Verdict::Safe { end_marker: false };
        }
        let Ok(data) = std::str::from_utf8(&self.data) else {
            return Verdict::End(Ended::Unclassifiable);
        };
        // Spaces and tabs only: a Unicode space around the marker is not one.
        if self.surface == StreamSurface::OpenAi && data.trim_matches([' ', '\t']) == "[DONE]" {
            return Verdict::Safe { end_marker: true };
        }
        // One JSON object. serde's struct deserialiser also accepts an array,
        // so the opening brace is checked first.
        if !data
            .trim_start_matches([' ', '\t', '\n', '\r'])
            .starts_with('{')
        {
            return Verdict::End(Ended::Unclassifiable);
        }
        let Ok(probe) = serde_json::from_str::<Probe>(data) else {
            return Verdict::End(Ended::Unclassifiable);
        };
        let kind = probe.kind.as_deref();
        if probe.error.is_some()
            || (self.surface == StreamSurface::Anthropic && kind == Some("error"))
        {
            return Verdict::End(Ended::ErrorEvent);
        }
        // The end marker is the `message_stop` event itself, so the client sees
        // the same event the relay stops on.
        let named_stop = self.name.as_deref().is_none_or(|n| n == "message_stop");
        Verdict::Safe {
            end_marker: self.surface == StreamSurface::Anthropic
                && kind == Some("message_stop")
                && named_stop,
        }
    }

    /// Forward the fixed error event, log once, and forward nothing more.
    fn end_stream(&mut self, out: &mut Vec<u8>, why: Ended) {
        // Read leniently, for the log and the Anthropic type only.
        let detail = RefusalDetail::new(&ResponseMeta::default(), &self.data, true);
        let provider_type = detail.error_type.as_deref().unwrap_or("");
        tracing::warn!(
            request_id = %self.request_id,
            provider = self.provider,
            model = %self.model,
            reason = why.as_str(),
            event_name = self.name.as_deref().unwrap_or(""),
            provider_error_type = provider_type,
            provider_error_code = detail.error_code.as_deref().unwrap_or(""),
            "upstream stream error; ending the client's stream and metering on"
        );
        tracing::trace!(
            target: "tollgate::upstream_body",
            request_id = %self.request_id,
            body = %detail.body,
            "upstream stream error body"
        );
        out.extend_from_slice(&self.replacement(provider_type));
        self.ended = Some(why);
        // Nothing more is forwarded; release what the event held.
        self.line = Vec::new();
        self.kept = Vec::new();
        self.data = Vec::new();
    }

    fn replacement(&self, provider_type: &str) -> Vec<u8> {
        let id = &self.request_id;
        match self.surface {
            StreamSurface::Anthropic => {
                let kind = if ANTHROPIC_KEPT_ERROR_TYPES.contains(&provider_type) {
                    provider_type
                } else {
                    "api_error"
                };
                let message = if kind == "rate_limit_error" {
                    format!(
                        "{} (request id {id})",
                        crate::gateway::UPSTREAM_RATE_LIMITED
                    )
                } else {
                    format!("upstream error (request id {id})")
                };
                let body = json!({"type": "error", "error": {"type": kind, "message": message}});
                format!("event: error\ndata: {body}\n\n").into_bytes()
            }
            StreamSurface::OpenAi => {
                let body = json!({"error": {
                    "message": format!("upstream provider error (request id {id})"),
                    "type": "upstream_error",
                }});
                format!("data: {body}\n\n").into_bytes()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENTINEL: &str = "projects/sse-sentinel-9/locations/us-east5";

    fn anthropic() -> SseRedactor {
        SseRedactor::new(StreamSurface::Anthropic, "rid-1", "anthropic", "m")
    }
    fn openai() -> SseRedactor {
        SseRedactor::new(StreamSurface::OpenAi, "rid-1", "openai", "m")
    }

    /// The whole stream through the redactor in one push, then `end()`.
    fn run(mut r: SseRedactor, input: &[u8]) -> (Vec<u8>, Option<Ended>) {
        let mut out = r.push(input).to_vec();
        out.extend_from_slice(&r.end());
        (out, r.ended())
    }

    /// The same, one byte at a time.
    fn run_bytewise(mut r: SseRedactor, input: &[u8]) -> (Vec<u8>, Option<Ended>) {
        let mut out = Vec::new();
        for b in input {
            out.extend_from_slice(&r.push(std::slice::from_ref(b)));
        }
        out.extend_from_slice(&r.end());
        (out, r.ended())
    }

    /// A client parser written to the SSE specification and the Python SDKs:
    /// lines end at CR, LF or CRLF; `data:` lines join with `\n`; a blank line
    /// dispatches; the last `event:` wins; anything else is ignored. Returns
    /// each dispatched (event name, data).
    fn oracle(bytes: &[u8]) -> Vec<(String, String)> {
        let text = String::from_utf8_lossy(bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let mut events = Vec::new();
        let (mut name, mut data, mut any) = (String::new(), Vec::<String>::new(), false);
        let mut lines = Vec::new();
        let mut cur = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    lines.push(std::mem::take(&mut cur));
                }
                '\n' => lines.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            }
        }
        for line in lines {
            if line.is_empty() {
                if any {
                    events.push((std::mem::take(&mut name), data.join("\n")));
                    data.clear();
                    any = false;
                }
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f.to_owned(), v.strip_prefix(' ').unwrap_or(v).to_owned()),
                None => (line.clone(), String::new()),
            };
            match field.as_str() {
                "data" => {
                    data.push(value);
                    any = true;
                }
                "event" => {
                    name = value;
                    any = true;
                }
                _ => {}
            }
        }
        events
    }

    /// Nothing a client dispatches may contain the sentinel, whole or one byte
    /// at a time.
    fn assert_no_leak(make: fn() -> SseRedactor, input: &[u8]) {
        for (label, (out, _)) in [
            ("whole", run(make(), input)),
            ("bytewise", run_bytewise(make(), input)),
        ] {
            for (name, data) in oracle(&out) {
                assert!(
                    !data.contains(SENTINEL) && !name.contains(SENTINEL),
                    "{label}: leaked in {name:?} {data:?} from {:?}",
                    String::from_utf8_lossy(input)
                );
            }
            assert!(
                !String::from_utf8_lossy(&out).contains(SENTINEL),
                "{label}: sentinel in raw output"
            );
        }
    }

    const ANTHROPIC_OK: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\
\n\
event: ping\n\
data: {\"type\": \"ping\"}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"the word error is fine here\"}}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";

    const OPENAI_OK: &str = "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\
\n\
data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\
\n\
data: [DONE]\n\
\n";

    #[test]
    fn normal_streams_are_forwarded_byte_for_byte() {
        for (make, input) in [
            (anthropic as fn() -> SseRedactor, ANTHROPIC_OK),
            (openai as fn() -> SseRedactor, OPENAI_OK),
        ] {
            let (out, ended) = run(make(), input.as_bytes());
            assert_eq!(String::from_utf8(out).unwrap(), input);
            assert_eq!(ended, None);
            let (out, ended) = run_bytewise(make(), input.as_bytes());
            assert_eq!(String::from_utf8(out).unwrap(), input);
            assert_eq!(ended, None);
        }
    }

    #[test]
    fn crlf_and_cr_streams_are_re_terminated_with_lf() {
        let crlf = OPENAI_OK.replace('\n', "\r\n");
        let cr = OPENAI_OK.replace('\n', "\r");
        for input in [crlf, cr] {
            let (out, ended) = run(openai(), input.as_bytes());
            assert_eq!(String::from_utf8(out).unwrap(), OPENAI_OK);
            assert_eq!(ended, None);
        }
    }

    #[test]
    fn a_bom_comments_ids_and_retries_are_dropped() {
        let input = format!(
            "\u{feff}: keepalive {SENTINEL}\n\nid: {SENTINEL}\nretry: 1\nfoo: {SENTINEL}\n{OPENAI_OK}"
        );
        let (out, ended) = run(openai(), input.as_bytes());
        assert_eq!(String::from_utf8(out).unwrap(), OPENAI_OK);
        assert_eq!(ended, None);
    }

    #[test]
    fn every_hostile_stream_is_ended_without_the_providers_text() {
        let s = SENTINEL;
        let anthropic_cases: Vec<String> = vec![
            // The documented shape.
            format!(
                "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"{s}\"}}}}\n\n"
            ),
            // Plain text under `event: error`: the SDK raises with it.
            format!("event: error\ndata: {s}\n\n"),
            // Ping-shaped JSON under `event: error`.
            format!("event: error\ndata: {{\"type\":\"ping\",\"note\":\"{s}\"}}\n\n"),
            // The last `event:` wins.
            format!("event: ping\nevent: error\ndata: {{\"type\":\"ping\",\"x\":\"{s}\"}}\n\n"),
            // An error object split over two data lines.
            format!(
                "event: message_delta\ndata: {{\"type\":\"error\",\ndata: \"error\":{{\"message\":\"{s}\"}}}}\n\n"
            ),
            // Bare CR line endings.
            format!(
                "event: error\rdata: {{\"type\":\"error\",\"error\":{{\"message\":\"{s}\"}}}}\r\r"
            ),
            // A top-level error key without the type.
            format!(
                "event: message_delta\ndata: {{\"type\":\"message_delta\",\"error\":\"{s}\"}}\n\n"
            ),
            // Unknown event name.
            format!("event: provider_note\ndata: {{\"type\":\"x\",\"text\":\"{s}\"}}\n\n"),
            // An unparseable payload.
            format!(
                "event: message_delta\ndata: {{\"type\":\"message_delta\",\"x\":NaN,\"m\":\"{s}\"}}\n\n"
            ),
            // Duplicate keys.
            format!(
                "event: message_delta\ndata: {{\"type\":\"error\",\"type\":\"ping\",\"m\":\"{s}\"}}\n\n"
            ),
            // An array.
            format!("event: message_delta\ndata: [\"{s}\",null]\n\n"),
            format!(
                "event: message_delta\ndata: [\"message_stop\",null]\n\ndata: {{\"m\":\"{s}\"}}\n\n"
            ),
            // A name that is not a plain token.
            format!("event: {s}\ndata: {{\"type\":\"ping\"}}\n\n"),
        ];
        for case in &anthropic_cases {
            assert_no_leak(anthropic, case.as_bytes());
            let (_, ended) = run(anthropic(), case.as_bytes());
            assert!(ended.is_some(), "not ended: {case:?}");
        }

        let openai_cases: Vec<String> = vec![
            format!("data: {{\"error\":{{\"message\":\"{s}\",\"type\":\"server_error\"}}}}\n\n"),
            format!("data: {{\"error\":\"{s}\"}}\n\n"),
            format!("data: [{{\"error\":{{\"message\":\"{s}\"}}}}]\n\n"),
            format!("data: [{{}},{{\"error\":{{\"message\":\"{s}\"}}}}]\n\n"),
            format!("data: [DONE] {s}\n\n"),
            format!("event: error\ndata: {{\"m\":\"{s}\"}}\n\n"),
            format!("event: thread.message\ndata: {{\"m\":\"{s}\"}}\n\n"),
            format!("data: {{\"error\":{{\"message\":\"{s}\"}},\"x\":Infinity}}\n\n"),
            format!("data: {{\"choices\":[],\ndata: \"error\":{{\"message\":\"{s}\"}}}}\n\n"),
        ];
        for case in &openai_cases {
            assert_no_leak(openai, case.as_bytes());
            let (_, ended) = run(openai(), case.as_bytes());
            assert!(ended.is_some(), "not ended: {case:?}");
        }

        // Invalid UTF-8 inside a string.
        let mut bad = b"data: {\"m\":\"".to_vec();
        bad.extend_from_slice(&[0xff, 0xfe]);
        bad.extend_from_slice(format!("{s}\"}}\n\n").as_bytes());
        assert_no_leak(openai, &bad);
    }

    #[test]
    fn the_client_gets_one_fixed_error_event_and_nothing_more() {
        let input = format!(
            "{}data: {{\"error\":{{\"message\":\"{SENTINEL}\"}}}}\n\n{}",
            &OPENAI_OK[..OPENAI_OK.find("data: [DONE]").unwrap()],
            "data: {\"choices\":[{\"delta\":{\"content\":\"after\"}}]}\n\ndata: [DONE]\n\n"
        );
        let (out, ended) = run(openai(), input.as_bytes());
        assert_eq!(ended, Some(Ended::ErrorEvent));
        let events = oracle(&out);
        let last = events.last().expect("an event");
        let v: serde_json::Value = serde_json::from_str(&last.1).unwrap();
        assert_eq!(v["error"]["type"], "upstream_error");
        assert!(v["error"]["message"].as_str().unwrap().contains("rid-1"));
        // Nothing after the error: not the later delta, not [DONE].
        assert!(!String::from_utf8_lossy(&out).contains("after"));
        assert!(!String::from_utf8_lossy(&out).contains("[DONE]"));
    }

    #[test]
    fn anthropic_keeps_only_load_error_types() {
        for (provider_type, want) in [
            ("overloaded_error", "overloaded_error"),
            ("rate_limit_error", "rate_limit_error"),
            ("timeout_error", "timeout_error"),
            ("invalid_request_error", "api_error"),
            ("authentication_error", "api_error"),
            ("permission_error", "api_error"),
            ("billing_error", "api_error"),
            ("not_found_error", "api_error"),
            ("something_new", "api_error"),
        ] {
            let input = format!(
                "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"{provider_type}\",\"message\":\"{SENTINEL}\"}}}}\n\n"
            );
            let (out, _) = run(anthropic(), input.as_bytes());
            let events = oracle(&out);
            assert_eq!(events.len(), 1, "{provider_type}");
            assert_eq!(events[0].0, "error");
            let v: serde_json::Value = serde_json::from_str(&events[0].1).unwrap();
            assert_eq!(v["type"], "error");
            assert_eq!(v["error"]["type"], want, "{provider_type}");
        }
    }

    #[test]
    fn an_over_long_event_ends_the_stream_and_its_tail_is_not_forwarded() {
        let mut input = b"data: {\"x\":\"".to_vec();
        input.extend(std::iter::repeat_n(b'a', MAX_EVENT_BYTES));
        input.extend_from_slice(format!("\"}}\n\ndata: {{\"m\":\"{SENTINEL}\"}}\n\n").as_bytes());
        let (out, ended) = run(openai(), &input);
        assert_eq!(ended, Some(Ended::OverLimit));
        assert!(!String::from_utf8_lossy(&out).contains(SENTINEL));
        assert!(out.len() < 1024, "only the replacement is forwarded");
    }

    #[test]
    fn an_end_marker_without_its_blank_line_is_still_dispatched() {
        // The relay calls `end()` as soon as the meter sees the end marker; the
        // blank line may still be in flight.
        let input = OPENAI_OK.strip_suffix('\n').unwrap();
        let (out, ended) = run(openai(), input.as_bytes());
        assert_eq!(String::from_utf8(out).unwrap(), OPENAI_OK);
        assert_eq!(ended, None);
        let input = ANTHROPIC_OK.strip_suffix('\n').unwrap();
        let (out, _) = run(anthropic(), input.as_bytes());
        assert_eq!(String::from_utf8(out).unwrap(), ANTHROPIC_OK);
    }

    #[test]
    fn nothing_after_the_end_marker_is_forwarded() {
        let input = format!("{OPENAI_OK}data: {{\"m\":\"{SENTINEL}\"}}\n\n");
        let (out, ended) = run(openai(), input.as_bytes());
        assert_eq!(String::from_utf8(out).unwrap(), OPENAI_OK);
        assert_eq!(ended, None);
        let input = format!("{ANTHROPIC_OK}event: error\ndata: {SENTINEL}\n\n");
        let (out, ended) = run(anthropic(), input.as_bytes());
        assert_eq!(String::from_utf8(out).unwrap(), ANTHROPIC_OK);
        assert_eq!(ended, None);
    }

    #[test]
    fn an_unterminated_last_line_is_classified_like_any_other() {
        // A safe object is dispatched, with its blank line added.
        let (out, ended) = run(openai(), b"data: {\"choices\":[]}");
        assert_eq!(out, b"data: {\"choices\":[]}\n\n");
        assert_eq!(ended, None);
        // An error ends the client's stream, as anywhere else.
        let input = format!("data: {{\"error\":{{\"message\":\"{SENTINEL}\"}}}}");
        assert_no_leak(openai, input.as_bytes());
        let (_, ended) = run(openai(), input.as_bytes());
        assert_eq!(ended, Some(Ended::ErrorEvent));
    }

    #[test]
    fn comments_ask_for_a_keepalive() {
        let mut r = openai();
        assert!(r.push(b": ping\n\n").is_empty());
        assert!(r.take_keepalive());
        assert!(!r.take_keepalive());
    }

    #[test]
    fn a_megabyte_one_byte_at_a_time_is_linear() {
        let mut input = Vec::new();
        while input.len() < MAX_EVENT_BYTES - 1024 {
            input.extend_from_slice(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"abcdefgh\"}}]}\n\n",
            );
        }
        let started = std::time::Instant::now();
        let (out, ended) = run_bytewise(openai(), &input);
        assert_eq!(out, input);
        assert_eq!(ended, None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }

    /// A client that splits lines on every Unicode line break, as httpx's
    /// `iter_lines` (and so some older SDK versions) do.
    fn splitlines_oracle(bytes: &[u8]) -> Vec<(String, String)> {
        let text = String::from_utf8_lossy(bytes);
        let normalised: String = text
            .chars()
            .map(|c| match c {
                '\u{2028}' | '\u{2029}' | '\u{85}' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}'
                | '\u{1e}' => '\n',
                other => other,
            })
            .collect();
        oracle(normalised.as_bytes())
    }

    #[test]
    fn unicode_line_breaks_inside_a_safe_event_are_escaped() {
        let input = format!(
            "event: ping\ndata: {{\"type\":\"ping\",\"x\":\"a\u{2028}\u{2028}event: error\u{2028}data: {SENTINEL}\u{2028}z:\"}}\n\n"
        );
        let (out, ended) = run(anthropic(), input.as_bytes());
        assert_eq!(ended, None, "a valid ping is forwarded");
        for (name, data) in splitlines_oracle(&out) {
            assert_ne!(
                name, "error",
                "a splitting client must not see an error event"
            );
            assert!(!data.contains(SENTINEL) || name == "ping", "{name} {data}");
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\\u2028"), "{text}");
        assert!(!text.contains('\u{2028}'), "{text}");
        // The escaped form decodes to the same JSON value.
        let (_, data) = &oracle(text.as_bytes())[0];
        let v: serde_json::Value = serde_json::from_str(data).unwrap();
        assert!(v["x"].as_str().unwrap().contains('\u{2028}'));
    }

    #[test]
    fn crlf_split_across_chunks_and_odd_field_lines() {
        let crlf = OPENAI_OK.replace('\n', "\r\n");
        let (out, ended) = run_bytewise(openai(), crlf.as_bytes());
        assert_eq!(String::from_utf8(out).unwrap(), OPENAI_OK);
        assert_eq!(ended, None);
        // A leading space or a different case is not the `data` field: dropped.
        let input = format!(
            " data: {{\"m\":\"{SENTINEL}\"}}\nDATA: {{\"m\":\"{SENTINEL}\"}}\n\n{OPENAI_OK}"
        );
        let (out, ended) = run(openai(), input.as_bytes());
        assert_eq!(String::from_utf8(out).unwrap(), OPENAI_OK);
        assert_eq!(ended, None);
    }

    #[test]
    fn a_repeated_event_line_ends_the_stream() {
        let input = "event: content_block_delta\nevent: ping\ndata: {\"type\":\"ping\"}\n\n";
        let (out, ended) = run(anthropic(), input.as_bytes());
        assert_eq!(ended, Some(Ended::Unclassifiable));
        assert!(!String::from_utf8_lossy(&out).contains("content_block_delta"));
    }

    #[test]
    fn message_stop_ends_only_under_its_own_name() {
        // `message_stop` data under a `ping` name is forwarded but is not the
        // end marker, so the real one still reaches the client.
        let input = "event: ping\ndata: {\"type\":\"message_stop\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let (out, ended) = run(anthropic(), input.as_bytes());
        assert_eq!(ended, None);
        assert_eq!(String::from_utf8(out).unwrap(), input);
    }

    #[test]
    fn an_unterminated_done_is_still_dispatched_and_spaces_are_tolerated() {
        let (out, _) = run(openai(), b"data: [DONE]");
        assert_eq!(oracle(&out), vec![(String::new(), "[DONE]".to_owned())]);
        let (out, ended) = run(openai(), b"data: [DONE] \n\n");
        assert_eq!(ended, None);
        assert_eq!(out, b"data: [DONE] \n\n");
    }

    #[test]
    fn deep_nesting_is_classified_by_its_top_level_keys() {
        let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        let benign = format!("data: {{\"choices\":[],\"x\":{deep}}}\n\n");
        let (_, ended) = run(openai(), benign.as_bytes());
        assert_eq!(ended, None);
        let hostile = format!("data: {{\"error\":{{\"m\":\"{SENTINEL}\",\"x\":{deep}}}}}\n\n");
        assert_no_leak(openai, hostile.as_bytes());
        let (_, ended) = run(openai(), hostile.as_bytes());
        assert_eq!(ended, Some(Ended::ErrorEvent));
    }
}
