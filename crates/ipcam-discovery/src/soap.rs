use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct SoapEnvelope {
    pub header: String,
    pub body: String,
    pub action: String,
}

impl SoapEnvelope {
    pub fn new(action: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            header: String::new(),
            body: body.into(),
            action: action.into(),
        }
    }

    pub fn with_header(mut self, header: impl Into<String>) -> Self {
        self.header = header.into();
        self
    }

    pub fn render(&self) -> Bytes {
        let header_xml = if self.header.is_empty() {
            String::new()
        } else {
            format!("<Header>{}</Header>", self.header)
        };
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope">
{header_xml}
  <Body>{body}</Body>
</Envelope>"#,
            header_xml = header_xml,
            body = self.body,
        );
        Bytes::from(xml)
    }

    pub fn content_type() -> &'static str {
        "application/soap+xml; charset=utf-8"
    }
}
