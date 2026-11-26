//! Owner-scoped Annotations reads through the authenticated gateway.

use crate::domain::models::{
    annotations::{NoteOmission, NoteSection, NotesApi, SelectedNote},
    chat::{ChatFailure, valid_id},
    native_auth::Session,
};
use base64::Engine;
use reqwest::{
    blocking::{Client, Response},
    redirect::Policy,
};
use serde_json::Value;
use std::{io::Read, sync::Arc, time::Duration};

const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct AnnotationsClient {
    client: Client,
    base: String,
    session: Arc<Session>,
}

impl AnnotationsClient {
    pub fn new(session: Arc<Session>) -> Result<Self, ChatFailure> {
        Self::with_base(
            &format!(
                "{}{}",
                crate::core::settings::GATEWAY_URL,
                "/apps/annotations/v1"
            ),
            session,
        )
    }

    fn with_base(base: &str, session: Arc<Session>) -> Result<Self, ChatFailure> {
        if !session.is_valid() {
            return Err(ChatFailure::Unauthorized);
        }
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| ChatFailure::Connection)?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            session,
        })
    }

    fn json(response: Response) -> Result<Value, ChatFailure> {
        if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
            return Err(ChatFailure::Unauthorized);
        }
        if !response.status().is_success() {
            return Err(ChatFailure::Rejected(None));
        }
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|_| ChatFailure::Connection)?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(ChatFailure::InvalidResponse);
        }
        serde_json::from_slice(&body).map_err(|_| ChatFailure::InvalidResponse)
    }

    fn read_note(&self, note_id: &str) -> Result<SelectedNote, ChatFailure> {
        if !valid_id(note_id) {
            return Err(ChatFailure::InvalidInput);
        }
        let response = self
            .client
            .get(format!("{}/me/notes/{note_id}?view=typed", self.base))
            .header(
                "Cookie",
                format!(
                    "sid={}; csrf={}",
                    self.session.session_id, self.session.csrf_token
                ),
            )
            .header("Accept", "application/json")
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        let value = Self::json(response)?;
        let returned_id = value["note_id"]
            .as_str()
            .filter(|id| valid_id(id) && id.eq_ignore_ascii_case(note_id))
            .ok_or(ChatFailure::InvalidResponse)?;
        let revision = value["revision"]
            .as_u64()
            .filter(|revision| *revision > 0)
            .ok_or(ChatFailure::InvalidResponse)?;
        let typed = value["representations"]
            .as_array()
            .ok_or(ChatFailure::InvalidResponse)?
            .iter()
            .filter(|representation| representation["representation"] == "typed")
            .collect::<Vec<_>>();
        if typed.len() != 1 {
            return Err(ChatFailure::InvalidResponse);
        }
        let revision_id = typed[0]["revision_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or(ChatFailure::InvalidResponse)?;
        let document = &typed[0]["document"];
        let blocks = document["blocks"]
            .as_array()
            .ok_or(ChatFailure::InvalidResponse)?;
        let mut sections = Vec::new();
        let mut omissions = std::collections::BTreeMap::<String, usize>::new();
        for block in blocks {
            let id = block["id"]
                .as_str()
                .filter(|id| valid_token(id, 64))
                .ok_or(ChatFailure::InvalidResponse)?;
            let extracted = block_text(block)?;
            if let Some(kind) = extracted.omitted_kind {
                *omissions.entry(kind).or_default() += 1;
            }
            if !extracted.text.is_empty() {
                sections.push(NoteSection {
                    id: id.to_owned(),
                    text: extracted.text,
                });
            }
        }
        let title = value["title"].as_str().map(str::to_owned);
        Ok(SelectedNote {
            note_id: returned_id.to_owned(),
            revision,
            revision_id: revision_id.to_owned(),
            title,
            sections,
            omissions: omissions
                .into_iter()
                .map(|(kind, count)| NoteOmission { kind, count })
                .collect(),
        })
    }
}

fn spans(value: &Value) -> Result<String, ChatFailure> {
    let values = value.as_array().ok_or(ChatFailure::InvalidResponse)?;
    let mut result = String::new();
    for span in values {
        result.push_str(span["text"].as_str().ok_or(ChatFailure::InvalidResponse)?);
    }
    Ok(result)
}

struct ExtractedBlock {
    text: String,
    omitted_kind: Option<String>,
}

fn extracted(text: String) -> ExtractedBlock {
    ExtractedBlock {
        text,
        omitted_kind: None,
    }
}

fn omitted(kind: &str) -> ExtractedBlock {
    ExtractedBlock {
        text: String::new(),
        omitted_kind: Some(kind.to_owned()),
    }
}

fn block_text(block: &Value) -> Result<ExtractedBlock, ChatFailure> {
    let kind = block["kind"].as_str().ok_or(ChatFailure::InvalidResponse)?;
    match kind {
        "paragraph" => spans(&block["spans"]).map(extracted),
        "heading" => {
            let level = block["level"]
                .as_u64()
                .filter(|n| (1..=6).contains(n))
                .ok_or(ChatFailure::InvalidResponse)?;
            Ok(extracted(format!(
                "{} {}",
                "#".repeat(level as usize),
                spans(&block["spans"])?
            )))
        }
        "list" => {
            let items = block["items"]
                .as_array()
                .ok_or(ChatFailure::InvalidResponse)?;
            let ordered = block["ordered"]
                .as_bool()
                .ok_or(ChatFailure::InvalidResponse)?;
            let mut lines = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let prefix = if ordered {
                    format!("{}. ", index + 1)
                } else {
                    "• ".to_owned()
                };
                lines.push(format!("{prefix}{}", spans(&item["spans"])?));
            }
            Ok(extracted(lines.join("\n")))
        }
        "table" => {
            let rows = block["rows"]
                .as_array()
                .ok_or(ChatFailure::InvalidResponse)?;
            let header = block
                .get("header")
                .map(|value| value.as_bool().ok_or(ChatFailure::InvalidResponse))
                .transpose()?
                .unwrap_or(false);
            let mut lines = Vec::with_capacity(rows.len() + usize::from(header));
            for (index, row) in rows.iter().enumerate() {
                let cells = row["cells"]
                    .as_array()
                    .ok_or(ChatFailure::InvalidResponse)?;
                let mut values = Vec::with_capacity(cells.len());
                for cell in cells {
                    values.push(spans(&cell["spans"])?);
                }
                if header {
                    lines.push(format!("| {} |", values.join(" | ")));
                    if index == 0 {
                        lines.push(format!("| {} |", vec!["---"; values.len()].join(" | ")));
                    }
                } else {
                    lines.push(values.join(" | "));
                }
            }
            Ok(extracted(lines.join("\n")))
        }
        "figure" => {
            require_fields(block, &["id", "kind", "file_id"], &["caption"])?;
            token_field(block, "file_id", 64)?;
            let text = block
                .get("caption")
                .map(spans)
                .transpose()?
                .unwrap_or_default();
            Ok(ExtractedBlock {
                text,
                omitted_kind: Some("figure_image".into()),
            })
        }
        "handwriting" => {
            validate_handwriting(block)?;
            Ok(omitted("handwriting"))
        }
        "file_card" | "csv_table" | "pdf_preview" | "link_card" | "website_card"
        | "youtube_card" | "reports_embed" | "task_card" | "quiz_card" => {
            validate_card(block, kind)?;
            Ok(omitted(kind))
        }
        _ => Err(ChatFailure::InvalidResponse),
    }
}

fn valid_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn require_fields(value: &Value, required: &[&str], optional: &[&str]) -> Result<(), ChatFailure> {
    let object = value.as_object().ok_or(ChatFailure::InvalidResponse)?;
    if required.iter().any(|key| !object.contains_key(*key))
        || object
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(ChatFailure::InvalidResponse);
    }
    Ok(())
}

fn token_field<'a>(value: &'a Value, key: &str, max: usize) -> Result<&'a str, ChatFailure> {
    value[key]
        .as_str()
        .filter(|value| valid_token(value, max))
        .ok_or(ChatFailure::InvalidResponse)
}

fn optional_text(value: &Value, key: &str) -> Result<(), ChatFailure> {
    if let Some(text) = value.get(key) {
        let text = text.as_str().ok_or(ChatFailure::InvalidResponse)?;
        if text.len() > 16 * 1024
            || text
                .chars()
                .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
            || text.to_ascii_lowercase().contains("javascript:")
            || contains_markup(text)
        {
            return Err(ChatFailure::InvalidResponse);
        }
    }
    Ok(())
}

fn contains_markup(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'<' {
            continue;
        }
        let mut next = index + 1;
        if bytes.get(next) == Some(&b'/') {
            next += 1;
        }
        if bytes
            .get(next)
            .is_some_and(|value| value.is_ascii_lowercase() || matches!(value, b'!' | b'?'))
            && bytes[next..]
                .iter()
                .take_while(|value| **value != b'<')
                .any(|value| *value == b'>')
        {
            return true;
        }
    }
    false
}

fn valid_url(value: &Value, key: &str, youtube: bool) -> Result<(), ChatFailure> {
    let url = reqwest::Url::parse(value[key].as_str().ok_or(ChatFailure::InvalidResponse)?)
        .map_err(|_| ChatFailure::InvalidResponse)?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        return Err(ChatFailure::InvalidResponse);
    }
    if youtube
        && !matches!(
            url.host_str(),
            Some("youtube.com" | "www.youtube.com" | "m.youtube.com" | "youtu.be")
        )
    {
        return Err(ChatFailure::InvalidResponse);
    }
    Ok(())
}

fn valid_slug(value: &str) -> bool {
    (2..=48).contains(&value.len())
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn validate_card(block: &Value, kind: &str) -> Result<(), ChatFailure> {
    match kind {
        "file_card" | "csv_table" | "pdf_preview" => {
            require_fields(block, &["id", "kind", "file_id"], &[])?;
            token_field(block, "file_id", 64)?;
        }
        "link_card" => {
            require_fields(block, &["id", "kind", "url"], &["title", "site_name"])?;
            valid_url(block, "url", false)?;
            optional_text(block, "title")?;
            optional_text(block, "site_name")?;
        }
        "website_card" => {
            require_fields(
                block,
                &["id", "kind", "url"],
                &["title", "site_name", "description", "preview_file_id"],
            )?;
            valid_url(block, "url", false)?;
            optional_text(block, "title")?;
            optional_text(block, "site_name")?;
            optional_text(block, "description")?;
            if block.get("preview_file_id").is_some() {
                token_field(block, "preview_file_id", 64)?;
            }
        }
        "youtube_card" => {
            require_fields(block, &["id", "kind", "url"], &["title", "preview_file_id"])?;
            valid_url(block, "url", true)?;
            optional_text(block, "title")?;
            if block.get("preview_file_id").is_some() {
                token_field(block, "preview_file_id", 64)?;
            }
        }
        "reports_embed" => {
            require_fields(
                block,
                &["id", "kind", "report_id", "source", "mode"],
                &[
                    "project_slug",
                    "report_slug",
                    "dataset_id",
                    "snapshot_id",
                    "captured_at",
                ],
            )?;
            token_field(block, "report_id", 128)?;
            let source = block["source"]
                .as_str()
                .ok_or(ChatFailure::InvalidResponse)?;
            let mode = block["mode"].as_str().ok_or(ChatFailure::InvalidResponse)?;
            if !matches!(source, "link" | "upload") || !matches!(mode, "live" | "snapshot") {
                return Err(ChatFailure::InvalidResponse);
            }
            let tuple = ["project_slug", "report_slug", "dataset_id"];
            let present = tuple
                .iter()
                .filter(|key| block.get(**key).is_some())
                .count();
            if present != 0 && present != tuple.len() {
                return Err(ChatFailure::InvalidResponse);
            }
            if present == tuple.len() {
                let project = token_field(block, "project_slug", 128)?;
                let report = token_field(block, "report_slug", 128)?;
                let dataset = token_field(block, "dataset_id", 128)?;
                if !valid_slug(project) || !valid_slug(report) || !valid_slug(dataset) {
                    return Err(ChatFailure::InvalidResponse);
                }
                if source == "link" && block["report_id"].as_str() != Some(report) {
                    return Err(ChatFailure::InvalidResponse);
                }
                if source == "upload"
                    && !is_uuid_v4(block["report_id"].as_str().unwrap_or_default())
                {
                    return Err(ChatFailure::InvalidResponse);
                }
            }
            match mode {
                "snapshot" => {
                    token_field(block, "snapshot_id", 128)?;
                    if block["captured_at"].as_i64().is_none_or(|n| n <= 0) {
                        return Err(ChatFailure::InvalidResponse);
                    }
                }
                _ if block.get("snapshot_id").is_some() || block.get("captured_at").is_some() => {
                    return Err(ChatFailure::InvalidResponse);
                }
                _ => {}
            }
        }
        "task_card" => {
            require_fields(block, &["id", "kind", "task_id"], &[])?;
            token_field(block, "task_id", 128)?;
        }
        "quiz_card" => {
            require_fields(block, &["id", "kind"], &["study_id", "quiz_id"])?;
            let study = block.get("study_id").is_some();
            let quiz = block.get("quiz_id").is_some();
            if !study && !quiz {
                return Err(ChatFailure::InvalidResponse);
            }
            if study {
                token_field(block, "study_id", 128)?;
            }
            if quiz {
                token_field(block, "quiz_id", 128)?;
            }
        }
        _ => return Err(ChatFailure::InvalidResponse),
    }
    Ok(())
}

fn validate_handwriting(block: &Value) -> Result<(), ChatFailure> {
    require_fields(block, &["id", "kind", "width", "height", "ink"], &[])?;
    let width = block["width"]
        .as_f64()
        .ok_or(ChatFailure::InvalidResponse)?;
    let height = block["height"]
        .as_f64()
        .ok_or(ChatFailure::InvalidResponse)?;
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(ChatFailure::InvalidResponse);
    }
    let ink = block["ink"]
        .as_array()
        .ok_or(ChatFailure::InvalidResponse)?;
    if ink.is_empty() {
        return Err(ChatFailure::InvalidResponse);
    }
    for item in ink {
        token_field(item, "id", 64)?;
        if !matches!(item["kind"].as_str(), Some("stroke" | "shape")) {
            return Err(ChatFailure::InvalidResponse);
        }
        if item["kind"] == "stroke" {
            validate_stroke(item)?;
        } else {
            validate_ink_shape(item)?;
        }
    }
    Ok(())
}

fn finite_number(value: &Value, key: &str) -> Result<f64, ChatFailure> {
    let number = value[key].as_f64().ok_or(ChatFailure::InvalidResponse)?;
    if !number.is_finite() {
        return Err(ChatFailure::InvalidResponse);
    }
    Ok(number)
}

fn valid_color(value: &Value, key: &str) -> Result<(), ChatFailure> {
    let color = value[key].as_str().ok_or(ChatFailure::InvalidResponse)?;
    if color.len() != 9
        || !color.starts_with('#')
        || !color[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ChatFailure::InvalidResponse);
    }
    Ok(())
}

fn validate_nodes(value: &Value, samples: Option<usize>) -> Result<(), ChatFailure> {
    let nodes = value.as_array().ok_or(ChatFailure::InvalidResponse)?;
    if nodes.is_empty() {
        return Err(ChatFailure::InvalidResponse);
    }
    for node in nodes {
        require_fields(
            node,
            &["x", "y", "mode", "corner"],
            &["in", "out", "sample"],
        )?;
        node["x"].as_i64().ok_or(ChatFailure::InvalidResponse)?;
        node["y"].as_i64().ok_or(ChatFailure::InvalidResponse)?;
        if !matches!(
            node["mode"].as_str(),
            Some("independent" | "smooth" | "symmetric")
        ) || !node["corner"].is_boolean()
        {
            return Err(ChatFailure::InvalidResponse);
        }
        for key in ["in", "out"] {
            if let Some(handle) = node.get(key) {
                require_fields(handle, &["x", "y"], &[])?;
                handle["x"].as_i64().ok_or(ChatFailure::InvalidResponse)?;
                handle["y"].as_i64().ok_or(ChatFailure::InvalidResponse)?;
            }
        }
        if let Some(index) = node.get("sample") {
            let index = index
                .as_i64()
                .filter(|n| *n >= 0)
                .ok_or(ChatFailure::InvalidResponse)? as usize;
            if samples.is_none_or(|count| index >= count) {
                return Err(ChatFailure::InvalidResponse);
            }
        }
    }
    Ok(())
}

fn validate_stroke(item: &Value) -> Result<(), ChatFailure> {
    require_fields(
        item,
        &[
            "id",
            "kind",
            "tool",
            "brush_version",
            "width",
            "color",
            "opacity",
            "samples",
            "nodes",
        ],
        &["tip_angle"],
    )?;
    let tool = item["tool"].as_str().ok_or(ChatFailure::InvalidResponse)?;
    if !matches!(tool, "round" | "flat" | "brush" | "highlighter")
        || item["brush_version"].as_i64().is_none_or(|n| n < 1)
        || finite_number(item, "width")? <= 0.0
        || !(0.0..=1.0).contains(&finite_number(item, "opacity")?)
        || finite_number(item, "opacity")? == 0.0
    {
        return Err(ChatFailure::InvalidResponse);
    }
    valid_color(item, "color")?;
    match (tool, item.get("tip_angle")) {
        ("flat", Some(_)) => {
            if finite_number(item, "tip_angle")?.abs() > std::f64::consts::PI {
                return Err(ChatFailure::InvalidResponse);
            }
        }
        ("flat", None) | (_, Some(_)) => return Err(ChatFailure::InvalidResponse),
        (_, None) => {}
    }
    let samples = &item["samples"];
    require_fields(samples, &["count", "data"], &[])?;
    let count = samples["count"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(ChatFailure::InvalidResponse)? as usize;
    let encoded = samples["data"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(ChatFailure::InvalidResponse)?;
    let valid_samples = [
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded),
        base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded),
        base64::engine::general_purpose::URL_SAFE.decode(encoded),
        base64::engine::general_purpose::STANDARD.decode(encoded),
    ]
    .iter()
    .any(Result::is_ok);
    if !valid_samples {
        return Err(ChatFailure::InvalidResponse);
    }
    validate_nodes(&item["nodes"], Some(count))
}

fn validate_ink_shape(item: &Value) -> Result<(), ChatFailure> {
    require_fields(
        item,
        &["id", "kind", "shape", "color", "line_width", "opacity"],
        &[
            "fill", "center", "radius", "x", "y", "width", "height", "points", "nodes", "closed",
        ],
    )?;
    let shape = item["shape"].as_str().ok_or(ChatFailure::InvalidResponse)?;
    if !matches!(shape, "circle" | "rectangle" | "triangle" | "path")
        || finite_number(item, "line_width")? <= 0.0
        || !(0.0..=1.0).contains(&finite_number(item, "opacity")?)
        || finite_number(item, "opacity")? == 0.0
    {
        return Err(ChatFailure::InvalidResponse);
    }
    valid_color(item, "color")?;
    if item.get("fill").is_some() {
        valid_color(item, "fill")?;
    }
    match shape {
        "circle" => {
            require_fields(
                item,
                &[
                    "id",
                    "kind",
                    "shape",
                    "color",
                    "line_width",
                    "opacity",
                    "center",
                    "radius",
                ],
                &["fill"],
            )?;
            require_fields(&item["center"], &["x", "y"], &[])?;
            finite_number(&item["center"], "x")?;
            finite_number(&item["center"], "y")?;
            if finite_number(item, "radius")? <= 0.0 {
                return Err(ChatFailure::InvalidResponse);
            }
        }
        "rectangle" => {
            require_fields(
                item,
                &[
                    "id",
                    "kind",
                    "shape",
                    "color",
                    "line_width",
                    "opacity",
                    "x",
                    "y",
                    "width",
                    "height",
                ],
                &["fill"],
            )?;
            finite_number(item, "x")?;
            finite_number(item, "y")?;
            if finite_number(item, "width")? <= 0.0 || finite_number(item, "height")? <= 0.0 {
                return Err(ChatFailure::InvalidResponse);
            }
        }
        "triangle" => {
            require_fields(
                item,
                &[
                    "id",
                    "kind",
                    "shape",
                    "color",
                    "line_width",
                    "opacity",
                    "points",
                ],
                &["fill"],
            )?;
            let points = item["points"]
                .as_array()
                .filter(|points| points.len() == 3)
                .ok_or(ChatFailure::InvalidResponse)?;
            for point in points {
                require_fields(point, &["x", "y"], &[])?;
                finite_number(point, "x")?;
                finite_number(point, "y")?;
            }
        }
        _ => {
            require_fields(
                item,
                &[
                    "id",
                    "kind",
                    "shape",
                    "color",
                    "line_width",
                    "opacity",
                    "nodes",
                    "closed",
                ],
                &["fill"],
            )?;
            if !item["closed"].is_boolean() {
                return Err(ChatFailure::InvalidResponse);
            }
            validate_nodes(&item["nodes"], None)?;
        }
    }
    Ok(())
}

fn is_uuid_v4(value: &str) -> bool {
    let bytes = value.as_bytes();
    value == value.to_ascii_lowercase()
        && bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|index| bytes[*index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && bytes[14] == b'4'
        && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
}

impl NotesApi for AnnotationsClient {
    fn selected_note(&self, note_id: &str) -> Result<SelectedNote, ChatFailure> {
        self.read_note(note_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener, thread};

    const NOTE: &str = "10000000-0000-4000-8000-000000000001";

    fn fixture(body: String, status: u16) -> (AnnotationsClient, thread::JoinHandle<String>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 65536);
            }
            write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8(request).unwrap()
        });
        let client = AnnotationsClient::with_base(
            &format!("http://{address}/apps/annotations/v1"),
            Arc::new(Session {
                session_id: "fixture-session".into(),
                csrf_token: "fixture-csrf".into(),
            }),
        )
        .unwrap();
        (client, worker)
    }

    #[test]
    fn selected_note_uses_owner_route_typed_view_and_gateway_session() {
        let body = serde_json::json!({
            "note_id": NOTE,
            "revision": 4,
            "title": "Cells",
            "representations": [{
                "representation": "typed",
                "revision_id": "20000000-0000-4000-8000-000000000002",
                "document": {"blocks": [
                    {"id": "b1", "kind": "paragraph", "spans": [{"text": "Cell "}, {"text": "wall ไทย"}]},
                    {"id": "b2", "kind": "list", "ordered": false, "items": [{"spans": [{"text": "Alpha"}]}, {"spans": [{"text": "Beta"}]}]},
                    {"id": "b3", "kind": "table", "header": true, "rows": [
                        {"cells": [{"spans": [{"text": "Los Angeles"}]}, {"spans": [{"text": "12"}]}]},
                        {"cells": [{"spans": [{"text": "New York"}]}, {"spans": [{"text": "13"}]}]}
                    ]},
                    {"id": "b4", "kind": "figure", "file_id": "30000000-0000-4000-8000-000000000003", "caption": [{"text": "Diagram"}]}
                ]}
            }],
            "publication": null
        }).to_string();
        let (client, worker) = fixture(body, 200);
        let note = client.selected_note(NOTE).unwrap();
        let request = worker.join().unwrap();
        assert!(request.starts_with(&format!(
            "GET /apps/annotations/v1/me/notes/{NOTE}?view=typed HTTP/1.1"
        )));
        let lower = request.to_lowercase();
        assert!(lower.contains("cookie: sid=fixture-session; csrf=fixture-csrf"));
        assert!(lower.contains("accept: application/json"));
        assert!(!lower.contains("x-uuid:"));
        assert_eq!(note.revision, 4);
        assert_eq!(note.sections[0].id, "b1");
        assert_eq!(note.sections[0].text, "Cell wall ไทย");
        assert_eq!(note.sections[1].text, "• Alpha\n• Beta");
        assert_eq!(
            note.sections[2].text,
            "| Los Angeles | 12 |\n| --- | --- |\n| New York | 13 |"
        );
        assert_eq!(note.sections[3].text, "Diagram");
        assert_eq!(
            note.omissions,
            [NoteOmission {
                kind: "figure_image".into(),
                count: 1,
            }]
        );
    }

    #[test]
    fn producer_shaped_paragraph_and_task_card_preserve_text_and_report_omission() {
        let body = serde_json::json!({
            "note_id": NOTE,
            "revision": 2,
            "representations": [{
                "representation": "typed",
                "revision_id": "20000000-0000-4000-8000-000000000002",
                "document": {"blocks": [
                    {"id":"p1","kind":"paragraph","spans":[{"text":"Keep this selected text."}]},
                    {"id":"k1","kind":"task_card","task_id":"task-7"}
                ]}
            }]
        })
        .to_string();
        let (client, worker) = fixture(body, 200);
        let selected = client.selected_note(NOTE).unwrap();
        worker.join().unwrap();
        assert_eq!(selected.sections.len(), 1);
        assert_eq!(selected.sections[0].text, "Keep this selected text.");
        assert_eq!(
            selected.omissions,
            [NoteOmission {
                kind: "task_card".into(),
                count: 1
            }]
        );
    }

    #[test]
    fn all_legal_typed_reference_cards_are_explicit_omissions() {
        let body = serde_json::json!({
            "note_id": NOTE,
            "revision": 2,
            "representations": [{
                "representation": "typed",
                "revision_id": "20000000-0000-4000-8000-000000000002",
                "document": {"blocks": [
                    {"id":"p1","kind":"paragraph","spans":[{"text":"Typed paragraph."}]},
                    {"id":"f1","kind":"file_card","file_id":"file-doc-1"},
                    {"id":"c1","kind":"csv_table","file_id":"file-csv-1"},
                    {"id":"d1","kind":"pdf_preview","file_id":"file-pdf-1"},
                    {"id":"l1","kind":"link_card","url":"https://asodya.com","title":"Link title","site_name":"Asodya"},
                    {"id":"w1","kind":"website_card","url":"https://asodya.com","title":"Site title","site_name":"Asodya","description":"Site summary","preview_file_id":"file-preview-1"},
                    {"id":"y1","kind":"youtube_card","url":"https://youtu.be/example","title":"Video title","preview_file_id":"file-preview-2"},
                    {"id":"r1","kind":"reports_embed","report_id":"summary","project_slug":"cortex","report_slug":"summary","dataset_id":"values","mode":"live","source":"link"},
                    {"id":"t1","kind":"task_card","task_id":"task-7"},
                    {"id":"q1","kind":"quiz_card","study_id":"study-7","quiz_id":"quiz-8"}
                ]}
            }]
        }).to_string();
        let (client, worker) = fixture(body, 200);
        let selected = client.selected_note(NOTE).unwrap();
        worker.join().unwrap();
        assert_eq!(selected.sections.len(), 1);
        assert_eq!(selected.sections[0].text, "Typed paragraph.");
        assert_eq!(selected.omissions.len(), 9);
        assert!(selected.omissions.iter().all(|item| item.count == 1));
    }

    #[test]
    fn figure_caption_text_survives_while_figure_and_handwriting_media_are_named() {
        let body = serde_json::json!({
            "note_id": NOTE,
            "revision": 2,
            "representations": [{
                "representation": "typed",
                "revision_id": "20000000-0000-4000-8000-000000000002",
                "document": {"blocks": [
                    {"id":"fig1","kind":"figure","file_id":"figure-file-1","caption":[{"text":"Caption remains."}]},
                    {"id":"ink1","kind":"handwriting","width":100,"height":100,"ink":[
                        {"id":"shape1","kind":"shape","shape":"circle","color":"#000000ff","line_width":1,"opacity":1,"center":{"x":50,"y":50},"radius":10}
                    ]}
                ]}
            }]
        }).to_string();
        let (client, worker) = fixture(body, 200);
        let selected = client.selected_note(NOTE).unwrap();
        worker.join().unwrap();
        assert_eq!(selected.sections.len(), 1);
        assert_eq!(selected.sections[0].text, "Caption remains.");
        assert_eq!(
            selected.omissions,
            [
                NoteOmission {
                    kind: "figure_image".into(),
                    count: 1,
                },
                NoteOmission {
                    kind: "handwriting".into(),
                    count: 1,
                },
            ]
        );
    }

    #[test]
    fn malformed_or_unknown_reference_blocks_fail_closed() {
        let blocks = [
            serde_json::json!({"id":"t1","kind":"task_card"}),
            serde_json::json!({"id":"t1","kind":"task_card","task_id":"task-7","description":"invented text"}),
            serde_json::json!({"id":"t1","kind":"task_card_v2","task_id":"task-7"}),
            serde_json::json!({"id":"q1","kind":"quiz_card"}),
            serde_json::json!({"id":"r1","kind":"reports_embed","report_id":"summary","project_slug":"cortex","mode":"live","source":"link"}),
            serde_json::json!({"id":"y1","kind":"youtube_card","url":"https://video.example/watch/1"}),
        ];
        for block in blocks {
            let body = serde_json::json!({
                "note_id": NOTE,
                "revision": 2,
                "representations": [{
                    "representation": "typed",
                    "revision_id": "20000000-0000-4000-8000-000000000002",
                    "document": {"blocks": [
                        {"id":"p1","kind":"paragraph","spans":[{"text":"Keep text."}]},
                        block
                    ]}
                }]
            })
            .to_string();
            let (client, worker) = fixture(body, 200);
            assert!(matches!(
                client.selected_note(NOTE),
                Err(ChatFailure::InvalidResponse)
            ));
            worker.join().unwrap();
        }
    }

    #[test]
    fn selected_note_rejects_wrong_owner_identity_and_wrong_shape() {
        let body =
            serde_json::json!({"note_id": "20000000-0000-4000-8000-000000000002"}).to_string();
        let (client, worker) = fixture(body, 200);
        assert!(matches!(
            client.selected_note(NOTE),
            Err(ChatFailure::InvalidResponse)
        ));
        worker.join().unwrap();

        let (client, worker) = fixture("{}".into(), 404);
        assert!(matches!(
            client.selected_note(NOTE),
            Err(ChatFailure::Rejected(None))
        ));
        worker.join().unwrap();

        let (client, worker) = fixture("{}".into(), 403);
        assert!(matches!(
            client.selected_note(NOTE),
            Err(ChatFailure::Unauthorized)
        ));
        worker.join().unwrap();
    }
}
