//! The Calls plugin: who is in a call in each channel, and joining one.

use super::Handle;
use crate::state::AppState;
use serde_json::Value;

/// Who is in a call where, at load.
pub async fn load(_state: &AppState, _handle: &Handle) {}

/// One of the plugin's websocket events.
pub fn event(_state: &AppState, _handle: &Handle, _kind: &str, _data: &Value, _broadcast: &Value) {}
