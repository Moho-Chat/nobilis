//! Rocket.Chat Message Reporting & Moderation Queue:
//! - Report message (`POST /api/v1/chat.reportMessage`)
//! - List reports by users (`GET /api/v1/moderation.reportsByUsers`)
//! - Dismiss reports (`POST /api/v1/moderation.dismissReports`)
//! - Delete reported messages (`POST /api/v1/moderation.user.deleteReportedMessages`)

use super::Handle;
use anyhow::Result;
use serde_json::{json, Value};

pub async fn report_message(handle: &Handle, msg_id: &str, description: &str) -> Result<Value> {
    Ok(handle
        .api
        .post(
            "/api/v1/chat.reportMessage",
            &json!({ "messageId": msg_id, "description": description }),
        )
        .await?)
}

pub async fn list_reports_by_users(handle: &Handle, count: usize) -> Result<Value> {
    Ok(handle
        .api
        .get("/api/v1/moderation.reportsByUsers", &[("count", count.to_string())])
        .await?)
}

pub async fn dismiss_reports(handle: &Handle, user_id: &str) -> Result<Value> {
    Ok(handle
        .api
        .post("/api/v1/moderation.dismissReports", &json!({ "userId": user_id }))
        .await?)
}

pub async fn delete_reported_messages(handle: &Handle, user_id: &str) -> Result<Value> {
    Ok(handle
        .api
        .post("/api/v1/moderation.user.deleteReportedMessages", &json!({ "userId": user_id }))
        .await?)
}
