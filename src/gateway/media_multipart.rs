//! Bounded, buffered MIME form parsing without decoding binary file contents.
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: String,
    pub bytes: Vec<u8>,
}
pub struct Form {
    pub parts: Vec<Part>,
}

fn parameter(header: &str, key: &str) -> Option<String> {
    header.split(';').skip(1).find_map(|p| {
        let (name, value) = p.trim().split_once('=')?;
        (name.eq_ignore_ascii_case(key)).then(|| value.trim().trim_matches('"').to_owned())
    })
}
fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|w| w == needle)
}
impl Form {
    pub fn parse(content_type: &str, bytes: &[u8]) -> Result<Self> {
        let boundary = parameter(content_type, "boundary")
            .ok_or_else(|| anyhow::anyhow!("missing boundary"))?;
        ensure!(
            !boundary.is_empty()
                && boundary.len() <= 70
                && boundary.bytes().all(|b| b.is_ascii_graphic() && b != b'"'),
            "invalid boundary"
        );
        let marker = format!("--{boundary}");
        let delimiter = format!("\r\n{marker}");
        let mut rest = bytes
            .strip_prefix(marker.as_bytes())
            .ok_or_else(|| anyhow::anyhow!("invalid form"))?;
        let mut parts = Vec::new();
        loop {
            if rest.starts_with(b"--") {
                break;
            }
            rest = rest
                .strip_prefix(b"\r\n")
                .ok_or_else(|| anyhow::anyhow!("invalid delimiter"))?;
            let end = find(rest, b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("missing headers"))?;
            ensure!(end <= 16384 && parts.len() < 1024, "form too complex");
            let headers = std::str::from_utf8(&rest[..end])?;
            let mut disposition = None;
            let mut content_type = "application/octet-stream".to_owned();
            for line in headers.split("\r\n") {
                let (name, value) = line
                    .split_once(':')
                    .ok_or_else(|| anyhow::anyhow!("invalid header"))?;
                if name.eq_ignore_ascii_case("content-disposition") {
                    ensure!(
                        disposition.is_none() && value.trim().starts_with("form-data;"),
                        "invalid disposition"
                    );
                    disposition = Some(value.trim());
                } else if name.eq_ignore_ascii_case("content-type") {
                    content_type = value.trim().to_owned();
                }
            }
            let disposition = disposition.ok_or_else(|| anyhow::anyhow!("missing disposition"))?;
            let name =
                parameter(disposition, "name").ok_or_else(|| anyhow::anyhow!("missing name"))?;
            let filename = parameter(disposition, "filename");
            ensure!(
                safe(&name) && filename.as_deref().is_none_or(safe),
                "invalid field name"
            );
            rest = &rest[end + 4..];
            let end = find(rest, delimiter.as_bytes())
                .ok_or_else(|| anyhow::anyhow!("unterminated form"))?;
            parts.push(Part {
                name,
                filename,
                content_type,
                bytes: rest[..end].to_vec(),
            });
            rest = &rest[end + delimiter.len()..];
        }
        Ok(Self { parts })
    }
    pub fn fields(&self) -> Result<Value> {
        let mut fields = json!({});
        for part in &self.parts {
            if part.filename.is_none() && fields.get(&part.name).is_none() {
                let value = std::str::from_utf8(&part.bytes)?.to_owned();
                fields[&part.name] = if part.name == "stream" {
                    json!(matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "true" | "1" | "on" | "yes"
                    ))
                } else {
                    value.into()
                };
            }
        }
        Ok(fields)
    }
    pub fn images(&self) -> Vec<&Part> {
        let array = self
            .parts
            .iter()
            .any(|p| p.filename.is_some() && p.name == "image[]");
        self.parts
            .iter()
            .filter(|p| p.filename.is_some() && p.name == if array { "image[]" } else { "image" })
            .collect()
    }
    pub fn payload(&self, fields: &Value) -> Value {
        let mut view = fields.clone();
        for part in self.parts.iter().filter(|p| p.filename.is_some()) {
            let file = json!({"filename":part.filename,"content_type":part.content_type,"data":STANDARD.encode(&part.bytes)});
            match view.get_mut(&part.name) {
                Some(Value::Array(items)) => items.push(file),
                Some(existing) => *existing = json!([existing.clone(), file]),
                None => view[&part.name] = file,
            }
        }
        view
    }
    pub fn encode(&self, fields: &Value) -> Result<(Vec<u8>, String)> {
        let boundary = format!("ai48-{}", uuid::Uuid::new_v4());
        let mut out = Vec::new();
        for (name, value) in fields
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("invalid fields"))?
        {
            ensure!(safe(name), "invalid field name");
            if name == "stream" && value == false {
                continue;
            }
            let items = value
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![value.clone()]);
            if items.iter().any(|v| v.get("filename").is_some()) {
                for item in items {
                    let filename = item
                        .get("filename")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("invalid file"))?;
                    let mime = item
                        .get("content_type")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream");
                    ensure!(safe(filename) && safe(mime), "invalid file headers");
                    let bytes =
                        STANDARD.decode(item.get("data").and_then(Value::as_str).unwrap_or(""))?;
                    out.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n").as_bytes());
                    out.extend(bytes);
                    out.extend_from_slice(b"\r\n");
                }
                continue;
            }
            let values = if name != "model" {
                self.parts
                    .iter()
                    .filter(|p| p.filename.is_none() && p.name == *name)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            // Preserve repeated original fields when payload rules did not change their first value.
            let unchanged = values
                .first()
                .is_some_and(|p| value.as_str() == std::str::from_utf8(&p.bytes).ok());
            let encoded = if unchanged {
                values.iter().map(|p| p.bytes.clone()).collect()
            } else {
                vec![
                    value
                        .as_str()
                        .map(str::as_bytes)
                        .map(Vec::from)
                        .unwrap_or_else(|| value.to_string().into_bytes()),
                ]
            };
            for bytes in encoded {
                out.extend_from_slice(
                    format!(
                        "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n"
                    )
                    .as_bytes(),
                );
                out.extend(bytes);
                out.extend_from_slice(b"\r\n");
            }
        }

        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Ok((out, format!("multipart/form-data; boundary={boundary}")))
    }
}
fn safe(value: &str) -> bool {
    !value
        .bytes()
        .any(|b| b < 32 || b == 127 || b == b'"' || b == b'\\')
}
