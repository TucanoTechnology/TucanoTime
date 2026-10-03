//! Notification port. Reminders (#22) and budget alerts (#30) produce
//! `Notification`s and hand them to a `NotificationSender`; the delivery
//! mechanism (in-app list now, email/push in Phase 6) is an adapter behind this
//! trait, so producers never depend on a transport.

/// A user-facing notification. `kind` is a stable tag consumers can filter on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Notification {
    pub kind: String,
    pub title: String,
    pub body: String,
}

impl Notification {
    pub fn new(kind: impl Into<String>, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            body: body.into(),
        }
    }
}

/// Delivers a notification. Implementations must not block the request path;
/// the default just logs it.
pub trait NotificationSender: Send + Sync {
    fn send(&self, notification: &Notification);
}

/// Default sender: structured log only. No external transport (that is a
/// Phase 6 adapter on this same seam).
#[derive(Debug, Default)]
pub struct LogNotifier;

impl NotificationSender for LogNotifier {
    fn send(&self, n: &Notification) {
        tracing::info!(kind = %n.kind, title = %n.title, "notification");
    }
}
