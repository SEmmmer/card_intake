//! Immich external libraries: Card Intake lists them, and asks the chosen one to rescan
//! after a delivery. It never changes a library's settings.
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImmichSettings {
    pub url: String,
    pub api_key: String,
    pub library_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub id: String,
    pub name: String,
    pub asset_count: u64,
    pub import_paths: Vec<String>,
}

pub struct Client {
    base: String,
    key: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(settings: &ImmichSettings) -> Result<Self, String> {
        let url = url::Url::parse(settings.url.trim())
            .map_err(|_| "Enter a full Immich URL, such as http://nas:2283.")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.path().trim_matches('/').is_empty()
        {
            return Err(
                "Use the Immich server URL without a path, password, query or fragment.".into(),
            );
        }
        if settings.api_key.trim().is_empty() || settings.api_key.contains(['\r', '\n']) {
            return Err(
                "Enter an admin API key with library.read and library.update permissions.".into(),
            );
        }
        Ok(Self {
            base: url.as_str().trim_end_matches('/').into(),
            key: settings.api_key.clone(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(20))
                .redirects(0)
                .build(),
        })
    }

    fn endpoint(&self, method: &str, path: &str) -> ureq::Request {
        self.agent
            .request(method, &format!("{}/api{path}", self.base))
            .set("x-api-key", &self.key)
    }
    fn id(id: &str) -> Result<&str, String> {
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err("Choose an external library first.".into());
        }
        Ok(id)
    }
    pub fn libraries(&self) -> Result<Vec<Library>, String> {
        self.endpoint("GET", "/libraries")
            .call()
            .map_err(message)?
            .into_json()
            .map_err(|_| "Immich returned an unreadable library list.".into())
    }
    pub fn scan(&self, id: &str) -> Result<(), String> {
        self.endpoint("POST", &format!("/libraries/{}/scan", Self::id(id)?))
            .call()
            .map_err(message)?;
        Ok(())
    }
}

fn message(error: ureq::Error) -> String {
    // Don't echo response bodies, URLs or request headers (which may contain secrets).
    match error {
        ureq::Error::Status(401, _) => "Immich didn't accept this API key. Use an admin key with library.read and library.update permissions.".into(),
        ureq::Error::Status(403, _) => "Immich library settings require an admin API key with library.read and library.update permissions.".into(),
        ureq::Error::Status(code, _) => format!("Immich returned HTTP {code}. Check the server and external library."),
        _ => "Couldn't reach Immich. Check its URL and connection.".into(),
    }
}

#[cfg(test)]
mod tests;
