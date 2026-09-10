// Sentinel CameraNode - Camera streaming node for Sentinel Command Center
// Copyright (C) 2026  SourceBox LLC
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//! Connection validation for CameraNode setup
//!
//! Validates API URL, Node ID, and API Key before saving configuration

use anyhow::Result;
use reqwest::Client;
use std::time::Duration;

/// Result of connection validation
pub struct ValidationResult {
    pub is_valid: bool,
    pub error_message: Option<String>,
    pub backend_version: Option<String>,
    pub node_name: Option<String>,
}

/// Validate connection to Command Center
///
/// Tests:
/// 1. API URL is reachable
/// 2. Node ID exists in database
/// 3. API Key is valid for this node
pub async fn validate_api_connection(
    api_url: &str,
    node_id: &str,
    api_key: &str,
) -> Result<ValidationResult> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .danger_accept_invalid_certs(false)
        .build()?;

    // Step 1: Check if URL is reachable (health endpoint)
    let health_url = format!("{}/api/health", api_url.trim_end_matches('/'));
    
    tracing::info!("[Validator] Checking health at: {}", health_url);
    
    let health_response = client
        .get(&health_url)
        .send()
        .await;

    if let Err(e) = health_response {
        let error_msg = format!(
            "Cannot reach Command Center\n  → URL: {}\n  → Error: {}\n  → Check your internet connection\n  → Verify the API URL is correct",
            api_url,
            e.to_string().lines().next().unwrap_or("Unknown error")
        );
        
        tracing::warn!("[Validator] Health check failed: {}", e);
        
        return Ok(ValidationResult {
            is_valid: false,
            error_message: Some(error_msg),
            backend_version: None,
            node_name: None,
        });
    }

    // Step 2: Validate Node ID and API Key
    let validate_url = format!("{}/api/nodes/validate", api_url.trim_end_matches('/'));
    
    tracing::info!("[Validator] Validating credentials at: {}", validate_url);
    
    let validate_response = client
        .post(&validate_url)
        .header("X-Node-API-Key", api_key)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "node_id": node_id
        }))
        .send()
        .await;

    match validate_response {
        Ok(response) => {
            let status = response.status();
            
            if status.is_success() {
                // Parse response to get node info
                let body = response.json::<serde_json::Value>().await
                    .unwrap_or_else(|_| serde_json::json!({}));
                
                let node_name = body.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                
                tracing::info!("[Validator] Credentials valid for node: {:?}", node_name);
                
                Ok(ValidationResult {
                    is_valid: true,
                    error_message: None,
                    backend_version: None,
                    node_name,
                })
            } else if status.as_u16() == 404 {
                let error_msg = format!(
                    "Node ID '{}' not found\n  → Go to Command Center\n  → Settings → Nodes → Add Node\n  → Create a new node and copy its ID",
                    node_id
                );
                
                tracing::warn!("[Validator] Node not found: {}", node_id);
                
                Ok(ValidationResult {
                    is_valid: false,
                    error_message: Some(error_msg),
                    backend_version: None,
                    node_name: None,
                })
            } else if status.as_u16() == 401 || status.as_u16() == 403 {
                let error_msg = format!(
                    "Invalid API Key for node '{}'\n  → Copy the API key from:\n  → Command Center → Settings → Nodes → [Your Node]\n  → Make sure you copied the entire key",
                    node_id
                );
                
                tracing::warn!("[Validator] Invalid API key for node: {}", node_id);
                
                Ok(ValidationResult {
                    is_valid: false,
                    error_message: Some(error_msg),
                    backend_version: None,
                    node_name: None,
                })
            } else {
                let error_body = response.text().await.unwrap_or_default();
                let error_msg = format!(
                    "Server error ({}): {}\n  → Try again in a moment\n  → If this persists, check Command Center logs",
                    status,
                    error_body.lines().next().unwrap_or("Unknown error")
                );
                
                tracing::error!("[Validator] Server error {}: {}", status, error_body);
                
                Ok(ValidationResult {
                    is_valid: false,
                    error_message: Some(error_msg),
                    backend_version: None,
                    node_name: None,
                })
            }
        }
        Err(e) => {
            let error_msg = format!(
                "Failed to connect to Command Center\n  → Error: {}\n  → Check your network connection\n  → Verify the URL: {}",
                e.to_string().lines().next().unwrap_or("Unknown error"),
                api_url
            );
            
            tracing::error!("[Validator] Connection error: {}", e);
            
            Ok(ValidationResult {
                is_valid: false,
                error_message: Some(error_msg),
                backend_version: None,
                node_name: None,
            })
        }
    }
}