//! Email delivery port (#35). Invoice + reminder emails are rendered by pure
//! functions in this module and handed to an `EmailSender` adapter behind this
//! seam, mirroring `NotificationSender` (#18). The production adapter speaks
//! SMTP with credentials from the vault (#77) / env; tests use an in-memory
//! recorder, so nothing sends email in CI. The trait is synchronous like every
//! other port here; callers on the request path use `spawn_blocking`.
//!
//! Credentials: `smtp.host`, `smtp.port`, `smtp.username`, `smtp.password`,
//! `smtp.from` in the secret vault, or `TUCANO_SMTP_*` env vars. If none are
//! configured the app runs with `DisabledEmailSender` — emails are logged, not
//! sent — degrading safely exactly like the vault itself.

#[derive(Debug, Clone, PartialEq)]
pub struct EmailMessage {
    pub to: String,
    pub subject: String,
    /// Plain-text body (always present).
    pub text: String,
    /// Optional HTML part.
    pub html: Option<String>,
    /// Optional attachment filename + bytes.
    pub attachment: Option<(String, Vec<u8>)>,
}

#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    #[error("email transport failed: {0}")]
    Transport(String),
    #[error("recipient address is invalid")]
    Recipient,
}

/// Delivers an `EmailMessage`. Adapters must never log message bodies or
/// credentials.
pub trait EmailSender: Send + Sync {
    fn send(&self, msg: &EmailMessage) -> Result<(), EmailError>;
}

/// Sends nothing; logs at info. The default when SMTP is not configured.
#[derive(Debug, Default)]
pub struct DisabledEmailSender;

impl EmailSender for DisabledEmailSender {
    fn send(&self, msg: &EmailMessage) -> Result<(), EmailError> {
        tracing::info!(to = %msg.to, subject = %msg.subject, "email (SMTP not configured, not sent)");
        Ok(())
    }
}

/// Test/CI adapter: records messages in memory, validating the recipient.
#[derive(Debug, Default)]
pub struct RecordingEmailSender {
    sent: std::sync::Mutex<Vec<EmailMessage>>,
}

impl RecordingEmailSender {
    pub fn messages(&self) -> Vec<EmailMessage> {
        self.sent.lock().unwrap().clone()
    }
}

impl EmailSender for RecordingEmailSender {
    fn send(&self, msg: &EmailMessage) -> Result<(), EmailError> {
        if !valid_address(&msg.to) {
            return Err(EmailError::Recipient);
        }
        self.sent.lock().unwrap().push(msg.clone());
        Ok(())
    }
}

/// A deliberately small, conservative address check (not full RFC 5322):
/// one `@`, no spaces, non-empty local, dotted domain.
pub fn valid_address(addr: &str) -> bool {
    let addr = addr.trim();
    if addr.is_empty() || addr.contains(' ') {
        return false;
    }
    let mut parts = addr.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = match parts.next() {
        Some(d) => d,
        None => return false, // no '@'
    };
    if parts.next().is_some() {
        return false; // more than one '@'
    }
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
}

// ------------------------------------------------------------ SMTP adapter --

/// SMTP connection settings. `username` + `password`, when both present,
/// enable `AUTH LOGIN`. This is a plaintext client intended for a trusted
/// network — point it at a local submit-mail relay (postfix, msmtp, or a TLS
/// sidecar), which is the deployment model this file-based app assumes.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub from: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl SmtpConfig {
    /// Builds a config from vault keys (`smtp.*`) falling back to
    /// `TUCANO_SMTP_*` env vars. `None` when no host is configured.
    pub fn from_sources(vault: Option<&crate::vault::SecretVault>) -> Option<Self> {
        let get = |key: &str, env: &str| -> Option<String> {
            vault
                .and_then(|v| v.get(key))
                .or_else(|| std::env::var(env).ok())
        };
        let host = get("smtp.host", "TUCANO_SMTP_HOST")?;
        let port = get("smtp.port", "TUCANO_SMTP_PORT")
            .and_then(|p| p.parse().ok())
            .unwrap_or(25);
        let from =
            get("smtp.from", "TUCANO_SMTP_FROM").unwrap_or_else(|| "noreply@localhost".into());
        Some(Self {
            host,
            port,
            from,
            username: get("smtp.username", "TUCANO_SMTP_USERNAME"),
            password: get("smtp.password", "TUCANO_SMTP_PASSWORD"),
        })
    }
}

/// Production `EmailSender`: SMTP over a blocking TCP connection.
#[derive(Debug)]
pub struct SmtpEmailSender {
    config: SmtpConfig,
}

impl SmtpEmailSender {
    pub fn new(config: SmtpConfig) -> Self {
        Self { config }
    }
}

impl EmailSender for SmtpEmailSender {
    fn send(&self, msg: &EmailMessage) -> Result<(), EmailError> {
        if !valid_address(&msg.to) {
            return Err(EmailError::Recipient);
        }
        let stream = std::net::TcpStream::connect((self.config.host.as_str(), self.config.port))
            .map_err(|e| EmailError::Transport(e.to_string()))?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(15)))
            .ok();
        SmtpConn { stream }.run(&self.config, msg)
    }
}

struct SmtpConn {
    stream: std::net::TcpStream,
}

impl SmtpConn {
    /// Reads SMTP replies until the final `NNN ` line; returns its code.
    fn read_reply(&mut self) -> Result<u16, EmailError> {
        use std::io::Read;
        let mut buf: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = self
                .stream
                .read(&mut byte)
                .map_err(|e| EmailError::Transport(e.to_string()))?;
            if n == 0 {
                return Err(EmailError::Transport("connection closed".into()));
            }
            buf.push(byte[0]);
            // A reply is done at a CRLF that closes a line starting "NNN ".
            if buf.len() >= 5 && buf.ends_with(b"\r\n") {
                let lines: Vec<&[u8]> = buf
                    .split(|b| *b == b'\n')
                    .filter(|l| !l.is_empty())
                    .collect();
                if lines
                    .iter()
                    .all(|l| l.len() >= 4 && l[..3].iter().all(|c| c.is_ascii_digit()))
                    && let Some(last) = lines.last()
                    && last[3] == b' '
                {
                    break;
                }
            }
        }
        let code = std::str::from_utf8(&buf[..3])
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| EmailError::Transport("malformed reply".into()))?;
        Ok(code)
    }

    fn command(&mut self, line: &str, expect: &[u16]) -> Result<(), EmailError> {
        use std::io::Write;
        self.stream
            .write_all(line.as_bytes())
            .and_then(|_| self.stream.write_all(b"\r\n"))
            .and_then(|_| self.stream.flush())
            .map_err(|e| EmailError::Transport(e.to_string()))?;
        let code = self.read_reply()?;
        if expect.contains(&code) {
            Ok(())
        } else {
            Err(EmailError::Transport(format!(
                "unexpected reply {code} to {line}"
            )))
        }
    }

    fn run(&mut self, cfg: &SmtpConfig, msg: &EmailMessage) -> Result<(), EmailError> {
        self.read_reply()?; // 220 greeting
        self.command("EHLO tucanotime", &[250])?;
        if let (Some(u), Some(p)) = (&cfg.username, &cfg.password) {
            self.command("AUTH LOGIN", &[334])?;
            self.command(&auth_line(u), &[334])?;
            self.command(&auth_line(p), &[235])?;
        }
        self.command(&format!("MAIL FROM:<{}>", cfg.from), &[250])?;
        self.command(&format!("RCPT TO:<{}>", msg.to), &[250, 251])?;
        self.command("DATA", &[354])?;
        self.write_message(msg)?;
        self.command("QUIT", &[221])?;
        Ok(())
    }

    fn write_message(&mut self, msg: &EmailMessage) -> Result<(), EmailError> {
        use std::io::Write;
        let body = build_mime(msg);
        let stuffed: String = body
            .lines()
            .map(|l| {
                if l.starts_with('.') {
                    format!(".{l}")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\r\n");
        self.stream
            .write_all(stuffed.as_bytes())
            .and_then(|_| self.stream.write_all(b"\r\n.\r\n"))
            .and_then(|_| self.stream.flush())
            .map_err(|e| EmailError::Transport(e.to_string()))?;
        let code = self.read_reply()?;
        if code == 250 {
            Ok(())
        } else {
            Err(EmailError::Transport(format!("data rejected: {code}")))
        }
    }
}

fn auth_line(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s)
}

/// Serialises headers + body, multipart when an attachment is present.
pub fn build_mime(msg: &EmailMessage) -> String {
    let mut h = format!(
        "From: TucanoTime\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\n",
        msg.to,
        msg.subject,
        chrono::Utc::now().to_rfc2822(),
    );
    match (&msg.html, &msg.attachment) {
        (None, None) => {
            h.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
            h.push_str(&msg.text);
        }
        (Some(html), None) => {
            h.push_str("MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n");
            h.push_str(html);
        }
        _ => {
            h.push_str(
                "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"tt\"\r\n\r\n",
            );
            h.push_str("--tt\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n");
            h.push_str(&msg.text);
            if let Some(html) = &msg.html {
                h.push_str("\r\n--tt\r\nContent-Type: text/html; charset=utf-8\r\n\r\n");
                h.push_str(html);
            }
            if let Some((name, bytes)) = &msg.attachment {
                use base64::Engine;
                let enc = base64::engine::general_purpose::STANDARD.encode(bytes);
                // #113: an invoice PDF must be labelled as such — clients key
                // their preview/save behaviour off this type.
                let ctype = if name.to_ascii_lowercase().ends_with(".pdf") {
                    "application/pdf"
                } else {
                    "application/octet-stream"
                };
                h.push_str(&format!(
                    "\r\n--tt\r\nContent-Type: {ctype}\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n"
                ));
                for chunk in enc.as_bytes().chunks(76) {
                    h.push_str(&String::from_utf8_lossy(chunk));
                    h.push_str("\r\n");
                }
            }
            h.push_str("--tt--");
        }
    }
    h
}

// ------------------------------------------------------------- templates ---

/// Subject for an invoice email.
pub fn invoice_subject(number: &str, org: &str) -> String {
    format!("Invoice {number} from {org}")
}

/// Plain-text invoice cover email. `amount` is pre-formatted (e.g.
/// "1,200.00 EUR") so this stays locale-agnostic. `pdf_attached` tells the
/// customer the document rides along (#113).
pub fn render_invoice_email(
    customer: &str,
    number: &str,
    amount: &str,
    due: Option<&str>,
    org: &str,
    pdf_attached: bool,
) -> String {
    let mut s = format!("Hi {customer},\n\nPlease find invoice {number} for {amount}");
    if let Some(due) = due {
        s.push_str(&format!(", due {due}"));
    }
    s.push('.');
    if pdf_attached {
        s.push_str(" The PDF document is attached.");
    }
    s.push_str(&format!("\n\nThank you,\n{org}\n"));
    s
}

/// Plain-text overdue-payment reminder for a single invoice. `pdf_attached`
/// names the archived document riding along (#113).
pub fn render_reminder_email(
    customer: &str,
    number: &str,
    amount: &str,
    due: &str,
    days_over: i64,
    org: &str,
    pdf_attached: bool,
) -> String {
    let document = if pdf_attached {
        " A PDF copy of the invoice is attached."
    } else {
        ""
    };
    format!(
        "Hi {customer},\n\nThis is a reminder that invoice {number} for {amount} was due {due} \
         and is now {days_over} day(s) overdue.{document}\n\nIf you have already paid, please \
         disregard this notice.\n\nThank you,\n{org}\n"
    )
}

/// Should an overdue invoice get a reminder now, given a cadence in days since
/// the last reminder? Pure so the schedule is testable with a `FixedClock`.
pub fn due_for_reminder(
    last_reminder: Option<chrono::NaiveDate>,
    today: chrono::NaiveDate,
    cadence_days: i64,
) -> bool {
    match last_reminder {
        None => true,
        Some(last) => (today - last).num_days() >= cadence_days,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn address_validation() {
        assert!(valid_address("a@b.co"));
        assert!(valid_address("  a@b.co  "));
        assert!(!valid_address("no-at"));
        assert!(!valid_address("two@@b.co"));
        assert!(!valid_address("a b@c.co"));
        assert!(!valid_address("a@b"));
        assert!(!valid_address("a@b."));
        assert!(!valid_address(""));
    }

    #[test]
    fn recording_sender_rejects_bad_recipient() {
        let s = RecordingEmailSender::default();
        let ok = s.send(&EmailMessage {
            to: "x@y.co".into(),
            subject: "s".into(),
            text: "t".into(),
            html: None,
            attachment: None,
        });
        assert!(ok.is_ok());
        let bad = s.send(&EmailMessage {
            to: "nope".into(),
            subject: "s".into(),
            text: "t".into(),
            html: None,
            attachment: None,
        });
        assert!(matches!(bad, Err(EmailError::Recipient)));
        assert_eq!(s.messages().len(), 1);
    }

    #[test]
    fn templates_include_key_fields() {
        let inv = render_invoice_email(
            "ACME",
            "INV-1",
            "1,200.00 EUR",
            Some("2026-10-19"),
            "Tucano",
            true,
        );
        assert!(
            inv.contains("INV-1") && inv.contains("1,200.00 EUR") && inv.contains("2026-10-19")
        );
        let rem = render_reminder_email(
            "ACME",
            "INV-1",
            "1,200.00 EUR",
            "2026-10-19",
            12,
            "Tucano",
            true,
        );
        assert!(
            rem.contains("12 day(s) overdue") && rem.contains("INV-1") && rem.contains("PDF copy")
        );
    }

    #[test]
    fn reminder_cadence_gate() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        assert!(due_for_reminder(None, today, 7));
        let last = NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
        assert!(!due_for_reminder(Some(last), today, 7));
        let last = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        assert!(due_for_reminder(Some(last), today, 7));
    }

    #[test]
    fn mime_builds_multipart_with_attachment() {
        let m = EmailMessage {
            to: "x@y.co".into(),
            subject: "S".into(),
            text: "body".into(),
            html: None,
            attachment: Some(("inv.csv".into(), b"hi".to_vec())),
        };
        let mime = build_mime(&m);
        assert!(mime.contains("multipart/mixed"));
        assert!(mime.contains("attachment; filename=\"inv.csv\""));
        assert!(mime.contains("Content-Type: application/octet-stream"));
        assert!(mime.contains("aGk")); // base64("hi")
    }

    #[test]
    fn pdf_attachments_are_labelled_as_pdf() {
        let m = EmailMessage {
            to: "x@y.co".into(),
            subject: "S".into(),
            text: "body".into(),
            html: None,
            attachment: Some(("INV-0001.PDF".into(), b"%PDF-1.4".to_vec())),
        };
        let mime = build_mime(&m);
        assert!(mime.contains("Content-Type: application/pdf"), "{mime}");
    }
}
