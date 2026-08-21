use serde_json::Value;
use tokio::sync::broadcast;

/// A push event, matching the wire contract's `{"event":..,"data":..}` shape.
/// `message` and `presenceChange` are only delivered to clients subscribed to
/// the buffer named in `data.bufferId` - everything else broadcasts to every
/// connected client. See daemon/nobilis/api.c's api_event_sink for the
/// original scoping rule this replicates.
#[derive(Clone, Debug)]
pub struct Event {
    pub name: &'static str,
    pub data: Value,
}

impl Event {
    pub fn is_scoped(&self) -> bool {
        matches!(self.name, "message" | "presenceChange" | "messageUpdated" | "messageDeleted" | "reactionsChanged")
    }

    pub fn buffer_id(&self) -> Option<&str> {
        self.data.get("bufferId").and_then(|v| v.as_str())
    }
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        // Bounded channel: a slow/stalled client shouldn't grow memory
        // unboundedly. Lagged receivers just skip ahead - acceptable for
        // live event delivery (backlog is what getBacklog is for).
        let (tx, _rx) = broadcast::channel(1024);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn emit(&self, name: &'static str, data: Value) {
        let _ = self.tx.send(Event { name, data });
    }
}
