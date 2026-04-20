use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct RecognitionService {
    client: Client,
    api_key: Option<String>,
    model: String,
    max_tokens: u32,
}

#[derive(Debug, Serialize)]
struct ClaudeRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<ClaudeMessage>,
}

#[derive(Debug, Serialize)]
struct ClaudeMessage {
    role: String,
    content: Vec<ClaudeContent>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ClaudeContent {
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecognitionResult {
    pub raw_text: String,
    pub structured_data: Option<serde_json::Value>,
    pub document_type: Option<String>,
    pub confidence: f64,
}

impl RecognitionService {
    pub fn new(api_key: Option<String>, model: String, max_tokens: u32) -> Self {
        Self {
            client: Client::new(),
            api_key,
            model,
            max_tokens,
        }
    }

    pub fn is_available(&self) -> bool {
        self.api_key.is_some()
    }

    /// Recognize text and structured data from a document image or PDF.
    pub async fn recognize(
        &self,
        file_bytes: &[u8],
        content_type: &str,
    ) -> Result<RecognitionResult, String> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| "Claude API key not configured".to_string())?;

        let b64 = base64::engine::general_purpose::STANDARD.encode(file_bytes);

        let media_type = match content_type {
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" => content_type.to_string(),
            "application/pdf" => "application/pdf".to_string(),
            _ => {
                return Err(format!(
                    "Unsupported content type for recognition: {}",
                    content_type
                ));
            }
        };

        let request = ClaudeRequest {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            messages: vec![ClaudeMessage {
                role: "user".to_string(),
                content: vec![
                    ClaudeContent::Image {
                        source: ImageSource {
                            source_type: "base64".to_string(),
                            media_type,
                            data: b64,
                        },
                    },
                    ClaudeContent::Text {
                        text: concat!(
                            "Extract all text and structured data from this document. ",
                            "Identify the document type (invoice, receipt, bank statement, ",
                            "contract, letter, form, report, etc). ",
                            "Return a JSON object with these fields:\n",
                            "- \"raw_text\": all extracted text\n",
                            "- \"document_type\": the identified type\n",
                            "- \"structured_data\": key-value pairs of important fields\n",
                            "- \"confidence\": 0.0-1.0 confidence score\n",
                            "Return ONLY the JSON, no markdown fences."
                        )
                        .to_string(),
                    },
                ],
            }],
        };

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Claude API request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Claude API error {}: {}", status, body));
        }

        let claude_resp: ClaudeResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse Claude response: {}", e))?;

        let text = claude_resp
            .content
            .first()
            .and_then(|c| c.text.as_ref())
            .ok_or_else(|| "No text in Claude response".to_string())?;

        // Parse the JSON response
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(json) => Ok(RecognitionResult {
                raw_text: json["raw_text"].as_str().unwrap_or("").to_string(),
                structured_data: json.get("structured_data").cloned(),
                document_type: json["document_type"].as_str().map(|s| s.to_string()),
                confidence: json["confidence"].as_f64().unwrap_or(0.5),
            }),
            Err(_) => {
                // If Claude didn't return valid JSON, use the raw text
                Ok(RecognitionResult {
                    raw_text: text.clone(),
                    structured_data: None,
                    document_type: None,
                    confidence: 0.3,
                })
            }
        }
    }
}
