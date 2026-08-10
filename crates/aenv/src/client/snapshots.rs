use super::{handle_status, Client};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Final command context published with a snapshot. When supplied, the
/// server replaces the sandbox's captured context wholesale, so omitted
/// fields are stored as empty rather than merged.
#[derive(Debug, Default, Serialize)]
pub struct SnapshotFinalContext {
    #[serde(rename = "envVars", skip_serializing_if = "HashMap::is_empty")]
    pub env_vars: HashMap<String, String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workdir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
}

#[derive(Debug, Default, Serialize)]
pub struct CreateSnapshotRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    #[serde(rename = "finalContext", skip_serializing_if = "Option::is_none")]
    pub final_context: Option<&'a SnapshotFinalContext>,
    #[serde(rename = "startCmd", skip_serializing_if = "Option::is_none")]
    pub start_cmd: Option<&'a str>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SnapshotInfo {
    #[serde(rename = "snapshotID")]
    pub snapshot_id: String,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(rename = "imageRef", default, skip_serializing_if = "Option::is_none")]
    pub image_ref: Option<String>,
}

impl Client {
    pub fn create_snapshot(&self, sandbox_id: &str, name: Option<&str>) -> Result<SnapshotInfo> {
        self.create_snapshot_with_request(
            sandbox_id,
            &CreateSnapshotRequest {
                name,
                ..CreateSnapshotRequest::default()
            },
        )
    }

    pub fn create_snapshot_with_request(
        &self,
        sandbox_id: &str,
        request: &CreateSnapshotRequest<'_>,
    ) -> Result<SnapshotInfo> {
        let resp = handle_status(
            self.post(&format!("/sandboxes/{}/snapshots", sandbox_id))
                .send_json(request),
        )?;
        Ok(resp.into_json()?)
    }

    pub fn list_snapshots(&self, sandbox_id: Option<&str>) -> Result<Vec<SnapshotInfo>> {
        let mut snapshots = Vec::new();
        let mut next_token: Option<String> = None;

        loop {
            let mut request = self.get("/snapshots").query("limit", "100");
            if let Some(sandbox_id) = sandbox_id {
                request = request.query("sandboxID", sandbox_id);
            }
            if let Some(token) = next_token.as_deref() {
                request = request.query("nextToken", token);
            }

            let resp = handle_status(request.call())?;
            next_token = resp
                .header("x-next-token")
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_string);
            let mut page: Vec<SnapshotInfo> = resp.into_json()?;
            snapshots.append(&mut page);

            if next_token.is_none() {
                break;
            }
        }

        Ok(snapshots)
    }
}

#[cfg(test)]
mod tests {
    use super::{CreateSnapshotRequest, SnapshotFinalContext, SnapshotInfo};
    use std::collections::HashMap;

    #[test]
    fn create_snapshot_serializes_optional_name() {
        let named = serde_json::to_value(CreateSnapshotRequest {
            name: Some("base"),
            ..CreateSnapshotRequest::default()
        })
        .unwrap();
        assert_eq!(named["name"], "base");

        let unnamed = serde_json::to_value(CreateSnapshotRequest::default()).unwrap();
        assert_eq!(unnamed, serde_json::json!({}));
    }

    #[test]
    fn create_snapshot_serializes_final_context_and_start_cmd() {
        let context = SnapshotFinalContext {
            env_vars: HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            workdir: "/app".to_string(),
            user: Some("builder".to_string()),
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: None,
        };
        let value = serde_json::to_value(CreateSnapshotRequest {
            name: Some("my-template"),
            final_context: Some(&context),
            start_cmd: Some("/bin/app serve"),
        })
        .unwrap();

        assert_eq!(value["name"], "my-template");
        assert_eq!(value["startCmd"], "/bin/app serve");
        assert_eq!(value["finalContext"]["workdir"], "/app");
        assert_eq!(value["finalContext"]["user"], "builder");
        assert_eq!(value["finalContext"]["envVars"]["PATH"], "/usr/bin");
        assert_eq!(value["finalContext"]["entrypoint"][0], "/bin/app");
        assert!(value["finalContext"].get("cmd").is_none());
    }

    #[test]
    fn snapshot_info_supports_optional_image_ref() {
        let with_ref: SnapshotInfo = serde_json::from_value(serde_json::json!({
            "snapshotID": "snap-1",
            "names": [],
            "imageRef": "registry.example/ns/app:agentenv-snapshot-snap-1"
        }))
        .unwrap();
        assert_eq!(
            with_ref.image_ref.as_deref(),
            Some("registry.example/ns/app:agentenv-snapshot-snap-1")
        );

        let without_ref: SnapshotInfo = serde_json::from_value(serde_json::json!({
            "snapshotID": "snap-2",
            "names": []
        }))
        .unwrap();
        assert_eq!(without_ref.image_ref, None);
        assert!(serde_json::to_value(without_ref)
            .unwrap()
            .get("imageRef")
            .is_none());
    }
}
