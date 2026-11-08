use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeUser {
    pub id: String,
    pub name: String,
    pub email: String,
    pub image: Option<String>,
    pub role: String,
    pub approval_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    pub organization_id: String,
    pub organization_name: String,
    pub organization_slug: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeSession {
    pub user: BridgeUser,
    pub active_organization_id: Option<String>,
    pub active_role: Option<String>,
    #[serde(default)]
    pub memberships: Vec<Membership>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedApiKey {
    pub id: String,
    pub organization_id: String,
    pub created_by_user_id: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub organization_id: String,
    pub role: String,
    pub is_global_admin: bool,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct ItemRow {
    pub id: Uuid,
    pub organization_id: String,
    pub created_by_user_id: Option<String>,
    pub created_by_key_id: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_payload: Option<String>,
    pub blob_id: Option<Uuid>,
    pub original_filename: Option<String>,
    pub virtual_path: String,
    pub source_url: Option<String>,
    pub tags: serde_json::Value,
    pub content_hash: String,
    pub size_bytes: i64,
    pub pinned: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl ItemRow {
    /// Tag strings stored in the `tags` JSON column, ready for template iteration.
    pub fn tag_list(&self) -> Vec<String> {
        self.tags
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Short human title: the original filename, else the first meaningful line of text.
    pub fn display_name(&self) -> String {
        if let Some(name) = self
            .original_filename
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            return name.to_string();
        }
        if let Some(line) = self
            .text_payload
            .as_deref()
            .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        {
            let title: String = line.chars().take(72).collect();
            return if line.chars().count() > 72 {
                format!("{title}\u{2026}")
            } else {
                title
            };
        }
        format!("Untitled {}", self.kind)
    }

    /// The first part of a text payload, bounded so cards stay light even for huge pastes.
    /// The full text is loaded on demand by the reader dialog.
    pub fn excerpt(&self) -> String {
        const LIMIT: usize = 900;
        let Some(text) = self.text_payload.as_deref() else {
            return String::new();
        };
        let mut excerpt: String = text.chars().take(LIMIT).collect();
        if text.chars().nth(LIMIT).is_some() {
            excerpt.push('\u{2026}');
        }
        excerpt
    }

    /// Whether the card excerpt hides part of the text.
    pub fn is_long(&self) -> bool {
        self.text_payload
            .as_deref()
            .is_some_and(|text| text.lines().nth(7).is_some() || text.chars().nth(600).is_some())
    }

    /// Lower-case filename extension without the dot, or an empty string.
    pub fn extension(&self) -> String {
        self.original_filename
            .as_deref()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .filter(|ext| ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or_default()
    }

    /// Broad media family used for card icons and inline thumbnails.
    pub fn media(&self) -> &'static str {
        if self.text_payload.is_some() {
            return "";
        }
        if self.kind == "image" {
            return "image";
        }
        match self.extension().as_str() {
            "svg" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "ico" => "image",
            "mp4" | "m4v" | "webm" | "mov" | "mkv" | "ogv" => "video",
            "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "oga" | "opus" => "audio",
            "pdf" => "pdf",
            "zip" | "tar" | "gz" | "tgz" | "7z" | "rar" | "xz" | "bz2" | "zst" => "archive",
            "md" | "txt" | "log" | "csv" | "json" | "yaml" | "yml" | "toml" | "xml" | "html"
            | "htm" | "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java" | "c" | "h"
            | "cpp" | "cs" | "sh" | "sql" | "css" | "rb" | "php" | "kt" | "swift" | "lua" => {
                "document"
            }
            _ => "file",
        }
    }

    /// Images small enough to show as a card thumbnail without a dedicated resize step.
    pub fn has_thumbnail(&self) -> bool {
        self.media() == "image" && self.size_bytes <= 25 * 1024 * 1024
    }

    /// Icon symbol in the sprite for this item.
    pub fn icon(&self) -> &'static str {
        match (self.kind.as_str(), self.media()) {
            ("code", _) => "i-code",
            ("url", _) => "i-link",
            ("html", _) => "i-html",
            ("text", _) => "i-text",
            (_, "image") => "i-image",
            (_, "video") => "i-video",
            (_, "audio") => "i-audio",
            (_, "archive") => "i-archive",
            (_, "pdf" | "document") => "i-doc",
            _ => "i-file",
        }
    }

    /// Short type label shown in the card metadata.
    pub fn type_label(&self) -> String {
        let ext = self.extension();
        if self.text_payload.is_none() && !ext.is_empty() {
            ext.to_uppercase()
        } else {
            self.kind.to_uppercase()
        }
    }

    /// Compact relative age such as `4m ago`, `6h ago`, or `12 Mar 2026`.
    pub fn age(&self) -> String {
        let elapsed = Utc::now().signed_duration_since(self.created_at);
        let minutes = elapsed.num_minutes();
        if minutes < 1 {
            "just now".to_string()
        } else if minutes < 60 {
            format!("{minutes}m ago")
        } else if elapsed.num_hours() < 24 {
            format!("{}h ago", elapsed.num_hours())
        } else if elapsed.num_days() < 7 {
            format!("{}d ago", elapsed.num_days())
        } else {
            self.created_at.format("%d %b %Y").to_string()
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct BlobRow {
    pub id: Uuid,
    pub storage_key: String,
    pub size_bytes: i64,
    pub mime_type: String,
}

#[derive(Debug, Deserialize)]
pub struct ItemCreate {
    pub payload: String,
    #[serde(rename = "type", default = "default_auto")]
    pub kind: String,
    #[serde(default = "default_path")]
    pub virtual_path: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source_url: Option<String>,
}

fn default_auto() -> String {
    "auto".into()
}
fn default_path() -> String {
    "/".into()
}

#[derive(Debug, Deserialize)]
pub struct ItemPatch {
    pub virtual_path: Option<String>,
    pub tags: Option<Vec<String>>,
    pub source_url: Option<Option<String>>,
    pub pinned: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub q: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub before: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
    pub trash: Option<bool>,
    pub tag: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ItemListResponse {
    pub items: Vec<ItemRow>,
    pub next_cursor: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::ItemRow;

    fn item(text: Option<&str>, filename: Option<&str>, kind: &str) -> ItemRow {
        ItemRow {
            id: uuid::Uuid::nil(),
            organization_id: "org".into(),
            created_by_user_id: None,
            created_by_key_id: None,
            kind: kind.into(),
            text_payload: text.map(str::to_owned),
            blob_id: None,
            original_filename: filename.map(str::to_owned),
            virtual_path: "/".into(),
            source_url: None,
            tags: serde_json::json!([]),
            content_hash: String::new(),
            size_bytes: 10,
            pinned: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
        }
    }

    #[test]
    fn long_text_is_excerpted_for_cards() {
        let long = "ა".repeat(5000);
        let row = item(Some(&long), None, "text");
        assert!(row.is_long());
        assert_eq!(row.excerpt().chars().count(), 901);
        assert!(!item(Some("short"), None, "text").is_long());
    }

    #[test]
    fn files_get_a_media_family_from_their_extension() {
        assert_eq!(item(None, Some("clip.MP4"), "file").media(), "video");
        assert_eq!(item(None, Some("song.flac"), "file").media(), "audio");
        assert_eq!(item(None, Some("logo.svg"), "file").media(), "image");
        assert_eq!(item(None, Some("notes"), "file").media(), "file");
        assert_eq!(
            item(None, Some("archive.tar.gz"), "file").type_label(),
            "GZ"
        );
    }
}
