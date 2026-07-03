// SPDX-License-Identifier: AGPL-3.0-or-later

// src/video/backend/comfyui.rs
//! Local ComfyUI **video** backend. Same `/prompt` → poll `/history` → `/view`
//! flow as the image ComfyUI backend, but driven by a user-supplied video
//! workflow (Wan / AnimateDiff / SVD ending in a video-combine node). There's
//! no universal default video workflow, so `workflow_json` is required; tokens:
//! `{{prompt}}` `{{negative}}` `{{seed}}` `{{width}}` `{{height}}` `{{frames}}`
//! `{{fps}}` `{{steps}}` `{{cfg}}` `{{ckpt}}`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::super::{VideoBackend, VideoError, VideoOutput, VideoRequest};
use crate::config::VideoComfyUiConfig;

const GEN_TIMEOUT: Duration = Duration::from_secs(900); // video is slow
const POLL_INTERVAL: Duration = Duration::from_millis(2000);

/// Turn a ComfyUI `/prompt` error body into a readable one-liner. ComfyUI
/// returns `{error, node_errors:{ "<id>": {class_type, errors:[{message,
/// details}]} }}`; we flatten the per-node errors (`node 37 (UNETLoader):
/// Value not in list: unet_name '' not in [...]`) so the user sees exactly
/// which node/input the render service rejected. Falls back to a truncated
/// raw body when the shape isn't what we expect.
fn summarise_comfy_error(body: &str) -> String {
    if let Ok(j) = serde_json::from_str::<Value>(body) {
        let mut parts: Vec<String> = Vec::new();
        if let Some(nodes) = j.get("node_errors").and_then(Value::as_object) {
            for (nid, err) in nodes {
                let class = err.get("class_type").and_then(Value::as_str).unwrap_or("?");
                for e in err.get("errors").and_then(Value::as_array).into_iter().flatten() {
                    let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
                    let det = e.get("details").and_then(Value::as_str).unwrap_or("");
                    parts.push(if det.is_empty() {
                        format!("node {nid} ({class}): {msg}")
                    } else {
                        format!("node {nid} ({class}): {msg}: {det}")
                    });
                }
            }
        }
        if parts.is_empty() {
            if let Some(msg) = j.get("error").and_then(|e| e.get("message")).and_then(Value::as_str) {
                parts.push(msg.to_string());
            }
        }
        if !parts.is_empty() {
            return parts.join("; ").chars().take(600).collect();
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() { "no response body".into() }
    else { trimmed.chars().take(400).collect() }
}

pub struct ComfyUiVideoBackend {
    base_url:        String,
    workflow_json:   String,
    model:           String,
    steps:           u32,
    cfg_scale:       f32,
    width:           u32,
    height:          u32,
    fps:             u32,
    negative_prompt: String,
    http:            reqwest::Client,
}

impl ComfyUiVideoBackend {
    pub fn new(cfg: &VideoComfyUiConfig) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap_or_default();
        Self {
            base_url:        cfg.base_url.trim().trim_end_matches('/').to_string(),
            workflow_json:   cfg.workflow_json.clone(),
            model:           cfg.model.trim().to_string(),
            steps:           cfg.steps.max(1),
            cfg_scale:       cfg.cfg_scale,
            width:           cfg.width.max(64),
            height:          cfg.height.max(64),
            fps:             cfg.fps.max(1),
            negative_prompt: cfg.negative_prompt.clone(),
            http,
        }
    }

    fn build_workflow(&self, req: &VideoRequest) -> Result<Value, VideoError> {
        let width  = if req.width  > 0 { req.width }  else { self.width };
        let height = if req.height > 0 { req.height } else { self.height };
        let frames = (req.seconds.max(1) * self.fps).max(1);
        let negative = req.negative_prompt.clone().filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.negative_prompt.clone());
        let seed = req.seed.filter(|s| *s >= 0).unwrap_or_else(|| rand::random::<u32>() as i64);
        let ckpt = req.model.as_deref().filter(|s| !s.is_empty())
            .map(str::to_string).unwrap_or_else(|| self.model.clone());

        let jstr = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
        let out = self.workflow_json
            .replace("\"{{prompt}}\"",   &jstr(&req.prompt))
            .replace("\"{{negative}}\"", &jstr(&negative))
            .replace("\"{{ckpt}}\"",     &jstr(&ckpt))
            .replace("{{seed}}",   &seed.to_string())
            .replace("{{steps}}",  &self.steps.to_string())
            .replace("{{cfg}}",    &self.cfg_scale.to_string())
            .replace("{{width}}",  &width.to_string())
            .replace("{{height}}", &height.to_string())
            .replace("{{frames}}", &frames.to_string())
            .replace("{{fps}}",    &self.fps.to_string());
        serde_json::from_str(&out)
            .map_err(|e| VideoError::Backend(format!("workflow_json invalid after substitution: {e}")))
    }
}

#[async_trait]
impl VideoBackend for ComfyUiVideoBackend {
    fn name(&self) -> &'static str { "comfyui" }
    // Needs both a server and a workflow (no default video workflow exists).
    fn enabled(&self) -> bool { !self.base_url.is_empty() && !self.workflow_json.trim().is_empty() }

    async fn generate(&self, req: &VideoRequest) -> Result<VideoOutput, VideoError> {
        let workflow = self.build_workflow(req)?;
        let client_id = uuid::Uuid::new_v4().to_string();

        let resp = self.http.post(format!("{}/prompt", self.base_url))
            .json(&json!({ "prompt": workflow, "client_id": client_id }))
            .send().await
            .map_err(|e| VideoError::Backend(format!("ComfyUI /prompt failed: {}", e.without_url())))?;
        let status = resp.status();
        if !status.is_success() {
            // ComfyUI puts the *actual* validation reason (e.g. an unknown
            // checkpoint name from an unset {{ckpt}}, a bad value, a missing
            // node) in the response body's `node_errors` — `error_for_status`
            // discards it, leaving an unactionable bare "400". Surface it.
            let body = resp.text().await.unwrap_or_default();
            return Err(VideoError::Backend(format!(
                "ComfyUI rejected the workflow ({status}): {}", summarise_comfy_error(&body))));
        }
        let enqueue: Value = resp.json().await
            .map_err(|e| VideoError::Backend(format!("/prompt parse: {e}")))?;
        let prompt_id = enqueue.get("prompt_id").and_then(Value::as_str)
            .ok_or_else(|| VideoError::Backend("/prompt returned no prompt_id".into()))?.to_string();

        let start = Instant::now();
        let (filename, subfolder, ftype) = loop {
            if start.elapsed() > GEN_TIMEOUT { return Err(VideoError::Timeout(GEN_TIMEOUT.as_secs())); }
            tokio::time::sleep(POLL_INTERVAL).await;
            let hist: Value = match self.http.get(format!("{}/history/{prompt_id}", self.base_url))
                .send().await.and_then(|r| r.error_for_status())
            {
                Ok(r) => r.json().await.map_err(|e| VideoError::Backend(format!("history parse: {e}")))?,
                Err(_) => continue,
            };
            let Some(entry) = hist.get(&prompt_id) else { continue };
            if entry.pointer("/status/status_str").and_then(Value::as_str) == Some("error") {
                let msg = entry.pointer("/status/messages").map(|m| m.to_string())
                    .unwrap_or_else(|| "workflow execution error".into());
                return Err(VideoError::Backend(format!("ComfyUI workflow error: {}",
                    msg.chars().take(300).collect::<String>())));
            }
            if let Some(outputs) = entry.get("outputs").and_then(Value::as_object) {
                // Video-combine nodes emit under "gifs"; some emit "images" or "videos".
                let item = outputs.values()
                    .filter_map(|n| ["gifs", "videos", "images"].iter()
                        .find_map(|k| n.get(*k).and_then(Value::as_array)))
                    .flatten()
                    .find(|i| i.get("filename").is_some());
                if let Some(it) = item {
                    break (
                        it.get("filename").and_then(Value::as_str).unwrap_or_default().to_string(),
                        it.get("subfolder").and_then(Value::as_str).unwrap_or_default().to_string(),
                        it.get("type").and_then(Value::as_str).unwrap_or("output").to_string(),
                    );
                }
            }
        };
        if filename.is_empty() {
            return Err(VideoError::Backend("ComfyUI produced no video output".into()));
        }

        let bytes = self.http.get(format!("{}/view", self.base_url))
            .query(&[("filename", filename.as_str()), ("subfolder", subfolder.as_str()), ("type", ftype.as_str())])
            .send().await
            .map_err(|e| VideoError::Backend(format!("/view failed: {}", e.without_url())))?
            .error_for_status()
            .map_err(|e| VideoError::Backend(format!("/view error: {}", e.without_url())))?
            .bytes().await
            .map_err(|e| VideoError::Backend(format!("/view read: {e}")))?
            .to_vec();
        let ext = filename.rsplit('.').next()
            .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("mp4").to_lowercase();
        Ok(VideoOutput { bytes, ext, note: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarise_comfy_error_flattens_node_errors() {
        // The real body ComfyUI returns for an empty {{ckpt}} (unset Model).
        let body = r#"{"error":{"type":"prompt_outputs_failed_validation","message":"Prompt outputs failed validation","details":"","extra_info":{}},"node_errors":{"37":{"class_type":"UNETLoader","errors":[{"type":"value_not_in_list","message":"Value not in list","details":"unet_name: '' not in ['wan2.1_t2v_1.3B_fp16.safetensors']","extra_info":{}}]}}}"#;
        let s = summarise_comfy_error(body);
        assert!(s.contains("node 37 (UNETLoader)"), "got: {s}");
        assert!(s.contains("Value not in list"), "got: {s}");
        assert!(s.contains("unet_name"), "got: {s}");
    }

    #[test]
    fn summarise_comfy_error_falls_back_to_raw_body() {
        assert_eq!(summarise_comfy_error(""), "no response body");
        assert_eq!(summarise_comfy_error("plain text boom"), "plain text boom");
    }

    // Real end-to-end against a local ComfyUI running a Wan T2V workflow.
    // Ignored (needs the server + GPU + the workflow file). Run with:
    //   COMFYUI_URL=http://windows-host:8188 WAN_WORKFLOW=/tmp/wan_t2v_template.json \
    //     cargo test --lib video::backend::comfyui::tests::generates -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn generates_video_against_local_comfyui() {
        let base = std::env::var("COMFYUI_URL").unwrap_or_else(|_| "http://windows-host:8188".into());
        let workflow = std::fs::read_to_string(
            std::env::var("WAN_WORKFLOW").expect("set WAN_WORKFLOW to the template path")
        ).expect("read workflow template");
        let cfg = VideoComfyUiConfig {
            enabled: true,
            base_url: base,
            workflow_json: workflow,
            model: String::new(), // ckpt is literal in this workflow
            steps: 30,
            width: 832, height: 480, fps: 16,
            cfg_scale: 6.0,
            negative_prompt: "blurry, low quality".into(),
        };
        let be = ComfyUiVideoBackend::new(&cfg);
        assert!(be.enabled(), "needs base_url + workflow");
        let req = VideoRequest {
            prompt: "a red fox leaping through a sunlit autumn forest, cinematic".into(),
            negative_prompt: None, model: None, width: 832, height: 480, seconds: 2, seed: Some(7),
        };
        let out = be.generate(&req).await.expect("comfyui video generate");
        eprintln!("ComfyUI video → {} bytes, ext={}", out.bytes.len(), out.ext);
        assert!(out.bytes.len() > 10_000, "video should be non-trivial");
    }
}
